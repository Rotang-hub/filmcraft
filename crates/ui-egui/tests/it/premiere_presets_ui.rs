//! Fork: Premiere Pro preset files (`.prfpset`) import into the Presets bin with their folders:
//! each Premiere bin is a folder row that opens like the Effects panel's own folders, and the
//! presets inside apply like any other.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

const FILE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
	<Tree ObjectRef="1"/>
	<Tree ObjectID="1"><RootBin ObjectRef="2"/></Tree>
	<BinTreeItem ObjectID="2"><Items><Item Index="0" ObjectRef="3"/></Items><TreeItemBase><Name>Root</Name></TreeItemBase></BinTreeItem>
	<BinTreeItem ObjectID="3"><Items><Item Index="0" ObjectRef="4"/></Items><TreeItemBase><Name>Pack</Name></TreeItemBase></BinTreeItem>
	<BinTreeItem ObjectID="4"><Items><Item Index="0" ObjectRef="5"/></Items><TreeItemBase><Name>Slides</Name></TreeItemBase></BinTreeItem>
	<TreeItem ObjectID="5"><TreeItemBase><Name>Pack Spin</Name><Data ObjectRef="6"/></TreeItemBase></TreeItem>
	<FilterPresetItem ObjectID="6"><FilterPresets><FilterPreset Index="0" ObjectRef="7"/></FilterPresets></FilterPresetItem>
	<FilterPreset ObjectID="7"><FilterMatchName>AE.ADBE Geometry2</FilterMatchName><Component ObjectRef="8"/><AnchorInPoint>0</AnchorInPoint><AnchorOutPoint>254016000000</AnchorOutPoint><Type>1</Type></FilterPreset>
	<VideoFilterComponent ObjectID="8"><Component><DisplayName>Transform</DisplayName><Params><Param Index="0" ObjectRef="9"/></Params></Component><MatchName>AE.ADBE Geometry2</MatchName></VideoFilterComponent>
	<VideoComponentParam ObjectID="9"><Name>Rotation</Name><Keyframes>0,0.,5,0,0,0.33,0,0.33;254016000000,360.,5,0,0,0.33,0,0.33;</Keyframes><ParameterID>7</ParameterID><StartKeyframe>-91445760000000000,0.,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
</PremiereData>"#;

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
    fn has(&mut self, id: &str) -> bool {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array().is_some_and(|a| a.iter().any(|e| e["id"] == json!(id)))
    }
}

#[test]
fn premiere_presets_import_into_folders() {
    let dir = std::env::temp_dir().join(format!("filmcraft-prfpset-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pack.prfpset");
    std::fs::write(&path, FILE).unwrap();

    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let r = s.execute("presets.import", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(r["imported"], json!(["Pack Spin"]));
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    let mut d = Driver { harness, tx };
    d.frames(4);
    d.ok("ui.menu.invoke", json!({"id": "window.panel.Effects"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effects.folder.Presets"}));
    d.frames(2);

    // closed folders hide what they hold
    assert!(d.has("effects.folder.Presets/Pack"));
    assert!(!d.has("effects.folder.Presets/Pack/Slides"));
    assert!(!d.has("effects.preset.Pack Spin"));
    d.ok("ui.click", json!({"id": "effects.folder.Presets/Pack"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effects.folder.Presets/Pack/Slides"}));
    d.frames(2);
    assert!(d.has("effects.preset.Pack Spin"), "inside its opened folders");

    // it applies like any preset, keyframes and all
    d.ok("engine.execute", json!({"command": "timeline.select", "params": {"clips": [clip]}}));
    d.ok("ui.click", json!({"id": "effects.preset.Pack Spin", "button": "right"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "effects.presetMenu.Pack Spin.apply"}));
    d.frames(2);
    let q = d.harness.state().session.active_sequence().unwrap();
    let it = q.find_item(filmcraft_project::ClipId(clip)).unwrap().1;
    let t = it.effects.iter().find(|e| e.effect == "transform").expect("the preset's Transform");
    assert_eq!(t.params["rotation"].keyframes.len(), 2);

    // a search shows the preset inside its folders without opening them
    d.ok("ui.click", json!({"id": "effects.folder.Presets/Pack"}));
    d.frames(2);
    assert!(!d.has("effects.preset.Pack Spin"));
    d.harness.state_mut().ui.effects_search = "spin".into();
    d.frames(2);
    assert!(d.has("effects.preset.Pack Spin"));
    let _ = std::fs::remove_dir_all(&dir);
}
