#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod app;
mod db;
mod embed;
#[allow(dead_code)] // process-based player kept as a reference fallback
mod player;
mod stream;
mod sync;
mod theme;

use app::App;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 740.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("RustTube")
            .with_app_id("rusttube")
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "RustTube",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// Embed the generated icon so bundled builds do not depend on the working
/// directory containing `icons/`.
fn window_icon() -> egui::IconData {
    let fallback = || egui::IconData {
        rgba: vec![0, 0, 0, 0],
        width: 1,
        height: 1,
    };
    let Ok(image) = image::load_from_memory(include_bytes!("../icons/128x128.png")) else {
        return fallback();
    };
    let rgba = image.to_rgba8();
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}
