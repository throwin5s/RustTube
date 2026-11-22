#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod app;
mod db;
mod embed;
mod player;
mod sponsor;
mod stream;
mod summary;
mod sync;
mod theme;
mod update;

use app::App;
use eframe::egui;

fn main() -> eframe::Result<()> {
    // Draw the app into the titlebar area: the top bar becomes the window
    // chrome. The macOS traffic lights start hidden and are revealed when the
    // pointer comes near them; on Windows the OS cannot hide just the caption
    // buttons, so the frame is dropped and the bar draws its own.
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 740.0])
        .with_min_inner_size([900.0, 560.0])
        .with_title("RustTube")
        .with_app_id("rusttube")
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false)
        .with_titlebar_buttons_shown(false)
        .with_icon(window_icon());
    #[cfg(target_os = "windows")]
    let viewport = viewport.with_decorations(false);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "RustTube",
        options,
        Box::new(|cc| {
            #[cfg(target_os = "windows")]
            round_window_corners(cc);
            Ok(Box::new(App::new(cc)))
        }),
    )
}

/// Windows 11 rounds decorated windows by itself. RustTube has no frame on
/// Windows (the top bar draws its own caption buttons), so the window opts into
/// DWM's rounding explicitly. On Windows 10 the attribute is simply ignored.
#[cfg(target_os = "windows")]
fn round_window_corners(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(window) = handle.as_raw() else {
        return;
    };
    let preference = DWMWCP_ROUND;
    unsafe {
        DwmSetWindowAttribute(
            window.hwnd.get() as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &preference as *const _ as *const _,
            std::mem::size_of_val(&preference) as u32,
        );
    }
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
