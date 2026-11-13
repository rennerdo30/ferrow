//! Ferrow desktop app.
//!
//! The window keeps the native OS frame, so the title bar and the
//! minimise / maximise / close buttons are the system's own (Windows 11
//! caption buttons with Snap Layouts, macOS traffic lights). The content
//! is drawn with egui on the OS backdrop (Mica on Windows 11).

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
#[cfg(windows)]
mod drag;
mod engine;
#[cfg(windows)]
mod frame;
mod logo;
mod platform;
mod settings;
#[cfg(windows)]
mod shell_menu;
mod theme;
#[cfg(windows)]
mod tray;
mod update;

use eframe::egui;

fn main() -> eframe::Result {
    // Ferrow keeps running in the tray: starting it again shows that
    // window instead of opening a second copy.
    if platform::activate_running_instance() {
        return Ok(());
    }
    let viewport = egui::ViewportBuilder::default()
        .with_title("Ferrow")
        .with_app_id("ferrow")
        .with_inner_size([1040.0, 680.0])
        .with_min_inner_size([560.0, 360.0])
        .with_transparent(cfg!(target_os = "macos"))
        .with_icon(logo::window_icon());
    // At sign-in, or after an update while it was in the tray, Ferrow
    // starts hidden in the tray; the tray icon or starting it again shows
    // the window.
    if platform::start_hidden() {
        platform::keep_hidden_until_opened();
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Ferrow",
        options,
        Box::new(|cc| match app::FerrowApp::new(cc) {
            Ok(app) => Ok(Box::new(app)),
            Err(e) => Err(e.into()),
        }),
    )
}
