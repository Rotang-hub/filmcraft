//! Fork fix: a linked clip isolated with Alt+click (to change one track's volume) is not left
//! acting alone. The next plain click on it selects its linked clips again, so it moves, trims and
//! deletes with them; with Linked Selection off a click still takes the one clip.

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
    fn selection(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.harness.state().session.state.selection.iter().map(|c| c.0).collect();
        v.sort_unstable();
        v
    }
}

#[test]
fn a_plain_click_on_an_alt_isolated_clip_takes_its_links_again() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let audio = q.audio_tracks.iter().flat_map(|t| t.items.iter()).find(|i| i.link.is_some()).expect("a linked audio clip");
    let (a, link) = (audio.id.0, audio.link);
    let mut group: Vec<u64> = q.all_tracks().flat_map(|t| t.items.iter()).filter(|i| i.link == link).map(|i| i.id.0).collect();
    group.sort_unstable();
    assert!(group.len() > 1);
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |cc| {
        filmcraft_ui_egui::framework::install_headless(&cc.egui_ctx);
        app
    });
    let mut d = Driver { harness, tx };
    d.frames(4);
    let id = format!("timeline.clip.{a}");

    // Alt+click isolates the one clip; its volume changes alone and the link stays
    d.ok("ui.click", json!({"id": id, "modifiers": {"alt": true}}));
    d.frames(2);
    assert_eq!(d.selection(), vec![a]);
    d.ok("engine.execute", json!({"command": "effects.setParam", "params": {"clip": a, "effect": "volume", "param": "level", "value": 3.0}}));
    let q = d.harness.state().session.active_sequence().unwrap();
    assert_eq!(q.find_item(filmcraft_project::ClipId(a)).unwrap().1.link, link);

    // a plain click on it: the linked clips are selected with it again
    d.ok("ui.click", json!({"id": id}));
    d.frames(2);
    assert_eq!(d.selection(), group);

    // Linked Selection off: a click takes the one clip, as before
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": [a], "linked": false}}));
    d.harness.state_mut().session.state.linked_selection = false;
    d.ok("ui.click", json!({"id": id}));
    d.frames(2);
    assert_eq!(d.selection(), vec![a]);
}
