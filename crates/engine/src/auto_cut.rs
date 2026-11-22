//! Fork: automatic camera cuts to whoever is talking (`edit.autoCut`).
//!
//! Each person has their own view (a video track) and their own microphone (one or more audio
//! tracks). The microphones are measured in 50 ms frames; a person is talking where their level is
//! well above their own background (each microphone gets its own threshold, so a quiet voice
//! counts as much as a loud one and voices leaking in from a call stay below it). The cut follows:
//!
//! - **The person talking.** How far each person is above their own threshold, smoothed over a
//!   second, says who is talking. A shot stays on its person while they talk; it cuts when they
//!   stop and someone else talks, or when someone else is clearly louder (a close call keeps
//!   whoever started first). Nobody talking: the shot holds.
//! - **Big moments.** A burst of loud voices (two or more people at once, or one person far above
//!   their usual level: a scream, an outburst of laughter) is a reaction. The view of whoever
//!   reacted first (usually the person something happened to) is shown from `buildup` seconds
//!   before it until `hold` seconds after it ends, whoever is talking: the lead-up and the payoff
//!   are seen from the view where it happens. Only the strongest are kept, at most one every 12
//!   seconds and each at most 4 seconds long (a lively stretch must not become one long shot). A
//!   marker names each reaction for review.
//! - **No jitter.** A shot shorter than `minShot` joins the shot before it (reaction shots stay).
//!
//! The cut is built on a video track of its own, "Auto Cut", above the people's tracks (below any
//! speaker caption tracks), from pieces of each person's own clips (same media, same timing, their
//! effects kept), so nothing on the original tracks changes and running it again replaces it. One
//! undo step.
//!
//! The work splits so a host can measure off the UI thread: [`level_jobs`] (needs the session),
//! [`levels`] and [`plan`] (pure), then `edit.autoCutPlace` with the shots. `edit.autoCut` runs
//! them all in one call.
//!
//! Kept in its own file so upstream updates rarely touch it.

use std::collections::HashMap;

use serde_json::{Value, json};

use filmcraft_media::SharedSource;
use filmcraft_project::{ClipId, MarkerId, Track, TrackId, TrackKind};
use filmcraft_project::{Label, Marker, MarkerKind};
use filmcraft_time::Tick;

use crate::commands::{CommandSpec, bad, f64_p, has_seq, track_p};
use crate::{EngineError, Result, Session};

/// The cut's own track.
pub const TRACK_NAME: &str = "Auto Cut";
/// Measuring frame (seconds).
pub const FRAME: f64 = 0.05;
/// Sample rate the microphones are read at (enough for voice levels).
const RATE: u32 = 8000;
/// Longest stretch analysed (4 hours of frames).
const MAX_FRAMES: usize = (4.0 * 3600.0 / FRAME) as usize;
/// Most people.
const MAX_PEOPLE: usize = 16;
/// Level of a frame with no sound.
const SILENT: f32 = -120.0;

/// One person: their name, view and microphone tracks.
#[derive(Clone, Debug)]
pub struct Person {
    pub name: String,
    pub video: TrackId,
    pub audio: Vec<TrackId>,
}

/// Tuning (seconds).
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub min_shot: f64,
    pub buildup: f64,
    pub hold: f64,
    pub reactions: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { min_shot: 1.5, buildup: 2.5, hold: 1.0, reactions: true }
    }
}

/// A shot: frames `[start, end)` from `person`'s view; `reaction` shots show a big moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    pub start: usize,
    pub end: usize,
    pub person: usize,
    pub reaction: bool,
}

/// A big moment: frames `[start, end)`, and who reacted first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reaction {
    pub start: usize,
    pub end: usize,
    pub person: usize,
}

/// One audio clip to measure, and where it sits on the timeline.
#[derive(Clone)]
pub struct LevelJob {
    pub person: usize,
    pub source: SharedSource,
    pub stream: usize,
    pub channels: Vec<u16>,
    pub media_in: Tick,
    pub seq_start: Tick,
    pub duration: Tick,
    pub speed: f64,
}

/// The audio clips of every person's microphone tracks within `range`.
pub fn level_jobs(s: &Session, people: &[Person], range: (Tick, Tick)) -> Result<Vec<LevelJob>> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut jobs = Vec::new();
    for (pi, p) in people.iter().enumerate() {
        for t in &p.audio {
            let Some(track) = q.audio_tracks.iter().find(|x| x.id == *t) else {
                return Err(bad("edit.autoCut", format!("{}: microphones must be audio tracks", p.name)));
            };
            for it in track.items.iter().filter(|i| i.enabled && !i.reverse && i.speed > 0.0 && i.speed.is_finite()) {
                if it.end() <= range.0 || it.start >= range.1 || s.project.resolve_media(it.item).is_none() {
                    continue;
                }
                let Some(source) = s.source(it.item) else { continue };
                jobs.push(LevelJob {
                    person: pi,
                    source,
                    stream: it.audio_stream,
                    channels: it.source_channels.clone(),
                    media_in: it.source_in,
                    seq_start: it.start,
                    duration: it.duration,
                    speed: it.speed,
                });
            }
        }
    }
    Ok(jobs)
}

/// Frames in `range`.
pub fn frame_count(range: (Tick, Tick)) -> usize {
    (((range.1 - range.0).seconds() / FRAME).ceil().max(0.0) as usize).min(MAX_FRAMES)
}

/// Level (dBFS) of every person in every frame of `range`: the loudest of their clips.
/// `progress(done, total)` returning false cancels.
pub fn levels(jobs: &[LevelJob], people: usize, range: (Tick, Tick), progress: &mut dyn FnMut(usize, usize) -> bool) -> Result<Vec<Vec<f32>>> {
    let frames = frame_count(range);
    let mut out = vec![vec![SILENT; frames]; people.min(MAX_PEOPLE)];
    let per_frame = (RATE as f64 * FRAME) as usize;
    for (ji, job) in jobs.iter().enumerate() {
        if !progress(ji, jobs.len()) {
            return Err(EngineError::Other("cancelled".into()));
        }
        let Some(row) = out.get_mut(job.person) else { continue };
        // the part of the clip inside the range, in timeline frames
        let a = job.seq_start.max(range.0);
        let b = (job.seq_start + job.duration).min(range.1);
        if b <= a {
            continue;
        }
        let f0 = ((a - range.0).seconds() / FRAME).floor() as usize;
        let f1 = (((b - range.0).seconds() / FRAME).ceil() as usize).min(frames);
        // read in chunks of 10 s of timeline
        let chunk = 200usize;
        let mut f = f0;
        while f < f1 {
            let n = chunk.min(f1 - f);
            let t = range.0 + Tick::from_seconds_f64(f as f64 * FRAME);
            let media = job.media_in + Tick::from_seconds_f64((t - job.seq_start).seconds() * job.speed);
            let start = media.to_units_floor(RATE as i64).max(0);
            let len = ((n as f64 * FRAME * job.speed) * RATE as f64) as usize;
            if let Ok(buf) = job.source.audio_stream(job.stream, start, len.max(1), RATE) {
                let chans: Vec<&Vec<f32>> = if job.channels.is_empty() {
                    buf.channels.iter().collect()
                } else {
                    job.channels.iter().filter_map(|c| buf.channels.get(*c as usize)).collect()
                };
                let step = ((per_frame as f64) * job.speed).max(1.0) as usize;
                for k in 0..n {
                    let (s0, s1) = (k * step, (k + 1) * step);
                    let mut sum = 0.0f64;
                    let mut cnt = 0usize;
                    for c in &chans {
                        for v in c.get(s0..s1.min(c.len())).unwrap_or(&[]) {
                            sum += (*v as f64) * (*v as f64);
                            cnt += 1;
                        }
                    }
                    let db = if cnt == 0 || sum <= 0.0 { SILENT } else { (10.0 * (sum / cnt as f64).log10()) as f32 };
                    if let Some(slot) = row.get_mut(f + k) {
                        *slot = slot.max(db.max(SILENT));
                    }
                }
            }
            f += n;
        }
    }
    Ok(out)
}

fn percentile(v: &mut [f32], p: f64) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v.get(i).copied()
}

/// Per person: talking in each frame, and the level that counts as loud (a reaction).
struct Voice {
    talking: Vec<bool>,
    /// The first frame of the run each talking frame belongs to.
    run_start: Vec<usize>,
    threshold: f32,
    loud: f32,
    scream: f32,
}

fn voice(levels: &[f32]) -> Voice {
    let n = levels.len();
    let mut sounding: Vec<f32> = levels.iter().copied().filter(|l| *l > -100.0).collect();
    let floor = percentile(&mut sounding, 0.10).unwrap_or(SILENT);
    let top = percentile(&mut sounding, 0.95).unwrap_or(SILENT);
    let threshold = (floor + 0.35 * (top - floor)).max(floor + 8.0);
    let mut talking: Vec<bool> = levels.iter().map(|l| *l > threshold).collect();
    // close short gaps (between syllables), drop clicks, then hang on a little after speech
    close_gaps(&mut talking, 8);
    drop_short(&mut talking, 4);
    extend(&mut talking, 6);
    let mut run_start = vec![0usize; n];
    let mut st = 0usize;
    for i in 0..n {
        if talking.get(i).copied().unwrap_or(false) {
            if i == 0 || !talking.get(i - 1).copied().unwrap_or(false) {
                st = i;
            }
            if let Some(r) = run_start.get_mut(i) {
                *r = st;
            }
        }
    }
    let mut active: Vec<f32> = levels.iter().zip(&talking).filter(|(_, t)| **t).map(|(l, _)| *l).collect();
    // loud: well above the person's usual talking level (laughing, shouting); a scream: far above
    let usual = percentile(&mut active, 0.5).unwrap_or(f32::MAX / 2.0);
    let loud = (usual + 10.0).max(threshold + 6.0);
    let scream = usual + 18.0;
    Voice { talking, run_start, threshold, loud, scream }
}

fn runs(v: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        if v[i] {
            let s = i;
            while i < v.len() && v[i] {
                i += 1;
            }
            out.push((s, i));
        } else {
            i += 1;
        }
    }
    out
}

fn close_gaps(v: &mut [bool], max: usize) {
    let r = runs(v);
    for w in r.windows(2) {
        if w[1].0 - w[0].1 <= max {
            for x in v.iter_mut().take(w[1].0).skip(w[0].1) {
                *x = true;
            }
        }
    }
}

fn drop_short(v: &mut [bool], min: usize) {
    for (s, e) in runs(v) {
        if e - s < min {
            for x in v.iter_mut().take(e).skip(s) {
                *x = false;
            }
        }
    }
}

fn extend(v: &mut [bool], by: usize) {
    let n = v.len();
    for (_, e) in runs(v) {
        for x in v.iter_mut().take((e + by).min(n)).skip(e) {
            *x = true;
        }
    }
}

/// The shots and the reactions for `levels` (one row per person, frames of [`FRAME`]).
pub fn plan(levels: &[Vec<f32>], tuning: &Tuning) -> (Vec<Shot>, Vec<Reaction>) {
    let n = levels.iter().map(Vec::len).max().unwrap_or(0);
    if n == 0 || levels.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let voices: Vec<Voice> = levels.iter().map(|l| voice(l)).collect();
    let lvl = |p: usize, f: usize| levels.get(p).and_then(|r| r.get(f)).copied().unwrap_or(SILENT);
    let talks = |p: usize, f: usize| voices.get(p).and_then(|v| v.talking.get(f)).copied().unwrap_or(false);

    // who has the shot in each frame: how far above their own threshold each person is,
    // averaged over the last second; the shot changes when someone else is clearly the one talking
    // (a close call keeps whoever started first)
    let half = (0.5 / FRAME) as usize;
    let score: Vec<Vec<f32>> = (0..voices.len())
        .map(|p| {
            let raw: Vec<f32> = (0..n).map(|f| if talks(p, f) { (lvl(p, f) - voices[p].threshold).max(0.5) } else { 0.0 }).collect();
            let mut pre = vec![0.0f64; n + 1];
            for f in 0..n {
                pre[f + 1] = pre[f] + raw[f] as f64;
            }
            // the last `half`·2 frames (what has been heard so far, so cuts never anticipate)
            (0..n)
                .map(|f| {
                    let a = (f + 1).saturating_sub(2 * half + 1);
                    ((pre[f + 1] - pre[a]) / (f + 1 - a).max(1) as f64) as f32
                })
                .collect()
        })
        .collect();
    let sc = |p: usize, f: usize| score.get(p).and_then(|r| r.get(f)).copied().unwrap_or(0.0);
    const CLEARLY: f32 = 4.0;
    let mut who: Vec<Option<usize>> = vec![None; n];
    let mut cur: Option<usize> = None;
    for (f, slot) in who.iter_mut().enumerate() {
        let best = (0..voices.len()).filter(|p| talks(*p, f)).max_by(|a, b| {
            sc(*a, f).total_cmp(&sc(*b, f)).then_with(|| {
                // equal: the one who started first
                let ra = voices[*a].run_start.get(f).copied().unwrap_or(f);
                let rb = voices[*b].run_start.get(f).copied().unwrap_or(f);
                rb.cmp(&ra)
            })
        });
        cur = match (cur, best) {
            (_, None) => cur,
            (None, b) => b,
            (Some(c), Some(b)) if !talks(c, f) => Some(b),
            (Some(c), Some(b)) if sc(b, f) > sc(c, f) + CLEARLY => Some(b),
            (c, _) => c,
        };
        *slot = cur;
    }
    // before anyone talks: the first person who does
    let first = who.iter().flatten().next().copied().unwrap_or(0);
    for s in who.iter_mut() {
        if s.is_none() {
            *s = Some(first);
        } else {
            break;
        }
    }

    // big moments: two or more people loud at the same moment, or one screaming. The strongest
    // are kept, at most one per `SPACING`, each shown for at most `LONGEST`
    let mut reactions = Vec::new();
    if tuning.reactions {
        const SPACING: f64 = 12.0;
        const LONGEST: f64 = 4.0;
        let loud = |p: usize, f: usize| lvl(p, f) > voices[p].loud;
        let near = (0.25 / FRAME) as usize;
        let hot: Vec<bool> = (0..n)
            .map(|f| {
                let lo = f.saturating_sub(near);
                let hi = (f + near).min(n - 1);
                let many = (0..voices.len()).filter(|p| (lo..=hi).any(|g| loud(*p, g))).count() >= 2 && (0..voices.len()).any(|p| loud(p, f));
                many || (0..voices.len()).any(|p| lvl(p, f) > voices[p].scream)
            })
            .collect();
        let mut hot = hot;
        close_gaps(&mut hot, 6);
        drop_short(&mut hot, 6);
        let mut found: Vec<(f32, Reaction)> = Vec::new();
        for (s, e) in runs(&hot) {
            // who got loud first, looking a little before the burst
            let look = s.saturating_sub(10);
            let Some(p) = (0..voices.len()).filter_map(|p| (look..e).find(|f| loud(p, *f)).map(|f| (f, p))).min().map(|x| x.1) else { continue };
            let strength: f32 = (0..voices.len()).map(|q| (s..e).map(|f| (lvl(q, f) - voices[q].loud).max(0.0)).fold(0.0, f32::max)).sum();
            let e = e.min(s + (LONGEST / FRAME) as usize);
            found.push((strength, Reaction { start: s, end: e, person: p }));
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        let spacing = (SPACING / FRAME) as usize;
        for (_, r) in found {
            if reactions.iter().all(|k: &Reaction| r.start.abs_diff(k.start) >= spacing) {
                reactions.push(r);
            }
        }
        reactions.sort_by_key(|r| r.start);
        let buildup = (tuning.buildup.max(0.0) / FRAME) as usize;
        let hold = (tuning.hold.max(0.0) / FRAME) as usize;
        for r in &reactions {
            for slot in who.iter_mut().take((r.end + hold).min(n)).skip(r.start.saturating_sub(buildup)) {
                *slot = Some(r.person);
            }
        }
    }
    let protected = |f: usize| {
        let buildup = (tuning.buildup.max(0.0) / FRAME) as usize;
        let hold = (tuning.hold.max(0.0) / FRAME) as usize;
        reactions.iter().any(|r| f + buildup >= r.start && f < r.end + hold)
    };

    // shots, then short ones join the one before
    let mut shots: Vec<Shot> = Vec::new();
    for (f, w) in who.iter().enumerate() {
        let p = w.unwrap_or(first);
        let reaction = protected(f);
        match shots.last_mut() {
            Some(s) if s.person == p && s.reaction == reaction => s.end = f + 1,
            _ => shots.push(Shot { start: f, end: f + 1, person: p, reaction }),
        }
    }
    let min = (tuning.min_shot.max(0.0) / FRAME) as usize;
    let mut out: Vec<Shot> = Vec::new();
    for s in shots {
        match out.last_mut() {
            Some(prev) if prev.person == s.person => {
                prev.end = s.end;
                prev.reaction |= s.reaction;
            }
            Some(prev) if s.end - s.start < min && !s.reaction => prev.end = s.end,
            _ => out.push(s),
        }
    }
    // a short first shot joins the next one
    if out.len() > 1 && out[0].end - out[0].start < min && !out[0].reaction {
        let first = out.remove(0);
        if let Some(next) = out.first_mut() {
            next.start = first.start;
        }
    }
    (out, reactions)
}

/// How much of the time each person is heard talking (0–1), to check the microphones.
pub fn talk_share(levels: &[Vec<f32>]) -> Vec<f32> {
    levels
        .iter()
        .map(|l| {
            let v = voice(l);
            v.talking.iter().filter(|t| **t).count() as f32 / v.talking.len().max(1) as f32
        })
        .collect()
}

/// Build the cut on the "Auto Cut" track (replacing what it held in `range`), one undo step.
pub fn place(s: &mut Session, people: &[Person], shots: &[Shot], reactions: &[Reaction], range: (Tick, Tick), markers: bool) -> Result<Value> {
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let rate = seq.settings.frame_rate;
    let at = |f: usize| rate.snap_nearest(range.0 + Tick::from_seconds_f64(f as f64 * FRAME)).clamp(range.0, range.1);
    let people = people.to_vec();
    let shots: Vec<(Tick, Tick, usize, bool)> = shots.iter().map(|sh| (at(sh.start), at(sh.end), sh.person, sh.reaction)).filter(|x| x.1 > x.0).collect();
    let marks: Vec<(Tick, String)> =
        if markers { reactions.iter().filter_map(|r| people.get(r.person).map(|p| (at(r.start), p.name.clone()))).collect() } else { Vec::new() };
    s.edit_sequence("Auto Cut", move |q, ctx, st| {
        // the cut's track: replaced in the range, or made above the people's views
        let idx = match q.video_tracks.iter().position(|t| t.name == TRACK_NAME) {
            Some(i) => {
                if let Some(t) = q.video_tracks.get_mut(i) {
                    if t.locked {
                        return Err(bad("edit.autoCut", "the Auto Cut track is locked"));
                    }
                    t.items.retain(|it| it.end() <= range.0 || it.start >= range.1);
                }
                i
            }
            None => {
                let below = q.video_tracks.iter().position(|t| t.name.starts_with(crate::speaker_captions::TRACK_NAME)).unwrap_or(q.video_tracks.len());
                q.video_tracks.insert(below, Track::new(TrackId(ctx.alloc()), TrackKind::Video, TRACK_NAME.to_string()));
                below
            }
        };
        let mut placed = Vec::new();
        let mut per: HashMap<usize, usize> = HashMap::new();
        for (a, b, p, _) in &shots {
            let Some(person) = people.get(*p) else { continue };
            let Some(view) = q.video_tracks.iter().find(|t| t.id == person.video) else { continue };
            let pieces: Vec<_> = view.items.iter().filter(|it| it.start < *b && it.end() > *a).cloned().collect();
            for it in pieces {
                // a copy of the clip, cut down to the shot
                let mut tmp = Track::new(TrackId(0), TrackKind::Video, String::new());
                let mut piece = it.clone();
                piece.id = ClipId(ctx.alloc());
                piece.link = None;
                tmp.items.push(piece);
                let mut links = HashMap::new();
                if it.start < *a {
                    filmcraft_edit::split_track_at(&mut tmp, *a, ctx, &mut links);
                    tmp.items.retain(|x| x.start >= *a);
                }
                if it.end() > *b {
                    filmcraft_edit::split_track_at(&mut tmp, *b, ctx, &mut links);
                    tmp.items.retain(|x| x.end() <= *b);
                }
                for mut x in tmp.items {
                    x.link = None;
                    placed.push(x.id);
                    if let Some(t) = q.video_tracks.get_mut(idx) {
                        t.items.push(x);
                    }
                }
            }
            *per.entry(*p).or_insert(0) += 1;
        }
        if let Some(t) = q.video_tracks.get_mut(idx) {
            t.sort();
        }
        for (t, name) in &marks {
            q.markers.push(Marker {
                url: String::new(),
                author: String::new(),
                custom_color: None,
                id: MarkerId(ctx.alloc()),
                start: *t,
                duration: Tick::ZERO,
                name: format!("Reaction: {name}"),
                comment: String::new(),
                kind: MarkerKind::Comment,
                color: Label::Rose,
            });
        }
        q.check().map_err(EngineError::Other)?;
        st.selection = placed.clone();
        let mut counts: Vec<Value> = per.iter().filter_map(|(p, n)| people.get(*p).map(|x| json!({"name": x.name, "shots": n}))).collect();
        counts.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_string());
        Ok(json!({"shots": shots.len(), "clips": placed.len(), "reactions": marks.len(), "people": counts}))
    })
}

/// The people of a request: `[{name, video, audio}]` (`audio` a track or a list).
pub fn people_p(s: &Session, p: &Value) -> Result<Vec<Person>> {
    const ID: &str = "edit.autoCut";
    let list = p.get("people").and_then(Value::as_array).ok_or_else(|| bad(ID, "need `people`: [{name, video, audio}]"))?;
    let mut out = Vec::new();
    for v in list.iter().take(MAX_PEOPLE) {
        let name = v.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or("?").to_string();
        let video = track_p(s, v, "video", ID)?.ok_or_else(|| bad(ID, format!("{name}: need `video` (their view's track)")))?;
        let audio: Vec<TrackId> = match v.get("audio") {
            Some(Value::Array(a)) => {
                let mut ts = Vec::new();
                for x in a.iter().take(16) {
                    if let Some(t) = track_p(s, &json!({"t": x}), "t", ID)? {
                        ts.push(t);
                    }
                }
                ts
            }
            Some(_) => track_p(s, v, "audio", ID)?.into_iter().collect(),
            None => Vec::new(),
        };
        if audio.is_empty() {
            return Err(bad(ID, format!("{name}: need `audio` (their microphone track)")));
        }
        out.push(Person { name, video, audio });
    }
    if out.len() < 2 {
        return Err(bad(ID, "need at least two people to cut between"));
    }
    Ok(out)
}

/// The range to cut: In to Out when both are marked and `inOut` is not false, else the sequence.
pub fn range_p(s: &Session, p: &Value) -> Result<(Tick, Tick)> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let end = q.duration();
    let use_marks = p.get("inOut").and_then(Value::as_bool).unwrap_or(true);
    let r = match (use_marks, q.mark_in, q.mark_out) {
        (true, Some(a), Some(b)) if b > a => (a.max(Tick::ZERO), b.min(end.max(b))),
        _ => (Tick::ZERO, end),
    };
    if r.1 <= r.0 {
        return Err(bad("edit.autoCut", "the sequence is empty"));
    }
    Ok(r)
}

pub fn tuning_p(p: &Value) -> Tuning {
    let d = Tuning::default();
    let sec = |k: &str, dflt: f64| f64_p(p, k).filter(|v| v.is_finite()).map_or(dflt, |v| v.clamp(0.0, 30.0));
    Tuning {
        min_shot: sec("minShot", d.min_shot),
        buildup: sec("buildup", d.buildup),
        hold: sec("hold", d.hold),
        reactions: p.get("reactions").and_then(Value::as_bool).unwrap_or(true),
    }
}

fn auto_cut(s: &mut Session, p: &Value) -> Result<Value> {
    let people = people_p(s, p)?;
    let range = range_p(s, p)?;
    let jobs = level_jobs(s, &people, range)?;
    if jobs.is_empty() {
        return Err(bad("edit.autoCut", "the microphone tracks have no clips in the range"));
    }
    let lv = levels(&jobs, people.len(), range, &mut |_, _| true)?;
    let (shots, reactions) = plan(&lv, &tuning_p(p));
    place(s, &people, &shots, &reactions, range, p.get("markers").and_then(Value::as_bool).unwrap_or(true))
}

fn place_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let people = people_p(s, p)?;
    let range = range_p(s, p)?;
    let n = frame_count(range);
    let shots: Vec<Shot> = p
        .get("shots")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("edit.autoCutPlace", "need `shots`"))?
        .iter()
        .take(MAX_FRAMES)
        .filter_map(|v| {
            let g = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as usize);
            let (a, b, who) = (g("start")?, g("end")?.min(n), g("person")?);
            (b > a && who < people.len()).then_some(Shot {
                start: a,
                end: b,
                person: who,
                reaction: v.get("reaction").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    let reactions: Vec<Reaction> = p
        .get("reactions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let g = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as usize);
                    Some(Reaction { start: g("start")?, end: g("end")?, person: g("person")? }).filter(|r| r.person < people.len() && r.start < n)
                })
                .collect()
        })
        .unwrap_or_default();
    if shots.is_empty() {
        return Err(bad("edit.autoCutPlace", "no shots"));
    }
    place(s, &people, &shots, &reactions, range, p.get("markers").and_then(Value::as_bool).unwrap_or(true))
}

pub fn shots_json(shots: &[Shot], reactions: &[Reaction]) -> (Value, Value) {
    (
        json!(shots.iter().map(|s| json!({"start": s.start, "end": s.end, "person": s.person, "reaction": s.reaction})).collect::<Vec<_>>()),
        json!(reactions.iter().map(|r| json!({"start": r.start, "end": r.end, "person": r.person})).collect::<Vec<_>>()),
    )
}

type Run = fn(&mut Session, &Value) -> Result<Value>;

fn spec(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: has_seq, run, journal: true, named: crate::commands::NamedKeys::NONE }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "edit.autoCut",
            "Auto Cut to Speaker…",
            r#"{"people":[{"name":"라스","video":"V2","audio":"A4"|["A4"]}],"minShot":1.5?,"buildup":2.5?,"hold":1.0?,"reactions":true?,"markers":true?,"inOut":true?}"#,
            auto_cut,
        ),
        spec(
            "edit.autoCutPlace",
            "Place Auto Cut",
            r#"{"people":[…],"shots":[{"start":frame,"end":frame,"person":n,"reaction":bool?}],"reactions":[{"start","end","person"}]?,"markers":true?,"inOut":true?}"#,
            place_cmd,
        ),
    ]
}

#[cfg(test)]
#[path = "auto_cut_tests.rs"]
mod tests;
