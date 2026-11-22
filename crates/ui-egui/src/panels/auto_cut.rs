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
//! `autoCut.markers`, `autoCut.muteViews`, `autoCut.trim`, `autoCut.minSilence`, `autoCut.pad`, `autoCut.make`, `autoCut.cancel`.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Rect, RichText, vec2};
use filmcraft_engine::auto_cut::{self as ac, Reaction, Shot};
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub people: Vec<Row>,
    pub min_shot: f64,
    pub buildup: f64,
    pub hold: f64,
    pub reactions: bool,
    pub markers: bool,
    pub mute_views: bool,
    pub trim: bool,
    pub min_silence: f64,
    pub pad: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let t = ac::Tuning::default();
        Self {
            people: Vec::new(),
            min_shot: t.min_shot,
            buildup: t.buildup,
            hold: t.hold,
            reactions: true,
            markers: true,
            mute_views: true,
            trim: t.trim,
            min_silence: t.min_silence,
            pad: t.pad,
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
            .map(|r| json!({"name": if r.name.trim().is_empty() { r.video.clone() } else { r.name.trim().to_string() }, "video": r.video, "audio": r.audio}))
            .collect();
        json!({"people": people, "minShot": self.min_shot, "buildup": self.buildup, "hold": self.hold, "reactions": self.reactions, "markers": self.markers, "muteViews": self.mute_views, "trim": self.trim, "minSilence": self.min_silence, "pad": self.pad})
    }
}

/// What the worker found: shots, big moments, silences to cut, and how much each person talks.
type Planned = (Vec<Shot>, Vec<Reaction>, Vec<(usize, usize)>, Vec<f32>);

#[derive(Default)]
struct Job {
    label: String,
    progress: f32,
    cancel: AtomicBool,
    done: Option<Result<Planned, String>>,
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
fn fill_in(app: &FilmcraftApp) -> Vec<Row> {
    let Some(q) = app.session.active_sequence() else { return Vec::new() };
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
            Row { name: name.clone(), video, audio: audio.clone() }
        })
        .collect();
    rows.sort_by_key(|r| r.audio.trim_start_matches('A').parse::<usize>().unwrap_or(usize::MAX));
    rows.truncate(MAX_PEOPLE);
    rows
}

fn start(app: &mut FilmcraftApp, st: &Settings) -> Result<(Arc<Mutex<Job>>, Value), String> {
    let req = st.request();
    let people = ac::people_p(&app.session, &req).map_err(|e| e.to_string())?;
    let range = ac::range_p(&app.session, &req).map_err(|e| e.to_string())?;
    let jobs = ac::level_jobs(&app.session, &people, range).map_err(|e| e.to_string())?;
    if jobs.is_empty() {
        return Err(tl!("The microphone tracks have no clips").into());
    }
    let tuning = ac::tuning_p(&req);
    let n = people.len();
    let job = Arc::new(Mutex::new(Job { label: tl!("Listening to the microphones…").into(), ..Job::default() }));
    let j = job.clone();
    let spawned = std::thread::Builder::new().name("auto-cut".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let lv = ac::levels(&jobs, n, range, &mut |done, total| {
                let mut g = lock(&j);
                g.progress = done as f32 / total.max(1) as f32;
                !g.cancel.load(Ordering::Relaxed)
            })
            .map_err(|e| e.to_string())?;
            let (shots, reactions) = ac::plan(&lv, &tuning);
            let cuts = ac::silences(&lv, &tuning, &reactions);
            Ok((shots, reactions, cuts, ac::talk_share(&lv)))
        }))
        .unwrap_or_else(|_| Err("the auto cut worker stopped unexpectedly".into()));
        lock(&j).done = Some(r);
    });
    if let Err(e) = spawned {
        return Err(e.to_string());
    }
    Ok((job, req))
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
    req["cuts"] = ac::cuts_json(&cuts);
    let mut status = match app.session.execute("edit.autoCutPlace", req) {
        Ok(v) => {
            let mut m = tlf!("Auto Cut: {n} shots, {k} big moments marked", n = v["shots"].as_u64().unwrap_or(0), k = v["reactions"].as_u64().unwrap_or(0));
            let secs = v["removed"].as_f64().unwrap_or(0.0);
            if secs > 0.0 {
                m.push_str(&format!(" · {}", tlf!("{n} silences cut ({secs} sec)", n = v["cuts"].as_u64().unwrap_or(0), secs = format!("{secs:.1}"))));
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

fn track_combo(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, value: &mut String, names: &[String]) {
    let shown = if value.is_empty() { "—".to_string() } else { value.clone() };
    let r = craft_ui::ComboBox::from_id_salt(id.to_string()).width(56.0).selected_text(shown).show_ui(ui, |ui| {
        for n in names {
            ui.selectable_value(value, n.clone(), n);
        }
    });
    app.auto.add_response(id, &r.response, id);
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
                finish(app, r, req);
            }
            None => ui.ctx().request_repaint_after(std::time::Duration::from_millis(200)),
        }
    }
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let ui = &mut child;
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
                let rows = fill_in(app);
                if rows.is_empty() {
                    app.ui.status = tl!("Name the people in the Speaker Captions panel first (who speaks on each audio track)").into();
                } else {
                    st.settings.people = rows;
                }
            }
            let r = ui.add_enabled(st.settings.people.len() < MAX_PEOPLE, egui::Button::new(tl!("Add Person")));
            app.auto.add_response("autoCut.add", &r, tl!("Add Person"));
            if r.clicked() {
                st.settings.people.push(Row::default());
            }
        });
        let mut remove = None;
        egui::Grid::new("auto-cut-people").num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new(tl!("Name")).small().color(t.text_dim));
            ui.label(RichText::new(tl!("View")).small().color(t.text_dim));
            ui.label(RichText::new(tl!("Mic")).small().color(t.text_dim));
            ui.label("");
            ui.end_row();
            for (i, row) in st.settings.people.iter_mut().enumerate() {
                let r = ui.add_sized(vec2(100.0, 20.0), egui::TextEdit::singleline(&mut row.name));
                app.auto.add_response(&format!("autoCut.name.{i}"), &r, tl!("Name"));
                track_combo(app, ui, &format!("autoCut.video.{i}"), &mut row.video, &videos);
                track_combo(app, ui, &format!("autoCut.audio.{i}"), &mut row.audio, &audios);
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
            ui.label(RichText::new(tl!("Every track is cut together, so the sound stays with the picture.")).small().color(t.text_dim));
        }
        ui.label(RichText::new(tl!("With In and Out marked, only that part is cut.")).small().color(t.text_dim));
        ui.add_space(6.0);
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
                    match start(app, &st.settings) {
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
