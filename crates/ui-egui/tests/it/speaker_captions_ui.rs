//! Fork feature: the Speaker Captions panel. Name the person on an audio track, and Make Captions
//! transcribes that track (on a worker thread) and places their head-caption graphics.

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::{Transcript, Word};
use filmcraft_speech::FixedTranscriber;
use filmcraft_time::Tick;
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
fn naming_a_track_and_making_captions_places_head_captions() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    // Ann's head-caption preset in the folder
    let r = s.execute("graphics.newText", json!({"text": "Ann", "time": 0, "seconds": 2})).unwrap();
    let path = dir.path().join("Ann_머리자막.fcgt");
    s.execute("graphics.template.export", json!({"clip": r["clip"], "name": "Ann head", "path": path.to_string_lossy()})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    // only the first A1 clip speaks: "안녕하세요"
    s.edit_sequence("test", |q, _, _| {
        for it in q.audio_tracks[0].items.iter_mut().skip(1) {
            it.enabled = false;
        }
        Ok(())
    })
    .unwrap();
    let mut t = Transcript { language: "ko".into(), ..Default::default() };
    t.words.push(Word::new("안녕하세요", Tick::from_seconds_f64(0.2), Tick::from_seconds_f64(0.8)));
    t.normalize();
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: t, id: "fixed".into() }));

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
    d.ok("ui.menu.invoke", json!({"id": "window.panel.SpeakerCaptions"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "speakerCaptions.folder"}));
    d.ok("ui.type", json!({"text": dir.path().to_string_lossy()}));
    d.ok("ui.click", json!({"id": "speakerCaptions.name.A1"}));
    d.ok("ui.type", json!({"text": "Ann"}));
    d.frames(2);
    if let Some(dir) = &snapshots {
        d.frames(2);
        match d.harness.render() {
            Ok(img) => {
                let _ = img.save(dir.join("speaker-captions.png"));
            }
            Err(e) => eprintln!("snapshot skipped: {e}"),
        }
    }
    d.ok("ui.click", json!({"id": "speakerCaptions.make"}));
    let mut made = false;
    for _ in 0..400 {
        d.frames(1);
        let q = d.harness.state().session.active_sequence().unwrap();
        if q.video_tracks.iter().any(|t| t.name.starts_with("Speaker Captions") && !t.items.is_empty()) {
            made = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(made, "captions placed; status: {}", d.harness.state().ui.status);
    let q = d.harness.state().session.active_sequence().unwrap();
    let caps: Vec<&str> =
        q.video_tracks.iter().filter(|t| t.name.starts_with("Speaker Captions")).flat_map(|t| t.items.iter().map(|i| i.name.as_str())).collect();
    assert_eq!(caps, ["Ann: 안녕하세요"]);
}
