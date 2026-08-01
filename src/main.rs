//! Desktop multi-agent manager for cursor-agent.

mod app;
mod session;

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Agent Manager")
            .with_app_id("agent-manager")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Agent Manager",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
