//! Fork feature: the Auto Cut panel measures each person's microphone on a worker thread and
//! builds the cut on its own "Auto Cut" track from their views.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::ClipId;
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
}

#[test]
fn make_auto_cut_builds_the_cut_track_from_the_views() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    // a second view on V2 (a copy of V1)
    s.edit_sequence("test", |q, ctx, _| {
        let mut copy = q.video_tracks[0].items.clone();
        for it in &mut copy {
            it.id = ClipId(ctx.alloc());
            it.link = None;
        }
        q.video_tracks[1].items = copy;
        Ok(())
    })
    .unwrap();
    s.prefs_path = Some(dir.path().join("preferences.json"));
    std::fs::write(
        dir.path().join("auto_cut.json"),
        json!({"people": [{"name": "A", "video": "V1", "audio": "A1"}, {"name": "B", "video": "V2", "audio": "A2"}]}).to_string(),
    )
    .unwrap();
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
    let mut b = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).with_max_steps(10_000);
    if snapshots.is_some() {
        b = b.wgpu();
    }
    let harness = b.build_eframe(move |_cc| app);
    let mut d = Driver { harness, tx };
    d.frames(4);
    d.ok("ui.menu.invoke", json!({"id": "window.panel.AutoCut"}));
    d.frames(3);
    if let Some(dir) = &snapshots {
        d.frames(2);
        if let Ok(img) = d.harness.render() {
            let _ = img.save(dir.join("auto-cut.png"));
        }
    }
    let els = d.ok("ui.elements", json!({})).to_string();
    for id in ["autoCut.trim", "autoCut.minSilence", "autoCut.pad", "autoCut.sound.0", "autoCut.voice.A3", "autoCut.keep", "autoCut.motion"] {
        assert!(els.contains(id), "{id} is shown");
    }
    // the button sits below the options: scroll the panel down to it
    for _ in 0..10 {
        d.ok("ui.scroll", json!({"id": "autoCut.minShot", "dx": 0.0, "dy": -400.0}));
        d.frames(2);
    }
    d.ok("ui.click", json!({"id": "autoCut.make"}));
    let mut made = false;
    for _ in 0..600 {
        d.frames(1);
        let q = d.harness.state().session.active_sequence().unwrap();
        if q.video_tracks.iter().any(|t| t.name == "Auto Cut" && !t.items.is_empty()) {
            made = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(made, "the cut was built; status: {}", d.harness.state().ui.status);
    assert!(d.harness.state().ui.status.contains("Auto Cut"), "{}", d.harness.state().ui.status);
}
