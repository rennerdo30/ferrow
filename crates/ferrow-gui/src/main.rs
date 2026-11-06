//! Ferrow desktop app.
//!
//! The window keeps the native OS frame, so the title bar and the
//! minimise / maximise / close buttons are the system's own (Windows 11
//! caption buttons with Snap Layouts, macOS traffic lights). The content
//! is drawn with egui on the OS backdrop (Mica on Windows 11).

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod engine;
#[cfg(windows)]
mod frame;
mod platform;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("Ferrow")
        .with_app_id("ferrow")
        .with_inner_size([1040.0, 680.0])
        .with_min_inner_size([560.0, 360.0])
        .with_transparent(cfg!(target_os = "macos"))
        .with_icon(app_icon(64));
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

/// The app icon, rendered from the logo's shapes: a rust-orange
/// magnifier ring with a dark lens and an iron handle.
fn app_icon(size: u32) -> egui::IconData {
    let s = size as f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = (s * 0.42, s * 0.42);
    let outer = s * 0.36;
    let inner = s * 0.25;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - center.0).powi(2) + (fy - center.1).powi(2)).sqrt();
            // Handle: a thick segment towards the bottom right.
            let t = (((fx - center.0) + (fy - center.1)) / 2.0 / (s * 0.5)).clamp(0.0, 1.0);
            let hx = center.0 + t * s * 0.5;
            let hy = center.1 + t * s * 0.5;
            let hd = ((fx - hx).powi(2) + (fy - hy).powi(2)).sqrt();
            let mut px = [0u8; 4];
            let mut cover = |c: [u8; 3], alpha: f32| {
                let a = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
                if a > px[3] {
                    px = [c[0], c[1], c[2], a];
                }
            };
            if t > 0.55 {
                cover([0x4A, 0x4D, 0x52], s * 0.075 - hd + 0.5);
            }
            if d < inner + 0.5 {
                cover([0x22, 0x20, 0x1E], inner - d + 0.5);
            }
            if d > inner - 0.5 && d < outer + 0.5 {
                let g = ((fy - (center.1 - outer)) / (2.0 * outer)).clamp(0.0, 1.0);
                let lerp = |a: f32, b: f32| (a + (b - a) * g) as u8;
                let color = [lerp(242.0, 168.0), lerp(154.0, 64.0), lerp(74.0, 26.0)];
                cover(color, (outer - d + 0.5).min(d - inner + 0.5));
            }
            let i = ((y * size + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    egui::IconData {
        rgba,
        width: size,
        height: size,
    }
}
