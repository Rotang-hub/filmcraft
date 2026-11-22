//! Fork: the Auto Cut panel (Window ▸ Auto Cut).
//!
//! A row per person: their name, their view (video track) and their microphone (audio track).
//! **Make Auto Cut** measures the microphones on a worker thread, plans the cuts (whoever talks;
//! big moments shown from the view of whoever reacted first, with a build-up before) and builds
//! them on the "Auto Cut" track (`edit.autoCutPlace`; see `filmcraft_engine::auto_cut`).
//! **Fill In** guesses the rows: the people named in the Speaker Captions panel (names with a
//! head-caption preset; notes like "라스의 디스코드" are skipped), and each person's view as
//! the video track whose clips carry their name.
//!
//! Settings are kept in `auto_cut.json` beside the preferences. Automation ids:
//! `autoCut.fill`, `autoCut.add`, `autoCut.name.<n>`, `autoCut.video.<n>`, `autoCut.audio.<n>`,
//! `autoCut.remove.<n>`, `autoCut.minShot`, `autoCut.reactions`, `autoCut.buildup`, `autoCut.hold`,
//! `autoCut.sound.<n>`, `autoCut.voice.<A1…>`, `autoCut.markers`, `autoCut.muteViews`, `autoCut.trim`, `autoCut.minSilence`, `autoCut.pad`, `autoCut.motion`, `autoCut.keep`, `autoCut.learnFrom`, `autoCut.learn`, `autoCut.makeUncut`, `autoCut.useStyle`, `autoCut.forgetStyle`, `autoCut.make`, `autoCut.cancel`.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Rect, RichText, vec2};
use filmcraft_engine::auto_cut::{self as ac, Reaction, Shot};
use filmcraft_engine::auto_cut_motion as motion;
use filmcraft_engine::auto_cut_style::{Labels, Style, labels, learn as learn_style, sequence_by, styled_cuts};
use filmcraft_engine::track_views::{audio_name, video_name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::FilmcraftApp;

pub const FILE: &str = "auto_cut.json";
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_PEOPLE: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Row {
    pub name: String,
    /// "V2", "A4": by position.
    pub video: String,
    pub audio: String,
    /// Their view's game sound track; empty: found from the view clip's links.
    pub sound: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub people: Vec<Row>,
    /// Other voice tracks (Discord recordings): kept whole, and talk on them is never cut.
    pub voices: Vec<String>,
    pub min_shot: f64,
    pub buildup: f64,
    pub hold: f64,
    pub reactions: bool,
    pub markers: bool,
    pub mute_views: bool,
    pub trim: bool,
    pub min_silence: f64,
    pub pad: f64,
    /// Percent of the length to keep once the dull parts go (100: off).
    pub keep_percent: f64,
    /// Look at how busy each view is, too (slower).
    pub motion: bool,
    /// The edited sequence to learn from (its name).
    pub learn_from: String,
    /// The learned style (`auto_cut_style::Style` as JSON).
    pub style: Option<Value>,
    /// Cut in the learned style.
    pub use_style: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let t = ac::Tuning::default();
        Self {
            people: Vec::new(),
            voices: Vec::new(),
            min_shot: t.min_shot,
            buildup: t.buildup,
            hold: t.hold,
            reactions: true,
            markers: true,
            mute_views: true,
            trim: t.trim,
            min_silence: t.min_silence,
            pad: t.pad,
            keep_percent: t.keep * 100.0,
            motion: true,
            learn_from: String::new(),
            style: None,
            use_style: true,
        }
    }
}

impl Settings {
    fn load(path: Option<&Path>) -> Self {
        use std::io::Read as _;
        let read = |p: &Path| -> Option<Settings> {
            let mut bytes = Vec::new();
            std::fs::File::open(p).ok()?.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        };
        let mut s = path.and_then(read).unwrap_or_default();
        s.people.truncate(MAX_PEOPLE);
        s.voices.truncate(64);
        s.keep_percent = if s.keep_percent.is_finite() { s.keep_percent.clamp(30.0, 100.0) } else { 80.0 };
        for v in [&mut s.min_shot, &mut s.buildup, &mut s.hold, &mut s.min_silence, &mut s.pad] {
            *v = if v.is_finite() { v.clamp(0.0, 30.0) } else { 0.0 };
        }
        s
    }

    fn request(&self) -> Value {
        let people: Vec<Value> = self
            .people
            .iter()
            .filter(|r| !r.video.is_empty() && !r.audio.is_empty())
            .map(|r| json!({"name": if r.name.trim().is_empty() { r.video.clone() } else { r.name.trim().to_string() }, "video": r.video, "audio": r.audio, "sound": r.sound}))
            .collect();
        json!({"people": people, "voices": self.voices, "minShot": self.min_shot, "buildup": self.buildup, "hold": self.hold, "reactions": self.reactions, "markers": self.markers, "muteViews": self.mute_views, "trim": self.trim, "minSilence": self.min_silence, "pad": self.pad, "keep": self.keep_percent / 100.0, "motion": self.motion})
    }
}

/// What the worker found: shots, big moments, silences to cut, and how much each person talks.
type Planned = (Vec<Shot>, Vec<Reaction>, Cuts, Vec<f32>);

/// Silences and dull parts to cut, in frames.
#[derive(Clone, Debug, Default)]
struct Cuts {
    silences: Vec<(usize, usize)>,
    dull: Vec<(usize, usize)>,
}

#[derive(Default)]
struct Job {
    label: String,
    progress: f32,
    cancel: AtomicBool,
    done: Option<Result<Outcome, String>>,
}

#[derive(Clone, Default)]
struct State {
    settings: Settings,
    path: Option<PathBuf>,
    job: Option<Arc<Mutex<Job>>>,
    /// The request the running job was started with.
    request: Option<Value>,
}

fn state_id() -> egui::Id {
    egui::Id::unique("filmcraft-auto-cut")
}

fn lock(job: &Arc<Mutex<Job>>) -> std::sync::MutexGuard<'_, Job> {
    job.lock().unwrap_or_else(PoisonError::into_inner)
}

fn state(app: &FilmcraftApp, ctx: &egui::Context) -> State {
    if let Some(s) = ctx.data(|d| d.get_temp::<State>(state_id())) {
        return s;
    }
    let path = app.session.prefs_path.as_ref().and_then(|p| p.parent()).map(|d| d.join(FILE));
    State { settings: Settings::load(path.as_deref()), path, ..State::default() }
}

/// Rows guessed from the Speaker Captions names and the clip names on the video tracks.
fn fill_in(app: &FilmcraftApp) -> (Vec<Row>, Vec<String>) {
    let Some(q) = app.session.active_sequence() else { return (Vec::new(), Vec::new()) };
    let sc_path = app.session.prefs_path.as_ref().and_then(|p| p.parent()).map(|d| d.join(super::speaker_captions::FILE));
    let sc = super::speaker_captions::Settings::load(sc_path.as_deref());
    // only people (a name with a head-caption preset); notes such as "라스의 디스코드" are skipped
    let mut rows: Vec<Row> = sc
        .names
        .iter()
        .filter(|(_, name)| filmcraft_engine::speaker_captions::template_for(&app.session, name.trim(), None, Some(&sc.template_dir)).is_ok())
        .map(|(audio, name)| {
            // the video track whose clips carry the person's name (most clips wins)
            let video = q
                .video_tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.name.starts_with(ac::TRACK_NAME) && !t.name.starts_with(filmcraft_engine::speaker_captions::TRACK_NAME))
                .map(|(i, t)| (t.items.iter().filter(|it| it.name.contains(name.trim())).count(), i))
                .filter(|(n, _)| *n > 0)
                .max()
                .map(|(_, i)| video_name(i))
                .unwrap_or_default();
            Row { name: name.clone(), video, audio: audio.clone(), sound: String::new() }
        })
        .collect();
    rows.sort_by_key(|r| r.audio.trim_start_matches('A').parse::<usize>().unwrap_or(usize::MAX));
    rows.truncate(MAX_PEOPLE);
    // the notes ("로탱의 디스코드"): other voices, kept whole
    let voices: Vec<String> =
        sc.names.iter().filter(|(audio, name)| !name.trim().is_empty() && !rows.iter().any(|r| &r.audio == *audio)).map(|(audio, _)| audio.clone()).collect();
    (rows, voices)
}

/// What a job is for.
enum Mode {
    /// Make the cut (in the learned style when given).
    Cut(Option<Style>),
    /// Learn from an edit of the active sequence.
    Learn(Labels, Option<Style>),
}

fn start(app: &mut FilmcraftApp, st: &Settings, mode: Mode) -> Result<(Arc<Mutex<Job>>, Value), String> {
    let req = st.request();
    let people = ac::people_p(&app.session, &req).map_err(|e| e.to_string())?;
    let range = ac::range_p(&app.session, &req).map_err(|e| e.to_string())?;
    if ac::level_jobs(&app.session, &people, range).map_err(|e| e.to_string())?.is_empty() {
        return Err(tl!("The microphone tracks have no clips").into());
    }
    let extra = ac::extra_p(&app.session, &req, &people).map_err(|e| e.to_string())?;
    let (jobs, rows) = ac::all_level_jobs(&app.session, &people, &extra, range).map_err(|e| e.to_string())?;
    let style_now = match &mode {
        Mode::Cut(s) => s.as_ref(),
        Mode::Learn(..) => None,
    };
    let tuning = ac::styled_tuning(ac::tuning_p(&req), style_now);
    let keep = ac::keep_ranges(&app.session, range, &tuning);
    let n = people.len();
    let look = st.motion && (tuning.trim || matches!(mode, Mode::Learn(..)));
    let views = if look { motion::motion_jobs(&app.session, &people, range).map_err(|e| e.to_string())? } else { Vec::new() };
    let job = Arc::new(Mutex::new(Job { label: tl!("Listening to the microphones…").into(), ..Job::default() }));
    let j = job.clone();
    let spawned = std::thread::Builder::new().name("auto-cut".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Outcome, String> {
            let lv = ac::levels(&jobs, rows, range, &mut |done, total| {
                let mut g = lock(&j);
                g.progress = done as f32 / total.max(1) as f32;
                !g.cancel.load(Ordering::Relaxed)
            })
            .map_err(|e| e.to_string())?;
            let mut heard = ac::Heard::split(&lv, n, &extra);
            let (shots, reactions) = ac::plan(heard.people, &tuning);
            let m = if views.is_empty() {
                Vec::new()
            } else {
                lock(&j).label = tl!("Watching the views…").into();
                motion::motion(&views, n, range, &mut |done, total| {
                    let mut g = lock(&j);
                    g.progress = done as f32 / total.max(1) as f32;
                    !g.cancel.load(Ordering::Relaxed)
                })
                .map_err(|e| e.to_string())?
            };
            match mode {
                Mode::Learn(labels, prior) => {
                    let style = learn_style(&heard, &m, &reactions, &labels, prior).map_err(|e| e.to_string())?;
                    Ok(Outcome::Learned(Box::new(style)))
                }
                Mode::Cut(style) => {
                    let on_screen = if m.is_empty() { Vec::new() } else { motion::shown(&m, &shots, ac::frame_count(range)) };
                    heard.motion = &on_screen;
                    let silences = ac::silences(&heard, &tuning, &reactions, &keep);
                    let dull = match &style {
                        Some(st) => styled_cuts(&heard, &m, &tuning, st, &reactions, &keep, &silences),
                        None => ac::dull_parts(&heard, &tuning, &reactions, &keep, &silences),
                    };
                    let cuts = Cuts { silences, dull };
                    Ok(Outcome::Cut((shots, reactions, cuts, ac::talk_share(heard.people))))
                }
            }
        }))
        .unwrap_or_else(|_| Err("the auto cut worker stopped unexpectedly".into()));
        lock(&j).done = Some(r);
    });
    if let Err(e) = spawned {
        return Err(e.to_string());
    }
    Ok((job, req))
}

/// Starts learning from `st.learn_from`, an edit of the active sequence.
fn start_learning(app: &mut FilmcraftApp, st: &Settings) -> Result<(Arc<Mutex<Job>>, Value), String> {
    let req = st.request();
    let people = ac::people_p(&app.session, &req).map_err(|e| e.to_string())?;
    let range = ac::range_p(&app.session, &req).map_err(|e| e.to_string())?;
    let raw_id = app.session.state.active_sequence.ok_or_else(|| tl!("No sequence").to_string())?;
    let edit_id = sequence_by(&app.session.project, Some(&json!(st.learn_from))).map_err(|e| e.to_string())?;
    if edit_id == raw_id {
        return Err(tl!("Pick the edited sequence, not the one that is open").into());
    }
    let (Some(raw), Some(edit)) = (app.session.project.sequence(raw_id), app.session.project.sequence(edit_id)) else {
        return Err(tl!("No sequence").into());
    };
    let labels = labels(raw, edit, &people, range);
    if !labels.kept.iter().any(|k| *k) {
        return Err(tl!("That edit uses none of this sequence's clips: open the uncut sequence it was cut from").into());
    }
    let prior = st.style.as_ref().and_then(|v| Style::from_value(v).ok());
    start(app, st, Mode::Learn(labels, prior))
}

/// What a finished job brings back.
enum Outcome {
    Cut(Planned),
    Learned(Box<Style>),
}

/// What was learned, in words.
fn style_text(v: &Value) -> String {
    let Ok(style) = Style::from_value(v) else { return String::new() };
    tlf!(
        "Learned from {n} edit(s): keeps {keep}% · shots about {shot} s · matches your edit {agree}% (without learning {before}%)",
        n = style.examples,
        keep = format!("{:.0}", style.keep() * 100.0),
        shot = format!("{:.1}", style.shot),
        agree = format!("{:.0}", style.agreement * 100.0),
        before = format!("{:.0}", style.before * 100.0)
    )
}

fn finish(app: &mut FilmcraftApp, result: Result<Planned, String>, req: Option<Value>) {
    let (shots, reactions, cuts, share) = match result {
        Ok(x) => x,
        Err(e) => {
            app.ui.status = e;
            return;
        }
    };
    let Some(mut req) = req else { return };
    // how much each microphone is heard talking: a check that the right tracks were picked
    let names: Vec<String> = req["people"].as_array().map(|a| a.iter().map(|p| p["name"].as_str().unwrap_or("?").to_string()).collect()).unwrap_or_default();
    let heard: Vec<String> = names.iter().zip(&share).map(|(n, s)| format!("{n} {:.0}%", s * 100.0)).collect();
    let odd: Vec<&String> = names.iter().zip(&share).filter(|(_, s)| **s > 0.7 || **s < 0.01).map(|(n, _)| n).collect();
    let (s, r) = ac::shots_json(&shots, &reactions);
    req["shots"] = s;
    req["reactions"] = r;
    let secs = |v: &[(usize, usize)]| v.iter().map(|(a, b)| b.saturating_sub(*a)).sum::<usize>() as f64 * ac::FRAME;
    let (quiet_secs, dull_secs) = (secs(&cuts.silences), secs(&cuts.dull));
    let all: Vec<(usize, usize)> = cuts.silences.iter().chain(&cuts.dull).copied().collect();
    req["cuts"] = ac::cuts_json(&all);
    let mut status = match app.session.execute("edit.autoCutPlace", req) {
        Ok(v) => {
            let mut m = tlf!("Auto Cut: {n} shots, {k} big moments marked", n = v["shots"].as_u64().unwrap_or(0), k = v["reactions"].as_u64().unwrap_or(0));
            let removed = v["removed"].as_f64().unwrap_or(0.0);
            if removed > 0.0 {
                m.push_str(&format!(
                    " · {}",
                    tlf!(
                        "removed {total} sec (silence {quiet} sec, dull parts {dull} sec)",
                        total = format!("{removed:.1}"),
                        quiet = format!("{quiet_secs:.1}"),
                        dull = format!("{dull_secs:.1}")
                    )
                ));
            }
            if v["soundTracks"].as_u64().unwrap_or(0) == 0 {
                m.push_str(&format!(" · {}", tl!("the views have no sound of their own linked (only microphones), so no Auto Cut audio track")));
            } else if let Some(names) = v["muted"].as_array().filter(|a| !a.is_empty()) {
                let list = names.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                m.push_str(&format!(" · {}", tlf!("muted the original view sound: {tracks}", tracks = list)));
            }
            m
        }
        Err(e) => e.to_string(),
    };
    status.push_str(&format!(" · {} {}", tl!("Heard talking:"), heard.join(", ")));
    if !odd.is_empty() {
        let list = odd.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ");
        status.push_str(&format!(" · {}", tlf!("check the microphone track of {who}", who = list)));
    }
    app.ui.status = status;
}

/// The game sound track, or "Auto" (empty): the view clip's linked sound.
fn sound_combo(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, value: &mut String, names: &[String]) {
    let shown = if value.is_empty() { tl!("Auto").to_string() } else { value.clone() };
    let r = craft_ui::ComboBox::from_id_salt(id.to_string()).width(64.0).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(value, String::new(), tl!("Auto"));
        for n in names {
            ui.selectable_value(value, n.clone(), n);
        }
    });
    app.auto.add_response(id, &r.response, id);
}

fn track_combo(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, value: &mut String, names: &[String]) {
    let shown = if value.is_empty() { "—".to_string() } else { value.clone() };
    let r = craft_ui::ComboBox::from_id_salt(id.to_string()).width(56.0).selected_text(shown).show_ui(ui, |ui| {
        for n in names {
            ui.selectable_value(value, n.clone(), n);
        }
    });
    app.auto.add_response(id, &r.response, id);
}

/// The Make button (or the running job's progress), kept at the top so it is always in view.
fn actions(app: &mut FilmcraftApp, ui: &mut egui::Ui, st: &mut State) {
    let t = app.tokens;
    match st.job.clone() {
        Some(job) => {
            let (label, p) = {
                let g = lock(&job);
                (g.label.clone(), g.progress)
            };
            ui.add(egui::ProgressBar::new(p).text(label).show_percentage());
            let r = ui.button(tl!("Cancel"));
            app.auto.add_response("autoCut.cancel", &r, tl!("Cancel"));
            if r.clicked() {
                lock(&job).cancel.store(true, Ordering::Relaxed);
            }
        }
        None => {
            let ready = st.settings.people.iter().filter(|r| !r.video.is_empty() && !r.audio.is_empty()).count() >= 2;
            let r = ui.add_enabled(ready, egui::Button::new(RichText::new(tl!("Make Auto Cut")).strong()).min_size(vec2(140.0, 26.0)));
            app.auto.add_response("autoCut.make", &r, tl!("Make Auto Cut"));
            if r.clicked() {
                let style = if st.settings.use_style { st.settings.style.as_ref().and_then(|v| Style::from_value(v).ok()) } else { None };
                match start(app, &st.settings, Mode::Cut(style)) {
                    Ok((job, req)) => {
                        st.job = Some(job);
                        st.request = Some(req);
                    }
                    Err(e) => app.ui.status = e,
                }
            }
            ui.label(
                RichText::new(tl!("The cut goes on its own \"Auto Cut\" track; your tracks are not changed. Running it again replaces it."))
                    .small()
                    .color(t.text_dim),
            );
        }
    }
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let mut st = state(app, ui.ctx());
    let before = st.settings.clone();
    if let Some(job) = st.job.clone() {
        let done = lock(&job).done.take();
        match done {
            Some(r) => {
                st.job = None;
                let req = st.request.take();
                match r {
                    Ok(Outcome::Cut(p)) => finish(app, Ok(p), req),
                    Ok(Outcome::Learned(style)) => match serde_json::to_value(&*style) {
                        Ok(v) => {
                            app.ui.status = style_text(&v);
                            st.settings.style = Some(v);
                            st.settings.use_style = true;
                        }
                        Err(e) => app.ui.status = e.to_string(),
                    },
                    Err(e) => finish(app, Err(e), req),
                }
            }
            None => ui.ctx().request_repaint_after(std::time::Duration::from_millis(200)),
        }
    }
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let ui = &mut child;
    actions(app, ui, &mut st);
    ui.separator();
    egui::ScrollArea::vertical().id_salt("auto-cut-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let Some((videos, audios)) = app
            .session
            .active_sequence()
            .map(|q| ((0..q.video_tracks.len()).map(video_name).collect::<Vec<_>>(), (0..q.audio_tracks.len()).map(audio_name).collect::<Vec<_>>()))
        else {
            ui.label(RichText::new(tl!("Open a sequence to make an auto cut.")).color(t.text_dim));
            return;
        };
        ui.label(RichText::new(tl!("People: their view and their microphone")).strong());
        ui.horizontal(|ui| {
            let r = ui.button(tl!("Fill In"));
            app.auto.add_response("autoCut.fill", &r, tl!("Fill In"));
            if r.clicked() {
                let (rows, voices) = fill_in(app);
                if rows.is_empty() {
                    app.ui.status = tl!("Name the people in the Speaker Captions panel first (who speaks on each audio track)").into();
                } else {
                    st.settings.people = rows;
                    st.settings.voices = voices;
                }
            }
            let r = ui.add_enabled(st.settings.people.len() < MAX_PEOPLE, egui::Button::new(tl!("Add Person")));
            app.auto.add_response("autoCut.add", &r, tl!("Add Person"));
            if r.clicked() {
                st.settings.people.push(Row::default());
            }
        });
        let mut remove = None;
        egui::Grid::new("auto-cut-people").num_columns(5).spacing([6.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new(tl!("Name")).small().color(t.text_dim));
            ui.label(RichText::new(tl!("View")).small().color(t.text_dim));
            ui.label(RichText::new(tl!("Mic")).small().color(t.text_dim));
            ui.label(RichText::new(tl!("Game sound")).small().color(t.text_dim));
            ui.label("");
            ui.end_row();
            for (i, row) in st.settings.people.iter_mut().enumerate() {
                let r = ui.add_sized(vec2(100.0, 20.0), egui::TextEdit::singleline(&mut row.name));
                app.auto.add_response(&format!("autoCut.name.{i}"), &r, tl!("Name"));
                track_combo(app, ui, &format!("autoCut.video.{i}"), &mut row.video, &videos);
                track_combo(app, ui, &format!("autoCut.audio.{i}"), &mut row.audio, &audios);
                sound_combo(app, ui, &format!("autoCut.sound.{i}"), &mut row.sound, &audios);
                let r = ui.small_button("✕");
                app.auto.add_response(&format!("autoCut.remove.{i}"), &r, "✕");
                if r.clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove
            && i < st.settings.people.len()
        {
            st.settings.people.remove(i);
        }
        // other voices: Discord recordings and anyone without a view; they play whole and their
        // talk is never cut
        ui.add_space(4.0);
        ui.label(RichText::new(tl!("Other voice tracks (Discord…): kept whole, their talk is never cut")).small().color(t.text_dim));
        ui.horizontal_wrapped(|ui| {
            let mics: Vec<String> = st.settings.people.iter().map(|r| r.audio.clone()).collect();
            for a in audios.iter().filter(|a| !mics.contains(a)) {
                let mut on = st.settings.voices.contains(a);
                let r = ui.checkbox(&mut on, a.as_str());
                app.auto.add_response(&format!("autoCut.voice.{a}"), &r, a);
                if r.changed() {
                    if on {
                        st.settings.voices.push(a.clone());
                    } else {
                        st.settings.voices.retain(|v| v != a);
                    }
                }
            }
        });
        ui.add_space(6.0);
        let num = |app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, v: &mut f64| {
            ui.horizontal(|ui| {
                ui.label(label);
                let r = ui.add(egui::DragValue::new(v).range(0.0..=30.0).speed(0.1).suffix(format!(" {}", tl!("sec"))).max_decimals(1));
                app.auto.add_response(id, &r, label);
            });
        };
        num(app, ui, "autoCut.minShot", tl!("Shortest shot"), &mut st.settings.min_shot);
        let r = ui.checkbox(&mut st.settings.reactions, tl!("Show big moments from whoever reacts first"));
        app.auto.add_response("autoCut.reactions", &r, tl!("Show big moments from whoever reacts first"));
        if st.settings.reactions {
            num(app, ui, "autoCut.buildup", tl!("Build-up before it"), &mut st.settings.buildup);
            num(app, ui, "autoCut.hold", tl!("Hold after it"), &mut st.settings.hold);
            let r = ui.checkbox(&mut st.settings.markers, tl!("Mark big moments on the timeline"));
            app.auto.add_response("autoCut.markers", &r, tl!("Mark big moments on the timeline"));
        }
        let r = ui.checkbox(&mut st.settings.mute_views, tl!("Mute the views' original sound (the Auto Cut audio track plays it)"));
        app.auto.add_response("autoCut.muteViews", &r, tl!("Mute the views' original sound (the Auto Cut audio track plays it)"));
        let r = ui.checkbox(&mut st.settings.trim, tl!("Cut out the parts where nobody talks"));
        app.auto.add_response("autoCut.trim", &r, tl!("Cut out the parts where nobody talks"));
        if st.settings.trim {
            num(app, ui, "autoCut.minSilence", tl!("Shortest silence to cut"), &mut st.settings.min_silence);
            num(app, ui, "autoCut.pad", tl!("Keep before and after words"), &mut st.settings.pad);
            let r = ui.checkbox(&mut st.settings.motion, tl!("Also look at the picture (a still screen goes first, action stays; takes longer)"));
            app.auto.add_response("autoCut.motion", &r, tl!("Also look at the picture (a still screen goes first, action stays; takes longer)"));
            ui.horizontal(|ui| {
                ui.label(tl!("Length to keep"));
                let r = ui.add(egui::Slider::new(&mut st.settings.keep_percent, 30.0..=100.0).suffix("%").step_by(5.0));
                app.auto.add_response("autoCut.keep", &r, tl!("Length to keep"));
            });
            ui.label(
                RichText::new(tl!("Below 100%, the dullest talk goes too, cut between sentences. Lively talk, big moments and marked parts stay."))
                    .small()
                    .color(t.text_dim),
            );
            ui.label(RichText::new(tl!("Every track is cut together, so the sound stays with the picture.")).small().color(t.text_dim));
        }
        ui.label(RichText::new(tl!("With In and Out marked, only that part is cut.")).small().color(t.text_dim));
        ui.add_space(6.0);
        ui.separator();
        ui.label(RichText::new(tl!("My editing style")).strong());
        ui.label(
            RichText::new(tl!("Open the uncut sequence, pick the edit you made from it, and Auto Cut learns what you keep and cut.")).small().color(t.text_dim),
        );
        let others: Vec<String> = {
            let active = app.session.state.active_sequence;
            let mut v: Vec<String> = app.session.project.sequences().filter(|i| Some(i.id) != active).map(|i| i.name.clone()).collect();
            v.sort();
            v
        };
        ui.horizontal(|ui| {
            ui.label(tl!("Edited sequence"));
            let shown = if st.settings.learn_from.is_empty() { tl!("Choose…").to_string() } else { st.settings.learn_from.clone() };
            let r = craft_ui::ComboBox::from_id_salt("autoCut.learnFrom").width(160.0).selected_text(shown).show_ui(ui, |ui| {
                for n in &others {
                    ui.selectable_value(&mut st.settings.learn_from, n.clone(), n);
                }
            });
            app.auto.add_response("autoCut.learnFrom", &r.response, tl!("Edited sequence"));
            let ready = st.job.is_none() && !st.settings.learn_from.is_empty() && st.settings.people.len() >= 2;
            let r = ui.add_enabled(ready, egui::Button::new(tl!("Learn")));
            app.auto.add_response("autoCut.learn", &r, tl!("Learn"));
            if r.clicked() {
                match start_learning(app, &st.settings) {
                    Ok((job, req)) => {
                        st.job = Some(job);
                        st.request = Some(req);
                    }
                    Err(e) => app.ui.status = e,
                }
            }
        });
        // no uncut sequence of the edit yet: make one from a sequence that has the recordings in sync
        let r = ui.add_enabled(st.job.is_none() && !st.settings.learn_from.is_empty(), egui::Button::new(tl!("Make the uncut sequence of this edit")));
        app.auto.add_response("autoCut.makeUncut", &r, tl!("Make the uncut sequence of this edit"));
        if r.clicked() {
            match app.session.execute("sequence.makeUncut", json!({"from": st.settings.learn_from})) {
                Ok(v) => {
                    let mut m = tlf!(
                        "Made \"{name}\" with {n} recordings: set up the people for it, then Learn",
                        name = v["name"].as_str().unwrap_or(""),
                        n = v["recordings"].as_u64().unwrap_or(0)
                    );
                    if let Some(miss) = v["missing"].as_array().filter(|a| !a.is_empty()) {
                        let list = miss.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                        m.push_str(&format!(" · {}", tlf!("not in sync, left out: {list}", list = list)));
                    }
                    app.ui.status = m;
                }
                Err(e) => app.ui.status = e.to_string(),
            }
        }
        ui.label(
            RichText::new(tl!("If no sequence has the whole recordings, this puts each one on whole, lined up from a sequence where they play in sync."))
                .small()
                .color(t.text_dim),
        );
        if let Some(v) = st.settings.style.clone() {
            ui.label(RichText::new(style_text(&v)).small());
            ui.horizontal(|ui| {
                let r = ui.checkbox(&mut st.settings.use_style, tl!("Cut in my style"));
                app.auto.add_response("autoCut.useStyle", &r, tl!("Cut in my style"));
                let r = ui.button(tl!("Forget"));
                app.auto.add_response("autoCut.forgetStyle", &r, tl!("Forget"));
                if r.clicked() {
                    st.settings.style = None;
                }
            });
            ui.label(RichText::new(tl!("Learning another edit adds to this style.")).small().color(t.text_dim));
        }
    });
    if st.settings != before
        && let Some(path) = &st.path
        && let Ok(text) = serde_json::to_string_pretty(&st.settings)
        && let Err(e) = std::fs::write(path, text)
    {
        app.ui.status = format!("saving auto cut settings: {e}");
    }
    ui.ctx().data_mut(|d| d.insert_temp(state_id(), st));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_settings_file_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "{nope").unwrap();
        assert_eq!(Settings::load(Some(&path)), Settings::default());
        let many: Vec<Value> = (0..100).map(|i| json!({"name": i.to_string()})).collect();
        std::fs::write(&path, json!({"people": many, "minShot": 1e9, "buildup": -5}).to_string()).unwrap();
        let s = Settings::load(Some(&path));
        assert_eq!(s.people.len(), MAX_PEOPLE);
        assert_eq!((s.min_shot, s.buildup), (30.0, 0.0));
    }
}
