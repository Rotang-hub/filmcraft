//! The dock's chrome (tab strips, the gaps between panels) is drawn by the shared UI runtime; it
//! must take FilmCraft's theme colours, not the suite default theme's lighter greys.

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use serde_json::json;

#[test]
fn the_dock_chrome_keeps_the_apps_dark_colours() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let app = FilmcraftApp::new(s);
    let gap = app.tokens.app_bg;
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 950.0)).with_max_steps(10_000).build_eframe(move |cc| {
        filmcraft_ui_egui::framework::install_headless(&cc.egui_ctx);
        app
    });
    for _ in 0..6 {
        h.step();
    }
    let img = match h.render() {
        Ok(img) => img,
        Err(e) => {
            eprintln!("rendering is not available here, skipped: {e}");
            return;
        }
    };
    let n = (img.width() * img.height()) as f64;
    let count = |c: [u8; 3]| img.pixels().filter(|p| p.0[..3] == c).count() as f64 / n;
    // the suite default's panel and background greys stay out of the frame...
    assert!(count([0x53, 0x53, 0x53]) + count([0x42, 0x42, 0x42]) < 0.005, "the dock is drawn in the suite's default greys");
    // ...and the app's own gap colour shows between the panels
    assert!(count([gap.r(), gap.g(), gap.b()]) > 0.02, "the gaps between panels are not the app's colour");
}
