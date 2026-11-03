//! Fork feature: fade handles. Hover a clip, drag the handle at its top corner inward: the clip
//! fades over that length (one undo step); dragging it back to the corner removes the fade.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also write `fade-handles.png` there.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_engine::fade_handles::{FadeEdge, fade_of};
use filmcraft_project::ClipId;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    /// A sequence with one clip (picture and sound) at 0, zoomed so the clip is wide.
    fn new() -> (Self, ClipId, ClipId) {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        session.execute("file.newSequence", json!({"name": "Fades", "video": 2, "audio": 2})).expect("sequence");
        let item = session.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        session.execute("timeline.place", json!({"item": item.0, "time": 0})).unwrap();
        let (v, a) = {
            let q = session.active_sequence().unwrap();
            (q.video_tracks[0].items[0].id, q.audio_tracks[0].items[0].id)
        };
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        (d, v, a)
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn rect(&mut self, id: &str) -> Option<[f32; 4]> {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array()?.iter().find(|e| e["id"] == json!(id)).map(|e| {
            let r: Vec<f32> = e["rect"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect();
            [r[0], r[1], r[2], r[3]]
        })
    }

    fn fade(&self, clip: ClipId, edge: FadeEdge) -> Option<filmcraft_time::Tick> {
        let q = self.harness.state().session.active_sequence().unwrap();
        let (tid, _) = q.find_item(clip).unwrap();
        fade_of(q.track(tid).unwrap(), clip, edge).map(|x| x.duration)
    }

    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        self.frames(2);
        match self.harness.render() {
            Ok(img) => {
                std::fs::create_dir_all(&dir).unwrap();
                img.save(dir.join(format!("{name}.png"))).unwrap();
            }
            Err(e) => eprintln!("snapshot {name} skipped: {e}"),
        }
    }
}

#[test]
fn dragging_a_corner_handle_fades_the_clip() {
    let (mut d, v, a) = Driver::new();
    d.ok("ui.set", json!({"timeline": {"videoTrackHeight": 64.0, "audioTrackHeight": 64.0}}));
    d.ok("ui.menu.invoke", json!({"id": "view.zoomToSequence"}));
    d.frames(4);
    // hover the video clip: its handles show
    d.ok("ui.move", json!({"id": format!("timeline.clip.{}", v.0)}));
    d.frames(2);
    let h = d.rect(&format!("timeline.fadeHandle.{}.in", v.0)).expect("the fade in handle shows on hover");
    let (hx, hy) = (h[0] + h[2] / 2.0, h[1] + h[3] / 2.0);
    let undo = d.harness.state().session.history.undo.len();
    d.ok("ui.drag", json!({"from": {"x": hx, "y": hy}, "to": {"x": hx + 120.0, "y": hy}, "steps": 8}));
    d.frames(3);
    let fin = d.fade(v, FadeEdge::In).expect("the drag made a fade in");
    assert!(fin > filmcraft_time::Tick::ZERO);
    assert_eq!(d.harness.state().session.history.undo.len(), undo + 1, "one undo step");
    assert_eq!(d.fade(a, FadeEdge::In), None, "the linked sound is left alone");
    // the sound fades out from its own handle
    d.ok("ui.move", json!({"id": format!("timeline.clip.{}", a.0)}));
    d.frames(2);
    let h = d.rect(&format!("timeline.fadeHandle.{}.out", a.0)).expect("the fade out handle shows on hover");
    let (ox, oy) = (h[0] + h[2] / 2.0, h[1] + h[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": ox, "y": oy}, "to": {"x": ox - 200.0, "y": oy}, "steps": 8}));
    d.frames(3);
    assert!(d.fade(a, FadeEdge::Out).is_some(), "the drag made a fade out");
    d.ok("ui.move", json!({"x": 10.0, "y": 10.0}));
    d.snapshot("fade-handles");
    // back to the corner: no fade
    d.ok("ui.move", json!({"id": format!("timeline.clip.{}", v.0)}));
    d.frames(2);
    let h = d.rect(&format!("timeline.fadeHandle.{}.in", v.0)).unwrap();
    let (hx, hy) = (h[0] + h[2] / 2.0, h[1] + h[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": hx, "y": hy}, "to": {"x": hx - 400.0, "y": hy}, "steps": 8}));
    d.frames(3);
    assert_eq!(d.fade(v, FadeEdge::In), None);
}
