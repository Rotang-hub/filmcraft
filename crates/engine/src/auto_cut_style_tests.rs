use serde_json::json;

use super::*;
use crate::Session;
use crate::auto_cut::{people_p, silences};
use filmcraft_project::{ClipId, ItemId};

/// Sentences with breaths, over `[a, b)`.
fn chatter(v: &mut [f32], a: f64, b: f64, len: f64, db: f32) {
    let n = v.len();
    let mut t = a;
    while t < b {
        let e = (t + len).min(b);
        for x in v.iter_mut().take(((e / FRAME) as usize).min(n)).skip((t / FRAME) as usize) {
            *x = db;
        }
        t = e + 0.3;
    }
}

/// 120 s where two people talk; the "editor" kept only the parts where both talk.
fn scene() -> (Vec<Vec<f32>>, Vec<bool>) {
    let n = (120.0 / FRAME) as usize;
    let mut a = vec![-60.0f32; n];
    let mut b = vec![-60.0f32; n];
    let mut kept = vec![false; n];
    for k in 0..12 {
        let t = k as f64 * 10.0;
        chatter(&mut a, t, t + 10.0, 2.0, -18.0);
        if k % 3 == 0 {
            // a lively back-and-forth: kept
            chatter(&mut b, t + 0.5, t + 10.0, 2.2, -16.0);
            for x in kept.iter_mut().skip((t / FRAME) as usize).take((10.0 / FRAME) as usize) {
                *x = true;
            }
        }
    }
    (vec![a, b], kept)
}

#[test]
fn it_learns_which_parts_the_editor_keeps_and_cuts_like_them() {
    let (rows, kept) = scene();
    let heard = Heard { people: &rows, voices: &[], game: &[], motion: &[] };
    let labels = Labels { kept: kept.clone(), shown: vec![None; kept.len()], shots: vec![3.0, 5.0, 4.0] };
    let style = learn(&heard, &[], &[], &labels, None).unwrap();
    assert_eq!(style.examples, 1);
    assert!((style.keep() - 4.0 / 12.0).abs() < 0.01, "keeps about a third: {}", style.keep());
    assert!((style.shot - 4.0).abs() < 1e-9);
    assert!(style.agreement > 0.85, "{}", style.agreement);
    // more talkers means kept
    assert!(style.weights[1] > 0.0, "{:?}", style.weights);
    // cutting in that style removes the one-person stretches
    let tuning = Tuning { reactions: false, ..Tuning::default() };
    let sil = silences(&heard, &tuning, &[], &[]);
    let cuts = styled_cuts(&heard, &[], &tuning, &style, &[], &[], &sil);
    let cut = mask(kept.len(), &cuts);
    let wrong = cut.iter().zip(&kept).filter(|(c, k)| **c && **k).count();
    assert!(wrong < (3.0 / FRAME) as usize, "it hardly cuts what the editor kept: {wrong}");
    let gone = cut.iter().filter(|c| **c).count() as f64 / kept.len() as f64;
    assert!(gone > 0.4, "it cuts most of the rest: {gone}");
    // a marked part stays
    let mark = [((15.0 / FRAME) as usize, (18.0 / FRAME) as usize)];
    let cuts = styled_cuts(&heard, &[], &tuning, &style, &[], &mark, &sil);
    assert!(cuts.iter().all(|(a, b)| *b <= mark[0].0 || *a >= mark[0].1));
}

#[test]
fn learning_adds_up_over_edits_and_the_style_survives_json() {
    let (rows, kept) = scene();
    let heard = Heard { people: &rows, voices: &[], game: &[], motion: &[] };
    let labels = Labels { kept, shown: Vec::new(), shots: vec![2.0] };
    let one = learn(&heard, &[], &[], &labels, None).unwrap();
    let two = learn(&heard, &[], &[], &labels, Some(one.clone())).unwrap();
    assert_eq!(two.examples, 2);
    assert_eq!(two.samples.len(), one.samples.len() * 2);
    let back = Style::from_value(&serde_json::to_value(&two).unwrap()).unwrap();
    assert_eq!(back.weights, two.weights);
    // a broken or hostile style is an error, not a crash
    for bad in [json!(1), json!({}), json!({"weights": [1.0]}), {
        let mut v = serde_json::to_value(&two).unwrap();
        v["weights"] = json!([1.0, 2.0]);
        v
    }] {
        assert!(Style::from_value(&bad).is_err(), "{bad}");
    }
    // an edit that uses nothing of the sequence cannot teach anything
    let none = Labels { kept: vec![false; rows[0].len()], shown: Vec::new(), shots: Vec::new() };
    assert!(learn(&heard, &[], &[], &none, None).is_err());
}

#[test]
fn rates_stay_finite_for_extreme_features() {
    let (rows, kept) = scene();
    let heard = Heard { people: &rows, voices: &[], game: &[], motion: &[] };
    let style = learn(&heard, &[], &[], &Labels { kept, shown: Vec::new(), shots: Vec::new() }, None).unwrap();
    for x in [[f64::MAX; FEATURES], [f64::MIN; FEATURES], [0.0; FEATURES]] {
        let r = style.rate(&x);
        assert!((0.0..=1.0).contains(&r), "{r}");
    }
}

/// The demo, a second view, and an edit sequence that keeps two stretches of V1's clips.
fn with_edit() -> (Session, ItemId, ItemId, Vec<(Tick, Tick)>) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let raw = s.state.active_sequence.unwrap();
    let v1 = s.active_sequence().unwrap().video_tracks[0].items.clone();
    s.execute("file.newSequence", json!({"name": "편집본"})).unwrap();
    let edit = s.state.active_sequence.unwrap();
    assert_ne!(raw, edit);
    // keep 0.5–1.5 s and 3–4 s of the uncut sequence, back to back
    let want = [(0.5, 1.5), (3.0, 4.0)];
    let mut kept = Vec::new();
    s.edit_sequence("edit", |q, ctx, _| {
        let mut at = Tick::ZERO;
        for (a, b) in want {
            let (a, b) = (Tick::from_seconds_f64(a), Tick::from_seconds_f64(b));
            let Some(src) = v1.iter().find(|c| c.start <= a && c.end() >= b) else { continue };
            let mut c = src.clone();
            c.id = ClipId(ctx.alloc());
            c.link = None;
            c.source_in = src.source_in + Tick::from_seconds_f64((a - src.start).seconds() * src.speed);
            c.duration = b - a;
            c.start = at;
            at += c.duration;
            q.video_tracks[0].items.push(c);
            kept.push((a, b));
        }
        Ok(())
    })
    .unwrap();
    s.state.active_sequence = Some(raw);
    (s, raw, edit, kept)
}

#[test]
fn the_edit_tells_exactly_which_frames_were_kept_and_whose_view() {
    let (s, raw, edit, kept) = with_edit();
    assert_eq!(kept.len(), 2);
    let people = people_p(&s, &json!({"people": [{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]})).unwrap();
    let (r, e) = (s.project.sequence(raw).unwrap(), s.project.sequence(edit).unwrap());
    let range = (Tick::ZERO, r.duration());
    let l = labels(r, e, &people, range);
    let at = |t: f64| (t / FRAME) as usize;
    for t in [0.6, 1.4, 3.1, 3.9] {
        assert!(l.kept[at(t)], "{t} kept");
        assert_eq!(l.shown[at(t)], Some(0));
    }
    for t in [0.2, 2.0, 2.9, 4.5] {
        assert!(!l.kept[at(t)], "{t} cut");
    }
    assert_eq!(l.shots.len(), 1, "one view throughout: {:?}", l.shots);
    assert!((l.shots[0] - 2.0).abs() < 0.02);
}

#[test]
fn the_learn_command_learns_from_the_named_edit_and_the_style_can_cut() {
    let (mut s, _, _, _) = with_edit();
    let people = json!([{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]);
    let r = s.execute("edit.autoCutLearn", json!({"people": people, "result": "편집본", "motion": false})).unwrap();
    assert_eq!(r["summary"]["examples"], 1, "{r}");
    let keep = r["summary"]["keep"].as_f64().unwrap();
    let whole = s.active_sequence().unwrap().duration().seconds();
    assert!((keep * whole - 2.0).abs() < 0.1, "2 s of {whole} s were kept: {keep}");
    // the active sequence cannot be its own edit; an unknown one is an error
    let raw = s.state.active_sequence.unwrap();
    assert!(s.execute("edit.autoCutLearn", json!({"people": people, "result": raw.0})).is_err());
    assert!(s.execute("edit.autoCutLearn", json!({"people": people, "result": "없음"})).is_err());
    assert!(s.execute("edit.autoCutLearn", json!({"people": people})).is_err());
    // cutting with the style works
    let style = r["style"].clone();
    let c = s.execute("edit.autoCut", json!({"people": people, "style": style, "motion": false})).unwrap();
    assert!(c["shots"].as_u64().unwrap() > 0, "{c}");
    assert!(s.execute("edit.autoCut", json!({"people": people, "style": {"weights": "x"}})).is_err());
}
