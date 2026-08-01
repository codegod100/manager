//! Main window: vidya chrome, session tabs, embedded agent terminals.

use crate::session::{AgentSession, NewSessionDraft};
use egui_term::{ColorPalette, PtyEvent, TerminalTheme, TerminalView};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use vidya::Theme;

pub struct App {
    theme: Theme,
    next_id: u64,
    sessions: BTreeMap<u64, AgentSession>,
    active: Option<u64>,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    new_dialog: Option<NewSessionDraft>,
    spawn_error: Option<String>,
    term_theme: TerminalTheme,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (pty_tx, pty_rx) = mpsc::channel();
        Self {
            theme: Theme::dark(),
            next_id: 1,
            sessions: BTreeMap::new(),
            active: None,
            pty_tx,
            pty_rx,
            new_dialog: None,
            spawn_error: None,
            term_theme: vidya_term_theme(),
        }
    }

    fn poll_pty_events(&mut self) {
        while let Ok((id, event)) = self.pty_rx.try_recv() {
            match event {
                PtyEvent::Exit => {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        session.alive = false;
                        if !session.title.ends_with(" (exited)") {
                            session.title.push_str(" (exited)");
                        }
                    }
                }
                PtyEvent::Title(title) => {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        if session.alive {
                            session.title = title;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn spawn_from_draft(&mut self, ctx: &egui::Context, draft: NewSessionDraft) {
        let id = self.next_id;
        self.next_id += 1;
        match AgentSession::spawn(id, ctx.clone(), self.pty_tx.clone(), &draft) {
            Ok(session) => {
                self.sessions.insert(id, session);
                self.active = Some(id);
                self.new_dialog = None;
                self.spawn_error = None;
            }
            Err(err) => {
                self.spawn_error = Some(err);
                // Keep dialog open so the user can fix the path / flags.
                self.new_dialog = Some(draft);
            }
        }
    }

    fn close_active(&mut self) {
        let Some(id) = self.active else {
            return;
        };
        self.sessions.remove(&id);
        self.active = self
            .sessions
            .range(..id)
            .next_back()
            .map(|(k, _)| *k)
            .or_else(|| self.sessions.keys().next().copied());
    }

    fn show_header(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let running = self.sessions.values().filter(|s| s.alive).count();
        let total = self.sessions.len();
        let active_ws = self
            .active
            .and_then(|id| self.sessions.get(&id))
            .map(|s| format!("#{} · {}", s.id, s.workspace.display()));
        let mut open_new = false;
        let mut close = false;

        vidya::top_header(ctx, &theme, |ui| {
            ui.horizontal(|ui| {
                vidya::title(ui, &theme, "Agent Manager");
                ui.add_space(theme.spacing.md);
                vidya::dim_label(ui, &theme, &format!("{running}/{total} running"));
                if let Some(ws) = &active_ws {
                    ui.add_space(theme.spacing.sm);
                    vidya::dim_label(ui, &theme, ws);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if vidya::button(ui, &theme, "Close").clicked() {
                        close = true;
                    }
                    ui.add_space(theme.spacing.sm);
                    if vidya::primary_button(ui, &theme, "New session").clicked() {
                        open_new = true;
                    }
                });
            });
        });

        if open_new {
            self.new_dialog = Some(NewSessionDraft::default());
            self.spawn_error = None;
        }
        if close {
            self.close_active();
        }
    }

    fn show_tabs(&mut self, ctx: &egui::Context) {
        if self.sessions.is_empty() {
            return;
        }

        let theme = self.theme.clone();
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        let mut select: Option<u64> = None;

        egui::TopBottomPanel::top("session_tabs")
            .frame(
                egui::Frame::NONE
                    .fill(theme.palette.view_bg)
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for id in ids {
                        let (title, alive) = self
                            .sessions
                            .get(&id)
                            .map(|s| (s.title.clone(), s.alive))
                            .unwrap_or_else(|| ("?".into(), false));
                        let active = self.active == Some(id);
                        let label = if alive {
                            title
                        } else {
                            format!("○ {title}")
                        };

                        let text = egui::RichText::new(label)
                            .size(theme.type_scale.body)
                            .color(if active {
                                theme.palette.accent_fg
                            } else {
                                theme.palette.button_fg
                            });
                        let fill = if active {
                            theme.palette.accent
                        } else {
                            theme.palette.button_bg
                        };
                        let btn = egui::Button::new(text)
                            .fill(fill)
                            .stroke(egui::Stroke::new(1.0, theme.palette.border_soft))
                            .corner_radius(theme.spacing.radius_md)
                            .min_size(egui::vec2(0.0, theme.spacing.control_height));
                        if ui.add(btn).clicked() {
                            select = Some(id);
                        }
                    }
                });
            });

        if let Some(id) = select {
            self.active = Some(id);
        }
    }

    fn show_new_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.new_dialog.take() else {
            return;
        };

        let theme = self.theme.clone();
        let mut spawn = false;
        let mut cancel = false;
        let mut keep_open = true;
        let error = self.spawn_error.clone();

        egui::Window::new("New session")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(theme.card_frame())
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                vidya::title_2(ui, &theme, "Launch cursor-agent");
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Workspace");
                vidya::text_field_singleline(ui, &theme, &mut draft.workspace);
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Model (optional)");
                vidya::text_field_singleline(ui, &theme, &mut draft.model);
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Initial prompt (optional)");
                vidya::text_field_multiline(ui, &theme, &mut draft.prompt, 3);
                ui.add_space(theme.spacing.sm);
                vidya::checkbox(ui, &theme, &mut draft.trust, "Trust workspace (--trust)");
                vidya::checkbox(ui, &theme, &mut draft.force, "Force / yolo (--force)");

                if let Some(err) = &error {
                    ui.add_space(theme.spacing.sm);
                    ui.colored_label(theme.palette.destructive, err);
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    if vidya::primary_button(ui, &theme, "Spawn").clicked() {
                        spawn = true;
                    }
                    if vidya::button(ui, &theme, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel {
            keep_open = false;
            self.spawn_error = None;
        }
        if spawn {
            self.spawn_from_draft(ctx, draft);
            return;
        }
        if keep_open {
            self.new_dialog = Some(draft);
        }
    }

    fn show_central(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let term_theme = self.term_theme.clone();
        let active = self.active;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.palette.view_bg))
            .show(ctx, |ui| {
                if let Some(id) = active {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        let terminal = TerminalView::new(ui, &mut session.backend)
                            .set_focus(session.alive)
                            .set_theme(term_theme.clone())
                            .set_size(ui.available_size());
                        ui.add(terminal);
                        return;
                    }
                }

                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    vidya::title(ui, &theme, "No agent sessions");
                    ui.add_space(theme.spacing.sm);
                    vidya::dim_label(
                        ui,
                        &theme,
                        "New session opens an interactive cursor-agent PTY.",
                    );
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        vidya::apply(ctx, &self.theme);

        if ctx.input(|i| i.viewport().close_requested()) {
            self.sessions.clear();
            self.active = None;
        }

        self.poll_pty_events();
        self.show_header(ctx);
        self.show_tabs(ctx);
        self.show_central(ctx);
        self.show_new_dialog(ctx);
    }
}

fn vidya_term_theme() -> TerminalTheme {
    // Roughly match vidya dark view / accent blues.
    TerminalTheme::new(Box::new(ColorPalette {
        foreground: "#ffffff".into(),
        background: "#1e1e1e".into(),
        black: "#1e1e1e".into(),
        red: "#c01c28".into(),
        green: "#2ec27e".into(),
        yellow: "#e5a50a".into(),
        blue: "#3584e4".into(),
        magenta: "#9141ac".into(),
        cyan: "#62a0ea".into(),
        white: "#ffffff".into(),
        bright_black: "#5e5c64".into(),
        bright_red: "#e01b24".into(),
        bright_green: "#57e389".into(),
        bright_yellow: "#f8e45c".into(),
        bright_blue: "#4a93e7".into(),
        bright_magenta: "#c061cb".into(),
        bright_cyan: "#93c0ea".into(),
        bright_white: "#ffffff".into(),
        bright_foreground: Some("#ffffff".into()),
        dim_foreground: "#9a9996".into(),
        dim_black: "#181818".into(),
        dim_red: "#a51d2c".into(),
        dim_green: "#26a269".into(),
        dim_yellow: "#c64600".into(),
        dim_blue: "#1c71d8".into(),
        dim_magenta: "#613583".into(),
        dim_cyan: "#0a7d9c".into(),
        dim_white: "#9a9996".into(),
    }))
}
