//! Effect presets in the UI: the Presets bin of the Effects panel (built-in and user presets; drag
//! onto a clip or double-click to apply to the selection; right-click a user preset to delete or
//! export it) and the Save Preset dialog (Effect Controls ▸ right-click an effect ▸ Save Preset…).
//!
//! Automation ids: `effects.preset.<name>`, `effects.folder.Presets/<folder path>` (fork),
//! `savePreset.name`, `savePreset.description`,
//! `savePreset.keyframes.<scale|anchorIn|anchorOut|none>`, `savePreset.ok`, `savePreset.cancel`.

use egui::{Align2, Rect, Sense, pos2, vec2};
use serde_json::json;

use crate::FilmcraftApp;
use crate::icons::{self, Icon};
use crate::native_dialogs::FileDialog;
use crate::state::SavePresetDraft;
use crate::theme::Tokens;

/// The rows of the Presets folder (inside the Effects panel's scroll area).
pub fn folder_rows(app: &mut FilmcraftApp, ui: &mut egui::Ui, filter: &str) -> Option<(String, String)> {
    let t = app.tokens;
    let mut action = None;
    // fork: presets in folders (imported Premiere bins) are listed under folder rows, after the
    // loose ones; `shown` is the folder path drawn last, with whether each level is open
    let mut presets: Vec<_> = app.session.presets.all().into_iter().filter(|p| crate::i18n::matches_query(&p.name, filter)).collect();
    presets.sort_by(|a, b| (!a.folder.is_empty(), &a.folder).cmp(&(!b.folder.is_empty(), &b.folder)));
    let mut shown: Vec<(String, bool)> = Vec::new();
    for p in presets {
        let segs: Vec<&str> = p.folder.split('/').filter(|s| !s.is_empty()).collect();
        let same = shown.iter().zip(&segs).take_while(|((a, _), b)| a == *b).count();
        shown.truncate(same);
        for (i, seg) in segs.iter().enumerate().skip(same) {
            // a folder inside a closed one is not drawn
            if shown.iter().all(|(_, open)| *open) {
                let key = format!("Presets/{}", segs[..=i].join("/"));
                let open = folder_row(app, ui, &key, seg, i + 1, filter);
                shown.push((seg.to_string(), open));
            } else {
                shown.push((seg.to_string(), false));
            }
        }
        if !shown.iter().all(|(_, open)| *open) {
            continue;
        }
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click_and_drag());
        if resp.hovered() {
            ui.painter().rect_filled(r, 0.0, t.hover);
        }
        // inside the Presets folder: icon and label one level deeper than it (#2329)
        let x = r.min.x + crate::panels::effects::tree_x(1 + segs.len()) + 18.0;
        icons::paint(
            ui.painter(),
            Rect::from_center_size(pos2(x, r.center().y), vec2(13.0, 13.0)),
            Icon::Sparkle,
            if p.builtin { t.text_dim } else { t.accent },
        );
        ui.painter().text(pos2(x + 12.0, r.center().y), Align2::LEFT_CENTER, crate::i18n::t(&p.name), Tokens::ui(12.0), t.text);
        app.auto.add(&format!("effects.preset.{}", p.name), r, &p.name);
        let tip = crate::i18n::t(if p.description.is_empty() { &p.name } else { &p.description }).to_string();
        let resp = resp.on_hover_text(tip);
        if resp.drag_started_by(egui::PointerButton::Primary) {
            crate::panels::start_drag_effect(ui, &format!("preset:{}", p.name));
        }
        if resp.double_clicked() {
            action = Some(("presets.apply".to_string(), p.name.clone()));
        }
        resp.context_menu(|ui| {
            // fork: apply without relying on a double-click
            let r = ui.add_enabled(!app.session.state.selection.is_empty(), egui::Button::new(tl!("Apply to Selected Clips")));
            app.auto.add(&format!("effects.presetMenu.{}.apply", p.name), r.rect, tl!("Apply to Selected Clips"));
            if r.clicked() {
                action = Some(("presets.apply".to_string(), p.name.clone()));
                ui.close();
            }
            ui.separator();
            if !p.builtin && ui.button(tl!("Delete Preset")).clicked() {
                action = Some(("presets.delete".to_string(), p.name.clone()));
                ui.close();
            }
            if ui.button(tl!("Export Preset…")).clicked() {
                action = Some(("presets.export".to_string(), p.name.clone()));
                ui.close();
            }
            if ui.button(tl!("Import Presets…")).clicked() {
                action = Some(("presets.import".to_string(), String::new()));
                ui.close();
            }
        });
    }
    action
}

/// Fork: a folder row inside the Presets bin (like the Effects panel's own folder rows), labelled
/// `name`; `key` is its full path, kept in the open-folder list. Returns whether it is open.
fn folder_row(app: &mut FilmcraftApp, ui: &mut egui::Ui, key: &str, name: &str, depth: usize, filter: &str) -> bool {
    let t = app.tokens;
    let open = !filter.is_empty() || app.ui.expanded_fx.iter().any(|k| k == key);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 0.0, t.hover);
    }
    let x = r.min.x + crate::panels::effects::tree_x(depth);
    let chevron = if open { Icon::ChevronDown } else { Icon::ChevronRight };
    icons::paint(ui.painter(), Rect::from_center_size(pos2(x + 4.0, r.center().y), vec2(10.0, 10.0)), chevron, t.text_dim);
    icons::paint(ui.painter(), Rect::from_center_size(pos2(x + 18.0, r.center().y), vec2(13.0, 13.0)), Icon::Folder, t.text_dim);
    ui.painter().text(pos2(x + 30.0, r.center().y), Align2::LEFT_CENTER, name, Tokens::ui(12.0), t.text);
    app.auto.add(&format!("effects.folder.{key}"), r, name);
    if resp.clicked() {
        if open {
            app.ui.expanded_fx.retain(|k| k != key);
        } else {
            app.ui.expanded_fx.push(key.to_string());
        }
    }
    open
}

/// Run a Presets-bin action.
pub fn run(app: &mut FilmcraftApp, cmd: &str, name: &str) {
    let r = match cmd {
        "presets.apply" => {
            // fork: say what was applied, so a double-click visibly did something
            let r = app.session.execute(cmd, json!({"preset": name}));
            if let Ok(v) = &r {
                let n = v["applied"].as_u64().unwrap_or(0);
                app.ui.status = tlf!("Applied {preset} to {n} clips", preset = crate::i18n::t(name), n);
            }
            r
        }
        "presets.delete" => app.session.execute(cmd, json!({"name": name})),
        "presets.export" => {
            let name = name.to_string();
            app.pick_ui(FileDialog::save_as(tl!("Effect presets"), &["json"], &format!("{name}.json")), move |app, paths| {
                let Some(path) = paths.into_iter().next() else { return };
                if let Err(e) = app.session.execute("presets.export", json!({"path": path, "names": [name]})) {
                    app.ui.status = e.to_string();
                }
            });
            return;
        }
        "presets.import" => {
            app.pick_ui(FileDialog::open_file(tl!("Effect presets"), &["json", "prfpset"]), |app, paths| {
                let Some(path) = paths.into_iter().next() else { return };
                match app.session.execute("presets.import", json!({"path": path})) {
                    // fork: say how many came in (a Premiere pack holds hundreds, in folders)
                    Ok(v) => {
                        let n = v["imported"].as_array().map_or(0, Vec::len);
                        app.ui.status = tlf!("Imported {n} presets", n);
                    }
                    Err(e) => app.ui.status = e.to_string(),
                }
            });
            return;
        }
        _ => return,
    };
    if let Err(e) = r {
        app.ui.status = e.to_string();
    }
}

/// Open the Save Preset dialog for effects of a clip.
pub fn open_save(app: &mut FilmcraftApp, clip: u64, effects: Vec<usize>, name: &str) {
    app.ui.save_preset = Some(SavePresetDraft { clip, effects, name: name.to_string(), description: String::new(), keyframes: "scale".into() });
}

pub fn save_dialog(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(mut d) = app.ui.save_preset.clone() else { return };
    let mut open = true;
    let mut close = false;
    let mut apply = false;
    egui::Window::new(tl!("Save Preset")).id(egui::Id::unique("Save Preset")).open(&mut open).collapsible(false).resizable(false).default_width(340.0).show(
        ctx,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(tl!("Name:"));
                let r = crate::widgets::accessible_name(ui.text_edit_singleline(&mut d.name), tl!("Name"));
                app.auto.add("savePreset.name", r.rect, "Name");
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new(tl!("Type")).strong());
            for (key, label) in [
                ("scale", tl!("Scale")),
                ("anchorIn", tl!("Anchor to In Point")),
                ("anchorOut", tl!("Anchor to Out Point")),
                ("none", tl!("Without keyframes")),
            ] {
                let r = ui.radio(d.keyframes == key, label);
                app.auto.add(&format!("savePreset.keyframes.{key}"), r.rect, label);
                if r.clicked() {
                    d.keyframes = key.into();
                }
            }
            ui.add_space(4.0);
            ui.label(tl!("Description:"));
            let r = crate::widgets::accessible_name(ui.text_edit_multiline(&mut d.description), tl!("Description"));
            app.auto.add("savePreset.description", r.rect, "Description");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let c = ui.button(tl!("Cancel"));
                app.auto.add("savePreset.cancel", c.rect, "Cancel");
                let o = ui.add_enabled(!d.name.trim().is_empty(), egui::Button::new(tl!("OK")));
                app.auto.add("savePreset.ok", o.rect, "OK");
                apply = o.clicked();
                close = apply || c.clicked();
            });
        },
    );
    app.ui.save_preset = if close || !open { None } else { Some(d.clone()) };
    if apply {
        let r = app.session.execute(
            "presets.save",
            json!({"clip": d.clip, "effects": d.effects, "name": d.name.trim(), "description": d.description, "keyframes": d.keyframes}),
        );
        match r {
            Ok(_) => app.ui.status = tlf!("Saved preset “{name}”", name = d.name.trim()),
            Err(e) => app.ui.status = e.to_string(),
        }
    }
}
