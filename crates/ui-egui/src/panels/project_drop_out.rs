//! Fork: move Project panel items out of the bin they are in by dragging.
//!
//! - Dropped on the **Up one level** button, the dragged items (the selection when the dragged
//!   item is in it) or bin move into the parent of the bin the view shows.
//! - Dropped on **empty space**, items that sit in a sub-bin twirled open in List view move into
//!   the bin the view shows (upstream does this for bins only).
//!
//! Both go through `project.moveToBin`. Kept in its own file so upstream updates rarely touch it.

use egui::{Rect, Stroke, StrokeKind};
use filmcraft_project::{BinId, ItemId};
use serde_json::json;

use super::project::{Actions, View, parent_bin};
use crate::FilmcraftApp;

/// The dragged items: the whole selection when the dragged item is part of it.
fn dragged_items(app: &FilmcraftApp, ui: &egui::Ui) -> Option<Vec<ItemId>> {
    let item = crate::panels::dragged_project_item(ui)?;
    let sel = &app.session.state.project_selection;
    Some(if sel.contains(&item) { sel.iter().copied().collect() } else { vec![item] })
}

/// A drop on the Up button (`r`) of a view that shows a bin below `top`.
pub fn up_button_drop(app: &FilmcraftApp, ui: &egui::Ui, r: Rect, v: &View, top: BinId, actions: &mut Actions) {
    if v.bin == top || !ui.rect_contains_pointer(r) {
        return;
    }
    let root = &app.session.project.root;
    let ids: Vec<u64> = if let Some(items) = dragged_items(app, ui) {
        items.iter().map(|i| i.0).collect()
    } else if let Some(bin) = crate::panels::dragged_bin(ui) {
        // only a bin shown here goes up (never the view's own bin or one it sits inside)
        if root.find_bin(BinId(bin)).is_none_or(|d| d.find_bin(v.bin).is_some()) {
            return;
        }
        vec![bin]
    } else {
        return;
    };
    ui.painter().rect_stroke(r, 3.0, Stroke::new(1.5, app.tokens.accent), StrokeKind::Inside);
    if ui.input(|i| i.pointer.any_released()) {
        // the root bin is `null` to the command
        let parent = parent_bin(root, v.bin).filter(|p| *p != root.id).map(|p| p.0);
        actions.push(("project.moveToBin".into(), json!({"items": ids, "bin": parent})));
        crate::panels::clear_drag(ui);
    }
}

/// A drop of items on the empty space (`r`) of a view: those not already in its bin move into it.
pub fn empty_space_drop(app: &FilmcraftApp, ui: &egui::Ui, r: Rect, v: &View, actions: &mut Actions) {
    let Some(items) = dragged_items(app, ui) else { return };
    if !ui.rect_contains_pointer(r) || !ui.input(|i| i.pointer.any_released()) {
        return;
    }
    let root = &app.session.project.root;
    let moving: Vec<u64> = items.iter().filter(|i| root.parent_of(**i).is_some_and(|p| p != v.bin)).map(|i| i.0).collect();
    if moving.is_empty() {
        return;
    }
    let bin = (v.bin != root.id).then_some(v.bin.0);
    actions.push(("project.moveToBin".into(), json!({"items": moving, "bin": bin})));
    crate::panels::clear_drag(ui);
}
