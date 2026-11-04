//! Fork: effect presets reach every selected clip without a double-click: the preset's
//! right-click menu has Apply to Selected Clips, dropping a preset on one of the selected clips
//! applies it to all of them, and the status bar says how many clips it went to.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
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
    fn effects(&self, clip: u64) -> Vec<String> {
        let q = self.harness.state().session.active_sequence().unwrap();
        q.find_item(filmcraft_project::ClipId(clip)).unwrap().1.effects.iter().map(|e| e.effect.clone()).collect()
    }
    fn status(&mut self) -> String {
        self.ok("ui.inspect", json!({}))["ui"]["status"].as_str().unwrap_or_default().to_string()
    }
}

#[test]
fn a_preset_goes_to_every_selected_clip() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newSequence", json!({"name": "S", "video": 1, "audio": 1})).unwrap();
    let item = s.project.items.values().find(|i| i.has_video() && i.as_media().is_some()).map(|i| i.id).unwrap();
    let sec = 254_016_000_000_i64;
    let mut clips = Vec::new();
    for k in 0..3 {
        s.execute("timeline.place", json!({"item": item.0, "time": k * 20 * sec})).unwrap();
    }
    for it in &s.active_sequence().unwrap().video_tracks[0].items {
        clips.push(it.id.0);
    }
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    let mut d = Driver { harness, tx };
    d.frames(4);
    d.ok("ui.menu.invoke", json!({"id": "window.panel.Effects"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effects.folder.Presets"}));
    d.frames(2);

    // the right-click menu
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": [clips[0], clips[1]]}}));
    d.ok("ui.click", json!({"id": "effects.preset.Blur In", "button": "right"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effects.presetMenu.Blur In.apply"}));
    d.frames(2);
    assert!(d.effects(clips[0]).contains(&"gaussian_blur".to_string()));
    assert!(d.effects(clips[1]).contains(&"gaussian_blur".to_string()));
    assert!(!d.effects(clips[2]).contains(&"gaussian_blur".to_string()), "only the selected clips");
    assert!(d.status().contains('2'), "the status bar counts the clips: {}", d.status());

    // a drop on one of the selected clips goes to all of them
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": clips.clone()}}));
    d.ok("ui.drag", json!({"from": {"id": "effects.preset.Warm Tint"}, "to": {"id": format!("timeline.clip.{}", clips[1])}, "steps": 10}));
    d.frames(3);
    for c in &clips {
        assert!(d.effects(*c).contains(&"tint".to_string()), "clip {c}: {:?}", d.effects(*c));
    }
    // a drop on a clip outside the selection goes to that clip only
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": [clips[0]]}}));
    d.ok("ui.drag", json!({"from": {"id": "effects.preset.Spotlight"}, "to": {"id": format!("timeline.clip.{}", clips[2])}, "steps": 10}));
    d.frames(3);
    assert_eq!(d.effects(clips[2]).iter().filter(|e| *e == "brightness_contrast").count(), 1);
    assert!(!d.effects(clips[0]).contains(&"brightness_contrast".to_string()));
}
