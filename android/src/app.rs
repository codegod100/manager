//! Agent Manager Android shell (vidya chrome).
//!
//! Full multi-agent PTY sessions stay on the desktop build — egui_term needs a
//! Unix PTY. The flake puts `cursor-agent` on PATH for `nix run` / the packaged
//! desktop app. This APK ships the branded UI for phone / Waydroid / emulator
//! smoke tests.

use eframe::egui::{
    self,
    text::{CCursor, CCursorRange},
    Align, Color32, FontId, Id, Key, Label, Layout, Margin, RichText, ScrollArea, Sense, TextEdit,
    Ui,
};
use vidya::{
    apply_dark, card, dim_label, primary_button, reserve_system_chrome, title, Theme,
};

const APP_TITLE: &str = "Agent Manager";

/// Copyable paragraph: drag scrolls on touch; long-press focuses, selects all, and
/// copies. egui only allows drag-to-select on touch once a `TextEdit` has focus.
fn copyable_text(
    ui: &mut Ui,
    id: Id,
    text: &str,
    size: f32,
    strong: bool,
    color: Color32,
    selecting: &mut Option<Id>,
    select_all_pending: &mut bool,
) {
    let touch = ui.input(|i| i.has_touch_screen());
    let active = *selecting == Some(id);

    if !touch {
        // Desktop / mouse: normal egui label selection (drag-to-select).
        let mut rich = RichText::new(text).size(size).color(color);
        if strong {
            rich = rich.strong();
        }
        ui.add(Label::new(rich).wrap().selectable(true));
        return;
    }

    if active {
        let mut buf = text.to_owned();
        let output = TextEdit::multiline(&mut buf)
            .id(id)
            .frame(false)
            .desired_width(ui.available_width())
            .font(FontId::proportional(size))
            .text_color(color)
            .margin(Margin::ZERO)
            .show(ui);

        if *select_all_pending {
            let char_len = text.chars().count();
            let mut state = output.state;
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(0),
                CCursor::new(char_len),
            )));
            state.store(ui.ctx(), output.response.id);
            output.response.request_focus();
            ui.ctx().copy_text(text.to_owned());
            *select_all_pending = false;
        } else if output.response.drag_stopped() {
            if let Some(range) = output.cursor_range {
                let selected = range.slice_str(text);
                if !selected.is_empty() {
                    ui.ctx().copy_text(selected.to_owned());
                }
            }
        }

        if output.response.clicked_elsewhere() || ui.input(|i| i.key_pressed(Key::Escape)) {
            *selecting = None;
            ui.memory_mut(|m| m.surrender_focus(id));
        }
        return;
    }

    // Idle touch label: click/long-press only — drag is left to ScrollArea.
    let mut rich = RichText::new(text).size(size).color(color);
    if strong {
        rich = rich.strong();
    }
    let response = ui.add(
        Label::new(rich)
            .wrap()
            .selectable(false)
            .sense(Sense::click()),
    );
    if response.long_touched() {
        *selecting = Some(id);
        *select_all_pending = true;
    }
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
    /// Touch long-press target currently in drag-select mode.
    selecting: Option<Id>,
    select_all_pending: bool,
}

impl ManagerShell {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_dark(&cc.egui_ctx);
        let mut theme = Theme::dark();
        theme.type_scale.caption = 13.0;
        Self {
            theme,
            selecting: None,
            select_all_pending: false,
        }
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

                // While a label is in select mode, don't let ScrollArea steal drags.
                let drag_to_scroll = self.selecting.is_none();

                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .drag_to_scroll(drag_to_scroll)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(self.theme.spacing.md);
                            ui.vertical(|ui| {
                                ui.set_max_width(
                                    (ui.available_width() - self.theme.spacing.md).max(280.0),
                                );

                                let text = self.theme.palette.text;
                                let secondary = self.theme.palette.text_secondary;

                                card(ui, &self.theme, |ui| {
                                    copyable_text(
                                        ui,
                                        Id::new("sel_desktop_title"),
                                        "Desktop sessions",
                                        self.theme.type_scale.title_2,
                                        true,
                                        text,
                                        &mut self.selecting,
                                        &mut self.select_all_pending,
                                    );
                                    ui.add_space(self.theme.spacing.sm);
                                    copyable_text(
                                        ui,
                                        Id::new("sel_desktop_body"),
                                        "Interactive cursor-agent PTYs (egui_term) need a \
                                         Linux desktop — they are not in this APK. The flake \
                                         already wraps cursor-agent into PATH for desktop runs.",
                                        self.theme.type_scale.body,
                                        false,
                                        text,
                                        &mut self.selecting,
                                        &mut self.select_all_pending,
                                    );
                                    ui.add_space(self.theme.spacing.md);
                                    copyable_text(
                                        ui,
                                        Id::new("sel_desktop_cmds"),
                                        "On your machine:\n\
                                         • nix run .#desktop\n\
                                         • nix develop && cargo run --release",
                                        self.theme.type_scale.body,
                                        false,
                                        text,
                                        &mut self.selecting,
                                        &mut self.select_all_pending,
                                    );
                                });

                                ui.add_space(self.theme.spacing.md);

                                card(ui, &self.theme, |ui| {
                                    copyable_text(
                                        ui,
                                        Id::new("sel_build_title"),
                                        "This build",
                                        self.theme.type_scale.title_2,
                                        true,
                                        text,
                                        &mut self.selecting,
                                        &mut self.select_all_pending,
                                    );
                                    ui.add_space(self.theme.spacing.sm);
                                    copyable_text(
                                        ui,
                                        Id::new("sel_build_body"),
                                        "Package uk.nandi.manager — NativeActivity shell \
                                         themed with Vidya, for install / Waydroid smoke tests.",
                                        self.theme.type_scale.body,
                                        false,
                                        text,
                                        &mut self.selecting,
                                        &mut self.select_all_pending,
                                    );
                                    ui.add_space(self.theme.spacing.md);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let _ = primary_button(ui, &self.theme, "Got it");
                                    });
                                });

                                ui.add_space(self.theme.spacing.xl);
                                copyable_text(
                                    ui,
                                    Id::new("sel_footer"),
                                    "nandi.uk/manager",
                                    self.theme.type_scale.caption,
                                    false,
                                    secondary,
                                    &mut self.selecting,
                                    &mut self.select_all_pending,
                                );
                                ui.add_space(self.theme.spacing.lg);
                            });
                        });
                    });
            });
    }
}
