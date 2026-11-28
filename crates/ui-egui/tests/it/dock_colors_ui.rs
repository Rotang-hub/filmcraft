//! The dock's chrome (tab strips, the gaps between panels) is drawn by the shared UI runtime; it
//! must take FilmCraft's theme colours, not the suite default theme's lighter greys, with the tab strips in
//! the panels' colour and only the thin gaps between panels in the app background.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use serde_json::json;

#[test]
fn the_dock_chrome_keeps_the_apps_dark_colours() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let app = FilmcraftApp::new(s);
    let gap = app.tokens.app_bg;
    let strip = app.tokens.tab_bg;
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 950.0)).with_max_steps(10_000).build_eframe(move |cc| {
        filmcraft_ui_egui::framework::install_headless(&cc.egui_ctx);
        app
    });
    for _ in 0..6 {
        h.step();
    }
    // the row of the top-left tab strip ("Effect Controls" is one of its tabs)
    let row = h.get_by_label("Effect Controls").rect().center().y;
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
    assert!(count([gap.r(), gap.g(), gap.b()]) > 0.01, "the gaps between panels are not the app's colour");
    // the tab strips are the panels' colour, not the gap's black
    let y = (row * h.ctx.pixels_per_point()).round() as u32;
    let in_strip = (0..img.width()).filter(|x| img.get_pixel(*x, y).0[..3] == [strip.r(), strip.g(), strip.b()]).count();
    assert!(in_strip * 2 > img.width() as usize, "most of the tab strip is the tab colour: {in_strip} of {}", img.width());
}
