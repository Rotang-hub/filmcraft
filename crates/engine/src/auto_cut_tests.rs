use serde_json::json;

use super::*;
use crate::Session;

/// A person's level over `n` frames: `quiet` dB, `talk` dB where `spans` (seconds) say so.
fn row(n: usize, spans: &[(f64, f64, f32)]) -> Vec<f32> {
    let mut v = vec![-60.0f32; n];
    for (a, b, db) in spans {
        for x in v.iter_mut().take(((*b / FRAME) as usize).min(n)).skip((*a / FRAME) as usize) {
            *x = *db;
        }
    }
    v
}

fn quiet_tuning() -> Tuning {
    Tuning { reactions: false, ..Tuning::default() }
}

fn people_of(shots: &[Shot]) -> Vec<usize> {
    shots.iter().map(|s| s.person).collect()
}

#[test]
fn the_shot_follows_whoever_talks() {
    let n = (20.0 / FRAME) as usize;
    // A talks 0–5 s, B 6–12 s, A 13–18 s
    let a = row(n, &[(0.0, 5.0, -20.0), (13.0, 18.0, -20.0)]);
    let b = row(n, &[(6.0, 12.0, -22.0)]);
    let (shots, _) = plan(&[a, b], &quiet_tuning());
    assert_eq!(people_of(&shots), [0, 1, 0]);
    assert!(shots.windows(2).all(|w| w[0].end == w[1].start), "back to back");
    // the cut to B comes as B starts (within the hang-on after A)
    let cut = shots[1].start as f64 * FRAME;
    assert!((5.0..=6.2).contains(&cut), "{cut}");
}

#[test]
fn whoever_started_first_keeps_the_shot_when_both_talk() {
    let n = (12.0 / FRAME) as usize;
    let a = row(n, &[(0.0, 8.0, -20.0)]);
    let b = row(n, &[(3.0, 10.0, -20.0)]);
    let (shots, _) = plan(&[a, b], &quiet_tuning());
    assert_eq!(people_of(&shots), [0, 1]);
    assert!(shots[1].start as f64 * FRAME >= 8.0, "B gets the shot only once A stops");
}

#[test]
fn a_quiet_microphone_counts_as_much_as_a_loud_one_and_leaks_do_not() {
    let n = (14.0 / FRAME) as usize;
    // A is loud; B's microphone is quiet overall, and A's voice leaks into it far below B's own
    let a = row(n, &[(0.0, 6.0, -12.0)]);
    let mut b = row(n, &[(7.0, 13.0, -38.0)]);
    for x in b.iter_mut().take((6.0 / FRAME) as usize) {
        *x = -54.0;
    }
    let (shots, _) = plan(&[a, b], &quiet_tuning());
    assert_eq!(people_of(&shots), [0, 1]);
}

#[test]
fn short_shots_join_the_one_before() {
    let n = (12.0 / FRAME) as usize;
    // B says one word (0.5 s) in the middle of A's long stretch of talk
    let a = row(n, &[(0.0, 4.0, -20.0), (5.5, 11.0, -20.0)]);
    let b = row(n, &[(4.4, 4.9, -20.0)]);
    let (shots, _) = plan(&[a, b], &quiet_tuning());
    assert_eq!(people_of(&shots), [0], "{shots:?}");
}

#[test]
fn a_reaction_shows_who_reacted_first_from_before_it_happens() {
    let n = (30.0 / FRAME) as usize;
    // A narrates the whole time; at 20 s B screams first and A laughs a moment later
    let mut a = row(n, &[(0.0, 30.0, -24.0)]);
    for (i, x) in a.iter_mut().enumerate() {
        // talk with pauses so A's levels vary
        if (i / 20) % 3 == 2 {
            *x = -60.0;
        }
    }
    for x in a.iter_mut().take((22.0 / FRAME) as usize).skip((20.4 / FRAME) as usize) {
        *x = -4.0;
    }
    let mut b = row(n, &[(2.0, 3.0, -26.0), (8.0, 9.0, -26.0)]);
    for x in b.iter_mut().take((21.5 / FRAME) as usize).skip((20.0 / FRAME) as usize) {
        *x = -3.0;
    }
    let t = Tuning { buildup: 3.0, hold: 1.0, ..Tuning::default() };
    let (shots, reactions) = plan(&[a, b], &t);
    assert_eq!(reactions.len(), 1, "{reactions:?}");
    assert_eq!(reactions[0].person, 1, "B got loud first");
    let r = shots.iter().find(|s| s.reaction).expect("a reaction shot");
    assert_eq!(r.person, 1);
    let start = r.start as f64 * FRAME;
    assert!((16.5..=17.5).contains(&start), "the build-up starts ~3 s before: {start}");
    assert!(r.end as f64 * FRAME >= 22.0, "and holds after it");
}

#[test]
fn nothing_to_measure_plans_nothing() {
    assert_eq!(plan(&[], &Tuning::default()), (Vec::new(), Vec::new()));
    let (shots, _) = plan(&[vec![SILENT; 100], vec![SILENT; 100]], &Tuning::default());
    assert_eq!(people_of(&shots), [0], "silence: one shot on the first view");
}

#[test]
fn long_silences_are_cut_with_padding_and_short_pauses_stay() {
    let n = (20.0 / FRAME) as usize;
    // A talks 0–5 s, a 0.5 s pause, 5.5–8 s; nobody until B at 12–18 s; then quiet to the end
    let a = row(n, &[(0.0, 5.0, -20.0), (5.5, 8.0, -20.0)]);
    let b = row(n, &[(12.0, 18.0, -20.0)]);
    let tuning = quiet_tuning();
    let cuts = silences(&[a.clone(), b.clone()], &tuning, &[]);
    let secs: Vec<(f64, f64)> = cuts.iter().map(|(x, y)| (*x as f64 * FRAME, *y as f64 * FRAME)).collect();
    assert_eq!(cuts.len(), 2, "{secs:?}");
    // the gap 8–12 s goes, less the padding (and the hang-on after words)
    let (s0, e0) = secs[0];
    assert!((8.0..=9.0).contains(&s0) && (11.0..12.0).contains(&e0), "{secs:?}");
    // the quiet tail goes to the very end
    assert!(secs[1].0 >= 18.0 && (secs[1].1 - 20.0).abs() < 1e-9, "{secs:?}");
    // the short pause inside A's talk stays
    assert!(secs.iter().all(|(x, y)| *y <= 5.0 || *x >= 5.5), "{secs:?}");
    // off: nothing is cut
    assert!(silences(&[a, b], &Tuning { trim: false, ..tuning }, &[]).is_empty());
}

#[test]
fn a_big_moment_and_its_build_up_are_never_cut() {
    let n = (20.0 / FRAME) as usize;
    let a = row(n, &[(0.0, 2.0, -20.0), (15.0, 20.0, -20.0)]);
    let b = row(n, &[]);
    let r = Reaction { start: (10.0 / FRAME) as usize, end: (11.0 / FRAME) as usize, person: 1 };
    let tuning = Tuning { buildup: 2.5, hold: 1.0, ..quiet_tuning() };
    let cuts = silences(&[a, b], &tuning, &[r]);
    let keep = ((7.5 / FRAME) as usize, (12.0 / FRAME) as usize);
    assert!(cuts.iter().all(|(x, y)| *y <= keep.0 || *x >= keep.1), "{cuts:?}");
    assert!(cuts.len() == 2, "before and after the moment: {cuts:?}");
}

#[test]
fn cutting_silences_cuts_sound_and_picture_together_in_one_undo() {
    let mut s = demo();
    let people = json!([{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]);
    let before = s.active_sequence().unwrap().clone();
    let end_of = |items: &[filmcraft_project::TrackItem]| items.iter().map(|i| i.end()).max().unwrap_or(Tick::ZERO);
    let (v_before, a_before) = (end_of(&before.video_tracks[0].items), end_of(&before.audio_tracks[0].items));
    // 1–2 s is cut (frames of 0.05 s)
    let r = s
        .execute(
            "edit.autoCutPlace",
            json!({"people": people, "shots": [{"start": 0, "end": 100, "person": 0}], "cuts": [{"start": 20, "end": 40}, {"start": 30, "end": 35}, {"start": 9, "end": 3}]}),
        )
        .unwrap();
    assert_eq!(r["cuts"], 1, "overlaps merge, backwards ranges are dropped: {r}");
    let q = s.active_sequence().unwrap();
    let rate = q.settings.frame_rate;
    let gone = rate.snap_nearest(Tick::from_seconds_f64(2.0)) - rate.snap_nearest(Tick::from_seconds_f64(1.0));
    assert!((r["removed"].as_f64().unwrap() - gone.seconds()).abs() < 1e-9);
    // picture, sound and the cut all shrink by the same amount
    assert_eq!(end_of(&q.video_tracks[0].items), v_before - gone);
    assert_eq!(end_of(&q.audio_tracks[0].items), a_before - gone);
    let cut = q.video_tracks.iter().find(|t| t.name == TRACK_NAME).unwrap();
    assert_eq!(end_of(&cut.items), rate.snap_nearest(Tick::from_seconds_f64(5.0)) - gone);
    q.check().unwrap();
    // one undo brings everything back
    s.execute("edit.undo", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    assert_eq!(q.video_tracks[0].items, before.video_tracks[0].items);
    assert_eq!(q.audio_tracks[0].items, before.audio_tracks[0].items);
}

/// The demo project with a second view on V2 (a copy of V1) and microphones on A1 / A2.
fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.edit_sequence("test", |q, ctx, _| {
        let mut copy: Vec<_> = q.video_tracks[0].items.clone();
        for it in &mut copy {
            it.id = ClipId(ctx.alloc());
            it.link = None;
        }
        q.video_tracks[1].items = copy;
        Ok(())
    })
    .unwrap();
    s
}

#[test]
fn the_cut_is_built_on_its_own_track_from_the_views() {
    let mut s = demo();
    let people = json!([{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": ["A2"]}]);
    let before = s.active_sequence().unwrap().clone();
    let shots = json!([{"start": 0, "end": 60, "person": 0}, {"start": 60, "end": 100, "person": 1, "reaction": true}]);
    let r = s.execute("edit.autoCutPlace", json!({"people": people, "shots": shots, "reactions": [{"start": 70, "end": 80, "person": 1}]})).unwrap();
    assert_eq!(r["shots"], 2);
    assert_eq!(r["reactions"], 1);
    let q = s.active_sequence().unwrap();
    let cut = q.video_tracks.iter().find(|t| t.name == TRACK_NAME).expect("the Auto Cut track");
    let src_ids: Vec<_> = before.video_tracks[0].items.iter().chain(&before.video_tracks[1].items).map(|i| i.id).collect();
    assert!(cut.items.iter().all(|i| !src_ids.contains(&i.id) && i.link.is_none()), "copies, with ids of their own");
    let end = cut.items.iter().map(|i| i.end()).max().unwrap();
    let rate = q.settings.frame_rate;
    assert_eq!(end, rate.snap_nearest(Tick::from_seconds_f64(5.0)));
    // the piece at 3 s+ comes from V2 and shows the same media time as V2 does there
    let t = Tick::from_seconds_f64(4.0);
    let piece = cut.items.iter().find(|i| i.start <= t && t < i.end()).unwrap();
    let orig = q.video_tracks[1].items.iter().find(|i| i.start <= t && t < i.end()).unwrap();
    assert_eq!(piece.item, orig.item);
    assert_eq!(piece.source_in + (piece.start - piece.start), orig.source_in + (piece.start - orig.start));
    assert!(q.markers.iter().any(|m| m.name == "Reaction: B"));
    // the original tracks are untouched
    assert_eq!(q.video_tracks[0].items, before.video_tracks[0].items);
    // running again replaces the cut; one undo step each
    s.execute("edit.autoCutPlace", json!({"people": people, "shots": [{"start": 0, "end": 40, "person": 1}], "markers": false})).unwrap();
    let q = s.active_sequence().unwrap();
    assert_eq!(q.video_tracks.iter().filter(|t| t.name == TRACK_NAME).count(), 1);
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active_sequence().unwrap().video_tracks.iter().all(|t| t.name != TRACK_NAME));
}

#[test]
fn the_whole_command_measures_the_microphones() {
    let mut s = demo();
    let r = s.execute("edit.autoCut", json!({"people": [{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]})).unwrap();
    assert!(r["shots"].as_u64().unwrap() >= 1, "{r}");
    assert!(s.active_sequence().unwrap().video_tracks.iter().any(|t| t.name == TRACK_NAME && !t.items.is_empty()));
}

#[test]
fn bad_requests_are_errors_not_crashes() {
    let mut s = demo();
    for p in [
        json!({}),
        json!({"people": []}),
        json!({"people": [{"name": "A", "video": "V1", "audio": "A1"}]}),
        json!({"people": [{"name": "A", "video": "V1"}, {"name": "B", "video": "V2", "audio": "A2"}]}),
        json!({"people": [{"name": "A", "video": "V1", "audio": "V2"}, {"name": "B", "video": "V2", "audio": "A2"}]}),
        json!({"people": [{"name": "A", "video": "V99", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]}),
    ] {
        assert!(s.execute("edit.autoCut", p.clone()).is_err(), "{p}");
    }
    let people = json!([{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]);
    for shots in [json!([]), json!([{"start": 5, "end": 2, "person": 0}]), json!([{"start": 0, "end": 9, "person": 7}]), json!("x")] {
        assert!(s.execute("edit.autoCutPlace", json!({"people": people, "shots": shots})).is_err());
    }
}

/// A tiny deterministic random source for the busy-scene test.
fn lcg(seed: &mut u64) -> f64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*seed >> 11) as f64 / (1u64 << 53) as f64
}

#[test]
fn a_busy_scene_with_laughs_all_the_time_still_cuts_between_people() {
    // regression: frequent laughter merged into one endless "big moment" that held one view
    let secs = 180.0;
    let n = (secs / FRAME) as usize;
    let mut rows = vec![vec![-62.0f32; n]; 4];
    let mut seed = 7u64;
    let mut t = 0.0;
    // people take turns talking (with mid-word dips), each 2–6 s
    while t < secs {
        let p = (lcg(&mut seed) * 4.0) as usize % 4;
        let len = 2.0 + lcg(&mut seed) * 4.0;
        for f in ((t / FRAME) as usize)..(((t + len) / FRAME) as usize).min(n) {
            rows[p][f] = if f % 7 == 0 { -45.0 } else { -24.0 + (lcg(&mut seed) * 6.0) as f32 };
        }
        t += len + lcg(&mut seed) * 0.6;
    }
    // laughs every 3–7 s: two or three people loud for about a second
    let mut t = 3.0;
    while t < secs {
        let k = 2 + (lcg(&mut seed) * 2.0) as usize;
        for j in 0..k {
            let p = (j + (lcg(&mut seed) * 4.0) as usize) % 4;
            let off = (lcg(&mut seed) * 0.3 / FRAME) as usize;
            for f in ((t / FRAME) as usize + off)..(((t + 1.0) / FRAME) as usize).min(n) {
                rows[p][f] = -8.0;
            }
        }
        t += 3.0 + lcg(&mut seed) * 4.0;
    }
    let (shots, reactions) = plan(&rows, &Tuning::default());
    let mut people: Vec<usize> = shots.iter().map(|s| s.person).collect();
    people.sort_unstable();
    people.dedup();
    assert_eq!(people.len(), 4, "everyone gets shots: {shots:?}");
    let longest = shots.iter().map(|s| s.end - s.start).max().unwrap_or(0) as f64 * FRAME;
    assert!(longest < 25.0, "no endless shot: {longest} s");
    let reaction_time: usize = shots.iter().filter(|s| s.reaction).map(|s| s.end - s.start).sum();
    assert!((reaction_time as f64) < 0.5 * n as f64, "big moments are moments, not the whole scene");
    assert!(reactions.len() <= (secs / 12.0) as usize + 1);
    assert!(shots.len() > 20, "it cuts: {} shots", shots.len());
}
