//! Main window: vidya chrome, session sidebar, embedded agent terminals.

use crate::session::{list_saved_chats, AgentSession, NewSessionDraft, SavedChat};
use crate::subagents::SubagentStatus;
use egui_term::{BackendCommand, ColorPalette, PtyEvent, TerminalTheme, TerminalView};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use vidya::Theme;

struct ResumeDialog {
    filter: String,
    chats: Vec<SavedChat>,
    selected: Option<String>,
    model: String,
    trust: bool,
    force: bool,
    load_error: Option<String>,
}

struct RenameDialog {
    id: u64,
    draft: String,
    /// Request focus once when the dialog opens.
    focus: bool,
}

impl ResumeDialog {
    fn load() -> Self {
        match list_saved_chats() {
            Ok(chats) => Self {
                filter: String::new(),
                chats,
                selected: None,
                model: String::new(),
                trust: true,
                force: false,
                load_error: None,
            },
            Err(err) => Self {
                filter: String::new(),
                chats: Vec::new(),
                selected: None,
                model: String::new(),
                trust: true,
                force: false,
                load_error: Some(err),
            },
        }
    }

    fn filtered(&self) -> Vec<&SavedChat> {
        let q = self.filter.trim().to_lowercase();
        self.chats
            .iter()
            .filter(|c| {
                if q.is_empty() {
                    return true;
                }
                c.title.to_lowercase().contains(&q)
                    || c.workspace_label().to_lowercase().contains(&q)
                    || c.id.to_lowercase().contains(&q)
            })
            .collect()
    }

    fn selected_chat(&self) -> Option<&SavedChat> {
        let id = self.selected.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    fn to_draft(&self) -> Result<NewSessionDraft, String> {
        let chat = self
            .selected_chat()
            .ok_or_else(|| "select a chat to resume".to_string())?;
        let workspace = chat
            .cwd
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                "selected chat has no workspace path; cannot resume".to_string()
            })?;
        Ok(NewSessionDraft {
            workspace,
            model: self.model.clone(),
            prompt: String::new(),
            trust: self.trust,
            force: self.force,
            resume_chat_id: Some(chat.id.clone()),
            tab_title: Some(chat.title.clone()),
        })
    }
}

pub struct App {
    theme: Theme,
    next_id: u64,
    sessions: BTreeMap<u64, AgentSession>,
    active: Option<u64>,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    new_dialog: Option<NewSessionDraft>,
    resume_dialog: Option<ResumeDialog>,
    rename_dialog: Option<RenameDialog>,
    spawn_error: Option<String>,
    term_theme: TerminalTheme,
    last_subagent_poll: Instant,
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
            resume_dialog: None,
            rename_dialog: None,
            spawn_error: None,
            term_theme: vidya_term_theme(),
            last_subagent_poll: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        }
    }

    fn dialog_open(&self) -> bool {
        self.new_dialog.is_some()
            || self.resume_dialog.is_some()
            || self.rename_dialog.is_some()
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
                        if session.alive && !session.title_locked {
                            session.title = title;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn poll_subagents(&mut self, ctx: &egui::Context) {
        if self.last_subagent_poll.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_subagent_poll = Instant::now();

        let mut changed = false;
        for session in self.sessions.values_mut() {
            if session.poll_chat_state() {
                changed = true;
            }
        }
        if changed || self.sessions.values().any(|s| s.alive) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn spawn_from_draft(&mut self, ctx: &egui::Context, draft: NewSessionDraft) -> bool {
        let id = self.next_id;
        self.next_id += 1;
        match AgentSession::spawn(id, ctx.clone(), self.pty_tx.clone(), &draft) {
            Ok(session) => {
                self.sessions.insert(id, session);
                self.active = Some(id);
                self.new_dialog = None;
                self.resume_dialog = None;
                self.spawn_error = None;
                true
            }
            Err(err) => {
                self.spawn_error = Some(err);
                false
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
        let mut open_resume = false;
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
                    if vidya::button(ui, &theme, "Resume").clicked() {
                        open_resume = true;
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
            self.resume_dialog = None;
            self.spawn_error = None;
        }
        if open_resume {
            self.resume_dialog = Some(ResumeDialog::load());
            self.new_dialog = None;
            self.spawn_error = None;
        }
        if close {
            self.close_active();
        }
    }

    fn show_sidebar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        let mut select: Option<u64> = None;
        let mut auto_rename: Option<u64> = None;
        let mut open_rename: Option<u64> = None;

        egui::SidePanel::left("agents")
            .resizable(true)
            .default_width(260.0)
            .width_range(200.0..=400.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme.palette.view_bg)
                    .stroke(egui::Stroke::new(1.0, theme.palette.border_soft))
                    .inner_margin(egui::Margin::symmetric(10, 10)),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                vidya::title_2(ui, &theme, "Agents");
                ui.add_space(theme.spacing.sm);

                if ids.is_empty() {
                    vidya::dim_label(ui, &theme, "No sessions yet.");
                    ui.add_space(theme.spacing.xs);
                    vidya::dim_label(ui, &theme, "New session or Resume to start.");
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for id in ids {
                            let Some(session) = self.sessions.get(&id) else {
                                continue;
                            };
                            let active = self.active == Some(id);
                            let ws_label = session
                                .workspace
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("workspace");
                            let running_subs = session
                                .subagents
                                .iter()
                                .filter(|s| s.status.is_live())
                                .count();
                            let sub_note = if session.subagents.is_empty() {
                                None
                            } else if running_subs > 0 {
                                Some(format!(
                                    "{running_subs}/{} subagents",
                                    session.subagents.len()
                                ))
                            } else {
                                Some(format!("{} subagents", session.subagents.len()))
                            };

                            let fill = if active {
                                theme.palette.accent
                            } else {
                                theme.palette.button_bg
                            };
                            let stroke = egui::Stroke::new(
                                1.0,
                                if active {
                                    theme.palette.accent
                                } else {
                                    theme.palette.border_soft
                                },
                            );
                            let title_color = if active {
                                theme.palette.accent_fg
                            } else {
                                theme.palette.text
                            };
                            let dim_color = if active {
                                theme.palette.accent_fg
                            } else {
                                theme.palette.text_secondary
                            };

                            let row = egui::Frame::NONE
                                .fill(fill)
                                .stroke(stroke)
                                .corner_radius(theme.spacing.radius_md)
                                .inner_margin(egui::Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        vidya::status_dot(ui, &theme, session.alive);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&session.title)
                                                    .size(theme.type_scale.body)
                                                    .color(title_color),
                                            )
                                            .truncate(),
                                        );
                                    });
                                    ui.label(
                                        egui::RichText::new(ws_label)
                                            .size(theme.type_scale.caption)
                                            .color(dim_color),
                                    );
                                    if let Some(note) = &sub_note {
                                        ui.label(
                                            egui::RichText::new(note)
                                                .size(theme.type_scale.caption)
                                                .color(dim_color),
                                        );
                                    }
                                });

                            let response = row.response.interact(egui::Sense::click());
                            if response.clicked() {
                                select = Some(id);
                            }
                            response.context_menu(|ui| {
                                if ui.button("Auto-rename from content").clicked() {
                                    auto_rename = Some(id);
                                    ui.close_menu();
                                }
                                if ui.button("Rename…").clicked() {
                                    open_rename = Some(id);
                                    ui.close_menu();
                                }
                            });

                            // Nested subagent rows (informational; select parent).
                            if !session.subagents.is_empty() {
                                ui.add_space(2.0);
                                for sub in &session.subagents {
                                    let child = egui::Frame::NONE
                                        .inner_margin(egui::Margin {
                                            left: 18,
                                            right: 4,
                                            top: 2,
                                            bottom: 2,
                                        })
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                vidya::status_dot(
                                                    ui,
                                                    &theme,
                                                    sub.status.is_live(),
                                                );
                                                let label = subagent_label(sub);
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(label)
                                                            .size(theme.type_scale.caption)
                                                            .color(
                                                                theme.palette.text_secondary,
                                                            ),
                                                    )
                                                    .truncate(),
                                                );
                                            });
                                        });
                                    if child
                                        .response
                                        .interact(egui::Sense::click())
                                        .clicked()
                                    {
                                        select = Some(id);
                                    }
                                }
                            }

                            ui.add_space(theme.spacing.sm);
                        }
                    });
            });

        if let Some(id) = select {
            self.active = Some(id);
        }
        if let Some(id) = auto_rename {
            if let Some(session) = self.sessions.get_mut(&id) {
                session.auto_rename_from_content();
            }
        }
        if let Some(id) = open_rename {
            if let Some(session) = self.sessions.get(&id) {
                let mut draft = session.title.clone();
                if let Some(base) = draft.strip_suffix(" (exited)") {
                    draft = base.to_string();
                }
                self.rename_dialog = Some(RenameDialog {
                    id,
                    draft,
                    focus: true,
                });
            }
        }
    }

    fn show_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.rename_dialog.take() else {
            return;
        };
        if !self.sessions.contains_key(&dialog.id) {
            return;
        }

        let theme = self.theme.clone();
        let mut apply = false;
        let mut cancel = false;
        let mut keep_open = true;

        egui::Window::new("Rename tab")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(theme.card_frame())
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                vidya::title_2(ui, &theme, "Tab title");
                ui.add_space(theme.spacing.sm);
                let response = vidya::text_field_singleline(ui, &theme, &mut dialog.draft);
                if dialog.focus {
                    response.request_focus();
                    dialog.focus = false;
                }
                if response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !dialog.draft.trim().is_empty()
                {
                    apply = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!dialog.draft.trim().is_empty(), |ui| {
                        if vidya::primary_button(ui, &theme, "Rename").clicked() {
                            apply = true;
                        }
                    });
                    if vidya::button(ui, &theme, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel {
            keep_open = false;
        }
        if apply {
            let title = dialog.draft.trim().to_string();
            if let Some(session) = self.sessions.get_mut(&dialog.id) {
                session.set_user_title(title);
            }
            return;
        }
        if keep_open {
            self.rename_dialog = Some(dialog);
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
            if !self.spawn_from_draft(ctx, draft.clone()) {
                self.new_dialog = Some(draft);
            }
            return;
        }
        if keep_open {
            self.new_dialog = Some(draft);
        }
    }

    fn show_resume_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.resume_dialog.take() else {
            return;
        };

        let theme = self.theme.clone();
        let mut resume = false;
        let mut cancel = false;
        let mut refresh = false;
        let mut keep_open = true;
        let error = self.spawn_error.clone();
        let filtered: Vec<SavedChat> = dialog.filtered().into_iter().cloned().collect();
        let selected = dialog.selected.clone();

        egui::Window::new("Resume session")
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(theme.card_frame())
            .show(ctx, |ui| {
                ui.set_min_width(480.0);
                ui.set_min_height(360.0);
                vidya::title_2(ui, &theme, "Past cursor-agent chats");
                ui.add_space(theme.spacing.sm);
                ui.horizontal(|ui| {
                    vidya::dim_label(ui, &theme, "Filter");
                    ui.add_space(theme.spacing.sm);
                    let _ = vidya::text_field_singleline(ui, &theme, &mut dialog.filter);
                    if vidya::button(ui, &theme, "Refresh").clicked() {
                        refresh = true;
                    }
                });
                ui.add_space(theme.spacing.sm);

                if let Some(err) = &dialog.load_error {
                    ui.colored_label(theme.palette.destructive, err);
                    ui.add_space(theme.spacing.sm);
                }

                let list_height = (ui.available_height() - 140.0).max(160.0);
                egui::ScrollArea::vertical()
                    .max_height(list_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if filtered.is_empty() {
                            vidya::dim_label(ui, &theme, "No saved chats match.");
                            return;
                        }
                        for chat in &filtered {
                            let is_sel = selected.as_deref() == Some(chat.id.as_str());
                            let title = egui::RichText::new(&chat.title)
                                .size(theme.type_scale.body)
                                .color(if is_sel {
                                    theme.palette.accent_fg
                                } else {
                                    theme.palette.text
                                });
                            let meta = format!(
                                "{} · {} · {}",
                                chat.workspace_label(),
                                chat.age_label(),
                                &chat.id[..chat.id.len().min(8)]
                            );
                            let fill = if is_sel {
                                theme.palette.accent
                            } else {
                                theme.palette.popover_bg
                            };
                            let stroke = egui::Stroke::new(
                                1.0,
                                if is_sel {
                                    theme.palette.accent
                                } else {
                                    theme.palette.border_soft
                                },
                            );
                            let row = egui::Frame::NONE
                                .fill(fill)
                                .stroke(stroke)
                                .corner_radius(theme.spacing.radius_sm)
                                .inner_margin(egui::Margin::symmetric(
                                    theme.spacing.md as i8,
                                    theme.spacing.sm as i8,
                                ))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.vertical(|ui| {
                                        ui.label(title);
                                        ui.label(
                                            egui::RichText::new(meta)
                                                .size(theme.type_scale.caption)
                                                .color(if is_sel {
                                                    theme.palette.accent_fg
                                                } else {
                                                    theme.palette.text_secondary
                                                }),
                                        );
                                    });
                                });
                            if row.response.interact(egui::Sense::click()).clicked() {
                                dialog.selected = Some(chat.id.clone());
                            }
                            ui.add_space(theme.spacing.xs);
                        }
                    });

                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Model (optional)");
                vidya::text_field_singleline(ui, &theme, &mut dialog.model);
                ui.add_space(theme.spacing.sm);
                vidya::checkbox(ui, &theme, &mut dialog.trust, "Trust workspace (--trust)");
                vidya::checkbox(ui, &theme, &mut dialog.force, "Force / yolo (--force)");

                if let Some(err) = &error {
                    ui.add_space(theme.spacing.sm);
                    ui.colored_label(theme.palette.destructive, err);
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    let can_resume = dialog.selected.is_some();
                    ui.add_enabled_ui(can_resume, |ui| {
                        if vidya::primary_button(ui, &theme, "Resume").clicked() {
                            resume = true;
                        }
                    });
                    if vidya::button(ui, &theme, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel {
            keep_open = false;
            self.spawn_error = None;
        }
        if refresh {
            let filter = dialog.filter;
            let model = dialog.model;
            let trust = dialog.trust;
            let force = dialog.force;
            let selected = dialog.selected;
            let mut next = ResumeDialog::load();
            next.filter = filter;
            next.model = model;
            next.trust = trust;
            next.force = force;
            if selected
                .as_ref()
                .is_some_and(|id| next.chats.iter().any(|c| &c.id == id))
            {
                next.selected = selected;
            }
            self.resume_dialog = Some(next);
            self.spawn_error = None;
            return;
        }
        if resume {
            match dialog.to_draft() {
                Ok(draft) => {
                    if !self.spawn_from_draft(ctx, draft) {
                        self.resume_dialog = Some(dialog);
                    }
                    return;
                }
                Err(err) => {
                    self.spawn_error = Some(err);
                    self.resume_dialog = Some(dialog);
                    return;
                }
            }
        }
        if keep_open {
            self.resume_dialog = Some(dialog);
        }
    }

    fn show_central(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let term_theme = self.term_theme.clone();
        let active = self.active;
        // egui_term's set_focus(true) calls request_focus() every frame, which would
        // steal keys from dialogs (new session / resume filter fields).
        let term_focus = !self.dialog_open();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.palette.view_bg))
            .show(ctx, |ui| {
                if let Some(id) = active {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        let terminal = TerminalView::new(ui, &mut session.backend)
                            .set_focus(session.alive && term_focus)
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
                    ui.add_space(theme.spacing.xs);
                    vidya::dim_label(
                        ui,
                        &theme,
                        "Resume reopens a past chat from ~/.cursor/chats.",
                    );
                });
            });
    }

    /// Forward Ctrl/Cmd+V to the agent as `^V` so it can attach clipboard images.
    ///
    /// egui-winit treats Ctrl+V as a paste shortcut and only emits `Event::Paste` when the
    /// clipboard has text. Image-only clipboards therefore produce neither Paste nor a Key
    /// press — only the Key *release* survives. We send `^V` on that release (cursor-agent
    /// then reads the image via wl-paste/xclip). Text pastes are also deferred to release:
    /// we drop the egui Paste event so egui_term does not double-send `^V`.
    fn handle_agent_paste(&mut self, ctx: &egui::Context) {
        // Dialog fields need normal egui text paste.
        if self.dialog_open() {
            return;
        }

        let (steal_paste, send_caret_v) = ctx.input(|i| {
            let steal_paste = i.modifiers.command && !i.modifiers.shift;
            let send_caret_v = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command && !modifiers.shift && !modifiers.alt
                )
            });
            (steal_paste, send_caret_v)
        });

        if steal_paste {
            ctx.input_mut(|i| {
                i.events
                    .retain(|e| !matches!(e, egui::Event::Paste(_)));
            });
        }

        if !send_caret_v {
            return;
        }

        let Some(id) = self.active else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !session.alive {
            return;
        }

        session
            .backend
            .process_command(BackendCommand::Write(vec![0x16]));
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
        self.poll_subagents(ctx);
        // Before the terminal widget reads input, so we can steal Paste events.
        self.handle_agent_paste(ctx);
        self.show_header(ctx);
        self.show_sidebar(ctx);
        self.show_central(ctx);
        self.show_new_dialog(ctx);
        self.show_resume_dialog(ctx);
        self.show_rename_dialog(ctx);
    }
}

fn subagent_label(sub: &crate::subagents::Subagent) -> String {
    let status = match sub.status {
        SubagentStatus::Running => "…",
        SubagentStatus::Done => "✓",
        SubagentStatus::Failed => "!",
    };
    match &sub.kind {
        Some(kind) if !kind.is_empty() => {
            format!("{status} {kind} · {}", sub.title)
        }
        _ => format!("{status} {}", sub.title),
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
