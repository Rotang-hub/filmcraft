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
//! - **Sound follows the picture.** Each view clip's own sound (its linked audio clips, or the same
//!   media placed with it; never a microphone track) is cut the same way onto the "Auto Cut" audio
//!   track(s), linked to the picture. With `muteViews` (on by default) the original view sound
//!   tracks are muted so it is not heard twice. Microphones stay as they are.
//! - **Dead air goes.** With `trim` (on by default), every stretch where nobody talks for at least
//!   `minSilence` is cut out of every unlocked track (sound with picture, captions too), keeping
//!   `pad` on each side and never inside a big moment.
//! - **The picture counts.** With `motion` (on by default) each view's movement is measured
//!   ([`crate::auto_cut_motion`]): a busy view on screen counts like lively talk and a burst of
//!   action is never cut; a still screen (menu, loading, waiting) goes first.
//! - **Dull parts go too.** After the silences, the least lively ~5 s pieces (cut between
//!   sentences) are removed until `keep` (80% by default) of the length is left. Pieces livelier
//!   than the middle, big moments, loud game or Discord moments and marked parts always stay.
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

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use filmcraft_edit::EditCtx;
use filmcraft_media::SharedSource;
use filmcraft_project::{ClipId, MarkerId, Sequence, Track, TrackId, TrackItem, TrackKind};
use filmcraft_project::{Label, Marker, MarkerKind};
use filmcraft_time::Tick;

use crate::commands::{CommandSpec, bad, f64_p, has_seq, track_p};
use crate::{EngineError, Result, Session};

/// The cut's own track.
pub const TRACK_NAME: &str = "Auto Cut";
/// At most this many sound tracks follow the cut (a view clip's linked audio clips).
const MAX_SOUND_TRACKS: usize = 8;
/// Measuring frame (seconds).
pub const FRAME: f64 = 0.05;
/// Sample rate the microphones are read at (enough for voice levels).
const RATE: u32 = 8000;
/// Longest stretch analysed (4 hours of frames).
const MAX_FRAMES: usize = (4.0 * 3600.0 / FRAME) as usize;
/// Most people.
const MAX_PEOPLE: usize = 16;
/// Level rows: people, other voices and game sound.
const MAX_ROWS: usize = 64;
/// Level of a frame with no sound.
const SILENT: f32 = -120.0;

/// One person: their name, view and microphone tracks.
#[derive(Clone, Debug)]
pub struct Person {
    pub name: String,
    pub video: TrackId,
    pub audio: Vec<TrackId>,
    /// Their view's own sound (game sound) that follows the cut; empty: found from the view's links.
    pub sound: Vec<TrackId>,
}

/// The other sound the cut listens to, besides the people's microphones.
#[derive(Clone, Debug, Default)]
pub struct Extra {
    /// Other voice tracks (a Discord recording, people without a view): kept whole, never muted,
    /// and anyone heard on them counts as talking.
    pub voices: Vec<TrackId>,
    /// The views' own sound (game sound): a loud moment there is kept with its build-up.
    pub game: Vec<TrackId>,
}

/// Every level row: the people first, then `extra.voices`, then `extra.game`.
pub struct Heard<'a> {
    pub people: &'a [Vec<f32>],
    pub voices: &'a [Vec<f32>],
    pub game: &'a [Vec<f32>],
    /// How busy the view on screen is in each frame (1.0 usual, 0 still, below 0 unknown;
    /// [`crate::auto_cut_motion`]); empty when the picture was not looked at.
    pub motion: &'a [f32],
}

impl<'a> Heard<'a> {
    pub fn split(levels: &'a [Vec<f32>], people: usize, extra: &Extra) -> Self {
        let n = people.min(levels.len());
        let v = (n + extra.voices.len()).min(levels.len());
        let (people, rest) = levels.split_at(n);
        let (voices, game) = rest.split_at(v - n);
        Self { people, voices, game, motion: &[] }
    }
}

/// Tuning (seconds).
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub min_shot: f64,
    pub buildup: f64,
    pub hold: f64,
    pub reactions: bool,
    /// Cut out the stretches where nobody talks (on every track, so sound and picture stay together).
    pub trim: bool,
    /// Shortest silence that is cut out.
    pub min_silence: f64,
    /// Kept on each side of a cut, so words are not clipped.
    pub pad: f64,
    /// Share of the (silence-trimmed) length to keep: the dullest parts go until this is left
    /// (1.0: off). Lively talk, big moments and markers always stay.
    pub keep: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { min_shot: 1.5, buildup: 2.5, hold: 1.0, reactions: true, trim: true, min_silence: 2.0, pad: 0.4, keep: 0.8 }
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
    let rows: Vec<(String, Vec<TrackId>)> = people.iter().map(|p| (p.name.clone(), p.audio.clone())).collect();
    row_jobs(s, &rows, range)
}

/// The jobs for every row: the people's microphones, then `extra.voices`, then `extra.game`
/// (one row per track). The row count is `people.len() + voices + game`.
pub fn all_level_jobs(s: &Session, people: &[Person], extra: &Extra, range: (Tick, Tick)) -> Result<(Vec<LevelJob>, usize)> {
    let mut rows: Vec<(String, Vec<TrackId>)> = people.iter().map(|p| (p.name.clone(), p.audio.clone())).collect();
    rows.extend(extra.voices.iter().chain(&extra.game).map(|t| ("voice".to_string(), vec![*t])));
    rows.truncate(MAX_ROWS);
    let n = rows.len();
    Ok((row_jobs(s, &rows, range)?, n))
}

fn row_jobs(s: &Session, rows: &[(String, Vec<TrackId>)], range: (Tick, Tick)) -> Result<Vec<LevelJob>> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut jobs = Vec::new();
    for (pi, (name, tracks)) in rows.iter().enumerate() {
        for t in tracks {
            let Some(track) = q.audio_tracks.iter().find(|x| x.id == *t) else {
                return Err(bad("edit.autoCut", format!("{name}: microphones must be audio tracks")));
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
    let mut out = vec![vec![SILENT; frames]; people.min(MAX_ROWS)];
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
pub(crate) struct Voice {
    pub(crate) talking: Vec<bool>,
    /// The first frame of the run each talking frame belongs to.
    run_start: Vec<usize>,
    pub(crate) threshold: f32,
    pub(crate) loud: f32,
    scream: f32,
}

pub(crate) fn voice(levels: &[f32]) -> Voice {
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

pub(crate) fn runs(v: &[bool]) -> Vec<(usize, usize)> {
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

/// Loud moments in a row: runs of at least 3 frames `jump` dB above its usual sounding level.
pub(crate) fn loud_moments(levels: &[f32], jump: f32) -> Vec<(usize, usize)> {
    let mut sounding: Vec<f32> = levels.iter().copied().filter(|l| *l > -100.0).collect();
    let Some(usual) = percentile(&mut sounding, 0.5) else { return Vec::new() };
    let mut loud: Vec<bool> = levels.iter().map(|l| *l > usual + jump).collect();
    close_gaps(&mut loud, 4);
    drop_short(&mut loud, 3);
    runs(&loud)
}

/// Remove the windows from the pieces.
fn carve(pieces: Vec<(usize, usize)>, windows: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut pieces = pieces;
    for (ra, rb) in windows {
        pieces = pieces
            .into_iter()
            .flat_map(
                |(x, y)| if y <= *ra || x >= *rb { vec![(x, y)] } else { [(x, (*ra).max(x)), ((*rb).min(y), y)].into_iter().filter(|(p, q)| q > p).collect() },
            )
            .collect();
    }
    pieces
}

/// The stretches to cut out: where nobody is heard talking (microphones and the other voice
/// tracks) for at least `min_silence`, less `pad` on each side. Never cut: a big moment, a loud
/// moment on a voice track (laughing on Discord) or in the game sound, each with its build-up
/// and hold, and the `keep` ranges (the user's markers). Frames `[start, end)`.
pub fn silences(heard: &Heard, tuning: &Tuning, reactions: &[Reaction], keep: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if !tuning.trim || heard.people.is_empty() {
        return Vec::new();
    }
    let n = heard.people.iter().chain(heard.voices).map(Vec::len).max().unwrap_or(0);
    let talkers: Vec<Voice> = heard.people.iter().chain(heard.voices).map(|l| voice(l)).collect();
    let quiet: Vec<bool> = (0..n).map(|f| !talkers.iter().any(|v| v.talking.get(f).copied().unwrap_or(false))).collect();
    let min = (tuning.min_silence.max(0.2) / FRAME).ceil() as usize;
    let pad = (tuning.pad.max(0.0) / FRAME).round() as usize;
    let windows = protected(heard, tuning, reactions, keep);
    let mut out = Vec::new();
    for (a, b) in runs(&quiet) {
        if b - a < min {
            continue;
        }
        // the very start and end keep no padding outside the range
        let s = if a == 0 { 0 } else { a + pad };
        let e = if b >= n { n } else { b.saturating_sub(pad) };
        if e <= s {
            continue;
        }
        out.extend(carve(vec![(s, e)], &windows).into_iter().filter(|(x, y)| y - x >= min.min(4)));
    }
    out
}

/// What is never cut: big moments, loud moments on a voice track or in the game sound (each with
/// its build-up and hold), and the `keep` ranges.
pub(crate) fn protected(heard: &Heard, tuning: &Tuning, reactions: &[Reaction], keep: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let buildup = (tuning.buildup.max(0.0) / FRAME) as usize;
    let hold = (tuning.hold.max(0.0) / FRAME) as usize;
    let around = |(a, b): (usize, usize)| (a.saturating_sub(buildup), b.saturating_add(hold));
    let mut windows: Vec<(usize, usize)> = reactions.iter().map(|r| around((r.start, r.end))).collect();
    for l in heard.voices.iter().chain(heard.game) {
        windows.extend(loud_moments(l, 10.0).into_iter().map(around));
    }
    // a burst of action on screen (a crash, a fight, a sharp turn), held for a second or more
    if !heard.motion.is_empty() {
        let mut busy: Vec<bool> = heard.motion.iter().map(|m| *m >= 1.8).collect();
        close_gaps(&mut busy, 10);
        drop_short(&mut busy, 20);
        windows.extend(runs(&busy).into_iter().map(around));
    }
    windows.extend(keep.iter().copied());
    windows
}

/// The dull parts to cut after the silences, until `tuning.keep` of the length is left.
///
/// The timeline is split into pieces of about 5 s (3–10 s) at the moments nobody is making a
/// sound (between sentences), so a cut never lands inside a word when it can be helped. Each
/// piece scores how lively it is: how many people talk and how far above their usual level,
/// with laughing and shouting counting extra. The lowest-scoring pieces go first; a piece that
/// touches a protected moment or scores above the middle never goes. Frames `[start, end)`.
pub fn dull_parts(heard: &Heard, tuning: &Tuning, reactions: &[Reaction], keep: &[(usize, usize)], silences: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if !tuning.trim || tuning.keep >= 0.999 {
        return Vec::new();
    }
    let Some(an) = analyse(heard) else { return Vec::new() };
    let n = an.n;
    let mut live = an.live.clone();
    // the picture: a busy view counts like someone talking, a still one (menu, loading) adds nothing
    for (f, m) in heard.motion.iter().enumerate() {
        if *m >= 0.0
            && let Some(s) = live.get_mut(f)
        {
            *s += 1.5 * (*m as f64).min(2.0);
        }
    }
    let removed = mask(n, silences);
    let windows = protected(heard, tuning, reactions, keep);
    // score what is left of each piece once the silences are gone
    let mut scored: Vec<(f64, usize, usize, usize)> = an
        .pieces
        .iter()
        .filter_map(|(a, b)| {
            let left = (*a..*b).filter(|f| !removed.get(*f).copied().unwrap_or(true)).count();
            if left == 0 || windows.iter().any(|(x, y)| x < b && y > a) {
                return None;
            }
            let sum: f64 = (*a..*b).filter(|f| !removed.get(*f).copied().unwrap_or(true)).filter_map(|f| live.get(f)).sum();
            Some((sum / left as f64, *a, *b, left))
        })
        .collect();
    if scored.is_empty() {
        return Vec::new();
    }
    let mut all: Vec<f64> = scored.iter().map(|x| x.0).collect();
    all.sort_by(f64::total_cmp);
    let middle = all.get(all.len() / 2).copied().unwrap_or(0.0);
    let total = removed.iter().filter(|r| !**r).count();
    let target = (total as f64 * (1.0 - tuning.keep.clamp(0.0, 1.0))) as usize;
    scored.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out = Vec::new();
    let mut gone = 0usize;
    for (score, a, b, left) in scored {
        if gone >= target || score > middle {
            break;
        }
        out.push((a, b));
        gone += left;
    }
    out.sort_unstable();
    out
}

/// Frames inside any of `ranges`.
pub(crate) fn mask(n: usize, ranges: &[(usize, usize)]) -> Vec<bool> {
    let mut r = vec![false; n];
    for (a, b) in ranges {
        for x in r.iter_mut().take((*b).min(n)).skip(*a) {
            *x = true;
        }
    }
    r
}

/// The talk of a stretch, split into pieces to keep or cut.
pub(crate) struct Analysis {
    pub(crate) n: usize,
    /// Liveliness of the talk per frame (people and other voices).
    pub(crate) live: Vec<f64>,
    /// About 5 s (3–10 s) each, split where nobody makes a sound (else at the quietest frame).
    pub(crate) pieces: Vec<(usize, usize)>,
    /// Per row (people, then other voices).
    pub(crate) talkers: Vec<Voice>,
}

pub(crate) fn analyse(heard: &Heard) -> Option<Analysis> {
    if heard.people.is_empty() {
        return None;
    }
    let n = heard.people.iter().chain(heard.voices).map(Vec::len).max().unwrap_or(0);
    if n == 0 {
        return None;
    }
    let rows: Vec<&Vec<f32>> = heard.people.iter().chain(heard.voices).collect();
    let talkers: Vec<Voice> = rows.iter().map(|l| voice(l)).collect();
    // liveliness per frame, and where nobody makes a sound at all (a safe place to cut)
    let mut live = vec![0.0f64; n];
    let mut gap = vec![true; n];
    // how much sound there is in each frame (rows sounding, then how far above): where to cut when
    // there is no gap
    let mut sound = vec![0.0f64; n];
    for (l, v) in rows.iter().zip(&talkers) {
        for f in 0..n {
            let x = l.get(f).copied().unwrap_or(SILENT);
            if x > v.threshold {
                if let Some(g) = gap.get_mut(f) {
                    *g = false;
                }
                if let Some(s) = sound.get_mut(f) {
                    *s += 100.0 + (x - v.threshold) as f64;
                }
            }
            if v.talking.get(f).copied().unwrap_or(false)
                && let Some(s) = live.get_mut(f)
            {
                let above = ((x - v.threshold) / 10.0).clamp(0.0, 1.5) as f64;
                let loud = if x > v.loud { 1.0 } else { 0.0 };
                *s += 1.0 + above + loud;
            }
        }
    }
    let (lo, hi, aim) = ((3.0 / FRAME) as usize, (10.0 / FRAME) as usize, (5.0 / FRAME) as usize);
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    let mut a = 0usize;
    while a < n {
        let end = (a + hi).min(n);
        if n - a <= hi {
            pieces.push((a, n));
            break;
        }
        // the gap nearest the aim, else the quietest frame
        let b = (a + lo..end)
            .filter(|f| gap.get(*f).copied().unwrap_or(false))
            .min_by_key(|f| f.abs_diff(a + aim))
            .or_else(|| (a + lo..end).min_by(|x, y| sound.get(*x).unwrap_or(&0.0).total_cmp(sound.get(*y).unwrap_or(&0.0))))
            .unwrap_or(end);
        pieces.push((a, b));
        a = b;
    }
    Some(Analysis { n, live, pieces, talkers })
}

/// The user's markers in `range` as frames to keep: a marker with a duration keeps its span, a
/// point marker keeps its build-up and hold. Auto Cut's own "Reaction:" markers are not counted.
pub fn keep_ranges(s: &Session, range: (Tick, Tick), tuning: &Tuning) -> Vec<(usize, usize)> {
    let Some(q) = s.active_sequence() else { return Vec::new() };
    let frame = |t: Tick| (((t.max(range.0) - range.0).seconds() / FRAME).max(0.0) as usize).min(MAX_FRAMES);
    q.markers
        .iter()
        .filter(|m| !m.name.starts_with("Reaction: ") && m.start + m.duration >= range.0 && m.start <= range.1)
        .map(|m| {
            if m.duration > Tick::ZERO {
                (frame(m.start), frame(m.start + m.duration) + 1)
            } else {
                let f = frame(m.start);
                (f.saturating_sub((tuning.buildup / FRAME) as usize), f + (tuning.hold / FRAME) as usize + 1)
            }
        })
        .collect()
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
pub fn place(
    s: &mut Session,
    people: &[Person],
    shots: &[Shot],
    reactions: &[Reaction],
    cuts: &[(usize, usize)],
    range: (Tick, Tick),
    opts: PlaceOpts,
) -> Result<Value> {
    let PlaceOpts { markers, mute_views, extra } = opts;
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let rate = seq.settings.frame_rate;
    let at = |f: usize| rate.snap_nearest(range.0 + Tick::from_seconds_f64(f as f64 * FRAME)).clamp(range.0, range.1);
    let people = people.to_vec();
    let shots: Vec<(Tick, Tick, usize, bool)> = shots.iter().map(|sh| (at(sh.start), at(sh.end), sh.person, sh.reaction)).filter(|x| x.1 > x.0).collect();
    let marks: Vec<(Tick, String)> =
        if markers { reactions.iter().filter_map(|r| people.get(r.person).map(|p| (at(r.start), p.name.clone()))).collect() } else { Vec::new() };
    let cuts: Vec<filmcraft_time::TimeRange> =
        cuts.iter().map(|(a, b)| (at(*a), at(*b))).filter(|(a, b)| b > a).map(|(a, b)| filmcraft_time::TimeRange::from_bounds(a, b)).collect();
    let cuts = filmcraft_edit::transcript::merge_ranges(cuts);
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
        let mics: HashSet<TrackId> = people.iter().flat_map(|p| p.audio.iter().copied()).chain(extra.voices.iter().copied()).collect();
        // each view clip's own sound: its linked audio (or the same media placed with it), never a
        // microphone or another voice track; the person's own `sound` tracks when they are given
        let sound_of = |q: &Sequence, person: &Person, it: &TrackItem| -> Vec<(usize, TrackItem)> {
            if !person.sound.is_empty() {
                return person
                    .sound
                    .iter()
                    .enumerate()
                    .filter_map(|(k, id)| q.audio_tracks.iter().find(|t| t.id == *id).map(|t| (k, t)))
                    .flat_map(|(k, t)| {
                        t.items
                            .iter()
                            .filter(|x| x.start < it.end() && x.end() > it.start)
                            .map(|x| (k, x.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect();
            }
            q.audio_tracks
                .iter()
                .filter(|t| !mics.contains(&t.id) && !t.name.starts_with(TRACK_NAME))
                .flat_map(|t| {
                    t.items
                        .iter()
                        .filter(|x| match (it.link, x.link) {
                            (Some(a), Some(b)) => a == b,
                            _ => x.item == it.item && x.start == it.start && x.source_in == it.source_in,
                        })
                        .map(|x| (t.id, x.clone()))
                        .collect::<Vec<_>>()
                })
                .enumerate()
                .map(|(k, (_, x))| (k, x))
                .collect()
        };
        let mut view_sound: HashSet<TrackId> = HashSet::new();
        let mut placed = Vec::new();
        let mut sound: Vec<Vec<TrackItem>> = Vec::new();
        let mut per: HashMap<usize, usize> = HashMap::new();
        for (a, b, p, _) in &shots {
            let Some(person) = people.get(*p) else { continue };
            let Some(view) = q.video_tracks.iter().find(|t| t.id == person.video) else { continue };
            let pieces: Vec<_> = view.items.iter().filter(|it| it.start < *b && it.end() > *a).cloned().collect();
            for it in pieces {
                let partners = sound_of(q, person, &it);
                for t in &q.audio_tracks {
                    if partners.iter().any(|(_, x)| t.items.iter().any(|y| y.id == x.id)) {
                        view_sound.insert(t.id);
                    }
                }
                // picture and sound of the shot, linked to each other
                let link = if partners.is_empty() { None } else { Some(ctx.alloc()) };
                for mut x in slice(&it, *a, *b, TrackKind::Video, ctx) {
                    x.link = link;
                    placed.push(x.id);
                    if let Some(t) = q.video_tracks.get_mut(idx) {
                        t.items.push(x);
                    }
                }
                let (sa, sb) = ((*a).max(it.start), (*b).min(it.end()));
                for (k, x) in partners.into_iter().filter(|(k, _)| *k < MAX_SOUND_TRACKS) {
                    while sound.len() <= k {
                        sound.push(Vec::new());
                    }
                    for mut y in slice(&x, sa, sb, TrackKind::Audio, ctx) {
                        y.link = link;
                        if let Some(v) = sound.get_mut(k) {
                            v.push(y);
                        }
                    }
                }
            }
            *per.entry(*p).or_insert(0) += 1;
        }
        if let Some(t) = q.video_tracks.get_mut(idx) {
            t.sort();
        }
        // the cut's sound tracks ("Auto Cut", "Auto Cut 2"…): replaced in the range, or made at the bottom
        let template = q.audio_tracks.iter().find(|t| view_sound.contains(&t.id)).map(|t| t.channels);
        for (k, items) in sound.into_iter().enumerate() {
            let name = if k == 0 { TRACK_NAME.to_string() } else { format!("{TRACK_NAME} {}", k + 1) };
            let i = match q.audio_tracks.iter().position(|t| t.name == name) {
                Some(i) => i,
                None => {
                    let mut t = Track::new(TrackId(ctx.alloc()), TrackKind::Audio, name.clone());
                    if let Some(c) = template {
                        t.channels = c;
                    }
                    q.audio_tracks.push(t);
                    q.audio_tracks.len() - 1
                }
            };
            let Some(t) = q.audio_tracks.get_mut(i) else { continue };
            if t.locked {
                return Err(bad("edit.autoCut", format!("the {name} audio track is locked")));
            }
            t.items.retain(|it| it.end() <= range.0 || it.start >= range.1);
            t.items.extend(items);
            t.sort();
        }
        // the views' own sound now plays from the cut: mute the originals so it is not heard twice
        let mut muted = Vec::new();
        if mute_views {
            for t in q.audio_tracks.iter_mut().filter(|t| view_sound.contains(&t.id) && !t.muted) {
                t.muted = true;
                muted.push(t.name.clone());
            }
        }
        // the silences go, on every unlocked track (sound with picture), right to left
        let removed = filmcraft_edit::transcript::ripple_delete_ranges(q, cuts.clone(), ctx);
        let shift = |t: Tick| cuts.iter().filter(|c| c.end() <= t).fold(Tick::ZERO, |acc, c| acc + c.duration);
        for (t, name) in &marks {
            let t = &(*t - shift(*t));
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
        st.selection.retain(|id| q.find_item(*id).is_some());
        let sound_tracks = q.audio_tracks.iter().filter(|t| t.name.starts_with(TRACK_NAME)).count();
        Ok(json!({"shots": shots.len(), "clips": placed.len(), "reactions": marks.len(), "people": counts, "cuts": cuts.len(), "removed": removed.seconds(), "soundTracks": sound_tracks, "muted": muted}))
    })
}

/// Switches for [`place`].
#[derive(Clone, Debug)]
pub struct PlaceOpts {
    /// The other voice tracks, which never follow the cut.
    pub extra: Extra,
    /// Add a marker at each big moment.
    pub markers: bool,
    /// Mute the views' original sound tracks once the Auto Cut audio track plays it.
    pub mute_views: bool,
}

impl PlaceOpts {
    fn from_p(p: &Value, extra: Extra) -> Self {
        let on = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(true);
        Self { markers: on("markers"), mute_views: on("muteViews"), extra }
    }
}

/// A copy of `it` (new ids, unlinked) cut down to `[a, b)`.
fn slice(it: &TrackItem, a: Tick, b: Tick, kind: TrackKind, ctx: &mut EditCtx) -> Vec<TrackItem> {
    let mut tmp = Track::new(TrackId(0), kind, String::new());
    let mut piece = it.clone();
    piece.id = ClipId(ctx.alloc());
    piece.link = None;
    tmp.items.push(piece);
    let mut links = HashMap::new();
    if it.start < a {
        filmcraft_edit::split_track_at(&mut tmp, a, ctx, &mut links);
        tmp.items.retain(|x| x.start >= a);
    }
    if it.end() > b {
        filmcraft_edit::split_track_at(&mut tmp, b, ctx, &mut links);
        tmp.items.retain(|x| x.end() <= b);
    }
    tmp.items
        .into_iter()
        .map(|mut x| {
            x.link = None;
            x
        })
        .collect()
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
        let sound = tracks_list(s, v.get("sound"), ID)?;
        if sound.iter().any(|t| audio.contains(t)) {
            return Err(bad(ID, format!("{name}: the game sound track cannot also be their microphone")));
        }
        out.push(Person { name, video, audio, sound });
    }
    if out.len() < 2 {
        return Err(bad(ID, "need at least two people to cut between"));
    }
    Ok(out)
}

/// A track or a list of tracks (`"A3"`, `["A3", "A4"]`); missing or empty: none.
fn tracks_list(s: &Session, v: Option<&Value>, id: &str) -> Result<Vec<TrackId>> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().take(16).collect(),
        Some(Value::String(x)) if x.trim().is_empty() => Vec::new(),
        Some(x) => vec![x],
    };
    for x in items {
        if let Some(t) = track_p(s, &json!({"t": x}), "t", id)?
            && !out.contains(&t)
        {
            out.push(t);
        }
    }
    Ok(out)
}

/// The other voice tracks (`voices`) and the views' game sound: each person's `sound` tracks, or
/// the audio linked to their view clips that is not a microphone or a voice track.
pub fn extra_p(s: &Session, p: &Value, people: &[Person]) -> Result<Extra> {
    const ID: &str = "edit.autoCut";
    let mics: HashSet<TrackId> = people.iter().flat_map(|x| x.audio.iter().copied()).collect();
    let voices: Vec<TrackId> = tracks_list(s, p.get("voices"), ID)?.into_iter().filter(|t| !mics.contains(t)).collect();
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    if voices.iter().any(|t| !q.audio_tracks.iter().any(|a| a.id == *t)) {
        return Err(bad(ID, "`voices` must be audio tracks"));
    }
    let mut game: Vec<TrackId> = Vec::new();
    for person in people {
        let found: Vec<TrackId> = if person.sound.is_empty() {
            let views: Vec<&TrackItem> = q.video_tracks.iter().filter(|t| t.id == person.video).flat_map(|t| &t.items).collect();
            q.audio_tracks
                .iter()
                .filter(|t| !mics.contains(&t.id) && !voices.contains(&t.id) && !t.name.starts_with(TRACK_NAME))
                .filter(|t| t.items.iter().any(|x| views.iter().any(|v| v.link.is_some() && v.link == x.link)))
                .map(|t| t.id)
                .collect()
        } else {
            person.sound.clone()
        };
        for t in found {
            if !game.contains(&t) && !voices.contains(&t) {
                game.push(t);
            }
        }
    }
    Ok(Extra { voices, game })
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
        trim: p.get("trim").and_then(Value::as_bool).unwrap_or(true),
        min_silence: sec("minSilence", d.min_silence).max(0.2),
        pad: sec("pad", d.pad).min(5.0),
        keep: f64_p(p, "keep").filter(|v| v.is_finite()).map_or(d.keep, |v| v.clamp(0.3, 1.0)),
    }
}

/// Everything Auto Cut hears and sees of the active sequence, for a cut or for learning.
struct Measured {
    people: Vec<Person>,
    extra: Extra,
    range: (Tick, Tick),
    levels: Vec<Vec<f32>>,
    /// Each person's relative motion (empty with `motion: false`).
    views: Vec<Vec<f32>>,
}

fn measure(s: &Session, p: &Value, id: &str) -> Result<Measured> {
    let people = people_p(s, p)?;
    let range = range_p(s, p)?;
    let extra = extra_p(s, p, &people)?;
    if level_jobs(s, &people, range)?.is_empty() {
        return Err(bad(id, "the microphone tracks have no clips in the range"));
    }
    let (jobs, rows) = all_level_jobs(s, &people, &extra, range)?;
    let levels = levels(&jobs, rows, range, &mut |_, _| true)?;
    let views = if p.get("motion").and_then(Value::as_bool).unwrap_or(true) {
        let jobs = crate::auto_cut_motion::motion_jobs(s, &people, range)?;
        crate::auto_cut_motion::motion(&jobs, people.len(), range, &mut |_, _| true)?
    } else {
        Vec::new()
    };
    Ok(Measured { people, extra, range, levels, views })
}

/// With a learned style, its usual shot length sets the shortest shot.
pub fn styled_tuning(mut tuning: Tuning, style: Option<&crate::auto_cut_style::Style>) -> Tuning {
    if let Some(st) = style {
        tuning.min_shot = (st.shot * 0.6).clamp(1.0, 8.0);
    }
    tuning
}

fn auto_cut(s: &mut Session, p: &Value) -> Result<Value> {
    let style = crate::auto_cut_style::style_p(p)?;
    let m = measure(s, p, "edit.autoCut")?;
    let tuning = styled_tuning(tuning_p(p), style.as_ref());
    let mut heard = Heard::split(&m.levels, m.people.len(), &m.extra);
    let (shots, reactions) = plan(heard.people, &tuning);
    let keep = keep_ranges(s, m.range, &tuning);
    let on_screen = if m.views.is_empty() { Vec::new() } else { crate::auto_cut_motion::shown(&m.views, &shots, frame_count(m.range)) };
    heard.motion = &on_screen;
    let mut cuts = silences(&heard, &tuning, &reactions, &keep);
    let more = match &style {
        Some(st) => crate::auto_cut_style::styled_cuts(&heard, &m.views, &tuning, st, &reactions, &keep, &cuts),
        None => dull_parts(&heard, &tuning, &reactions, &keep, &cuts),
    };
    cuts.extend(more);
    let range = m.range;
    place(s, &m.people, &shots, &reactions, &cuts, range, PlaceOpts::from_p(p, m.extra))
}

fn learn_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "edit.autoCutLearn";
    let raw_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let edit_id = crate::auto_cut_style::sequence_by(&s.project, p.get("result"))?;
    if edit_id == raw_id {
        return Err(bad(ID, "`result` must be the edited sequence, not the active (uncut) one"));
    }
    let prior = crate::auto_cut_style::style_p(p)?;
    let m = measure(s, p, ID)?;
    let tuning = tuning_p(p);
    let heard = Heard::split(&m.levels, m.people.len(), &m.extra);
    let (_, reactions) = plan(heard.people, &tuning);
    let (Some(raw), Some(edit)) = (s.project.sequence(raw_id), s.project.sequence(edit_id)) else { return Err(EngineError::NoSequence) };
    let labels = crate::auto_cut_style::labels(raw, edit, &m.people, m.range);
    let style = crate::auto_cut_style::learn(&heard, &m.views, &reactions, &labels, prior)?;
    Ok(json!({"summary": style.summary(), "style": style}))
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
    let cuts: Vec<(usize, usize)> = p
        .get("cuts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(MAX_FRAMES)
                .filter_map(|v| {
                    let g = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as usize);
                    let (x, y) = (g("start")?, g("end")?.min(n));
                    (y > x).then_some((x, y))
                })
                .collect()
        })
        .unwrap_or_default();
    let extra = extra_p(s, p, &people)?;
    place(s, &people, &shots, &reactions, &cuts, range, PlaceOpts::from_p(p, extra))
}

/// Cuts as JSON (`[{start, end}]`, frames).
pub fn cuts_json(cuts: &[(usize, usize)]) -> Value {
    json!(cuts.iter().map(|(a, b)| json!({"start": a, "end": b})).collect::<Vec<_>>())
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
            r#"{"people":[{"name":"라스","video":"V2","audio":"A4"|["A4"],"sound":"A5"?}],"voices":["A1"]?,"minShot":1.5?,"buildup":2.5?,"hold":1.0?,"reactions":true?,"markers":true?,"muteViews":true?,"trim":true?,"minSilence":2.0?,"pad":0.4?,"keep":0.8?,"motion":true?,"style":{…}?,"inOut":true?}"#,
            auto_cut,
        ),
        spec(
            "edit.autoCutPlace",
            "Place Auto Cut",
            r#"{"people":[…],"shots":[{"start":frame,"end":frame,"person":n,"reaction":bool?}],"reactions":[{"start","end","person"}]?,"cuts":[{"start":frame,"end":frame}]?,"voices":["A1"]?,"markers":true?,"muteViews":true?,"inOut":true?}"#,
            place_cmd,
        ),
        spec(
            "edit.autoCutLearn",
            "Learn Auto Cut from an Edit…",
            r#"{"result":"비하인드"|id,"people":[…],"voices":["A1"]?,"style":{…}?,"motion":true?,"inOut":true?}"#,
            learn_cmd,
        ),
    ]
}

#[cfg(test)]
#[path = "auto_cut_tests.rs"]
mod tests;
