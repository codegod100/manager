//! Agent Manager Android shell (vidya chrome).
//!
//! Full multi-agent PTY sessions stay on the desktop build — egui_term needs a
//! Unix PTY. The flake puts `cursor-agent` on PATH for `nix run` / the packaged
//! desktop app. This APK ships the branded UI for phone / Waydroid / emulator
//! smoke tests.

use eframe::egui::{self, Align, Label, Layout, RichText, ScrollArea, Sense, Ui};
use vidya::{
    apply_dark, card, dim_label, primary_button, reserve_system_chrome, title, Theme,
};

const APP_TITLE: &str = "Agent Manager";

/// egui skips drag-to-select when a touchscreen is present (scroll wins). This APK
/// runs under Waydroid / phones, so force click-and-drag selection for copyable text.
fn selectable_title_2(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.add(
        Label::new(
            RichText::new(text)
                .size(theme.type_scale.title_2)
                .strong()
                .color(theme.palette.text),
        )
        .wrap()
        .selectable(true)
        .sense(Sense::click_and_drag()),
    );
}

fn selectable_body(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.add(
        Label::new(
            RichText::new(text)
                .size(theme.type_scale.body)
                .color(theme.palette.text),
        )
        .wrap()
        .selectable(true)
        .sense(Sense::click_and_drag()),
    );
}

/// Desktop smoke-test entry (`cargo run --manifest-path android/Cargo.toml` is
/// not wired; use the root crate). Kept so the lib type-checks off-Android.
#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 720.0])
            .with_title(APP_TITLE),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(ManagerShell::new(cc)))),
    )
}

#[cfg(target_os = "android")]
pub fn run_android(android_app: winit::platform::android::activity::AndroidApp) -> eframe::Result {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(APP_TITLE),
        ..Default::default()
    };
    options.android_app = Some(android_app);
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(ManagerShell::new(cc)))),
    )
}

struct ManagerShell {
    theme: Theme,
}

impl ManagerShell {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_dark(&cc.egui_ctx);
        let mut theme = Theme::dark();
        theme.type_scale.caption = 13.0;
        Self { theme }
    }
}

impl eframe::App for ManagerShell {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_dark(ctx);
        reserve_system_chrome(ctx, &self.theme);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.theme.palette.window_bg))
            .show(ctx, |ui| {
                ui.add_space(self.theme.spacing.md);
                ui.horizontal(|ui| {
                    ui.add_space(self.theme.spacing.md);
                    title(ui, &self.theme, APP_TITLE);
                });
                ui.add_space(self.theme.spacing.sm);
                ui.horizontal(|ui| {
                    ui.add_space(self.theme.spacing.md);
                    dim_label(ui, &self.theme, "Multi-instance cursor-agent");
                });

                ui.add_space(self.theme.spacing.lg);

                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    // Prefer label drag-select over touch pan on this short page.
                    .drag_to_scroll(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(self.theme.spacing.md);
                            ui.vertical(|ui| {
                                ui.set_max_width(
                                    (ui.available_width() - self.theme.spacing.md).max(280.0),
                                );

                                card(ui, &self.theme, |ui| {
                                    selectable_title_2(ui, &self.theme, "Desktop sessions");
                                    ui.add_space(self.theme.spacing.sm);
                                    selectable_body(
                                        ui,
                                        &self.theme,
                                        "Interactive cursor-agent PTYs (egui_term) need a \
                                         Linux desktop — they are not in this APK. The flake \
                                         already wraps cursor-agent into PATH for desktop runs.",
                                    );
                                    ui.add_space(self.theme.spacing.md);
                                    selectable_body(
                                        ui,
                                        &self.theme,
                                        "On your machine:\n\
                                         • nix run .#desktop\n\
                                         • nix develop && cargo run --release",
                                    );
                                });

                                ui.add_space(self.theme.spacing.md);

                                card(ui, &self.theme, |ui| {
                                    selectable_title_2(ui, &self.theme, "This build");
                                    ui.add_space(self.theme.spacing.sm);
                                    selectable_body(
                                        ui,
                                        &self.theme,
                                        "Package uk.nandi.manager — NativeActivity shell \
                                         themed with Vidya, for install / Waydroid smoke tests.",
                                    );
                                    ui.add_space(self.theme.spacing.md);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let _ = primary_button(ui, &self.theme, "Got it");
                                    });
                                });

                                ui.add_space(self.theme.spacing.xl);
                                ui.add(
                                    Label::new(
                                        RichText::new("nandi.uk/manager")
                                            .size(self.theme.type_scale.caption)
                                            .color(self.theme.palette.text_secondary),
                                    )
                                    .selectable(true)
                                    .sense(Sense::click_and_drag()),
                                );
                                ui.add_space(self.theme.spacing.lg);
                            });
                        });
                    });
            });
    }
}
