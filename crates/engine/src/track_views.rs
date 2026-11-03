//! Track views (custom feature of this fork): show and hear only a chosen set of tracks.
//!
//! | Id | What |
//! |---|---|
//! | `timeline.trackView` | which video tracks are shown (eye on) and which audio tracks are heard (not muted) |
//! | `timeline.applyTrackView` | turn on the listed video tracks and turn off the rest; unmute the listed audio tracks and mute the rest (one undo step) |
//!
//! The Track Views panel (`crates/ui-egui/src/panels/track_views.rs`) keeps its buttons'
//! assignments and calls `timeline.applyTrackView` when one is pressed. Tracks are named like the
//! rest of the commands: `"V1"`, `"A2"` (position in the active sequence, 1-based) or a track id.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use std::collections::HashSet;

use filmcraft_project::TrackId;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, has_seq, str_p, track_p};
use crate::{EngineError, Result, Session};

/// Most tracks one call may list per kind; a script can't make the command allocate without bound.
pub const MAX_LISTED_TRACKS: usize = 1024;
/// Longest undo label accepted (characters).
const MAX_LABEL_CHARS: usize = 80;

/// `"V1"`, `"V2"`… for the video track at `index` (0-based).
pub fn video_name(index: usize) -> String {
    format!("V{}", index.saturating_add(1))
}

/// `"A1"`, `"A2"`… for the audio track at `index` (0-based).
pub fn audio_name(index: usize) -> String {
    format!("A{}", index.saturating_add(1))
}

/// `timeline.trackView`: the names of the shown video tracks and the heard audio tracks.
fn track_view_get(s: &mut Session, _: &Value) -> Result<Value> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let any_solo = q.audio_tracks.iter().any(|t| t.solo);
    let video: Vec<String> = q.video_tracks.iter().enumerate().filter(|(_, t)| t.enabled).map(|(i, _)| video_name(i)).collect();
    let audio: Vec<String> = q.audio_tracks.iter().enumerate().filter(|(_, t)| !t.muted && (!any_solo || t.solo)).map(|(i, _)| audio_name(i)).collect();
    Ok(json!({"video": video, "audio": audio, "videoTracks": q.video_tracks.len(), "audioTracks": q.audio_tracks.len()}))
}

/// The tracks a `video` / `audio` list names. Names of tracks this sequence doesn't have (a view
/// made for a sequence with more tracks) are skipped and returned in the second list; a value
/// that is not a track name or id at all is an error.
fn listed_tracks(s: &Session, p: &Value, key: &str) -> Result<(HashSet<TrackId>, Vec<String>)> {
    const CMD: &str = "timeline.applyTrackView";
    let mut found = HashSet::new();
    let mut missing = Vec::new();
    let list = match p.get(key) {
        None | Some(Value::Null) => return Ok((found, missing)),
        Some(Value::Array(list)) => list,
        Some(other) => return Err(bad(CMD, format!("`{key}` must be a list of tracks (\"V1\", \"A2\" or ids), not {other}"))),
    };
    if list.len() > MAX_LISTED_TRACKS {
        return Err(bad(CMD, format!("`{key}` lists {} tracks (at most {MAX_LISTED_TRACKS})", list.len())));
    }
    for v in list {
        if !(v.is_string() || v.is_u64()) {
            return Err(bad(CMD, format!("`{key}` entries must be track names (\"V1\") or ids, not {v}")));
        }
        match track_p(s, &json!({ "track": v }), "track", CMD) {
            Ok(Some(id)) => {
                found.insert(id);
            }
            Ok(None) => {}
            Err(_) => missing.push(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
        }
    }
    Ok((found, missing))
}

/// `timeline.applyTrackView {video, audio, all?, label?}`: show only the listed video tracks and
/// hear only the listed audio tracks. Solo is cleared on every audio track, since a soloed track
/// would otherwise silence the view. `all: true` shows and unmutes every track instead. One undo
/// step; nothing is recorded when every track already matches.
fn track_view_apply(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "timeline.applyTrackView";
    let all = bool_p(p, "all").unwrap_or(false);
    let (video, mut missing) = listed_tracks(s, p, "video")?;
    let (audio, missing_audio) = listed_tracks(s, p, "audio")?;
    missing.extend(missing_audio);
    let label: String = str_p(p, "label").unwrap_or("Track View").chars().take(MAX_LABEL_CHARS).collect();
    if !all && video.is_empty() && audio.is_empty() {
        return Err(bad(CMD, "no track of this sequence is listed (assign tracks to the view first)"));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let show = |id: TrackId| all || video.contains(&id);
    let hear = |id: TrackId| all || audio.contains(&id);
    let changes = q.video_tracks.iter().any(|t| t.enabled != show(t.id)) || q.audio_tracks.iter().any(|t| t.muted == hear(t.id) || t.solo);
    if changes {
        s.edit_sequence(&label, |q, _, _| {
            for t in &mut q.video_tracks {
                t.enabled = show(t.id);
            }
            for t in &mut q.audio_tracks {
                t.muted = !hear(t.id);
                t.solo = false;
            }
            Ok(())
        })?;
    }
    let mut out = track_view_get(s, p)?;
    out["changed"] = json!(changes);
    out["missing"] = json!(missing);
    Ok(out)
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "timeline.trackView",
            label: "Get Track View",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: has_seq,
            run: track_view_get,
            journal: false,
        },
        CommandSpec {
            id: "timeline.applyTrackView",
            label: "Apply Track View",
            menu: &[],
            shortcut: None,
            params: r#"{"video":["V1"|id]?,"audio":["A1"|id]?,"all":bool?,"label":str?}"#,
            enabled: has_seq,
            run: track_view_apply,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    /// A sequence with 3 video and 3 audio tracks.
    fn session() -> Session {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Views", "video": 3, "audio": 3})).unwrap();
        s
    }

    fn state(s: &Session) -> (Vec<bool>, Vec<bool>) {
        let q = s.active_sequence().unwrap();
        (q.video_tracks.iter().map(|t| t.enabled).collect(), q.audio_tracks.iter().map(|t| t.muted).collect())
    }

    #[test]
    fn shows_and_hears_only_the_listed_tracks_in_one_undo_step() {
        let mut s = session();
        s.execute("timeline.setTrack", json!({"track": "A3", "solo": true})).unwrap();
        let undo = s.history.undo.len();
        let r = s.execute("timeline.applyTrackView", json!({"video": ["V2"], "audio": ["A1", "A3"], "label": "View 1"})).unwrap();
        assert_eq!(state(&s), (vec![false, true, false], vec![false, true, false]));
        assert!(s.active_sequence().unwrap().audio_tracks.iter().all(|t| !t.solo), "solo is cleared");
        assert_eq!(r["video"], json!(["V2"]));
        assert_eq!(r["audio"], json!(["A1", "A3"]));
        assert_eq!(s.history.undo.len(), undo + 1);
        assert_eq!(s.history.undo.last().unwrap().0, "View 1");
        // applying it again changes nothing and records nothing
        let r = s.execute("timeline.applyTrackView", json!({"video": ["V2"], "audio": ["A1", "A3"]})).unwrap();
        assert_eq!(r["changed"], json!(false));
        assert_eq!(s.history.undo.len(), undo + 1);
        // undo restores every track
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(state(&s), (vec![true, true, true], vec![false, false, false]));
    }

    #[test]
    fn all_shows_every_track() {
        let mut s = session();
        s.execute("timeline.applyTrackView", json!({"video": ["V1"], "audio": []})).unwrap();
        assert_eq!(state(&s), (vec![true, false, false], vec![true, true, true]));
        s.execute("timeline.applyTrackView", json!({"all": true})).unwrap();
        assert_eq!(state(&s), (vec![true, true, true], vec![false, false, false]));
    }

    #[test]
    fn tracks_the_sequence_lacks_are_skipped_and_reported() {
        let mut s = session();
        let r = s.execute("timeline.applyTrackView", json!({"video": ["V1", "V9"], "audio": ["A7"]})).unwrap();
        assert_eq!(r["missing"], json!(["V9", "A7"]));
        assert_eq!(state(&s), (vec![true, false, false], vec![true, true, true]));
    }

    #[test]
    fn hostile_parameters_are_errors_not_panics() {
        let mut s = session();
        let before = s.project.clone();
        for p in [
            json!({}),
            json!({"video": []}),
            json!({"video": "V1"}),
            json!({"video": [1.5]}),
            json!({"video": [null]}),
            json!({"audio": [{"track": "A1"}]}),
            json!({"video": ["V99999999999999999999"]}),
            json!({"video": vec!["V1"; super::MAX_LISTED_TRACKS + 1]}),
        ] {
            assert!(s.execute("timeline.applyTrackView", p.clone()).is_err(), "{p}");
        }
        assert_eq!(*s.project, *before);
        // a long label is cut, not refused
        let long = "x".repeat(10_000);
        s.execute("timeline.applyTrackView", json!({"video": ["V1"], "label": long})).unwrap();
        assert!(s.history.undo.last().unwrap().0.chars().count() <= 80);
    }

    /// Fork fix: Audio Gain on one clip picked with Alt+click (Linked Selection on) changes that
    /// clip only; it used to give every linked clip the same gain.
    #[test]
    fn audio_gain_changes_only_the_selected_linked_clip() {
        let mut s = session();
        let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        s.execute("timeline.place", json!({"item": item.0, "time": 0, "track": "V1", "audioTrack": "A1"})).unwrap();
        let q = s.active_sequence().unwrap();
        let (v, a) = (q.video_tracks[0].items[0].id, q.audio_tracks[0].items[0].id);
        assert!(q.video_tracks[0].items[0].link.is_some(), "placed picture and sound are linked");
        // Alt+click: only the audio clip
        s.execute("timeline.select", json!({"clips": [a.0], "linked": false})).unwrap();
        s.execute("clip.audioGain", json!({"mode": "set", "db": 20.0})).unwrap();
        let gain = |s: &Session, c| s.active_sequence().unwrap().find_item(c).unwrap().1.gain_db;
        assert_eq!(gain(&s, a), 20.0);
        assert_eq!(gain(&s, v), 0.0);
        // a plain click selects both, and both get it
        s.execute("timeline.select", json!({"clips": [v.0]})).unwrap();
        s.execute("clip.audioGain", json!({"mode": "set", "db": 30.0})).unwrap();
        assert_eq!(gain(&s, a), 30.0);
    }
}
