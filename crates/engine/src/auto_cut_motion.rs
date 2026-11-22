//! Picture motion for Auto Cut: how busy each person's view is over time.
//!
//! Twice a second, a small grey picture (32×18) of each view clip is compared with the one half a
//! second before. The mean change says how much is moving: a still screen (standing, a menu, a
//! loading or respawn screen) is near zero; running and turning the view is high. Each person's
//! numbers are then divided by their own usual value, so a fast-paced view and a calm one count
//! the same, and the result follows whoever's view is shown.
//!
//! Views are measured on one thread each. A frame that fails to decode is left unknown (counted
//! as usual), never an error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use filmcraft_frame::{PixelData, VideoFrame};
use filmcraft_media::{FrameRequest, SharedSource};
use filmcraft_time::Tick;

use crate::auto_cut::{FRAME, Person, Shot, frame_count};
use crate::{EngineError, Result, Session};

/// Samples per second of timeline.
const STEP: usize = 10; // frames of 0.05 s: one sample every 0.5 s
const GRID_W: usize = 32;
const GRID_H: usize = 18;
/// A change below this (0–255 grey levels) is a still picture.
const STILL: f32 = 0.6;
/// Unknown (no clip, or the frame did not decode).
pub const UNKNOWN: f32 = -1.0;

/// One view clip to measure.
#[derive(Clone)]
pub struct MotionJob {
    pub person: usize,
    pub source: SharedSource,
    pub media_in: Tick,
    pub seq_start: Tick,
    pub duration: Tick,
    pub speed: f64,
}

/// The view clips of every person within `range` (enabled, forward, with picture).
pub fn motion_jobs(s: &Session, people: &[Person], range: (Tick, Tick)) -> Result<Vec<MotionJob>> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut jobs = Vec::new();
    for (pi, p) in people.iter().enumerate() {
        let Some(track) = q.video_tracks.iter().find(|t| t.id == p.video) else { continue };
        for it in track.items.iter().filter(|i| i.enabled && !i.reverse && i.speed > 0.0 && i.speed.is_finite()) {
            if it.end() <= range.0 || it.start >= range.1 {
                continue;
            }
            let Some(source) = s.source(it.item) else { continue };
            if source.info().video.is_none() {
                continue;
            }
            jobs.push(MotionJob { person: pi, source, media_in: it.source_in, seq_start: it.start, duration: it.duration, speed: it.speed });
        }
    }
    Ok(jobs)
}

/// The mean of the grey levels of `f` in a `GRID_W`×`GRID_H` grid (0–255), or `None` for an
/// empty or malformed frame.
pub fn grid(f: &VideoFrame) -> Option<Vec<f32>> {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let px = w.checked_mul(h)?;
    // grey level at (x, y)
    let grey: Box<dyn Fn(usize, usize) -> Option<f32> + '_> = match &f.data {
        PixelData::Rgba8(d) => {
            if d.len() < px.checked_mul(4)? {
                return None;
            }
            Box::new(move |x, y| {
                let i = (y * w + x) * 4;
                let p = d.get(i..i + 3)?;
                Some(0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
            })
        }
        PixelData::RgbaF32(d) => {
            if d.len() < px.checked_mul(4)? {
                return None;
            }
            Box::new(move |x, y| {
                let i = (y * w + x) * 4;
                let p = d.get(i..i + 3)?;
                let l = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
                Some(if l.is_finite() { l.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0 } else { 0.0 })
            })
        }
        PixelData::Yuv8 { planes, .. } => {
            let y = &planes[0];
            if y.len() < px {
                return None;
            }
            Box::new(move |x, yy| y.get(yy * w + x).map(|v| *v as f32))
        }
        PixelData::Yuv16 { planes, bits, .. } => {
            let y = &planes[0];
            if y.len() < px {
                return None;
            }
            let scale = 255.0 / ((1u32 << (*bits).clamp(1, 16)) - 1) as f32;
            Box::new(move |x, yy| y.get(yy * w + x).map(|v| *v as f32 * scale))
        }
    };
    let mut out = Vec::with_capacity(GRID_W * GRID_H);
    for gy in 0..GRID_H {
        let (y0, y1) = (gy * h / GRID_H, ((gy + 1) * h / GRID_H).max(gy * h / GRID_H + 1).min(h));
        for gx in 0..GRID_W {
            let (x0, x1) = (gx * w / GRID_W, ((gx + 1) * w / GRID_W).max(gx * w / GRID_W + 1).min(w));
            // a few points per cell are enough
            let (sx, sy) = (((x1 - x0) / 4).max(1), ((y1 - y0) / 4).max(1));
            let (mut sum, mut n) = (0.0f32, 0u32);
            for y in (y0..y1).step_by(sy) {
                for x in (x0..x1).step_by(sx) {
                    if let Some(v) = grey(x, y) {
                        sum += v;
                        n += 1;
                    }
                }
            }
            out.push(if n == 0 { 0.0 } else { sum / n as f32 });
        }
    }
    Some(out)
}

fn change(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return UNKNOWN;
    }
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// Raw motion of one job's samples into `row` (frames of the range).
fn measure(job: &MotionJob, row: &mut [f32], range: (Tick, Tick), done: &AtomicUsize, cancel: &AtomicBool) {
    let frames = row.len();
    let a = job.seq_start.max(range.0);
    let b = (job.seq_start + job.duration).min(range.1);
    if b <= a {
        return;
    }
    let f0 = ((a - range.0).seconds() / FRAME).ceil() as usize;
    let f1 = (((b - range.0).seconds() / FRAME).floor() as usize).min(frames);
    let mut prev: Option<Vec<f32>> = None;
    let mut f = f0;
    while f < f1 {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let t = range.0 + Tick::from_seconds_f64(f as f64 * FRAME);
        let media = job.media_in + Tick::from_seconds_f64((t - job.seq_start).seconds() * job.speed);
        let g = job.source.video_frame(FrameRequest { time: media, scale: 0.125 }).ok().and_then(|fr| grid(&fr));
        let v = match (&prev, &g) {
            (Some(p), Some(c)) => change(p, c),
            _ => UNKNOWN,
        };
        if v >= 0.0 {
            // the change over the half second before this sample
            for x in row.iter_mut().take(f.min(frames)).skip(f.saturating_sub(STEP)) {
                *x = v;
            }
        }
        prev = g;
        done.fetch_add(1, Ordering::Relaxed);
        f += STEP;
    }
}

/// Samples the jobs will take (for progress).
fn samples(jobs: &[MotionJob], range: (Tick, Tick)) -> usize {
    jobs.iter()
        .map(|j| {
            let a = j.seq_start.max(range.0);
            let b = (j.seq_start + j.duration).min(range.1);
            if b <= a { 0 } else { ((b - a).seconds() / FRAME / STEP as f64).ceil() as usize }
        })
        .sum()
}

/// Motion of every person's view in every frame of `range`, relative to their usual (1.0), 0 for
/// a still picture, [`UNKNOWN`] where nothing was measured. One thread per person;
/// `progress(done, total)` returning false cancels.
pub fn motion(jobs: &[MotionJob], people: usize, range: (Tick, Tick), progress: &mut dyn FnMut(usize, usize) -> bool) -> Result<Vec<Vec<f32>>> {
    let frames = frame_count(range);
    let people = people.min(64);
    let total = samples(jobs, range);
    let done = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut rows: Vec<Vec<f32>> = vec![vec![UNKNOWN; frames]; people];
    std::thread::scope(|sc| {
        let mut handles = Vec::new();
        for (pi, row) in rows.iter_mut().enumerate() {
            let mine: Vec<&MotionJob> = jobs.iter().filter(|j| j.person == pi).collect();
            let (done, cancel) = (done.clone(), cancel.clone());
            handles.push(sc.spawn(move || {
                // a decoder panic loses this view's motion, not the cut
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for j in mine {
                        measure(j, row, range, &done, &cancel);
                    }
                }));
            }));
        }
        loop {
            let finished = handles.iter().all(|h| h.is_finished());
            if !progress(done.load(Ordering::Relaxed).min(total), total.max(1)) {
                cancel.store(true, Ordering::Relaxed);
            }
            if finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        for h in handles {
            let _ = h.join();
        }
    });
    if cancel.load(Ordering::Relaxed) {
        return Err(EngineError::Other("cancelled".into()));
    }
    Ok(rows.into_iter().map(|r| relative(&r)).collect())
}

/// A person's raw motion divided by their usual (the median of their moving samples); a still
/// picture is 0, unknown stays unknown.
pub fn relative(raw: &[f32]) -> Vec<f32> {
    let mut moving: Vec<f32> = raw.iter().copied().filter(|v| *v >= STILL && v.is_finite()).collect();
    moving.sort_by(f32::total_cmp);
    let usual = moving.get(moving.len() / 2).copied().unwrap_or(STILL).max(STILL);
    raw.iter()
        .map(|v| {
            if !v.is_finite() || *v < 0.0 {
                UNKNOWN
            } else if *v < STILL {
                0.0
            } else {
                (v / usual).min(4.0)
            }
        })
        .collect()
}

/// The motion of the view on screen in each frame, following `shots`; unknown where nothing
/// is shown or measured.
pub fn shown(motion: &[Vec<f32>], shots: &[Shot], frames: usize) -> Vec<f32> {
    let mut out = vec![UNKNOWN; frames];
    for sh in shots {
        let Some(row) = motion.get(sh.person) else { continue };
        for f in sh.start..sh.end.min(frames) {
            if let (Some(o), Some(v)) = (out.get_mut(f), row.get(f)) {
                *o = *v;
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "auto_cut_motion_tests.rs"]
mod tests;
