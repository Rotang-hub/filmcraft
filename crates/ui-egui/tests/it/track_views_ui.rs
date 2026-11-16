//! Fork feature: the Track Views panel. Assign tracks to a button in Setup, press it, and only
//! those tracks are shown and heard; "All" brings every track back.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also write `track-views.png` there.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    fn new() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        session.execute("file.newSequence", json!({"name": "Views", "video": 3, "audio": 3})).expect("sequence");
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
        d
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

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(2);
    }

    fn view(&mut self) -> (Vec<bool>, Vec<bool>) {
        let q = self.harness.state().session.active_sequence().unwrap();
        (q.video_tracks.iter().map(|t| t.enabled).collect(), q.audio_tracks.iter().map(|t| !t.muted).collect())
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
fn a_view_button_shows_and_plays_only_its_tracks() {
    let mut d = Driver::new();
    d.ok("ui.menu.invoke", json!({"id": "window.panel.TrackViews"}));
    d.frames(3);
    // assign V2 + A1 + A3 to button 1 in Setup
    d.click("trackViews.setup");
    d.click("trackViews.edit.1");
    for id in ["trackViews.video.V2", "trackViews.audio.A1", "trackViews.audio.A3"] {
        d.click(id);
    }
    d.snapshot("track-views");
    let undo = d.harness.state().session.history.undo.len();
    d.click("trackViews.view.1");
    assert_eq!(d.view(), (vec![false, true, false], vec![true, false, true]));
    assert_eq!(d.harness.state().session.history.undo.len(), undo + 1, "one undo step");
    d.click("trackViews.all");
    assert_eq!(d.view(), (vec![true, true, true], vec![true, true, true]));
    // "Use current tracks" assigns what is shown now
    d.ok("engine.execute", json!({"command": "timeline.setTrack", "params": {"track": "V1", "enabled": false}}));
    d.click("trackViews.edit.2");
    d.click("trackViews.capture");
    d.click("trackViews.all");
    d.click("trackViews.view.2");
    assert_eq!(d.view(), (vec![false, true, true], vec![true, true, true]));
}

#[test]
fn the_number_of_buttons_is_set_in_setup() {
    let mut d = Driver::new();
    d.ok("ui.menu.invoke", json!({"id": "window.panel.TrackViews"}));
    d.frames(3);
    let has = |d: &mut Driver, id: &str| d.ok("ui.elements", json!({"prefix": id})).as_array().is_some_and(|a| a.iter().any(|e| e["id"] == json!(id)));
    assert!(has(&mut d, "trackViews.view.4") && !has(&mut d, "trackViews.view.5"), "four buttons to start");
    d.click("trackViews.setup");
    d.click("trackViews.count.plus");
    d.click("trackViews.count.plus");
    assert!(has(&mut d, "trackViews.view.6") && has(&mut d, "trackViews.edit.6"));
    // a new button is assigned and used like the others
    d.click("trackViews.edit.6");
    d.click("trackViews.video.V1");
    d.click("trackViews.view.6");
    assert_eq!(d.view().0, vec![true, false, false]);
    // removing takes the last button away; the one being edited stays in range
    for _ in 0..4 {
        d.click("trackViews.count.minus");
    }
    assert!(has(&mut d, "trackViews.view.2") && !has(&mut d, "trackViews.view.3"));
    d.click("trackViews.video.V3");
    d.click("trackViews.count.minus");
    assert!(has(&mut d, "trackViews.view.1") && !has(&mut d, "trackViews.view.2"), "one button is the least");
}

#[test]
fn number_keys_press_the_view_buttons_in_the_timeline() {
    let mut d = Driver::new();
    d.ok("ui.menu.invoke", json!({"id": "window.panel.TrackViews"}));
    d.frames(3);
    d.click("trackViews.setup");
    d.click("trackViews.edit.2");
    for id in ["trackViews.video.V2", "trackViews.audio.A1"] {
        d.click(id);
    }
    let press = |d: &mut Driver, key: egui::Key| {
        d.harness.key_press(key);
        d.frames(3);
    };
    // with the Timeline focused, 2 is button 2
    d.harness.state_mut().ui.focused = filmcraft_ui_egui::dock::PanelKind::Timeline;
    press(&mut d, egui::Key::Num2);
    assert_eq!(d.view(), (vec![false, true, false], vec![true, false, false]));
    // a button with no tracks changes nothing
    let undo = d.harness.state().session.history.undo.len();
    press(&mut d, egui::Key::Num3);
    assert_eq!(d.harness.state().session.history.undo.len(), undo);
    // the key after the last button (5 with four buttons) is "All"; a later one is nothing
    press(&mut d, egui::Key::Num5);
    assert_eq!(d.view(), (vec![true, true, true], vec![true, true, true]));
    press(&mut d, egui::Key::Num2);
    press(&mut d, egui::Key::Num6);
    assert_eq!(d.view(), (vec![false, true, false], vec![true, false, false]));
    // elsewhere the key is not a view button
    d.click("trackViews.all");
    d.harness.state_mut().ui.focused = filmcraft_ui_egui::dock::PanelKind::EffectControls;
    press(&mut d, egui::Key::Num2);
    assert_eq!(d.view(), (vec![true, true, true], vec![true, true, true]));
    // nor while a text field has focus, even with the Timeline the focused panel
    d.click("trackViews.name");
    d.harness.state_mut().ui.focused = filmcraft_ui_egui::dock::PanelKind::Timeline;
    press(&mut d, egui::Key::Num2);
    assert_eq!(d.view(), (vec![true, true, true], vec![true, true, true]));
}
