//! Fade handles (custom feature of this fork), like Premiere Pro: hover a clip and a small handle
//! shows at each top corner; drag one inward and the clip fades in (or out) over that length.
//! Dragging it back to the corner removes the fade. One drag is one undo step.
//!
//! The fade is a one-sided transition (`timeline.setFade`, `crates/engine/src/fade_handles.rs`),
//! drawn here as Premiere draws a fade: the faded part of the clip shaded over a ramp that rises
//! from the clip's bottom corner to the top where the fade ends (a straight line on video, a
//! constant power curve on audio), with the handle on top at its end.
//!
//! Automation ids: `timeline.fadeHandle.<clip>.in` / `.out` (the hovered clip's handles).
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};
use filmcraft_engine::fade_handles::{FadeEdge, fade_of, has_crossfade};
use filmcraft_project::{ClipId, Sequence, TrackId, Transition};
use filmcraft_time::Tick;
use serde_json::json;

use super::layout::{Layout, Row};
use crate::FilmcraftApp;
use crate::state::Tool;
use crate::theme::Tokens;

/// Side of the drawn handle (points).
const HANDLE: f32 = 7.0;
/// How far from the clip's corner the handle of a clip with no fade sits (points).
const INSET: f32 = 6.0;
/// How far below the clip's top the handle's centre is (points).
const DROP: f32 = 4.5;
/// Half the width of the area a press grabs the handle in (points).
const GRAB: f32 = 7.0;
/// How far the grab area reaches above and below the handle's centre (points): only a thin band
/// at the top of the clip, so the rest of each edge still trims.
const GRAB_UP: f32 = 6.0;
const GRAB_DOWN: f32 = 2.5;
/// Narrowest clip (points) that shows handles: a sliver keeps its trim edges.
const MIN_CLIP_W: f32 = 28.0;

/// A handle drag in progress (kept in egui's temp data, so the Timeline's own drags are untouched).
#[derive(Clone, Copy, Debug)]
struct FadeDrag {
    clip: ClipId,
    track: TrackId,
    edge: FadeEdge,
    /// The fade's length now (as last sent), and whether a change was sent yet (the first begins
    /// the undo step, the rest merge into it).
    duration: Tick,
    begun: bool,
}

fn drag_id() -> egui::Id {
    egui::Id::unique("filmcraft-fade-handle-drag")
}

/// The clip body on `row` for a clip spanning `start..end`, as the Timeline draws it.
fn body(layout: &Layout, row: &Row, start: Tick, end: Tick) -> Rect {
    let x0 = layout.x_of(start);
    Rect::from_min_max(pos2(x0, row.rect.min.y + 1.0), pos2(layout.x_of(end).max(x0 + 1.0), row.rect.max.y - 1.0))
}

/// Where the handle of `edge` is drawn on a clip body, for a fade `fade` long.
fn handle_center(layout: &Layout, body: Rect, start: Tick, end: Tick, edge: FadeEdge, fade: Tick) -> Pos2 {
    let y = body.min.y + DROP;
    let x = match edge {
        FadeEdge::In if fade > Tick::ZERO => layout.x_of(start + fade).max(body.min.x + INSET),
        FadeEdge::In => body.min.x + INSET,
        FadeEdge::Out if fade > Tick::ZERO => layout.x_of(end - fade).min(body.max.x - INSET),
        FadeEdge::Out => body.max.x - INSET,
    };
    pos2(x, y)
}

/// The handles a clip offers: (edge, centre, fade length). None on a locked track or a narrow clip;
/// an edge with a transition between two clips has none (its ends are dragged instead).
fn handles(seq: &Sequence, layout: &Layout, row: &Row, clip: ClipId) -> Vec<(FadeEdge, Pos2, Tick)> {
    let Some(tr) = seq.track(row.track).filter(|t| !t.locked) else { return Vec::new() };
    let Some(it) = tr.item(clip) else { return Vec::new() };
    let b = body(layout, row, it.start, it.end());
    if b.width() < MIN_CLIP_W || b.height() < 12.0 {
        return Vec::new();
    }
    [FadeEdge::In, FadeEdge::Out]
        .into_iter()
        .filter(|e| !has_crossfade(tr, clip, *e))
        .map(|e| {
            let fade = fade_of(tr, clip, e).map_or(Tick::ZERO, |x| x.duration);
            (e, handle_center(layout, b, it.start, it.end(), e, fade), fade)
        })
        .collect()
}

fn paint_handle(p: &egui::Painter, c: Pos2, hot: bool, t: &Tokens) {
    let r = Rect::from_center_size(c, vec2(HANDLE, HANDLE));
    p.rect_filled(r, 1.0, if hot { t.accent } else { Color32::WHITE });
    p.rect_stroke(r, 1.0, Stroke::new(1.0, Color32::from_black_alpha(200)), StrokeKind::Outside);
}

/// The fade's length for a pointer at `x` dragging `edge` of a clip spanning `start..end`.
fn duration_at(layout: &Layout, start: Tick, end: Tick, edge: FadeEdge, x: f32) -> Tick {
    let t = layout.tick_at(x);
    match edge {
        FadeEdge::In => t - start,
        FadeEdge::Out => end - t,
    }
    .max(Tick::ZERO)
}

/// Hover and drag the handles. Returns whether the pointer is on a handle or dragging one, in
/// which case the Timeline's own press, drag and cursor handling skip this frame.
pub(super) fn interact(app: &mut FilmcraftApp, ui: &mut egui::Ui, seq: &Sequence, layout: &Layout) -> bool {
    let ctx = ui.ctx().clone();
    let t = app.tokens;
    let held: Option<FadeDrag> = ctx.data(|d| d.get_temp(drag_id()));
    if app.ui.tool != Tool::Selection {
        if held.is_some() {
            ctx.data_mut(|d| d.remove::<FadeDrag>(drag_id()));
        }
        return false;
    }
    let pointer = ctx.input(|i| i.pointer.latest_pos());
    let rate = seq.settings.frame_rate.sane();

    // ---- a drag in progress
    if let Some(mut d) = held {
        let down = ctx.input(|i| i.pointer.primary_down());
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            if d.begun {
                let _ = app.session.execute("edit.undo", json!({}));
            }
            ctx.data_mut(|m| m.remove::<FadeDrag>(drag_id()));
            return true;
        }
        let row = layout.rows.iter().find(|r| r.track == d.track);
        let it = seq.track(d.track).and_then(|tr| tr.item(d.clip));
        if let (true, Some(p), Some(row), Some(it)) = (down, pointer, row, it) {
            let wanted = rate.snap_nearest(duration_at(layout, it.start, it.end(), d.edge, p.x));
            if wanted != d.duration {
                let params = json!({"clip": d.clip.0, "edge": d.edge.id(), "duration": wanted.0, "merge": true, "begin": !d.begun});
                match app.session.execute("timeline.setFade", params) {
                    Ok(r) => {
                        d.duration = r["duration"].as_i64().map(Tick).unwrap_or(wanted);
                        d.begun = true;
                    }
                    Err(e) => app.ui.status = e.to_string(),
                }
            }
            // the handle where the fade now ends, and its length beside the pointer
            let b = body(layout, row, it.start, it.end());
            let c = handle_center(layout, b, it.start, it.end(), d.edge, d.duration);
            let shown = layout.shown(row);
            paint_handle(&ui.painter().with_clip_rect(shown.expand(4.0)), c, true, &t);
            let text = if d.duration > Tick::ZERO {
                tlf!("Fade {time}", time = filmcraft_time::format_time(d.duration, rate, false, filmcraft_time::TimeDisplay::Timecode, 48000))
            } else {
                tl!("No fade").to_string()
            };
            ui.painter().text(p + vec2(14.0, -14.0), egui::Align2::LEFT_BOTTOM, text, Tokens::ui(11.0), Color32::WHITE);
            ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
            ctx.data_mut(|m| m.insert_temp(drag_id(), d));
            return true;
        }
        // released (or the clip is gone): the drag ends
        ctx.data_mut(|m| m.remove::<FadeDrag>(drag_id()));
        return true;
    }

    // ---- hover: the handles of the clip under the pointer
    if app.tl.drag.is_some() {
        return false;
    }
    let Some(p) = pointer.filter(|p| layout.content.contains(*p)) else { return false };
    // a press that started elsewhere (a marquee, a scrub) is not ours
    if ctx.input(|i| i.pointer.primary_down() && !i.pointer.primary_pressed()) {
        return false;
    }
    let Some(row) = layout.row_at(p.y) else { return false };
    let Some(tr) = seq.track(row.track) else { return false };
    // the clip under the pointer, or the one whose corner it is just past
    let tick = layout.tick_at(p.x);
    let near = |dx: f32| dx.abs() <= GRAB;
    let Some(it) = tr.item_at(tick).or_else(|| tr.items.iter().find(|it| near(layout.x_of(it.start) - p.x) || near(layout.x_of(it.end()) - p.x))) else {
        return false;
    };
    let clip = it.id;
    let hs = handles(seq, layout, row, clip);
    let painter = ui.painter().with_clip_rect(layout.shown(row));
    let mut on: Option<(FadeEdge, Tick)> = None;
    for (edge, c, fade) in &hs {
        let grab = Rect::from_min_max(pos2(c.x - GRAB, c.y - GRAB_UP), pos2(c.x + GRAB, c.y + GRAB_DOWN));
        let hot = grab.contains(p);
        paint_handle(&painter, *c, hot, &t);
        let label = if *edge == FadeEdge::In { tl!("Fade in") } else { tl!("Fade out") };
        app.auto.add(&format!("timeline.fadeHandle.{}.{}", clip.0, edge.id()), grab, label);
        if hot && on.is_none() {
            on = Some((*edge, *fade));
        }
    }
    let Some((edge, fade)) = on else { return false };
    ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.data_mut(|m| m.insert_temp(drag_id(), FadeDrag { clip, track: row.track, edge, duration: fade, begun: false }));
    }
    true
}

/// Draw a one-sided transition (a fade) the way Premiere draws a fade, over the whole clip height
/// of `row`. Returns false for a transition between two clips (the Timeline draws those itself).
pub(super) fn draw_fade(p: &egui::Painter, row: Rect, r: Rect, trn: &Transition, selected: bool, t: &Tokens) -> bool {
    let fade_in = trn.from.is_none() && trn.to.is_some();
    let fade_out = trn.to.is_none() && trn.from.is_some();
    if !(fade_in || fade_out) {
        return false;
    }
    let area = Rect::from_min_max(pos2(r.min.x, row.min.y + 1.0), pos2(r.max.x, row.max.y - 1.0));
    if area.width() <= 0.0 || area.height() <= 0.0 {
        return true;
    }
    let cp = p.with_clip_rect(area.intersect(p.clip_rect()));
    let audio = matches!(trn.effect.def().map(|d| d.kind), Some(filmcraft_project::EffectKind::AudioTransition));
    // the level at progress 0..1 through the fade (0 silent / black, 1 full)
    let level = |u: f32| if audio { (u * std::f32::consts::FRAC_PI_2).sin() } else { u };
    let n = if audio { 24 } else { 1 };
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let u = i as f32 / n as f32;
            let v = if fade_in { level(u) } else { level(1.0 - u) };
            pos2(area.min.x + area.width() * u, area.max.y - area.height() * v)
        })
        .collect();
    // the faded part: shaded above the ramp
    for w in pts.windows(2) {
        if let [a, b] = w {
            cp.add(egui::Shape::convex_polygon(vec![pos2(a.x, area.min.y), *a, *b, pos2(b.x, area.min.y)], Color32::from_black_alpha(120), Stroke::NONE));
        }
    }
    let col = if selected { t.accent } else { Color32::WHITE };
    cp.add(egui::Shape::line(pts.iter().map(|q| *q + vec2(0.0, 1.0)).collect(), Stroke::new(1.0, Color32::from_black_alpha(160))));
    cp.add(egui::Shape::line(pts, Stroke::new(if selected { 2.0 } else { 1.5 }, col)));
    if selected {
        cp.rect_stroke(area, 0.0, Stroke::new(1.0, t.accent), StrokeKind::Inside);
    }
    // the handle stays on the fade's end
    let c = pos2(if fade_in { area.max.x } else { area.min.x }, area.min.y + DROP);
    if area.width() >= HANDLE {
        paint_handle(&p.with_clip_rect(area.expand(HANDLE).intersect(p.clip_rect())), c, false, t);
    }
    true
}
