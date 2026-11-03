//! Fade handles (custom feature of this fork): drag the handle at a clip's top corner inward and
//! the clip fades in (or out) over that length, like Premiere Pro's fade handles.
//!
//! | Id | What |
//! |---|---|
//! | `timeline.setFade` | give a clip a fade in or out of a duration (0 removes it) |
//!
//! A fade is a one-sided transition on the clip's edge (from or to nothing), made with the
//! sequence's default video or audio transition (Cross Dissolve / Constant Power unless changed in
//! the Effects panel), so it plays, renders, exports and can be selected and edited like any other
//! transition. The handle only works on an edge that has no transition between two clips.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use filmcraft_edit as edit;
use filmcraft_edit::EditError;
use filmcraft_project::{ClipId, EffectKind, TrackKind, Transition, TransitionAlign, TransitionId};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

const CMD: &str = "timeline.setFade";
/// The undo label of every fade change; a drag's steps share one undo step (same label and key).
pub const LABEL: &str = "Fade";

/// Which edge of the clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeEdge {
    In,
    Out,
}

impl FadeEdge {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "in" | "In" => Some(Self::In),
            "out" | "Out" => Some(Self::Out),
            _ => None,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

/// The one-sided fade on `clip`'s `edge` in `track`, if any.
pub fn fade_of(track: &filmcraft_project::Track, clip: ClipId, edge: FadeEdge) -> Option<&Transition> {
    track.transitions.iter().find(|x| match edge {
        FadeEdge::In => x.to == Some(clip) && x.from.is_none(),
        FadeEdge::Out => x.from == Some(clip) && x.to.is_none(),
    })
}

/// Whether a transition between two clips sits on `clip`'s `edge` (the handle leaves it alone).
pub fn has_crossfade(track: &filmcraft_project::Track, clip: ClipId, edge: FadeEdge) -> bool {
    track.transitions.iter().any(|x| match edge {
        FadeEdge::In => x.to == Some(clip) && x.from.is_some(),
        FadeEdge::Out => x.from == Some(clip) && x.to.is_some(),
    })
}

/// `timeline.setFade {clip, edge, duration | frames, begin?, merge?}`.
fn set_fade(s: &mut Session, p: &Value) -> Result<Value> {
    let clip = ClipId(u64_p(p, "clip").ok_or_else(|| bad(CMD, "need `clip` (a clip id)"))?);
    let edge = str_p(p, "edge").and_then(FadeEdge::parse).ok_or_else(|| bad(CMD, "`edge` is \"in\" or \"out\""))?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let rate = q.settings.frame_rate.sane();
    let frame = rate.frame_duration();
    let (tid, it) = q.find_item(clip).ok_or_else(|| bad(CMD, format!("no clip {} in this sequence", clip.0)))?;
    let track = q.track(tid).ok_or_else(|| bad(CMD, "the clip's track is gone"))?;
    if track.locked {
        return Err(EditError::Locked.into());
    }
    if has_crossfade(track, clip, edge) {
        return Err(bad(CMD, "a transition between two clips is on this edge; drag its ends to change it"));
    }
    // hostile numbers: no overflow, nothing negative, never past the clip
    let wanted = match (p.get("duration").and_then(Value::as_i64), p.get("frames").and_then(Value::as_i64)) {
        (Some(_), Some(_)) => return Err(bad(CMD, "give `duration` or `frames`, not both")),
        (Some(d), None) => Tick(d.max(0)),
        (None, Some(f)) => rate.tick_of(f.clamp(0, 10_000_000)),
        (None, None) => return Err(bad(CMD, "need `duration` (ticks) or `frames`")),
    };
    // the fade on the other edge keeps its length: the two never overlap
    let other = match edge {
        FadeEdge::In => fade_of(track, clip, FadeEdge::Out),
        FadeEdge::Out => fade_of(track, clip, FadeEdge::In),
    }
    .map_or(Tick::ZERO, |x| x.duration);
    let room = Tick(it.duration.0.saturating_sub(other.0).max(0));
    let mut d = rate.snap_nearest(wanted).min(room);
    if d < frame {
        d = Tick::ZERO;
    }
    let existing = fade_of(track, clip, edge).map(|x| (x.id, x.duration));
    let (clip_start, clip_end) = (it.start, it.end());
    let kind = track.kind;
    if existing.map_or(Tick::ZERO, |e| e.1) == d {
        return Ok(json!({"transition": existing.map(|e| e.0.0), "duration": d.0, "changed": false}));
    }
    let effect = if kind == TrackKind::Video { s.state.default_video_transition.clone() } else { s.state.default_audio_transition.clone() };
    let ekind = if kind == TrackKind::Video { EffectKind::VideoTransition } else { EffectKind::AudioTransition };
    let def = filmcraft_project::vtransition::find_transition(&effect, ekind)
        .or_else(|| filmcraft_project::vtransition::find_transition(if kind == TrackKind::Video { "cross_dissolve" } else { "constant_power" }, ekind))
        .ok_or_else(|| bad(CMD, format!("unknown transition `{effect}`")))?;
    if bool_p(p, "begin").unwrap_or(false) {
        s.history.merge_key = None;
    }
    let key = format!("fade:{}:{}", clip.0, edge.id());
    let merge = bool_p(p, "merge").unwrap_or(false).then_some(key.as_str());
    let start = match edge {
        FadeEdge::In => clip_start,
        FadeEdge::Out => clip_end - d,
    };
    let id = s.edit_sequence_as(LABEL, merge, |q, ctx, _| match existing {
        Some((id, _)) if d == Tick::ZERO => {
            if let Some(t) = q.track_mut(tid) {
                t.transitions.retain(|x| x.id != id);
            }
            Ok(None)
        }
        Some((id, _)) => {
            edit::transitions::set_span(q, id, start, d, frame, frame)?;
            Ok(Some(id))
        }
        None => {
            let (from, to, align) = match edge {
                FadeEdge::In => (None, Some(clip), TransitionAlign::StartAtCut),
                FadeEdge::Out => (Some(clip), None, TransitionAlign::EndAtCut),
            };
            let tr = Transition { id: TransitionId(0), effect: def.instance(), start, duration: d, from, to, align, reverse: false };
            Ok(Some(edit::add_transition(q, tid, tr, ctx)?))
        }
    })?;
    Ok(json!({"transition": id.map(|i: TransitionId| i.0), "duration": d.0, "changed": true}))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "Set Fade",
        menu: &[],
        shortcut: None,
        params: r#"{"clip":id,"edge":"in"|"out","duration":ticks?|"frames":n?,"begin":bool?,"merge":bool?}"#,
        enabled: has_seq,
        run: set_fade,
        journal: true,
    }]
}

#[cfg(test)]
mod tests {
    use filmcraft_project::ClipId;
    use serde_json::json;

    use super::{FadeEdge, fade_of};
    use crate::Session;

    /// A sequence with one clip (picture and sound) at 0 on V1 / A1.
    fn session() -> (Session, ClipId, ClipId) {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Fades", "video": 1, "audio": 1})).unwrap();
        let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        s.execute("timeline.place", json!({"item": item.0, "time": 0})).unwrap();
        let (v, a) = {
            let q = s.active_sequence().unwrap();
            (q.video_tracks[0].items[0].id, q.audio_tracks[0].items[0].id)
        };
        (s, v, a)
    }

    fn fade(s: &Session, clip: ClipId, edge: FadeEdge) -> Option<(i64, i64, Option<ClipId>, Option<ClipId>)> {
        let q = s.active_sequence().unwrap();
        let (tid, _) = q.find_item(clip).unwrap();
        fade_of(q.track(tid).unwrap(), clip, edge).map(|x| (x.start.0, x.duration.0, x.from, x.to))
    }

    #[test]
    fn a_drag_makes_resizes_and_removes_a_fade_in_one_undo_step() {
        let (mut s, v, a) = session();
        let rate = s.sequence_rate();
        let undo = s.history.undo.len();
        let f = |n| rate.tick_of(n).0;
        // a drag: begin, then every move merges
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "frames": 5, "begin": true, "merge": true})).unwrap();
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "frames": 12, "merge": true})).unwrap();
        assert_eq!(fade(&s, v, FadeEdge::In), Some((0, f(12), None, Some(v))));
        assert_eq!(s.history.undo.len(), undo + 1, "the drag is one undo step");
        // only the clip it was dragged on: the linked sound has no fade
        assert_eq!(fade(&s, a, FadeEdge::In), None);
        // a fade out sits at the clip's end
        let end = s.active_sequence().unwrap().video_tracks[0].items[0].end().0;
        s.execute("timeline.setFade", json!({"clip": a.0, "edge": "out", "frames": 10, "begin": true, "merge": true})).unwrap();
        assert_eq!(fade(&s, a, FadeEdge::Out), Some((end - f(10), f(10), Some(a), None)));
        let audio_fx = &s.active_sequence().unwrap().audio_tracks[0].transitions[0].effect.effect;
        assert_eq!(audio_fx, "constant_power");
        // dragged back to the corner: gone
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "duration": 0, "begin": true, "merge": true})).unwrap();
        assert_eq!(fade(&s, v, FadeEdge::In), None);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(fade(&s, v, FadeEdge::In).map(|x| x.1), Some(f(12)));
    }

    #[test]
    fn a_fade_never_runs_past_the_clip_or_into_the_other_fade() {
        let (mut s, v, _) = session();
        let dur = s.active_sequence().unwrap().video_tracks[0].items[0].duration.0;
        let rate = s.sequence_rate();
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "out", "frames": 10})).unwrap();
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "duration": i64::MAX})).unwrap();
        let fin = fade(&s, v, FadeEdge::In).unwrap().1;
        assert_eq!(fin + rate.tick_of(10).0, dur, "the fade in stops where the fade out begins");
        s.active_sequence().unwrap().check().unwrap();
    }

    #[test]
    fn hostile_parameters_are_errors_not_panics() {
        let (mut s, v, _) = session();
        let before = s.project.clone();
        for p in [
            json!({}),
            json!({"clip": v.0}),
            json!({"clip": v.0, "edge": "middle", "frames": 3}),
            json!({"clip": 999_999, "edge": "in", "frames": 3}),
            json!({"clip": v.0, "edge": "in"}),
            json!({"clip": v.0, "edge": "in", "frames": 3, "duration": 3}),
            json!({"clip": "V1", "edge": "in", "frames": 3}),
        ] {
            assert!(s.execute("timeline.setFade", p.clone()).is_err(), "{p}");
        }
        // negative and huge values are clamped, not refused
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "frames": -50})).unwrap();
        s.execute("timeline.setFade", json!({"clip": v.0, "edge": "in", "frames": i64::MIN})).unwrap();
        assert_eq!(*s.project, *before, "a negative length is no fade");
    }
}
