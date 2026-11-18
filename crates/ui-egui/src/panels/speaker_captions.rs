//! Fork: the Speaker Captions panel (Window ▸ Speaker Captions).
//!
//! Each audio track holding one person's voice gets that person's name; the panel finds their
//! head-caption template (`<name>_머리자막.fcgt` in the template folder), and **Make Captions**
//! transcribes every named track on a worker thread, then places the captions in one undo step
//! (`captions.placeSpeakerPhrases`; see `filmcraft_engine::speaker_captions`). The speech model
//! is downloaded here on first use. The spell-check section writes all caption text to a file to
//! be checked and corrected, and reads the corrected file back.
//!
//! Settings (template folder, model, names per track) are kept in `speaker_captions.json` beside
//! the preferences. Automation ids: `speakerCaptions.folder`, `speakerCaptions.browse`,
//! `speakerCaptions.name.A<n>`, `speakerCaptions.model`, `speakerCaptions.download`,
//! `speakerCaptions.maxChars`, `speakerCaptions.make`, `speakerCaptions.cancel`,
//! `speakerCaptions.export`, `speakerCaptions.import`.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Rect, RichText, vec2};
use filmcraft_engine::speaker_captions::{self as sc, Phrase};
use filmcraft_engine::track_views::audio_name;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::FilmcraftApp;
use crate::native_dialogs::FileDialog;

/// The settings file, beside `preferences.json`.
pub const FILE: &str = "speaker_captions.json";
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_NAME_CHARS: usize = 40;
/// The model used until the user picks another (best Korean of the three).
const DEFAULT_MODEL: &str = "whisper-small";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub template_dir: String,
    pub model: String,
    pub max_chars: usize,
    /// Person per audio track, by position ("A4" → "라스").
    pub names: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { template_dir: String::new(), model: DEFAULT_MODEL.into(), max_chars: sc::MAX_CHARS, names: BTreeMap::new() }
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
        s.max_chars = s.max_chars.clamp(4, 60);
        s.names.retain(|k, v| k.starts_with('A') && k.len() <= 6 && !v.trim().is_empty());
        for v in s.names.values_mut() {
            *v = v.chars().take(MAX_NAME_CHARS).collect();
        }
        s
    }
}

/// Work running on a worker thread: a model download or a transcription.
#[derive(Default)]
pub struct Job {
    pub label: String,
    pub progress: f32,
    pub cancel: AtomicBool,
    /// Download: `Some(Ok(vec![]))`; transcription: the phrases; or the error.
    pub done: Option<Result<Vec<Phrase>, String>>,
    pub download: bool,
}

#[derive(Clone, Default)]
struct State {
    settings: Settings,
    path: Option<PathBuf>,
    job: Option<Arc<Mutex<Job>>>,
}

fn state_id() -> egui::Id {
    egui::Id::unique("filmcraft-speaker-captions")
}

/// A folder picked in the dialog, read on the next frame (the dialog answers outside the panel).
static PICKED: Mutex<Option<String>> = Mutex::new(None);

fn state(app: &FilmcraftApp, ctx: &egui::Context) -> State {
    if let Some(s) = ctx.data(|d| d.get_temp::<State>(state_id())) {
        return s;
    }
    let path = app.session.prefs_path.as_ref().and_then(|p| p.parent()).map(|d| d.join(FILE));
    State { settings: Settings::load(path.as_deref()), path, job: None }
}

fn lock(job: &Arc<Mutex<Job>>) -> std::sync::MutexGuard<'_, Job> {
    job.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `work` on a worker thread under `catch_unwind`; its result lands in the job.
fn spawn(job: &Arc<Mutex<Job>>, work: impl FnOnce(&Arc<Mutex<Job>>) -> Result<Vec<Phrase>, String> + Send + 'static) {
    let j = job.clone();
    let spawned = std::thread::Builder::new().name("speaker-captions".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&j))).unwrap_or_else(|_| Err("the speech worker stopped unexpectedly".into()));
        lock(&j).done = Some(r);
    });
    if let Err(e) = spawned {
        lock(job).done = Some(Err(e.to_string()));
    }
}

/// The people with a name and a template found: (track, name, template).
fn speakers(app: &FilmcraftApp, st: &Settings) -> Vec<(String, String, Result<String, String>)> {
    let Some(q) = app.session.active_sequence() else { return Vec::new() };
    (0..q.audio_tracks.len())
        .filter_map(|i| {
            let track = audio_name(i);
            let name = st.names.get(&track)?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let t = sc::template_for(&app.session, &name, None, Some(&st.template_dir)).map_err(|e| e.to_string());
            Some((track, name, t))
        })
        .collect()
}

fn start_transcription(app: &mut FilmcraftApp, st: &Settings) -> Result<Arc<Mutex<Job>>, String> {
    let list: Vec<serde_json::Value> =
        speakers(app, st).into_iter().filter_map(|(track, name, t)| t.ok().map(|t| json!({"track": track, "name": name, "template": t}))).collect();
    if list.is_empty() {
        return Err(tl!("Name at least one audio track whose head-caption preset is in the folder").into());
    }
    let resolved = sc::speakers_p(&app.session, &json!({"speakers": list})).map_err(|e| e.to_string())?;
    let jobs = sc::audio_jobs(&app.session, &resolved).map_err(|e| e.to_string())?;
    if jobs.is_empty() {
        return Err(tl!("The named tracks have no clips").into());
    }
    let installed = app.session.transcriber.clone();
    let (model, max_chars) = (st.model.clone(), st.max_chars);
    let job = Arc::new(Mutex::new(Job { label: tl!("Loading the speech model…").into(), ..Job::default() }));
    spawn(&job, move |j| {
        let t = match installed {
            Some(t) => t,
            None => sc::load_model(&model).map_err(|e| e.to_string())?,
        };
        sc::transcribe_jobs(&*t, &jobs, Some("ko"), max_chars, &mut |i, n, f| {
            let mut g = lock(j);
            g.label = tlf!("Transcribing clip {k} of {n}…", k = i + 1, n);
            g.progress = ((i as f32 + f.clamp(0.0, 1.0)) / n.max(1) as f32).clamp(0.0, 1.0);
            !g.cancel.load(Ordering::Relaxed)
        })
        .map_err(|e| e.to_string())
    });
    Ok(job)
}

fn start_download(model: String) -> Arc<Mutex<Job>> {
    let job = Arc::new(Mutex::new(Job { label: tl!("Downloading the speech model…").into(), download: true, ..Job::default() }));
    spawn(&job, move |j| {
        sc::download_model(&model, &mut |done, total| {
            let mut g = lock(j);
            g.progress = if total > 0 { (done as f64 / total as f64) as f32 } else { 0.0 };
            !g.cancel.load(Ordering::Relaxed)
        })
        .map(|()| Vec::new())
        .map_err(|e| e.to_string())
    });
    job
}

/// A finished job: place the captions (or say what went wrong).
fn finish(app: &mut FilmcraftApp, job: Job) {
    match job.done {
        Some(Ok(_)) if job.download => app.ui.status = tl!("Speech model downloaded").into(),
        Some(Ok(phrases)) if phrases.is_empty() => app.ui.status = tl!("No speech was found on the named tracks").into(),
        Some(Ok(phrases)) => {
            let list: Vec<serde_json::Value> = phrases.iter().map(Phrase::to_json).collect();
            app.ui.status = match app.session.execute("captions.placeSpeakerPhrases", json!({"phrases": list})) {
                Ok(r) => tlf!(
                    "Made {n} captions ({k} while someone else was talking)",
                    n = r["captions"].as_u64().unwrap_or(0),
                    k = r["overlapping"].as_u64().unwrap_or(0)
                ),
                Err(e) => e.to_string(),
            };
        }
        Some(Err(e)) => app.ui.status = e,
        None => {}
    }
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let mut st = state(app, ui.ctx());
    let before = st.settings.clone();
    if let Some(dir) = PICKED.lock().unwrap_or_else(PoisonError::into_inner).take() {
        st.settings.template_dir = dir;
    }
    // a finished job reports; a running one keeps the panel repainting
    if let Some(job) = st.job.clone() {
        let done = lock(&job).done.is_some();
        if done {
            let taken = std::mem::take(&mut *lock(&job));
            st.job = None;
            finish(app, taken);
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let ui = &mut child;
    egui::ScrollArea::vertical().id_salt("speaker-captions-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let Some(tracks) = app.session.active_sequence().map(|q| q.audio_tracks.len()) else {
            ui.label(RichText::new(tl!("Open a sequence to make speaker captions.")).color(t.text_dim));
            return;
        };
        if !sc::speech_available() && app.session.transcriber.is_none() {
            ui.label(RichText::new(tl!("This build has no speech recognition (build with the `whisper` feature).")).color(t.text_dim));
        }

        // ---- head-caption presets folder
        ui.label(RichText::new(tl!("Head-caption preset folder")).strong());
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut st.settings.template_dir)
                    .desired_width((ui.available_width() - 70.0).max(80.0))
                    .hint_text("E:\\filmcraft\\자막 프리셋"),
            );
            app.auto.add_response("speakerCaptions.folder", &r, tl!("Head-caption preset folder"));
            let r = ui.button(tl!("Browse…"));
            app.auto.add_response("speakerCaptions.browse", &r, tl!("Browse…"));
            if r.clicked() {
                let at = Some(st.settings.template_dir.clone()).filter(|d| !d.is_empty());
                app.pick_ui(FileDialog::Folder { at }, |_, paths| {
                    if let Some(p) = paths.into_iter().next() {
                        *PICKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(p);
                    }
                });
            }
        });
        ui.label(RichText::new(tl!("A person's preset is \"<name>_머리자막.fcgt\" in this folder.")).small().color(t.text_dim));
        ui.add_space(6.0);

        // ---- who speaks on which track
        ui.label(RichText::new(tl!("Who speaks on each audio track")).strong());
        egui::Grid::new("speaker-captions-tracks").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
            for i in 0..tracks {
                let track = audio_name(i);
                ui.label(&track);
                let mut name = st.settings.names.get(&track).cloned().unwrap_or_default();
                let r = ui.add_sized(vec2(120.0, 20.0), egui::TextEdit::singleline(&mut name).hint_text(tl!("(nobody)")));
                app.auto.add_response(&format!("speakerCaptions.name.{track}"), &r, &track);
                if r.changed() {
                    let name: String = name.chars().take(MAX_NAME_CHARS).collect();
                    if name.trim().is_empty() {
                        st.settings.names.remove(&track);
                    } else {
                        st.settings.names.insert(track.clone(), name.clone());
                    }
                }
                if name.trim().is_empty() {
                    ui.label("");
                } else {
                    match sc::template_for(&app.session, name.trim(), None, Some(&st.settings.template_dir)) {
                        Ok(_) => ui.label(RichText::new(tl!("✓ preset")).color(t.accent)),
                        Err(_) => ui.label(RichText::new(tl!("no preset")).color(egui::Color32::from_rgb(230, 110, 90))),
                    };
                }
                ui.end_row();
            }
        });
        ui.add_space(6.0);

        // ---- speech model and caption length
        let models = sc::models();
        ui.horizontal(|ui| {
            ui.label(tl!("Speech model"));
            let shown = models.iter().find(|m| m.0 == st.settings.model).map_or(st.settings.model.clone(), |m| m.1.to_string());
            let r = craft_ui::ComboBox::from_id_salt("speakerCaptions.model").selected_text(shown).show_ui(ui, |ui| {
                for (id, name, size, installed) in &models {
                    let label = format!("{name} ({} MB){}", size / 1_000_000, if *installed { " ✓" } else { "" });
                    ui.selectable_value(&mut st.settings.model, (*id).to_string(), label);
                }
            });
            app.auto.add_response("speakerCaptions.model", &r.response, tl!("Speech model"));
        });
        let installed = app.session.transcriber.is_some() || models.iter().any(|m| m.0 == st.settings.model && m.3);
        if !installed && sc::speech_available() {
            let r = ui.add_enabled(st.job.is_none(), egui::Button::new(tl!("Download the speech model")));
            app.auto.add_response("speakerCaptions.download", &r, tl!("Download the speech model"));
            if r.clicked() {
                st.job = Some(start_download(st.settings.model.clone()));
            }
            ui.label(RichText::new(tl!("Downloaded once from Hugging Face (OpenAI Whisper, MIT licence).")).small().color(t.text_dim));
        }
        ui.horizontal(|ui| {
            ui.label(tl!("Most characters per caption"));
            let r = ui.add(egui::DragValue::new(&mut st.settings.max_chars).range(4..=60));
            app.auto.add_response("speakerCaptions.maxChars", &r, tl!("Most characters per caption"));
        });
        ui.add_space(6.0);

        // ---- make
        match st.job.clone() {
            Some(job) => {
                let (label, progress) = {
                    let g = lock(&job);
                    (g.label.clone(), g.progress)
                };
                ui.add(egui::ProgressBar::new(progress).text(label).show_percentage());
                let r = ui.button(tl!("Cancel"));
                app.auto.add_response("speakerCaptions.cancel", &r, tl!("Cancel"));
                if r.clicked() {
                    lock(&job).cancel.store(true, Ordering::Relaxed);
                }
            }
            None => {
                let r = ui.add_enabled(installed, egui::Button::new(RichText::new(tl!("Make Captions")).strong()).min_size(vec2(140.0, 26.0)));
                app.auto.add_response("speakerCaptions.make", &r, tl!("Make Captions"));
                if r.clicked() {
                    match start_transcription(app, &st.settings) {
                        Ok(job) => st.job = Some(job),
                        Err(e) => app.ui.status = e,
                    }
                }
                ui.label(
                    RichText::new(tl!("Whoever starts talking first keeps the main place; anyone talking over them goes above.")).small().color(t.text_dim),
                );
            }
        }
        ui.separator();

        // ---- spell check round trip
        ui.label(RichText::new(tl!("Spell check")).strong());
        ui.label(
            RichText::new(tl!("Export the caption text, have it checked (send the file to Claude), then import the corrected file.")).small().color(t.text_dim),
        );
        ui.horizontal(|ui| {
            let r = ui.button(tl!("Export Caption Text…"));
            app.auto.add_response("speakerCaptions.export", &r, tl!("Export Caption Text…"));
            if r.clicked() {
                app.pick_ui(FileDialog::save_as(tl!("Caption text"), &["txt"], "captions.txt"), |app, paths| {
                    if let Some(p) = paths.first() {
                        app.ui.status = match app.session.execute("captions.exportText", json!({"path": p})) {
                            Ok(r) => tlf!("Exported {n} captions", n = r["captions"].as_u64().unwrap_or(0)),
                            Err(e) => e.to_string(),
                        };
                    }
                });
            }
            let r = ui.button(tl!("Import Corrected Text…"));
            app.auto.add_response("speakerCaptions.import", &r, tl!("Import Corrected Text…"));
            if r.clicked() {
                app.pick_ui(FileDialog::open_file(tl!("Caption text"), &["txt"]), |app, paths| {
                    if let Some(p) = paths.first() {
                        app.ui.status = match app.session.execute("captions.importText", json!({"path": p})) {
                            Ok(r) => tlf!("Corrected {n} captions", n = r["changed"].as_u64().unwrap_or(0)),
                            Err(e) => e.to_string(),
                        };
                    }
                });
            }
        });
    });
    if st.settings != before
        && let Some(path) = &st.path
        && let Ok(text) = serde_json::to_string_pretty(&st.settings)
        && let Err(e) = std::fs::write(path, text)
    {
        app.ui.status = format!("saving speaker captions settings: {e}");
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
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Settings::load(Some(&path)), Settings::default());
        std::fs::write(&path, json!({"maxChars": 9999, "names": {"A1": "  ", "V1": "x", "A2": "라스"}}).to_string()).unwrap();
        let s = Settings::load(Some(&path));
        assert_eq!(s.max_chars, 60);
        assert_eq!(s.names.len(), 1);
        std::fs::write(&path, vec![b' '; (MAX_FILE_BYTES + 10) as usize]).unwrap();
        assert_eq!(Settings::load(Some(&path)), Settings::default());
    }
}
