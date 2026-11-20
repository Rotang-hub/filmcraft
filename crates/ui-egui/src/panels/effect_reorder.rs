//! Fork: drag an added effect's header in Effect Controls to change its place in the stack.
//!
//! While dragging, a line shows where it will land (between the other added effects; the fixed
//! effects and graphic layers don't move); releasing runs `effects.move`. Escape cancels.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use egui::{Rect, Stroke, pos2};
use filmcraft_project::ClipId;
use serde_json::{Value, json};

use crate::FilmcraftApp;

/// An effect header drawn this frame: its index on the clip, its row, whether it can move.
pub struct Header {
    pub index: usize,
    pub rect: Rect,
    pub movable: bool,
}

/// The effect being dragged: (clip, index).
fn id() -> egui::Id {
    egui::Id::unique("filmcraft-effect-reorder")
}

/// Record a header; a drag that starts on a movable one picks that effect up.
pub fn header(ui: &egui::Ui, clip: ClipId, h: Header, resp: &egui::Response, headers: &mut Vec<Header>) {
    if h.movable && resp.drag_started() {
        ui.ctx().data_mut(|d| d.insert_temp(id(), (clip.0, h.index)));
    }
    headers.push(h);
}

/// Where the dragged effect would land: its new place among the added effects, and the y of the
/// line to draw.
fn target(headers: &[Header], dragged: usize, y: f32) -> Option<(usize, f32)> {
    let movable: Vec<&Header> = headers.iter().filter(|h| h.movable).collect();
    let others: Vec<&&Header> = movable.iter().filter(|h| h.index != dragged).collect();
    if movable.len() < 2 {
        return None;
    }
    let to = others.iter().filter(|h| h.rect.center().y < y).count();
    // the line: on top of the header it goes before, or under the last added effect's block
    let line = match others.get(to) {
        Some(h) => h.rect.min.y,
        None => {
            let last = others.last()?;
            // the next header after the last added one closes its block
            headers
                .iter()
                .map(|h| h.rect.min.y)
                .filter(|t| *t > last.rect.min.y)
                .fold(None, |a: Option<f32>, t| Some(a.map_or(t, |a| a.min(t))))
                .unwrap_or(last.rect.max.y)
        }
    };
    Some((to, line))
}

/// After the headers: draw the drop line, and on release queue the move.
pub fn finish(app: &mut FilmcraftApp, ui: &egui::Ui, clip: ClipId, headers: &[Header], actions: &mut Vec<(String, Value)>) {
    let Some((c, index)) = ui.ctx().data(|d| d.get_temp::<(u64, usize)>(id())) else { return };
    if c != clip.0 {
        return;
    }
    let (pointer, released, escape) = ui.input(|i| (i.pointer.interact_pos(), i.pointer.any_released(), i.key_pressed(egui::Key::Escape)));
    let clear = |ui: &egui::Ui| ui.ctx().data_mut(|d| d.remove::<(u64, usize)>(id()));
    if escape || !headers.iter().any(|h| h.index == index && h.movable) {
        clear(ui);
        return;
    }
    let Some(p) = pointer else { return };
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    let Some((to, y)) = target(headers, index, p.y) else {
        clear(ui);
        return;
    };
    let (x0, x1) = headers.first().map_or((0.0, 0.0), |h| (h.rect.min.x, h.rect.max.x));
    ui.painter().line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(2.0, app.tokens.accent));
    app.auto.add("effectControls.reorderLine", Rect::from_min_max(pos2(x0, y - 1.0), pos2(x1, y + 1.0)), "drop here");
    if released {
        actions.push(("effects.move".into(), json!({"clip": clip.0, "index": index, "to": to})));
        clear(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    fn h(index: usize, y: f32, movable: bool) -> Header {
        Header { index, rect: Rect::from_min_size(pos2(0.0, y), vec2(100.0, 20.0)), movable }
    }

    #[test]
    fn the_drop_place_counts_the_other_added_effects_above_the_pointer() {
        // Motion (fixed), A at 100, B at 200, C at 300
        let hs = [h(0, 0.0, false), h(3, 100.0, true), h(4, 200.0, true), h(5, 300.0, true)];
        assert_eq!(target(&hs, 3, 50.0), Some((0, 200.0)), "above all: before B");
        assert_eq!(target(&hs, 3, 250.0), Some((1, 300.0)), "between B and C");
        assert_eq!(target(&hs, 3, 400.0).map(|t| t.0), Some(2), "after C: last");
        assert_eq!(target(&hs, 5, 150.0), Some((1, 200.0)), "C between A and B");
        assert_eq!(target(&[h(0, 0.0, false), h(3, 100.0, true)], 3, 50.0), None, "one added effect: nowhere to go");
    }
}
