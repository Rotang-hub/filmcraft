//! Track Views panel (custom feature of this fork): a row of buttons, each assigned a set of
//! video and audio tracks. Pressing one shows only its video tracks (the others' eye is turned
//! off) and plays only its audio tracks (the others are muted) through `timeline.applyTrackView`,
//! as one undo step. "All" shows and unmutes every track again.
//!
//! Assignments are by track position ("V1", "A2"), so one set of buttons works in every sequence
//! and project. They are kept in [`FILE`] beside `preferences.json` in the data directory.
//!
//! Automation ids: `trackViews.view.<n>` (the buttons, 1-based), `trackViews.all`,
//! `trackViews.setup`, and in the setup section `trackViews.edit.<n>`, `trackViews.name`,
//! `trackViews.video.V<k>`, `trackViews.audio.A<k>`, `trackViews.capture`, `trackViews.clear`.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use std::path::{Path, PathBuf};

use egui::{Color32, Rect, RichText, vec2};
use filmcraft_engine::track_views::{audio_name, video_name};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::FilmcraftApp;

/// How many view buttons the panel has.
pub const BUTTONS: usize = 4;
/// The file the assignments are kept in, beside `preferences.json`.
pub const FILE: &str = "track_views.json";
/// Largest file read (the file is untrusted).
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Longest button name kept (characters).
const MAX_NAME_CHARS: usize = 40;
/// Most tracks one view keeps per kind.
const MAX_TRACKS: usize = 256;

/// One button: its name and the tracks it shows and plays.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub name: String,
    /// Video tracks shown, by position: "V1", "V2"…
    pub video: Vec<String>,
    /// Audio tracks heard, by position: "A1", "A2"…
    pub audio: Vec<String>,
}

impl View {
    fn is_empty(&self) -> bool {
        self.video.is_empty() && self.audio.is_empty()
    }
    /// Drop what a hand-edited or damaged file could put here: names that aren't "V<n>" / "A<n>",
    /// duplicates, too many entries, an overlong name.
    fn sanitized(mut self) -> Self {
        fn keep(list: &mut Vec<String>, prefix: char) {
            let mut seen = std::collections::HashSet::new();
            list.retain(|n| {
                let ok = n.strip_prefix(prefix).and_then(|d| d.parse::<u32>().ok()).is_some_and(|k| k >= 1);
                ok && seen.insert(n.clone())
            });
            list.truncate(MAX_TRACKS);
            list.sort_by_key(|n| n.get(1..).and_then(|d| d.parse::<u32>().ok()).unwrap_or(u32::MAX));
        }
        keep(&mut self.video, 'V');
        keep(&mut self.audio, 'A');
        self.name = self.name.chars().take(MAX_NAME_CHARS).collect();
        self
    }
}

/// Everything the panel keeps: the buttons, and (not saved) which one the setup section edits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackViews {
    pub views: Vec<View>,
    #[serde(skip)]
    editing: usize,
    #[serde(skip)]
    setup_open: bool,
    /// Where the file is (None: not saved, e.g. in tests).
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl TrackViews {
    /// The assignments in `path`, or [`BUTTONS`] empty buttons when it's missing or unreadable.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut v: TrackViews = path.as_deref().and_then(read_bounded).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        v.views = v.views.into_iter().take(BUTTONS).map(View::sanitized).collect();
        v.views.resize_with(BUTTONS, View::default);
        v.path = path;
        v
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("saving track views: {e}"))
    }
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes)
}

fn state_id() -> egui::Id {
    egui::Id::unique("filmcraft-track-views")
}

/// The panel's state, loaded from the data directory the first time it is drawn.
fn state(app: &FilmcraftApp, ctx: &egui::Context) -> TrackViews {
    if let Some(v) = ctx.data(|d| d.get_temp::<TrackViews>(state_id())) {
        return v;
    }
    let path = app.session.prefs_path.as_ref().and_then(|p| p.parent()).map(|d| d.join(FILE));
    TrackViews::load(path)
}

/// What the active sequence shows and plays now: (video names, audio names, video count, audio count).
fn current(app: &FilmcraftApp) -> Option<(Vec<String>, Vec<String>, usize, usize)> {
    let q = app.session.active_sequence()?;
    let any_solo = q.audio_tracks.iter().any(|t| t.solo);
    let video = q.video_tracks.iter().enumerate().filter(|(_, t)| t.enabled).map(|(i, _)| video_name(i)).collect();
    let audio = q.audio_tracks.iter().enumerate().filter(|(_, t)| !t.muted && (!any_solo || t.solo)).map(|(i, _)| audio_name(i)).collect();
    Some((video, audio, q.video_tracks.len(), q.audio_tracks.len()))
}

/// Whether the sequence now shows exactly `view` (tracks it names that the sequence lacks don't count).
fn matches(view: &View, video: &[String], audio: &[String], nv: usize, na: usize) -> bool {
    if view.is_empty() {
        return false;
    }
    let mut want_v: Vec<&String> = view.video.iter().filter(|n| (0..nv).any(|i| video_name(i) == **n)).collect();
    let mut want_a: Vec<&String> = view.audio.iter().filter(|n| (0..na).any(|i| audio_name(i) == **n)).collect();
    let mut have_v: Vec<&String> = video.iter().collect();
    let mut have_a: Vec<&String> = audio.iter().collect();
    want_v.sort();
    want_a.sort();
    have_v.sort();
    have_a.sort();
    want_v == have_v && want_a == have_a
}

fn button_label(i: usize, view: &View) -> String {
    let n = i.saturating_add(1);
    if view.name.trim().is_empty() { n.to_string() } else { format!("{n} · {}", view.name.trim()) }
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let mut views = state(app, ui.ctx());
    let before = views.clone();
    let now = current(app);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let ui = &mut child;
    let mut apply: Option<serde_json::Value> = None;

    egui::ScrollArea::vertical().id_salt("track-views-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let Some((video, audio, nv, na)) = now.clone() else {
            ui.label(RichText::new(tl!("Open a sequence to use track views.")).color(t.text_dim));
            return;
        };

        // ---- the view buttons
        let width = ui.available_width();
        let gap = ui.spacing().item_spacing.x;
        let cols = BUTTONS.saturating_add(1) as f32;
        let w = ((width - gap * (cols - 1.0)) / cols - 2.0).floor().max(40.0);
        ui.horizontal_wrapped(|ui| {
            for (i, view) in views.views.iter().enumerate() {
                let on = matches(view, &video, &audio, nv, na);
                let mut b = egui::Button::new(RichText::new(button_label(i, view)).strong()).min_size(vec2(w, 36.0)).selected(on);
                if view.is_empty() {
                    b = b.fill(Color32::TRANSPARENT);
                }
                let r = ui.add_enabled(!view.is_empty(), b).on_disabled_hover_text(tl!("No tracks assigned yet: open Setup below.")).on_hover_text(tlf!(
                    "Show {video} · Hear {audio}",
                    video = list_text(&view.video),
                    audio = list_text(&view.audio)
                ));
                app.auto.add_response(&format!("trackViews.view.{}", i.saturating_add(1)), &r, &button_label(i, view));
                if r.clicked() {
                    apply = Some(json!({"video": view.video, "audio": view.audio, "label": tlf!("Track View {n}", n = i.saturating_add(1))}));
                }
            }
            let all_on = video.len() == nv && audio.len() == na;
            let r = ui
                .add(egui::Button::new(RichText::new(tl!("All")).strong()).min_size(vec2(w, 36.0)).selected(all_on))
                .on_hover_text(tl!("Show and hear every track"));
            app.auto.add_response("trackViews.all", &r, tl!("All"));
            if r.clicked() {
                apply = Some(json!({"all": true, "label": tl!("Show All Tracks")}));
            }
        });

        ui.add_space(6.0);
        ui.label(RichText::new(tlf!("Now: show {video} · hear {audio}", video = list_text(&video), audio = list_text(&audio))).color(t.text_dim).small());
        ui.add_space(4.0);
        ui.separator();

        // ---- setup: which tracks each button shows and plays
        let r = ui.selectable_label(views.setup_open, if views.setup_open { tl!("▾ Setup") } else { tl!("▸ Setup") });
        app.auto.add_response("trackViews.setup", &r, tl!("Setup"));
        if r.clicked() {
            views.setup_open = !views.setup_open;
        }
        if !views.setup_open {
            return;
        }
        ui.horizontal(|ui| {
            ui.label(tl!("Button:"));
            for i in 0..BUTTONS {
                let r = ui.selectable_label(views.editing == i, (i.saturating_add(1)).to_string());
                app.auto.add_response(&format!("trackViews.edit.{}", i.saturating_add(1)), &r, &(i.saturating_add(1)).to_string());
                if r.clicked() {
                    views.editing = i;
                }
            }
        });
        let editing = views.editing.min(BUTTONS.saturating_sub(1));
        let Some(view) = views.views.get_mut(editing) else { return };
        ui.horizontal(|ui| {
            ui.label(tl!("Name:"));
            let r = ui.add(egui::TextEdit::singleline(&mut view.name).hint_text(tl!("optional")).desired_width(160.0).char_limit(MAX_NAME_CHARS));
            app.auto.add_response("trackViews.name", &r, &view.name);
        });
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            if let [left, right] = cols {
                left.label(RichText::new(tl!("Video (show)")).strong());
                for i in (0..nv).rev() {
                    let name = video_name(i);
                    toggle(app, left, &mut view.video, &name, "video");
                }
                right.label(RichText::new(tl!("Audio (hear)")).strong());
                for i in 0..na {
                    let name = audio_name(i);
                    toggle(app, right, &mut view.audio, &name, "audio");
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let r = ui.button(tl!("Use current tracks")).on_hover_text(tl!("Assign the tracks that are shown and heard now"));
            app.auto.add_response("trackViews.capture", &r, tl!("Use current tracks"));
            if r.clicked() {
                view.video = video.clone();
                view.audio = audio.clone();
            }
            let r = ui.button(tl!("Clear"));
            app.auto.add_response("trackViews.clear", &r, tl!("Clear"));
            if r.clicked() {
                view.video.clear();
                view.audio.clear();
            }
        });
        let extra: Vec<&String> = view
            .video
            .iter()
            .filter(|n| !(0..nv).any(|i| video_name(i) == **n))
            .chain(view.audio.iter().filter(|n| !(0..na).any(|i| audio_name(i) == **n)))
            .collect();
        if !extra.is_empty() {
            let names: Vec<String> = extra.into_iter().cloned().collect();
            ui.label(RichText::new(tlf!("Also assigned (not in this sequence): {tracks}", tracks = names.join(", "))).color(t.text_dim).small());
        }
    });

    if let Some(p) = apply {
        match app.session.execute("timeline.applyTrackView", p) {
            Ok(r) => {
                let missing: Vec<String> =
                    r["missing"].as_array().map(|m| m.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                if !missing.is_empty() {
                    app.ui.status = tlf!("This sequence has no {tracks}; the other tracks of the view were applied.", tracks = missing.join(", "));
                }
            }
            Err(e) => app.ui.status = e.to_string(),
        }
    }
    views.views = views.views.into_iter().map(View::sanitized).collect();
    if views.views != before.views
        && let Err(e) = views.save()
    {
        app.ui.status = e;
    }
    ui.ctx().data_mut(|d| d.insert_temp(state_id(), views));
}

/// A checkbox that adds `name` to or removes it from `list`.
fn toggle(app: &mut FilmcraftApp, ui: &mut egui::Ui, list: &mut Vec<String>, name: &str, kind: &str) {
    let mut on = list.iter().any(|n| n == name);
    let r = ui.checkbox(&mut on, name);
    app.auto.add_response(&format!("trackViews.{kind}.{name}"), &r, name);
    if r.changed() {
        list.retain(|n| n != name);
        if on {
            list.push(name.to_string());
        }
    }
}

/// "V1, V3", or "none".
fn list_text(names: &[String]) -> String {
    if names.is_empty() { tl!("none").to_string() } else { names.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_file_gives_empty_buttons() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        for text in ["", "not json", "{\"views\": 5}", "[]"] {
            std::fs::write(&path, text).unwrap();
            let v = TrackViews::load(Some(path.clone()));
            assert_eq!(v.views.len(), BUTTONS, "{text:?}");
            assert!(v.views.iter().all(View::is_empty), "{text:?}");
        }
    }

    #[test]
    fn views_round_trip_and_bad_entries_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let text = json!({"views": [
            {"name": "Cam A", "video": ["V2", "V1", "V1", "X1", "V0", "V-3", ""], "audio": ["A1", "V1", "A99999999999999"]},
            {"name": "x".repeat(500)},
            {}, {}, {"name": "sixth"}, {"name": "seventh"}
        ]});
        std::fs::write(&path, text.to_string()).unwrap();
        let v = TrackViews::load(Some(path.clone()));
        assert_eq!(v.views.len(), BUTTONS);
        assert_eq!(v.views[0], View { name: "Cam A".into(), video: vec!["V1".into(), "V2".into()], audio: vec!["A1".into()] });
        assert_eq!(v.views[1].name.chars().count(), MAX_NAME_CHARS);
        v.save().unwrap();
        assert_eq!(TrackViews::load(Some(path)).views, v.views);
    }

    #[test]
    fn a_view_matches_only_its_own_tracks() {
        let s = |v: &[&str]| v.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let view = View { name: String::new(), video: s(&["V1", "V5"]), audio: s(&["A2"]) };
        // V5 isn't in a 2-track sequence, so the view is V1 + A2 there
        assert!(matches(&view, &s(&["V1"]), &s(&["A2"]), 2, 2));
        assert!(!matches(&view, &s(&["V1", "V2"]), &s(&["A2"]), 2, 2));
        assert!(!matches(&View::default(), &[], &[], 2, 2), "an empty view is never shown as on");
    }

    #[test]
    fn oversized_files_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, vec![b' '; (MAX_FILE_BYTES + 10) as usize]).unwrap();
        assert!(read_bounded(&path).is_none());
    }
}
