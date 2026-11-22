use serde_json::json;

use super::*;

fn item_named(s: &Session, name: &str) -> ItemId {
    s.project.items.values().find(|i| i.name == name).map(|i| i.id).unwrap()
}

fn secs(t: f64) -> Tick {
    Tick::from_seconds_f64(t)
}

/// A demo clip of `item` (copied from the demo sequence's first clip, so it is valid).
fn clip(s: &Session, item: ItemId, start: f64, src: f64, len: f64, id: u64) -> filmcraft_project::TrackItem {
    let demo = s.project.sequences().filter_map(|i| i.as_sequence()).flat_map(|q| q.video_tracks.iter().flat_map(|t| t.items.iter())).next().unwrap().clone();
    let mut c = demo;
    c.id = ClipId(id);
    c.item = item;
    c.start = secs(start);
    c.source_in = secs(src);
    c.duration = secs(len);
    c.speed = 1.0;
    c.reverse = false;
    c.link = None;
    c
}

/// X and Y recordings: "synced" plays them together (Y's source is 0.5 s behind X's); "본편"
/// shows one at a time.
fn project() -> (Session, ItemId, ItemId) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let (x, y) = (item_named(&s, "Desert_Dunes.mp4"), item_named(&s, "Misty_Forest.mp4"));
    s.execute("file.newSequence", json!({"name": "synced"})).unwrap();
    let a = clip(&s, x, 0.0, 1.0, 2.0, 0);
    let b = clip(&s, y, 0.0, 0.5, 2.0, 0);
    s.edit_sequence("synced", |q, ctx, _| {
        for (k, mut c) in [a, b].into_iter().enumerate() {
            c.id = ClipId(ctx.alloc());
            q.video_tracks[k].items.push(c);
        }
        Ok(())
    })
    .unwrap();
    s.execute("file.newSequence", json!({"name": "본편"})).unwrap();
    let a = clip(&s, x, 0.0, 1.2, 0.5, 0);
    let b = clip(&s, y, 0.5, 2.0, 0.5, 0);
    s.edit_sequence("edit", |q, ctx, _| {
        for mut c in [a, b] {
            c.id = ClipId(ctx.alloc());
            q.video_tracks[0].items.push(c);
        }
        Ok(())
    })
    .unwrap();
    (s, x, y)
}

#[test]
fn every_recording_goes_on_whole_and_in_sync() {
    let (mut s, x, y) = project();
    let undo = s.history.undo.len();
    // found by itself: a sequence that plays both together
    let found = s.execute("sequence.makeUncut", json!({"from": "본편", "name": "found"})).unwrap();
    assert_eq!(found["recordings"], 2, "{found}");
    s.execute("edit.undo", json!({})).unwrap();
    let r = s.execute("sequence.makeUncut", json!({"from": "본편", "synced": "synced"})).unwrap();
    assert_eq!(r["name"], "본편 원본");
    assert_eq!(r["recordings"], 2, "{r}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    let id = ItemId(r["sequence"].as_u64().unwrap());
    assert_eq!(s.state.active_sequence, Some(id), "it is opened");
    let q = s.project.sequence(id).unwrap();
    let cx = &q.video_tracks[0].items[0];
    let cy = &q.video_tracks[1].items[0];
    assert_eq!((cx.item, cy.item), (x, y));
    // both from their first frame: X starts at 0, Y half a second later (its source is behind)
    let rate = q.settings.frame_rate;
    assert_eq!(cx.source_in, Tick::ZERO);
    assert!(cy.source_in < rate.tick_of(1), "within a frame of its start: {:?}", cy.source_in);
    assert_eq!(cx.start, Tick::ZERO);
    assert_eq!(cy.start, rate.snap_nearest(secs(0.5)));
    // whole
    let len = |m: ItemId| s.project.item(m).unwrap().as_media().unwrap().info.duration;
    assert!(len(x) - cx.duration < rate.tick_of(1));
    // in sync: the same moment shows X at t and Y at t - 0.5 s
    let t = secs(1.5);
    let sx = cx.source_in + (t - cx.start);
    let sy = cy.source_in + (t - cy.start);
    assert!(((sx - sy).seconds() - 0.5).abs() < 0.02);
    assert!(cx.link.is_some() && cx.link != cy.link);
    q.check().unwrap();
    // the edit can now be learned from it
    let l = crate::auto_cut_style::labels(
        q,
        s.project.sequence(sequence_by(&s.project, Some(&json!("본편"))).unwrap()).unwrap(),
        &crate::auto_cut::people_p(&s, &json!({"people": [{"name": "X", "video": "V1", "audio": "A1"}, {"name": "Y", "video": "V2", "audio": "A2"}]})).unwrap(),
        (Tick::ZERO, q.duration()),
    );
    let kept = l.kept.iter().filter(|k| **k).count() as f64 * crate::auto_cut::FRAME;
    assert!((kept - 1.0).abs() < 0.1, "{kept}");
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.item(id).is_none());
}

#[test]
fn missing_or_wrong_sequences_are_errors() {
    let (mut s, _, _) = project();
    assert!(s.execute("sequence.makeUncut", json!({})).is_err());
    assert!(s.execute("sequence.makeUncut", json!({"from": "없음"})).is_err());
    assert!(s.execute("sequence.makeUncut", json!({"from": "본편", "synced": "본편"})).is_err());
    assert!(s.execute("sequence.makeUncut", json!({"from": "본편", "synced": "없음"})).is_err());
    // nothing plays the edit's recordings together: no way to sync them
    let mut t = Session::default();
    t.execute("file.openDemoProject", json!({})).unwrap();
    t.execute("file.newSequence", json!({"name": "solo"})).unwrap();
    let x = item_named(&t, "Desert_Dunes.mp4");
    let c = clip(&t, x, 0.0, 0.0, 1.0, 0);
    t.edit_sequence("e", |q, ctx, _| {
        let mut c = c;
        c.id = ClipId(ctx.alloc());
        q.video_tracks[0].items.push(c);
        Ok(())
    })
    .unwrap();
    let r = t.execute("sequence.makeUncut", json!({"from": "solo"}));
    assert!(r.is_err() || r.unwrap()["recordings"].as_u64().unwrap() >= 1);
}
