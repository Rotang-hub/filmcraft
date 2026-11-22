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

fn people_only(rows: &[Vec<f32>]) -> Heard<'_> {
    Heard { people: rows, voices: &[], game: &[] }
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
    let tuning = Tuning { min_silence: 1.0, pad: 0.25, ..quiet_tuning() };
    let cuts = silences(&people_only(&[a.clone(), b.clone()]), &tuning, &[], &[]);
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
    assert!(silences(&people_only(&[a, b]), &Tuning { trim: false, ..tuning }, &[], &[]).is_empty());
}

#[test]
fn a_big_moment_and_its_build_up_are_never_cut() {
    let n = (20.0 / FRAME) as usize;
    let a = row(n, &[(0.0, 2.0, -20.0), (15.0, 20.0, -20.0)]);
    let b = row(n, &[]);
    let r = Reaction { start: (10.0 / FRAME) as usize, end: (11.0 / FRAME) as usize, person: 1 };
    let tuning = Tuning { buildup: 2.5, hold: 1.0, ..quiet_tuning() };
    let cuts = silences(&people_only(&[a, b]), &tuning, &[r], &[]);
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

#[test]
fn talk_on_a_discord_track_loud_game_moments_and_markers_are_not_cut() {
    let n = (40.0 / FRAME) as usize;
    // the microphones: A at 0–3 s and 36–40 s only
    let a = row(n, &[(0.0, 3.0, -20.0), (36.0, 40.0, -20.0)]);
    let b = row(n, &[]);
    let tuning = Tuning { min_silence: 1.0, pad: 0.25, buildup: 3.0, hold: 1.0, ..quiet_tuning() };
    let rows = [a.clone(), b.clone()];
    let all = silences(&people_only(&rows), &tuning, &[], &[]);
    assert_eq!(all.len(), 1, "{all:?}");
    // a friend talks on the Discord track at 5–9 s; the game blasts at 20–21 s (music around −40)
    let discord = row(n, &[(5.0, 9.0, -25.0)]);
    let mut game = row(n, &[(0.0, 40.0, -40.0)]);
    for x in game.iter_mut().skip((20.0 / FRAME) as usize).take((1.0 / FRAME) as usize) {
        *x = -15.0;
    }
    let voices = [discord];
    let games = [game];
    let heard = Heard { people: &rows, voices: &voices, game: &games };
    // and the user marked 30 s
    let keep = [((29.0 / FRAME) as usize, (31.5 / FRAME) as usize)];
    let cuts = silences(&heard, &tuning, &[], &keep);
    let secs: Vec<(f64, f64)> = cuts.iter().map(|(x, y)| (*x as f64 * FRAME, *y as f64 * FRAME)).collect();
    let kept = |a: f64, b: f64| secs.iter().all(|(x, y)| *y <= a || *x >= b);
    assert!(kept(5.0, 9.0), "the friend's talk stays: {secs:?}");
    assert!(kept(17.0, 22.0), "the game moment and its build-up stay: {secs:?}");
    assert!(kept(29.0, 31.5), "the marked part stays: {secs:?}");
    assert!(!secs.is_empty(), "the rest of the silence still goes");
}

#[test]
fn markers_become_kept_ranges_but_reaction_markers_do_not() {
    let mut s = demo();
    s.edit_sequence("test", |q, ctx, _| {
        for (t, name) in [(2.0, "keep"), (3.0, "Reaction: B")] {
            q.markers.push(Marker {
                url: String::new(),
                author: String::new(),
                custom_color: None,
                id: MarkerId(ctx.alloc()),
                start: Tick::from_seconds_f64(t),
                duration: Tick::ZERO,
                name: name.into(),
                comment: String::new(),
                kind: MarkerKind::Comment,
                color: Label::Rose,
            });
        }
        Ok(())
    })
    .unwrap();
    let end = s.active_sequence().unwrap().duration();
    let keep = keep_ranges(&s, (Tick::ZERO, end), &Tuning { buildup: 1.0, hold: 0.5, ..Tuning::default() });
    assert!(keep.contains(&(20, 51)), "{keep:?}");
    assert!(!keep.iter().any(|(a, _)| *a == 40), "a Reaction marker keeps nothing: {keep:?}");
}

/// Adds audio tracks until there are `n`.
fn audio_tracks(s: &mut Session, n: usize) {
    s.edit_sequence("test", |q, ctx, _| {
        while q.audio_tracks.len() < n {
            let k = q.audio_tracks.len() + 1;
            q.audio_tracks.push(Track::new(TrackId(ctx.alloc()), TrackKind::Audio, format!("Audio {k}")));
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_discord_track_linked_to_the_view_stays_whole_and_only_the_game_sound_follows() {
    // 로탱's recording: V1 linked to A1 (Discord), A2 (microphone) and A3 (game sound); B: V2 + A4
    let mut s = demo();
    audio_tracks(&mut s, 4);
    s.edit_sequence("test", |q, ctx, _| {
        let base = q.audio_tracks[0].items.clone();
        let mut tracks: [Vec<_>; 4] = Default::default();
        for v in &mut q.video_tracks[0].items {
            let g = ctx.alloc();
            v.link = Some(g);
            let Some(src) = base.iter().find(|a| a.start == v.start) else { continue };
            for (k, t) in tracks.iter_mut().enumerate().take(3) {
                let mut a = src.clone();
                a.id = ClipId(ctx.alloc());
                a.link = Some(g);
                a.duration = v.duration;
                a.name = format!("rec {k}");
                t.push(a);
            }
        }
        for v in &mut q.video_tracks[1].items {
            let g = ctx.alloc();
            v.link = Some(g);
            if let Some(src) = base.iter().find(|a| a.start == v.start) {
                let mut a = src.clone();
                a.id = ClipId(ctx.alloc());
                a.link = Some(g);
                a.duration = v.duration;
                tracks[3].push(a);
            }
        }
        for (t, items) in q.audio_tracks.iter_mut().zip(tracks) {
            t.items = items;
        }
        Ok(())
    })
    .unwrap();
    let before = s.active_sequence().unwrap().clone();
    let people = json!([{"name": "로탱", "video": "V1", "audio": "A2"}, {"name": "B", "video": "V2", "audio": "A4"}]);
    let extra = extra_p(&s, &json!({"voices": ["A1"]}), &people_p(&s, &json!({"people": people})).unwrap()).unwrap();
    assert_eq!(extra.voices, [before.audio_tracks[0].id]);
    assert_eq!(extra.game, [before.audio_tracks[2].id], "the game sound is found from the links");
    let shots = json!([{"start": 0, "end": 40, "person": 0}, {"start": 40, "end": 100, "person": 1}]);
    let r = s.execute("edit.autoCutPlace", json!({"people": people, "voices": ["A1"], "shots": shots, "markers": false})).unwrap();
    assert_eq!(r["soundTracks"], 1, "{r}");
    assert_eq!(r["muted"], json!([before.audio_tracks[2].name.clone()]), "only the game sound is muted");
    let q = s.active_sequence().unwrap();
    // Discord and the microphones play whole, as before
    for k in [0, 1, 3] {
        assert_eq!(q.audio_tracks[k].items, before.audio_tracks[k].items, "A{} untouched", k + 1);
        assert!(!q.audio_tracks[k].muted);
    }
    let snd = q.audio_tracks.iter().find(|t| t.name == TRACK_NAME).unwrap();
    assert!(snd.items.iter().all(|a| a.name == "rec 2"), "only the game sound follows the cut");
    let two = q.settings.frame_rate.snap_nearest(Tick::from_seconds_f64(2.0));
    assert_eq!(snd.items.iter().map(|a| a.end()).max(), Some(two));
    // the same with the game sound named
    let people2 = json!([{"name": "로탱", "video": "V1", "audio": "A2", "sound": "A3"}, {"name": "B", "video": "V2", "audio": "A4"}]);
    let r = s.execute("edit.autoCutPlace", json!({"people": people2, "voices": "A1", "shots": shots, "markers": false})).unwrap();
    assert_eq!(r["soundTracks"], 1, "{r}");
    let q = s.active_sequence().unwrap();
    let snd = q.audio_tracks.iter().find(|t| t.name == TRACK_NAME).unwrap();
    assert!(!snd.items.is_empty() && snd.items.iter().all(|a| a.name == "rec 2"));
    q.check().unwrap();
    // a microphone cannot also be the game sound
    let bad = json!([{"name": "로탱", "video": "V1", "audio": "A2", "sound": "A2"}, {"name": "B", "video": "V2", "audio": "A4"}]);
    assert!(s.execute("edit.autoCutPlace", json!({"people": bad, "shots": shots})).is_err());
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
fn each_views_own_sound_is_cut_with_it_onto_an_auto_cut_audio_track() {
    let mut s = demo();
    // B's view (V2) has its own linked game sound on A3; microphones are A1 and A2
    s.edit_sequence("test", |q, ctx, _| {
        let mut sound = Vec::new();
        for v in &mut q.video_tracks[1].items {
            let g = ctx.alloc();
            v.link = Some(g);
            let src = q.audio_tracks[0].items.iter().find(|a| a.start == v.start).cloned();
            if let Some(mut a) = src {
                a.id = ClipId(ctx.alloc());
                a.link = Some(g);
                a.duration = v.duration;
                a.item = v.item;
                a.source_in = v.source_in;
                sound.push(a);
            }
        }
        q.audio_tracks[2].items = sound;
        Ok(())
    })
    .unwrap();
    let a3 = s.active_sequence().unwrap().audio_tracks[2].clone();
    assert!(!a3.items.is_empty());
    let people = json!([{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]);
    let shots = json!([{"start": 0, "end": 30, "person": 0}, {"start": 30, "end": 70, "person": 1}, {"start": 70, "end": 100, "person": 0}]);
    let r = s.execute("edit.autoCutPlace", json!({"people": people, "shots": shots, "markers": false})).unwrap();
    assert_eq!(r["soundTracks"], 1, "{r}");
    assert_eq!(r["muted"], json!([a3.name.clone()]));
    let q = s.active_sequence().unwrap();
    let cut = q.video_tracks.iter().find(|t| t.name == TRACK_NAME).unwrap();
    let snd = q.audio_tracks.iter().find(|t| t.name == TRACK_NAME).expect("the Auto Cut audio track");
    let rate = q.settings.frame_rate;
    let (b0, b1) = (rate.snap_nearest(Tick::from_seconds_f64(1.5)), rate.snap_nearest(Tick::from_seconds_f64(3.5)));
    // sound only where B's view is shown, matching B's picture piece for piece, linked to it
    assert!(!snd.items.is_empty());
    for a in &snd.items {
        assert!(a.start >= b0 && a.end() <= b1, "{:?}", (a.start, a.end()));
        let v = cut.items.iter().find(|v| v.start == a.start && v.duration == a.duration).expect("a matching picture piece");
        assert!(v.link.is_some() && v.link == a.link);
        let orig = a3.items.iter().find(|o| o.start <= a.start && a.start < o.end()).unwrap();
        assert_eq!(a.source_in, orig.source_in + (a.start - orig.start));
    }
    let covered: Tick = snd.items.iter().fold(Tick::ZERO, |acc, a| acc + a.duration);
    assert_eq!(covered, b1 - b0);
    // A's view has no sound of its own (A1 is a microphone): its pieces stay unlinked; mics untouched
    assert!(cut.items.iter().filter(|v| v.end() <= b0).all(|v| v.link.is_none()));
    assert!(!q.audio_tracks[0].muted && !q.audio_tracks[1].muted);
    assert!(q.audio_tracks.iter().find(|t| t.id == a3.id).unwrap().muted);
    q.check().unwrap();
    // run again: still one sound track; undo puts the mute back
    s.execute("edit.autoCutPlace", json!({"people": people, "shots": shots, "markers": false, "muteViews": false})).unwrap();
    assert_eq!(s.active_sequence().unwrap().audio_tracks.iter().filter(|t| t.name == TRACK_NAME).count(), 1);
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    assert!(q.audio_tracks.iter().all(|t| t.name != TRACK_NAME));
    assert!(!q.audio_tracks.iter().find(|t| t.id == a3.id).unwrap().muted);
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
