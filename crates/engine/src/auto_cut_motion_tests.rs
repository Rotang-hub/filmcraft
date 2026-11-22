use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::auto_cut::{Heard, Tuning, dull_parts, people_p};

fn grey(w: u32, h: u32, v: u8) -> VideoFrame {
    VideoFrame::rgba8(w, h, [v, v, v, 255].repeat((w * h) as usize))
}

#[test]
fn the_grid_is_the_grey_level_of_each_cell() {
    let g = grid(&grey(64, 36, 100)).unwrap();
    assert_eq!(g.len(), GRID_W * GRID_H);
    assert!(g.iter().all(|v| (v - 100.0).abs() < 0.5));
    // tiny and odd sizes are fine
    assert_eq!(grid(&grey(3, 1, 7)).unwrap().len(), GRID_W * GRID_H);
}

#[test]
fn malformed_frames_are_not_measured_and_do_not_crash() {
    let short = VideoFrame { data: PixelData::Rgba8(Arc::new(vec![0; 10])), ..grey(8, 8, 0) };
    assert!(grid(&short).is_none());
    let empty = VideoFrame { width: 0, ..grey(1, 1, 0) };
    assert!(grid(&empty).is_none());
    let yuv = VideoFrame {
        data: PixelData::Yuv16 {
            planes: [Arc::new(vec![1023; 4]), Arc::new(vec![]), Arc::new(vec![])],
            chroma: filmcraft_frame::Chroma::C420,
            bits: 99,
            alpha: None,
        },
        ..grey(2, 2, 0)
    };
    assert!(grid(&yuv).is_some());
    let huge = VideoFrame { width: u32::MAX, height: u32::MAX, ..grey(1, 1, 0) };
    assert!(grid(&huge).is_none());
}

#[test]
fn motion_is_relative_to_the_persons_usual_and_a_still_picture_is_zero() {
    let raw = [UNKNOWN, 0.1, 4.0, 4.0, 8.0, 2.0];
    let r = relative(&raw);
    assert_eq!(r[0], UNKNOWN);
    assert_eq!(r[1], 0.0, "still");
    assert!((r[2] - 1.0).abs() < 1e-6 && (r[4] - 2.0).abs() < 1e-6);
}

#[test]
fn the_shown_motion_follows_the_shots() {
    let m = vec![vec![1.0; 10], vec![3.0; 10]];
    let shots = [Shot { start: 0, end: 4, person: 0, reaction: false }, Shot { start: 4, end: 8, person: 1, reaction: false }];
    let s = shown(&m, &shots, 10);
    assert_eq!(s, [1.0, 1.0, 1.0, 1.0, 3.0, 3.0, 3.0, 3.0, UNKNOWN, UNKNOWN]);
}

#[test]
fn a_still_screen_goes_before_a_busy_one_when_the_talk_is_the_same() {
    let n = (60.0 / FRAME) as usize;
    // one person mumbling the whole time, with breaths
    let mut a = vec![-60.0f32; n];
    let mut t = 0.0;
    while t < 60.0 {
        for x in a.iter_mut().take((((t + 3.0) / FRAME) as usize).min(n)).skip((t / FRAME) as usize) {
            *x = -25.0;
        }
        t += 3.3;
    }
    let rows = [a, vec![-60.0; n]];
    // the view is busy 0–30 s and still (a menu) 30–60 s
    let motion: Vec<f32> = (0..n).map(|f| if (f as f64) * FRAME < 30.0 { 1.0 } else { 0.0 }).collect();
    let heard = Heard { people: &rows, voices: &[], game: &[], motion: &motion };
    let tuning = Tuning { keep: 0.75, reactions: false, ..Tuning::default() };
    let dull = dull_parts(&heard, &tuning, &[], &[], &[]);
    assert!(!dull.is_empty());
    assert!(dull.iter().all(|(x, _)| *x as f64 * FRAME >= 29.0), "only the still part goes: {dull:?}");
}

#[test]
fn a_burst_of_action_is_never_cut_as_silence() {
    let n = (30.0 / FRAME) as usize;
    let rows = [vec![-60.0f32; n], vec![-60.0f32; n]];
    let motion: Vec<f32> = (0..n).map(|f| if (10.0..13.0).contains(&((f as f64) * FRAME)) { 3.0 } else { 1.0 }).collect();
    let heard = Heard { people: &rows, voices: &[], game: &[], motion: &motion };
    let tuning = Tuning { reactions: false, buildup: 2.0, hold: 1.0, ..Tuning::default() };
    let cuts = crate::auto_cut::silences(&heard, &tuning, &[], &[]);
    assert!(!cuts.is_empty());
    assert!(cuts.iter().all(|(x, y)| (*y as f64 * FRAME) <= 8.0 || (*x as f64 * FRAME) >= 14.0), "{cuts:?}");
}

#[test]
fn the_demo_views_are_measured() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let people = people_p(&s, &json!({"people": [{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V1", "audio": "A2"}]})).unwrap();
    let q = s.active_sequence().unwrap();
    let range = (Tick::ZERO, q.duration());
    let jobs = motion_jobs(&s, &people, range).unwrap();
    assert!(!jobs.is_empty());
    let mut calls = 0;
    let m = motion(&jobs, 2, range, &mut |_, _| {
        calls += 1;
        true
    })
    .unwrap();
    assert_eq!(m.len(), 2);
    assert!(m[0].iter().any(|v| *v >= 0.0), "something was measured");
    // cancelling stops it
    assert!(motion(&jobs, 2, range, &mut |_, _| false).is_err());
}
