//! Auto Cut that learns from the user's own edits.
//!
//! The user points at two sequences of the same recordings: the uncut one (the active sequence,
//! with each person's tracks set up as for Auto Cut) and the edit they made from it. Every clip
//! of the edit names the source time it shows, so the frames of the uncut sequence it kept are
//! known exactly, and which person's view was on screen. No sound matching is needed.
//!
//! The uncut sequence is split into the same ~5 s pieces Auto Cut uses, and each piece is
//! described by what Auto Cut can measure: how much talk, how many talkers, laughing and
//! shouting, talk on the other voice tracks, silence, loud game moments, how busy and how still
//! the views are, and whether a big moment is near. A small logistic model learns which pieces
//! the user kept. Then a new cut keeps the pieces the model rates highest, as much as the user
//! usually keeps, with their usual shot length. More edits can be added to the same style.
//!
//! The model only sees these measurements: it cannot learn a choice made for what was said.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use filmcraft_project::{Project, Sequence, TrackId};
use filmcraft_time::Tick;

use crate::Result;
use crate::auto_cut::{FRAME, Heard, Person, Reaction, Tuning, analyse, loud_moments, mask, protected};
use crate::auto_cut_motion::UNKNOWN;
use crate::commands::bad;

/// Number of features per piece.
pub const FEATURES: usize = 10;
/// Feature names, for the report.
pub const FEATURE_NAMES: [&str; FEATURES] =
    ["talk", "talkers", "laughing/shouting", "other voices", "silence", "game moments", "picture motion", "still picture", "big moment near", "length"];
const MAX_SAMPLES: usize = 20_000;

/// One learned piece.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub x: Vec<f64>,
    /// 1.0 kept, 0.0 cut.
    pub y: f64,
    /// Its length in frames (longer pieces count more).
    pub w: f64,
}

/// A learned editing style.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Style {
    pub version: u32,
    /// Edits learned from.
    pub examples: u32,
    /// Frames of the uncut sequences, and of them kept.
    pub frames: f64,
    pub kept: f64,
    /// Median shot length in the edits (seconds).
    pub shot: f64,
    /// Feature means and spreads (for standardising) and the weights (bias last).
    pub mean: Vec<f64>,
    pub spread: Vec<f64>,
    pub weights: Vec<f64>,
    /// How often the model's choice matched the edit (by length), and Auto Cut's without it.
    pub agreement: f64,
    pub before: f64,
    pub samples: Vec<Sample>,
    /// Shot lengths seen (seconds), for the median.
    pub shots: Vec<f64>,
}

impl Style {
    /// Share of the length the user keeps.
    pub fn keep(&self) -> f64 {
        if self.frames > 0.0 { (self.kept / self.frames).clamp(0.05, 1.0) } else { 0.8 }
    }

    /// A style from JSON (a saved style), checked: wrong sizes or numbers are an error.
    pub fn from_value(v: &Value) -> Result<Self> {
        let s: Style = serde_json::from_value(v.clone()).map_err(|e| bad("edit.autoCut", format!("style: {e}")))?;
        let ok = s.weights.len() == FEATURES + 1
            && s.mean.len() == FEATURES
            && s.spread.len() == FEATURES
            && s.weights.iter().chain(&s.mean).chain(&s.spread).all(|x| x.is_finite())
            && s.samples.len() <= MAX_SAMPLES
            && s.samples.iter().all(|x| x.x.len() == FEATURES && x.x.iter().all(|v| v.is_finite()) && x.w.is_finite() && x.y.is_finite());
        if !ok {
            return Err(bad("edit.autoCut", "style: not a valid Auto Cut style"));
        }
        Ok(s)
    }

    /// The chance the user would keep a piece with these features.
    pub fn rate(&self, x: &[f64]) -> f64 {
        let z: f64 = x.iter().zip(self.mean.iter().zip(&self.spread)).zip(&self.weights).map(|((v, (m, s)), w)| w * (v - m) / s.max(1e-6)).sum::<f64>()
            + self.weights.get(FEATURES).copied().unwrap_or(0.0);
        sigmoid(z)
    }

    /// The report: what was learned, for the user.
    pub fn summary(&self) -> Value {
        let mut w: Vec<(&str, f64)> = FEATURE_NAMES.iter().copied().zip(self.weights.iter().copied()).collect();
        w.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
        json!({
            "examples": self.examples,
            "keep": self.keep(),
            "shot": self.shot,
            "agreement": self.agreement,
            "before": self.before,
            "pieces": self.samples.len(),
            "weights": w.iter().map(|(n, v)| json!({"feature": n, "weight": v})).collect::<Vec<_>>(),
        })
    }
}

fn sigmoid(z: f64) -> f64 {
    if !z.is_finite() {
        return if z > 0.0 { 1.0 } else { 0.0 };
    }
    1.0 / (1.0 + (-z.clamp(-40.0, 40.0)).exp())
}

/// What the edit kept: per frame of the uncut range, kept or not, and whose view was shown.
pub struct Labels {
    pub kept: Vec<bool>,
    pub shown: Vec<Option<usize>>,
    /// Shot lengths in the edit (seconds): runs of one person's view.
    pub shots: Vec<f64>,
}

/// Finds a sequence by id or name (`result` param).
pub fn sequence_by(project: &Project, v: Option<&Value>) -> Result<filmcraft_project::ItemId> {
    const ID: &str = "edit.autoCutLearn";
    let Some(v) = v else { return Err(bad(ID, "need `result`: the edited sequence (name or id)")) };
    let found = project.sequences().find(|i| match v {
        Value::Number(n) => n.as_u64() == Some(i.id.0),
        Value::String(s) => i.name == *s || s.parse::<u64>().ok() == Some(i.id.0),
        _ => false,
    });
    found.map(|i| i.id).ok_or_else(|| bad(ID, "no such sequence"))
}

/// What the `edit` kept of `raw` in `range`: an edit clip keeps the raw frames that show the
/// same source time of the same media (on any raw video track); it shows the person whose view
/// track that is.
pub fn labels(raw: &Sequence, edit: &Sequence, people: &[Person], range: (Tick, Tick)) -> Labels {
    let n = crate::auto_cut::frame_count(range);
    let mut kept = vec![false; n];
    let mut shown: Vec<Option<usize>> = vec![None; n];
    let frame = |t: Tick| ((t - range.0).seconds() / FRAME).floor();
    let person_of = |t: TrackId| people.iter().position(|p| p.video == t);
    // each edit clip in its own time order, with the person whose view it shows
    let mut by_time: Vec<(Tick, Tick, Option<usize>)> = Vec::new();
    for c in edit.video_tracks.iter().flat_map(|t| &t.items).filter(|c| c.enabled && !c.reverse && c.speed > 0.0 && c.speed.is_finite()) {
        let (ca, cb) = (c.source_in, c.source_in + Tick::from_seconds_f64(c.duration.seconds() * c.speed));
        let mut clip_who = None;
        for t in &raw.video_tracks {
            for r in t.items.iter().filter(|r| r.item == c.item && r.speed > 0.0 && r.speed.is_finite() && !r.reverse) {
                let (ra, rb) = (r.source_in, r.source_in + Tick::from_seconds_f64(r.duration.seconds() * r.speed));
                let (a, b) = (ca.max(ra), cb.min(rb));
                if b <= a {
                    continue;
                }
                // raw timeline times of that source stretch
                let ta = r.start + Tick::from_seconds_f64((a - ra).seconds() / r.speed);
                let tb = r.start + Tick::from_seconds_f64((b - ra).seconds() / r.speed);
                let (fa, fb) = (frame(ta.max(range.0)), frame(tb.min(range.1)));
                if !(fa.is_finite() && fb.is_finite()) || fb <= fa {
                    continue;
                }
                let who = person_of(t.id);
                clip_who = clip_who.or(who);
                for f in (fa.max(0.0) as usize)..(fb.max(0.0) as usize).min(n) {
                    if let Some(k) = kept.get_mut(f) {
                        *k = true;
                    }
                    if who.is_some()
                        && let Some(s) = shown.get_mut(f)
                    {
                        *s = who;
                    }
                }
            }
        }
        if clip_who.is_some() {
            by_time.push((c.start, c.end(), clip_who));
        }
    }
    // shots: one person's run of clips = one shot
    by_time.sort_by_key(|x| x.0);
    let mut shots = Vec::new();
    let mut cur: Option<(Tick, Tick, Option<usize>)> = None;
    for (a, b, w) in by_time {
        cur = match cur {
            Some((ca, cb, cw)) if cw == w && a <= cb + Tick::from_seconds_f64(0.05) => Some((ca, cb.max(b), cw)),
            Some((ca, cb, _)) => {
                shots.push((cb - ca).seconds());
                Some((a, b, w))
            }
            None => Some((a, b, w)),
        };
    }
    if let Some((a, b, _)) = cur {
        shots.push((b - a).seconds());
    }
    Labels { kept, shown, shots }
}

/// The features of each piece.
pub fn features(heard: &Heard, views: &[Vec<f32>], reactions: &[Reaction], pieces: &[(usize, usize)]) -> Vec<[f64; FEATURES]> {
    let Some(an) = analyse(heard) else { return Vec::new() };
    let np = heard.people.len();
    let rows: Vec<&Vec<f32>> = heard.people.iter().chain(heard.voices).collect();
    let game = mask(an.n, &heard.game.iter().flat_map(|l| loud_moments(l, 10.0)).collect::<Vec<_>>());
    let near = (3.0 / FRAME) as usize;
    pieces
        .iter()
        .map(|(a, b)| {
            let (a, b) = (*a, (*b).max(*a + 1));
            let len = (b - a) as f64;
            let mut x = [0.0; FEATURES];
            let (mut talkers, mut loud, mut other, mut silent, mut gm, mut still, mut motion, mut seen) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for f in a..b {
                let talking: Vec<bool> = an.talkers.iter().map(|v| v.talking.get(f).copied().unwrap_or(false)).collect();
                let count = talking.iter().filter(|t| **t).count();
                talkers += count as f64;
                if count == 0 {
                    silent += 1.0;
                }
                if talking.iter().skip(np).any(|t| *t) {
                    other += 1.0;
                }
                if rows.iter().zip(&an.talkers).any(|(l, v)| l.get(f).copied().unwrap_or(-120.0) > v.loud) {
                    loud += 1.0;
                }
                if game.get(f).copied().unwrap_or(false) {
                    gm += 1.0;
                }
                let known: Vec<f32> = views.iter().filter_map(|r| r.get(f).copied()).filter(|m| *m != UNKNOWN && *m >= 0.0).collect();
                if !known.is_empty() {
                    seen += 1.0;
                    motion += known.iter().map(|m| (*m as f64).min(3.0)).sum::<f64>() / known.len() as f64;
                    if known.iter().all(|m| *m == 0.0) {
                        still += 1.0;
                    }
                }
            }
            x[0] = (a..b).filter_map(|f| an.live.get(f)).sum::<f64>() / len;
            x[1] = talkers / len;
            x[2] = loud / len;
            x[3] = other / len;
            x[4] = silent / len;
            x[5] = gm / len;
            x[6] = if seen > 0.0 { motion / seen } else { 1.0 };
            x[7] = if seen > 0.0 { still / seen } else { 0.0 };
            x[8] = if reactions.iter().any(|r| r.start < b + near && r.end + near > a) { 1.0 } else { 0.0 };
            x[9] = len * FRAME / 5.0;
            x
        })
        .collect()
}

fn fit(samples: &[Sample]) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let wsum: f64 = samples.iter().map(|s| s.w).sum::<f64>().max(1e-9);
    let mut mean = vec![0.0; FEATURES];
    let mut spread = vec![0.0; FEATURES];
    for s in samples {
        for (m, v) in mean.iter_mut().zip(&s.x) {
            *m += s.w * v / wsum;
        }
    }
    for s in samples {
        for ((d, v), m) in spread.iter_mut().zip(&s.x).zip(&mean) {
            *d += s.w * (v - m).powi(2) / wsum;
        }
    }
    for d in &mut spread {
        *d = d.sqrt().max(1e-3);
    }
    let z: Vec<Vec<f64>> = samples.iter().map(|s| s.x.iter().zip(mean.iter().zip(&spread)).map(|(v, (m, d))| (v - m) / d).collect()).collect();
    let mut w = vec![0.0; FEATURES + 1];
    for _ in 0..600 {
        let mut g = vec![0.0; FEATURES + 1];
        for (s, x) in samples.iter().zip(&z) {
            let p = sigmoid(x.iter().zip(&w).map(|(a, b)| a * b).sum::<f64>() + w[FEATURES]);
            let e = (p - s.y) * s.w / wsum;
            for (gi, xi) in g.iter_mut().zip(x) {
                *gi += e * xi;
            }
            g[FEATURES] += e;
        }
        for (i, (wi, gi)) in w.iter_mut().zip(&g).enumerate() {
            let l2 = if i < FEATURES { 0.01 * *wi } else { 0.0 };
            *wi -= 0.8 * (gi + l2);
        }
    }
    (mean, spread, w)
}

/// Share of the length (by piece) where keeping the `keep` best-rated pieces matches the edit.
fn agreement(scores: &[f64], samples: &[Sample], keep: f64) -> f64 {
    let total: f64 = samples.iter().map(|s| s.w).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mut order: Vec<usize> = (0..samples.len().min(scores.len())).collect();
    order.sort_by(|a, b| scores.get(*b).unwrap_or(&0.0).total_cmp(scores.get(*a).unwrap_or(&0.0)));
    let mut kept = 0.0;
    let mut right = 0.0;
    for i in order {
        let Some(s) = samples.get(i) else { continue };
        let take = kept < keep * total;
        if take {
            kept += s.w;
        }
        if take == (s.y >= 0.5) {
            right += s.w;
        }
    }
    right / total
}

/// Learns from one edit (adding to `prior` when given). `views`: each person's relative motion.
pub fn learn(heard: &Heard, views: &[Vec<f32>], reactions: &[Reaction], labels: &Labels, prior: Option<Style>) -> Result<Style> {
    let an = analyse(heard).ok_or_else(|| bad("edit.autoCutLearn", "nothing to learn from: no sound"))?;
    let kept_frames = labels.kept.iter().filter(|k| **k).count();
    if kept_frames == 0 {
        return Err(bad("edit.autoCutLearn", "the edit uses none of this sequence's clips: pick the sequence it was cut from"));
    }
    let xs = features(heard, views, reactions, &an.pieces);
    let mut new: Vec<Sample> = an
        .pieces
        .iter()
        .zip(&xs)
        .map(|((a, b), x)| {
            let len = b.saturating_sub(*a).max(1);
            let k = (*a..*b).filter(|f| labels.kept.get(*f).copied().unwrap_or(false)).count();
            Sample { x: x.to_vec(), y: if k * 2 >= len { 1.0 } else { 0.0 }, w: len as f64 }
        })
        .collect();
    let mut style = prior.unwrap_or(Style {
        version: 1,
        examples: 0,
        frames: 0.0,
        kept: 0.0,
        shot: 4.0,
        mean: vec![0.0; FEATURES],
        spread: vec![1.0; FEATURES],
        weights: vec![0.0; FEATURES + 1],
        agreement: 0.0,
        before: 0.0,
        samples: Vec::new(),
        shots: Vec::new(),
    });
    style.examples = style.examples.saturating_add(1);
    style.frames += an.n as f64;
    style.kept += kept_frames as f64;
    style.samples.append(&mut new);
    if style.samples.len() > MAX_SAMPLES {
        let extra = style.samples.len() - MAX_SAMPLES;
        style.samples.drain(..extra);
    }
    style.shots.extend(labels.shots.iter().copied().filter(|s| s.is_finite() && *s > 0.0));
    if style.shots.len() > MAX_SAMPLES {
        let extra = style.shots.len() - MAX_SAMPLES;
        style.shots.drain(..extra);
    }
    let mut sorted = style.shots.clone();
    sorted.sort_by(f64::total_cmp);
    style.shot = sorted.get(sorted.len() / 2).copied().unwrap_or(4.0).clamp(1.0, 30.0);
    let (mean, spread, weights) = fit(&style.samples);
    style.mean = mean;
    style.spread = spread;
    style.weights = weights;
    let keep = style.keep();
    let learned: Vec<f64> = style.samples.iter().map(|s| style.rate(&s.x)).collect();
    // Auto Cut without a style ranks by liveliness: talk plus picture
    let plain: Vec<f64> = style.samples.iter().map(|s| s.x.first().copied().unwrap_or(0.0) + 1.5 * s.x.get(6).copied().unwrap_or(1.0).min(2.0)).collect();
    style.agreement = agreement(&learned, &style.samples, keep);
    style.before = agreement(&plain, &style.samples, keep);
    Ok(style)
}

/// The pieces to cut in the user's style, after the silences: the lowest-rated go until the
/// user's usual share is left. Marked parts and big moments (with build-up) stay.
pub fn styled_cuts(
    heard: &Heard,
    views: &[Vec<f32>],
    tuning: &Tuning,
    style: &Style,
    reactions: &[Reaction],
    keep: &[(usize, usize)],
    silences: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    if !tuning.trim {
        return Vec::new();
    }
    let Some(an) = analyse(heard) else { return Vec::new() };
    let xs = features(heard, views, reactions, &an.pieces);
    let removed = mask(an.n, silences);
    let windows = protected(heard, tuning, reactions, keep);
    let left_total = removed.iter().filter(|r| !**r).count();
    let goal = (an.n as f64 * style.keep()) as usize;
    let mut need = left_total.saturating_sub(goal);
    let mut rated: Vec<(f64, usize, usize, usize)> = an
        .pieces
        .iter()
        .zip(&xs)
        .filter_map(|((a, b), x)| {
            let left = (*a..*b).filter(|f| !removed.get(*f).copied().unwrap_or(true)).count();
            if left == 0 || windows.iter().any(|(p, q)| p < b && q > a) {
                return None;
            }
            Some((style.rate(x), *a, *b, left))
        })
        .collect();
    rated.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out = Vec::new();
    for (p, a, b, left) in rated {
        if need == 0 || p >= 0.8 {
            break;
        }
        out.push((a, b));
        need = need.saturating_sub(left);
    }
    out.sort_unstable();
    out
}

/// The `style` param, if any.
pub fn style_p(p: &Value) -> Result<Option<Style>> {
    match p.get("style") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Style::from_value(v).map(Some),
    }
}

#[cfg(test)]
#[path = "auto_cut_style_tests.rs"]
mod tests;
