//! An uncut sequence for learning: every recording of an edit, whole and in sync.
//!
//! To learn from a finished edit (`from`, say "본편"), Auto Cut needs the uncut recordings it
//! was cut from, side by side. They are often not in any sequence whole, and the edit itself
//! shows one view at a time, so it cannot tell how the recordings line up. Another sequence of
//! the same recordings does (`synced`, found by itself when not named): wherever two of them
//! play at the same moment there, the gap between their source times is their offset.
//!
//! The new sequence copies that synced sequence's tracks (so Auto Cut's person setup fits it
//! too) and puts each track's recording on it once, from its first frame to its last, at its
//! offset. A recording whose offset cannot be found is left out and named in the result.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use filmcraft_project::{ClipId, ItemId, ItemKind, Label, Project, Sequence};
use filmcraft_time::Tick;

use crate::auto_cut_style::sequence_by;
use crate::commands::{CommandSpec, NamedKeys, bad};
use crate::{EngineError, Result, Session};

const ID: &str = "sequence.makeUncut";

/// Media (with picture) used on the video tracks of `q`.
fn view_media(project: &Project, q: &Sequence) -> HashSet<ItemId> {
    q.video_tracks
        .iter()
        .flat_map(|t| &t.items)
        .map(|c| c.item)
        .filter(|i| project.item(*i).and_then(|it| it.as_media()).is_some_and(|m| m.info.video.is_some()))
        .collect()
}

/// Source time minus sequence time of a forward, normal-speed clip.
fn base(c: &filmcraft_project::TrackItem) -> Option<i128> {
    (c.speed == 1.0 && !c.reverse).then(|| c.source_in.0 as i128 - c.start.0 as i128)
}

/// Offsets of the media in `q` relative to `reference` (source time of m = reference's + d), from
/// clips of both playing at the same moment.
fn offsets(q: &Sequence, reference: ItemId) -> HashMap<ItemId, i128> {
    let all: Vec<&filmcraft_project::TrackItem> = q.video_tracks.iter().chain(&q.audio_tracks).flat_map(|t| &t.items).collect();
    let mut out = HashMap::new();
    out.insert(reference, 0);
    // spread from the reference through any media already placed
    for _ in 0..8 {
        let mut grew = false;
        for a in &all {
            let Some(&da) = out.get(&a.item) else { continue };
            let Some(ba) = base(a) else { continue };
            for b in &all {
                if out.contains_key(&b.item) || b.start >= a.end() || a.start >= b.end() {
                    continue;
                }
                let Some(bb) = base(b) else { continue };
                // at sequence time t: src_a = t + ba, src_b = t + bb; src_ref = src_a - da
                out.insert(b.item, bb - ba + da);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    out
}

/// The sequence (not `from`) where the most of `media` play together.
fn find_synced(project: &Project, from: ItemId, media: &HashSet<ItemId>) -> Option<ItemId> {
    project
        .sequences()
        .filter(|i| i.id != from)
        .filter_map(|i| {
            let q = i.as_sequence()?;
            let here: HashSet<ItemId> = q.video_tracks.iter().flat_map(|t| &t.items).map(|c| c.item).filter(|m| media.contains(m)).collect();
            let first = here.iter().min_by_key(|m| m.0).copied()?;
            let n = offsets(q, first).keys().filter(|m| here.contains(m)).count();
            (n >= 2).then_some((n, i.id))
        })
        .max_by_key(|(n, id)| (*n, std::cmp::Reverse(id.0)))
        .map(|(_, id)| id)
}

fn make_uncut(s: &mut Session, p: &Value) -> Result<Value> {
    let from = sequence_by(&s.project, p.get("from")).map_err(|_| bad(ID, "need `from`: the edited sequence (name or id)"))?;
    let edit = s.project.sequence(from).ok_or(EngineError::NoSequence)?;
    let media = view_media(&s.project, edit);
    let synced = match p.get("synced") {
        Some(v) if !v.is_null() => sequence_by(&s.project, Some(v)).map_err(|_| bad(ID, "no such `synced` sequence"))?,
        _ => find_synced(&s.project, from, &media)
            .ok_or_else(|| bad(ID, "no other sequence plays these recordings in sync: make one with two views lined up"))?,
    };
    if synced == from {
        return Err(bad(ID, "`synced` must be another sequence"));
    }
    let src = s.project.sequence(synced).ok_or(EngineError::NoSequence)?.clone();
    let Some(reference) = src.video_tracks.iter().flat_map(|t| &t.items).map(|c| c.item).find(|m| media.contains(m)) else {
        return Err(bad(ID, "the synced sequence has none of the edit's recordings"));
    };
    let offs = offsets(&src, reference);
    let rate = src.settings.frame_rate;
    let duration_of = |id: ItemId| s.project.item(id).and_then(|it| it.as_media()).map(|m| m.info.duration);
    // where each recording starts on the new timeline, and from which source time
    let mut place: HashMap<ItemId, (Tick, Tick, Tick)> = HashMap::new();
    let offs: HashMap<ItemId, i128> = offs.into_iter().filter(|(m, _)| media.contains(m)).collect();
    // the recording that starts earliest relative to the reference sits at 0
    let lead = offs.iter().filter(|(m, _)| duration_of(**m).is_some()).map(|(_, d)| *d).max().unwrap_or(0).max(0);
    for (m, d) in &offs {
        let Some(total) = duration_of(*m) else { continue };
        // at timeline t the reference shows source t - lead, so m shows t - lead + d
        let start = rate.snap_nearest(Tick(i64::try_from((lead - d).max(0)).unwrap_or(0)));
        let src_in = i128::from(start.0) - lead + d;
        let Ok(src_in) = i64::try_from(src_in.max(0)) else { continue };
        let len = rate.snap(Tick(total.0.saturating_sub(src_in)));
        if len > Tick::ZERO {
            place.insert(*m, (start, Tick(src_in), len));
        }
    }
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} 원본", s.project.item(from).map(|i| i.name.clone()).unwrap_or_default()));
    // recordings left out (longer than a minute: not a sticker or a sound effect)
    let minute = Tick::from_seconds_f64(60.0);
    let missing: Vec<String> = media
        .iter()
        .filter(|m| !place.contains_key(m) && duration_of(**m).is_some_and(|d| d > minute))
        .filter_map(|m| s.project.item(*m).map(|i| i.name.clone()))
        .collect();
    let bin = s.project.root.parent_of(from);
    let id = s.edit("Make Uncut Sequence", |pr, _| {
        let mut q = src.clone();
        let mut links: HashMap<ItemId, u64> = HashMap::new();
        for t in q.video_tracks.iter_mut().chain(q.audio_tracks.iter_mut()) {
            // an old Auto Cut stays empty: it is not a recording
            let template = if t.name.starts_with(crate::auto_cut::TRACK_NAME) { None } else { t.items.iter().find(|c| place.contains_key(&c.item)).cloned() };
            t.items.clear();
            let Some(mut c) = template else { continue };
            let Some((start, src_in, len)) = place.get(&c.item).copied() else { continue };
            c.id = ClipId(pr.alloc_id());
            c.start = start;
            c.source_in = src_in;
            c.duration = len;
            c.enabled = true;
            let item = c.item;
            c.link = Some(*links.entry(item).or_insert_with(|| pr.alloc_id()));
            t.items.push(c);
        }
        for t in &mut q.caption_tracks {
            t.captions.clear();
            t.vtt_blocks.clear();
        }
        q.markers.clear();
        q.mark_in = None;
        q.mark_out = None;
        q.check().map_err(|e| bad(ID, e.to_string()))?;
        Ok(pr.add_item(&name, Label::Forest, ItemKind::Sequence(Arc::new(q)), bin))
    })?;
    s.execute("sequence.open", json!({"item": id.0}))?;
    let used = place.len();
    Ok(json!({"sequence": id.0, "name": name, "synced": synced.0, "recordings": used, "missing": missing}))
}

pub fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: ID,
        label: "Make Uncut Sequence for Learning…",
        menu: &[],
        shortcut: None,
        params: r#"{"from":"본편"|id,"synced":"비하인드 Copy2"|id?,"name":"본편 원본"?}"#,
        enabled: crate::commands::always,
        run: make_uncut,
        journal: true,
        named: NamedKeys::NONE,
    }]
}

#[cfg(test)]
#[path = "auto_cut_uncut_tests.rs"]
mod tests;
