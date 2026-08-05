//! Desktop multi-agent manager for cursor-agent.

mod app;
mod auth;
mod clipboard;
mod convert;
mod oidc;
mod session;
mod subagents;

use vidya::with_app_icon_id;

/// Window / FreeDesktop icon (256² PNG; source SVG in `assets/manager.svg`).
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/manager-256.png");

fn main() -> eframe::Result {
    let viewport = with_app_icon_id(
        egui::ViewportBuilder::default()
            .with_title("Agent Manager")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0]),
        "manager", // Wayland app_id ↔ manager.desktop StartupWMClass
        APP_ICON_PNG,
    );

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Agent Manager",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
