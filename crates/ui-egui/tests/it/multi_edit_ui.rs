//! Fork: with several clips selected, a change in Effect Controls (a scrubbed value, a reset)
//! goes to all of them, as one undo step.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::{ClipId, ParamValue};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
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
    fn scale(&self, clip: u64) -> f64 {
        let q = self.harness.state().session.active_sequence().unwrap();
        let it = q.find_item(ClipId(clip)).unwrap().1;
        match it.effects.iter().find(|e| e.effect == "motion").unwrap().params["scale"].value {
            ParamValue::Float(x) => x,
            _ => f64::NAN,
        }
    }
}

#[test]
fn effect_controls_changes_every_selected_clip() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newSequence", json!({"name": "S", "video": 1, "audio": 1})).unwrap();
    let item = s.project.items.values().find(|i| i.has_video() && i.as_media().is_some()).map(|i| i.id).unwrap();
    let sec = 254_016_000_000_i64;
    for k in 0..3 {
        s.execute("timeline.place", json!({"item": item.0, "time": k * 20 * sec})).unwrap();
    }
    let clips: Vec<u64> = s.active_sequence().unwrap().video_tracks[0].items.iter().map(|i| i.id.0).collect();
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    let mut d = Driver { harness, tx };
    d.frames(4);
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": clips.clone()}}));
    d.ok("ui.menu.invoke", json!({"id": "window.panel.EffectControls"}));
    d.frames(3);
    // every clip scaled to 50 %, one by one
    for c in &clips {
        d.ok("engine.execute", json!({"command": "effects.setParam", "params": {"clip": c, "effect": "motion", "param": "scale", "value": 50.0}}));
    }
    d.frames(2);
    let undo = d.harness.state().session.history.undo.len();
    // Reset Parameter in Effect Controls (the panel shows one clip) resets all three
    d.ok("ui.click", json!({"id": "effectControls.motion.scale.reset"}));
    d.frames(3);
    for c in &clips {
        assert_eq!(d.scale(*c), 100.0, "clip {c}");
    }
    assert_eq!(d.harness.state().session.history.undo.len(), undo + 1, "one undo step");
    d.ok("engine.execute", json!({"command": "edit.undo", "params": {}}));
    for c in &clips {
        assert_eq!(d.scale(*c), 50.0, "undo brings clip {c} back");
    }
    // the panel's own change path: a value set for the shown clip goes to the selection
    let shown = clips[0];
    filmcraft_ui_egui::panels::multi_edit::execute(
        &mut d.harness.state_mut().session,
        "effects.setParam",
        json!({"clip": shown, "effect": "motion", "param": "scale", "value": 75.0}),
    )
    .unwrap();
    for c in &clips {
        assert_eq!(d.scale(*c), 75.0, "clip {c}");
    }
    // one clip selected: only that one changes
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": [clips[1]]}}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effectControls.motion.scale.reset"}));
    d.frames(3);
    assert_eq!(d.scale(clips[1]), 100.0);
    assert_eq!(d.scale(clips[0]), 75.0);
    assert_eq!(d.scale(clips[2]), 75.0);
}
