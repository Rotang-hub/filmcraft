use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::Session;
use filmcraft_project::{Transcript, Word};
use filmcraft_speech::FixedTranscriber;

fn sec(x: f64) -> Tick {
    Tick::from_seconds_f64(x)
}

/// The demo project, a head-caption template `<dir>/Ann_머리자막.fcgt` (one text layer), and a
/// fake transcriber that hears `words` (seconds from the clip's first sample).
fn session(words: &[(&str, f64, f64)]) -> (Session, tempfile::TempDir) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let r = s.execute("graphics.newText", json!({"text": "Ann", "time": 0, "seconds": 2})).unwrap();
    let clip = r["clip"].as_u64().unwrap();
    let path = dir.path().join(format!("Ann{TEMPLATE_SUFFIX}"));
    s.execute("graphics.template.export", json!({"clip": clip, "name": "Ann head", "path": path.to_string_lossy()})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    let mut t = Transcript { language: "ko".into(), ..Default::default() };
    for (text, a, b) in words {
        t.words.push(Word::new(*text, sec(*a), sec(*b)));
    }
    t.normalize();
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: t, id: "fixed".into() }));
    // one clip on A1 speaks (the fake transcriber hears the same words in every clip)
    s.edit_sequence("test", |q, _, _| {
        for it in q.audio_tracks[0].items.iter_mut().skip(1) {
            it.enabled = false;
        }
        Ok(())
    })
    .unwrap();
    (s, dir)
}

fn caption_texts(s: &Session) -> Vec<(String, String, Tick, Tick)> {
    let q = s.active_sequence().unwrap();
    let mut v = Vec::new();
    for t in q.video_tracks.iter().filter(|t| t.name.starts_with(TRACK_NAME)) {
        for it in &t.items {
            v.push((t.name.clone(), text_of(it).unwrap_or_default(), it.start, it.end()));
        }
    }
    v.sort_by_key(|x| x.2);
    v
}

#[test]
fn a_speaker_track_becomes_head_captions_with_the_spoken_text() {
    let (mut s, dir) = session(&[("안녕", 0.2, 0.5), ("하세요.", 0.5, 0.9), ("반가워요", 2.0, 2.6)]);
    let a1_start = s.active_sequence().unwrap().audio_tracks[0].items[0].start;
    let r = s.execute("captions.bySpeaker", json!({"speakers": [{"track": "A1", "name": "Ann"}], "templateDir": dir.path().to_string_lossy()})).unwrap();
    assert_eq!(r["captions"], 2, "{r}");
    let caps = caption_texts(&s);
    assert_eq!(caps.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["안녕 하세요.", "반가워요"]);
    assert!(caps.iter().all(|c| c.0 == format!("{TRACK_NAME} 1")));
    let rate = s.active_sequence().unwrap().settings.frame_rate;
    assert_eq!(caps[0].2, rate.snap(a1_start + sec(0.2)), "starts with its first word");
    assert!(caps[0].3 <= caps[1].2, "never runs into the next caption");
    // the graphics are template graphics: Properties shows the template's text property
    let clip = r["clips"][0]["clip"].as_u64().unwrap();
    let c = s.execute("graphics.template.controls", json!({"clip": clip})).unwrap();
    assert_eq!(c["controls"][0]["value"], "안녕 하세요.");
    // one undo step takes them all away
    s.execute("edit.undo", json!({})).unwrap();
    assert!(caption_texts(&s).is_empty());
}

#[test]
fn people_talking_at_once_stack_above_whoever_started_first() {
    let (mut s, dir) = session(&[]);
    let t = dir.path().join(format!("Ann{TEMPLATE_SUFFIX}")).to_string_lossy().to_string();
    let ph = |sp: &str, a: f64, b: f64, text: &str| json!({"speaker": sp, "template": t, "start": sec(a).0, "end": sec(b).0, "text": text});
    let r = s
        .execute(
            "captions.placeSpeakerPhrases",
            json!({"phrases": [ph("B", 1.0, 2.0, "끼어들기"), ph("A", 0.0, 3.0, "먼저 말함"), ph("A", 4.0, 5.0, "다시 혼자")], "spacing": 100.0}),
        )
        .unwrap();
    assert_eq!(r["captions"], 3);
    assert_eq!(r["overlapping"], 1);
    let lanes: Vec<(String, u64)> =
        r["clips"].as_array().unwrap().iter().map(|c| (c["speaker"].as_str().unwrap().to_string(), c["lane"].as_u64().unwrap())).collect();
    assert_eq!(lanes, [("A".into(), 0), ("B".into(), 1), ("A".into(), 0)], "the first to speak keeps the main place");
    let q = s.active_sequence().unwrap();
    let pos = |id: u64| {
        let (_, it) = q.find_item(ClipId(id)).unwrap();
        match &it.effects.iter().find(|e| e.effect == "motion").unwrap().params["position"].value {
            ParamValue::Vec2(v) => v.y,
            _ => f64::NAN,
        }
    };
    let main = pos(r["clips"][0]["clip"].as_u64().unwrap());
    let over = pos(r["clips"][1]["clip"].as_u64().unwrap());
    assert!((main - over - 100.0).abs() < 1e-6, "raised one step: {main} {over}");
    assert!(q.video_tracks.iter().any(|t| t.name == format!("{TRACK_NAME} 2")));
}

#[test]
fn words_group_into_short_phrases() {
    let w = |t: &str, a: f64, b: f64| (sec(a), sec(b), t.to_string());
    let g =
        group(&[w("가나다라", 0.0, 0.3), w("마바사아", 0.3, 0.6), w("자차카타", 0.6, 0.9), w("끝.", 0.9, 1.0), w("다음", 1.0, 1.2), w("멀리", 3.0, 3.2)], 10);
    let texts: Vec<&str> = g.iter().map(|p| p.2.as_str()).collect();
    assert_eq!(texts, ["가나다라 마바사아", "자차카타 끝.", "다음", "멀리"], "split by length, after a full stop, at a pause");
    assert!(g.windows(2).all(|p| p[0].1 <= p[1].0));
    assert!(g.iter().all(|p| p.1 > p.0));
    assert!(group(&[], 10).is_empty());
}

#[test]
fn caption_text_round_trips_through_a_file_for_spell_check() {
    let (mut s, dir) = session(&[("안넝", 0.2, 0.5), ("하세요", 0.5, 0.9)]);
    s.execute("captions.bySpeaker", json!({"speakers": [{"track": "A1", "name": "Ann"}], "templateDir": dir.path().to_string_lossy()})).unwrap();
    let file = dir.path().join("captions.txt");
    let r = s.execute("captions.exportText", json!({"path": file.to_string_lossy()})).unwrap();
    assert_eq!(r["captions"], 1);
    let text = std::fs::read_to_string(&file).unwrap();
    let line = text.lines().find(|l| !l.starts_with('#')).unwrap();
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols[2], "Ann");
    assert_eq!(cols[3], "안넝 하세요");
    std::fs::write(&file, text.replace("안넝 하세요", "안녕하세요")).unwrap();
    let r = s.execute("captions.importText", json!({"path": file.to_string_lossy()})).unwrap();
    assert_eq!(r, json!({"changed": 1, "notFound": 0}));
    assert_eq!(caption_texts(&s)[0].1, "안녕하세요");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(caption_texts(&s)[0].1, "안넝 하세요", "one undo step");
}

#[test]
fn bad_requests_are_errors_not_crashes() {
    let (mut s, dir) = session(&[("말", 0.2, 0.5)]);
    let d = dir.path().to_string_lossy().to_string();
    for p in [
        json!({}),
        json!({"speakers": []}),
        json!({"speakers": [{"track": "A1"}]}),
        json!({"speakers": [{"track": "A99", "name": "Ann"}], "templateDir": d}),
        json!({"speakers": [{"track": "V1", "name": "Ann"}], "templateDir": d}),
        json!({"speakers": [{"track": "A1", "name": "Nobody"}], "templateDir": d}),
    ] {
        assert!(s.execute("captions.bySpeaker", p.clone()).is_err(), "{p}");
    }
    let bad_phrases: Vec<Value> = vec![
        json!({"start": -5, "end": 10, "text": "x"}),
        json!({"start": 10, "end": 5, "text": "x"}),
        json!({"start": 0, "end": 5, "text": " "}),
        json!("nope"),
    ];
    assert!(s.execute("captions.placeSpeakerPhrases", json!({"phrases": bad_phrases})).is_err());
    assert!(s.execute("captions.placeSpeakerPhrases", json!({"phrases": [{"start": 0, "end": 99, "text": "x", "template": "missing.fcgt"}]})).is_err());
    let file = dir.path().join("junk.txt");
    std::fs::write(&file, "garbage\n\t\t\n#\n1\t2\n").unwrap();
    assert!(s.execute("captions.importText", json!({"path": file.to_string_lossy()})).is_err());
    assert!(s.execute("captions.exportText", json!({"path": file.to_string_lossy()})).is_err(), "no template captions yet");
}
