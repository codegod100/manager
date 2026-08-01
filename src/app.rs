//! Main window: vidya chrome, session sidebar, embedded agent terminals.

use crate::session::{
    list_saved_chats, title_case_words, AgentSession, NewSessionDraft, PreparedSession,
    SavedChat,
};
use crate::subagents::{self, ChatSnapshot, SessionSummary, SubagentStatus, SummaryKind};
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui::{ColorImage, FontData, FontFamily, FontId, TextureHandle};
use egui_term::{
    BackendCommand, ColorPalette, FontSettings, PtyEvent, TerminalFont, TerminalTheme,
    TerminalView,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use vidya::Theme;

/// JetBrains Mono — includes `→` at the same advance as ASCII, unlike egui’s
/// default monospace + vidya’s proportional symbol fallback (which overflows
/// into the block-cursor cell on cursor-agent’s “→ Add a follow-up” line).
const TERM_FONT_TTF: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
const TERM_FONT_NAME: &str = "jetbrains-mono";
const TERM_FONT_SIZE: f32 = 14.0;

/// How often the background chat poller re-reads store/transcripts.
const CHAT_POLL_INTERVAL: Duration = Duration::from_millis(750);

struct ResumeDialog {
    filter: String,
    chats: Vec<SavedChat>,
    selected: Option<String>,
    model: String,
    trust: bool,
    force: bool,
    load_error: Option<String>,
    /// True while `list_saved_chats` runs off-thread.
    loading: bool,
}

struct RenameDialog {
    id: u64,
    draft: String,
    /// Request focus once when the dialog opens.
    focus: bool,
}

/// One-shot background prepare for New/Resume spawn.
struct PendingSpawn {
    /// Monotonic id so cancelled / superseded prepares are ignored.
    gen: u64,
    /// Draft kept so we can reopen the dialog on failure.
    draft: NewSessionDraft,
    from_resume: bool,
}

impl ResumeDialog {
    fn loading() -> Self {
        Self {
            filter: String::new(),
            chats: Vec::new(),
            selected: None,
            model: String::new(),
            trust: true,
            force: false,
            load_error: None,
            loading: true,
        }
    }

    fn apply_load(&mut self, result: Result<Vec<SavedChat>, String>) {
        self.loading = false;
        match result {
            Ok(chats) => {
                self.chats = chats;
                self.load_error = None;
            }
            Err(err) => {
                self.chats = Vec::new();
                self.load_error = Some(err);
            }
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
            images: Vec::new(),
            trust: self.trust,
            force: self.force,
            resume_chat_id: Some(chat.id.clone()),
            tab_title: Some(chat.title.clone()),
        })
    }
}

/// Session id + chat id + workspace for the background poller.
type ChatWatch = (u64, String, PathBuf);

pub struct App {
    theme: Theme,
    next_id: u64,
    sessions: BTreeMap<u64, AgentSession>,
    active: Option<u64>,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    /// Publish the set of chats to watch (background thread polls them).
    chat_watch_tx: Sender<Vec<ChatWatch>>,
    chat_snap_rx: Receiver<(u64, ChatSnapshot)>,
    last_chat_watch: Vec<ChatWatch>,
    new_dialog: Option<NewSessionDraft>,
    resume_dialog: Option<ResumeDialog>,
    rename_dialog: Option<RenameDialog>,
    spawn_error: Option<String>,
    pending_spawn: Option<PendingSpawn>,
    /// Bumped on each [`Self::begin_spawn`] / cancel so stale prepares are dropped.
    spawn_gen: u64,
    spawn_rx: Receiver<(u64, Result<PreparedSession, String>)>,
    spawn_tx: Sender<(u64, Result<PreparedSession, String>)>,
    resume_load_rx: Option<Receiver<Result<Vec<SavedChat>, String>>>,
    term_theme: TerminalTheme,
    term_font: TerminalFont,
    /// Thumbnails for images pasted into the new-session prompt.
    prompt_image_textures: BTreeMap<PathBuf, TextureHandle>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_term_font(&cc.egui_ctx);
        let (pty_tx, pty_rx) = mpsc::channel();
        let (chat_watch_tx, chat_watch_rx) = mpsc::channel::<Vec<ChatWatch>>();
        let (chat_snap_tx, chat_snap_rx) = mpsc::channel::<(u64, ChatSnapshot)>();
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let poll_ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("chat-poller".into())
            .spawn(move || chat_poller_loop(chat_watch_rx, chat_snap_tx, poll_ctx))
            .expect("spawn chat-poller thread");

        let mut theme = Theme::dark();
        theme.type_scale.caption = 13.0;

        Self {
            theme,
            next_id: 1,
            sessions: BTreeMap::new(),
            active: None,
            pty_tx,
            pty_rx,
            chat_watch_tx,
            chat_snap_rx,
            last_chat_watch: Vec::new(),
            new_dialog: None,
            resume_dialog: None,
            rename_dialog: None,
            spawn_error: None,
            pending_spawn: None,
            spawn_gen: 0,
            spawn_rx,
            spawn_tx,
            resume_load_rx: None,
            term_theme: vidya_term_theme(),
            term_font: TerminalFont::new(FontSettings {
                font_type: term_font_id(),
            }),
            prompt_image_textures: BTreeMap::new(),
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
                PtyEvent::Wakeup => {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        if session.alive {
                            session.bump_activity();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Non-blocking: publish watches + apply snapshots from the poller thread.
    fn poll_subagents(&mut self, ctx: &egui::Context) {
        while let Ok((id, snap)) = self.chat_snap_rx.try_recv() {
            if let Some(session) = self.sessions.get_mut(&id) {
                let _ = session.apply_chat_snapshot(snap);
            }
        }

        let watches: Vec<ChatWatch> = self
            .sessions
            .iter()
            .filter(|(_, s)| !s.chat_id.is_empty())
            .map(|(id, s)| (*id, s.chat_id.clone(), s.workspace.clone()))
            .collect();
        if watches != self.last_chat_watch {
            let _ = self.chat_watch_tx.send(watches.clone());
            self.last_chat_watch = watches;
        }

        if !self.sessions.is_empty() {
            // Keep the UI alive so try_recv runs; poller also request_repaint's.
            ctx.request_repaint_after(CHAT_POLL_INTERVAL);
        }
    }

    fn start_resume_load(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let load_ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(list_saved_chats());
            load_ctx.request_repaint();
        });
        self.resume_load_rx = Some(rx);
    }

    fn drain_resume_load(&mut self) {
        let Some(rx) = &self.resume_load_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.resume_load_rx = None;
        if let Some(dialog) = self.resume_dialog.as_mut() {
            let selected = dialog.selected.clone();
            dialog.apply_load(result);
            if selected
                .as_ref()
                .is_some_and(|id| dialog.chats.iter().any(|c| &c.id == id))
            {
                dialog.selected = selected;
            } else {
                dialog.selected = None;
            }
        }
    }

    fn begin_spawn(&mut self, ctx: &egui::Context, draft: NewSessionDraft, from_resume: bool) {
        self.spawn_error = None;
        self.spawn_gen = self.spawn_gen.wrapping_add(1);
        let gen = self.spawn_gen;
        self.pending_spawn = Some(PendingSpawn {
            gen,
            draft: draft.clone(),
            from_resume,
        });
        let tx = self.spawn_tx.clone();
        let prep_ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((gen, AgentSession::prepare(&draft)));
            prep_ctx.request_repaint();
        });
    }

    fn cancel_pending_spawn(&mut self) {
        self.pending_spawn = None;
        self.spawn_gen = self.spawn_gen.wrapping_add(1);
    }

    fn drain_pending_spawn(&mut self, ctx: &egui::Context) {
        loop {
            let Ok((gen, result)) = self.spawn_rx.try_recv() else {
                return;
            };
            let Some(pending) = self.pending_spawn.take() else {
                // Cancelled — ignore.
                continue;
            };
            if pending.gen != gen {
                // Stale prepare; keep waiting for the current one.
                self.pending_spawn = Some(pending);
                continue;
            }

            match result {
                Ok(prepared) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    match AgentSession::spawn_prepared(
                        id,
                        ctx.clone(),
                        self.pty_tx.clone(),
                        prepared,
                    ) {
                        Ok(session) => {
                            self.sessions.insert(id, session);
                            self.active = Some(id);
                            self.new_dialog = None;
                            self.resume_dialog = None;
                            self.resume_load_rx = None;
                            self.spawn_error = None;
                            self.prompt_image_textures.clear();
                        }
                        Err(err) => {
                            self.spawn_error = Some(err);
                            self.restore_spawn_dialog(pending);
                        }
                    }
                }
                Err(err) => {
                    self.spawn_error = Some(err);
                    self.restore_spawn_dialog(pending);
                }
            }
            return;
        }
    }

    /// Push deferred new-session images/prompt into live agent composers.
    fn progress_composer_seeds(&mut self, ctx: &egui::Context) {
        let mut soonest: Option<Duration> = None;
        for session in self.sessions.values_mut() {
            if let Some(wait) = session.progress_composer_seed() {
                soonest = Some(match soonest {
                    Some(prev) => prev.min(wait),
                    None => wait,
                });
            }
        }
        if let Some(wait) = soonest {
            ctx.request_repaint_after(wait);
        }
    }

    fn restore_spawn_dialog(&mut self, pending: PendingSpawn) {
        if pending.from_resume {
            // Resume dialog is usually still open; keep spawn_error visible there.
        } else {
            self.new_dialog = Some(pending.draft);
        }
    }

    fn close_session(&mut self, id: u64) {
        self.sessions.remove(&id);
        if self.active == Some(id) {
            self.active = self
                .sessions
                .range(..id)
                .next_back()
                .map(|(k, _)| *k)
                .or_else(|| self.sessions.keys().next().copied());
        }
    }

    /// Focus an already-open Task tab, or spawn `--resume` for its chat id.
    fn open_or_focus_task(
        &mut self,
        ctx: &egui::Context,
        workspace: PathBuf,
        chat_id: String,
        title: String,
    ) {
        if let Some((id, _)) = self
            .sessions
            .iter()
            .find(|(_, s)| s.chat_id == chat_id)
        {
            self.active = Some(*id);
            return;
        }
        if self.pending_spawn.is_some() {
            return;
        }
        let draft = NewSessionDraft {
            workspace: workspace.display().to_string(),
            model: String::new(),
            prompt: String::new(),
            images: Vec::new(),
            trust: true,
            force: false,
            resume_chat_id: Some(chat_id),
            tab_title: Some(title),
        };
        self.begin_spawn(ctx, draft, true);
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
        let busy = self.pending_spawn.is_some();

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
                    ui.add_enabled_ui(!busy, |ui| {
                        if vidya::button(ui, &theme, "Resume").clicked() {
                            open_resume = true;
                        }
                    });
                    ui.add_space(theme.spacing.sm);
                    ui.add_enabled_ui(!busy, |ui| {
                        if vidya::primary_button(ui, &theme, "New session").clicked() {
                            open_new = true;
                        }
                    });
                });
            });
        });

        if open_new {
            let mut draft = NewSessionDraft::default();
            if let Some(ws) = self
                .active
                .and_then(|id| self.sessions.get(&id))
                .map(|s| s.workspace.display().to_string())
            {
                draft.workspace = ws;
            }
            self.new_dialog = Some(draft);
            self.resume_dialog = None;
            self.resume_load_rx = None;
            self.spawn_error = None;
        }
        if open_resume {
            self.resume_dialog = Some(ResumeDialog::loading());
            self.new_dialog = None;
            self.spawn_error = None;
            self.start_resume_load(ctx);
        }
    }

    fn show_sidebar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        // Sessions grouped by workspace path (BTreeMap → stable path order).
        let mut by_workspace: BTreeMap<PathBuf, Vec<u64>> = BTreeMap::new();
        for (id, session) in &self.sessions {
            by_workspace
                .entry(session.workspace.clone())
                .or_default()
                .push(*id);
        }
        let groups: Vec<(PathBuf, Vec<u64>)> = by_workspace.into_iter().collect();
        let mut select: Option<u64> = None;
        let mut close: Option<u64> = None;
        let mut auto_rename: Option<u64> = None;
        let mut open_rename: Option<u64> = None;
        let mut toggle_tasks_fold: Option<u64> = None;
        let mut open_task: Option<(PathBuf, String, String)> = None;

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

                if groups.is_empty() {
                    vidya::dim_label(ui, &theme, "No sessions yet.");
                    ui.add_space(theme.spacing.xs);
                    vidya::dim_label(ui, &theme, "New session or Resume to start.");
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (workspace, ids) in &groups {
                            let ws_label = workspace
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("workspace");
                            ui.label(
                                egui::RichText::new(ws_label)
                                    .size(theme.type_scale.caption)
                                    .strong()
                                    .color(theme.palette.text_secondary),
                            )
                            .on_hover_text(workspace.display().to_string());
                            ui.add_space(theme.spacing.xs);

                            for &id in ids {
                                let Some(session) = self.sessions.get(&id) else {
                                    continue;
                                };
                                let active = self.active == Some(id);
                                let running_tasks = session
                                    .subagents
                                    .iter()
                                    .filter(|s| s.status.is_live())
                                    .count();
                                let failed_tasks = session
                                    .subagents
                                    .iter()
                                    .filter(|s| s.status == SubagentStatus::Failed)
                                    .count();
                                let done_tasks = session
                                    .subagents
                                    .iter()
                                    .filter(|s| s.status == SubagentStatus::Done)
                                    .count();
                                let tasks_folded = session.tasks_folded;
                                let task_note = if session.subagents.is_empty() {
                                    None
                                } else {
                                    let chevron = if tasks_folded { "▶" } else { "▼" };
                                    let highlight = session
                                        .subagents
                                        .iter()
                                        .find(|s| s.status.is_live())
                                        .or_else(|| {
                                            session.subagents.iter().find(|s| {
                                                s.status == SubagentStatus::Failed
                                            })
                                        })
                                        .or_else(|| session.subagents.first())
                                        .map(|s| s.title.as_str());
                                    let body = task_header_label(
                                        running_tasks,
                                        failed_tasks,
                                        done_tasks,
                                        highlight,
                                    );
                                    Some(format!("{chevron} {body}"))
                                };

                                let fill = if active {
                                    theme.palette.accent
                                } else {
                                    theme.palette.button_bg
                                };
                                let stroke = egui::Stroke::new(
                                    1.0_f32,
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

                                ui.push_id(id, |ui| {
                                    let mut close_clicked = false;
                                    // Full Frame hit target: nested with_layout shrink-wraps to
                                    // title content, so short titles left dead zones on the row.
                                    // × stays preferred via egui’s thinner-widget hit testing.
                                    let row = egui::Frame::NONE
                                        .fill(fill)
                                        .stroke(stroke)
                                        .corner_radius(theme.spacing.radius_md)
                                        .inner_margin(egui::Margin::symmetric(8, 6))
                                        .show(ui, |ui| {
                                            ui.set_min_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        let x = ui
                                                            .add(
                                                                egui::Button::new(
                                                                    egui::RichText::new("×")
                                                                        .size(
                                                                            theme.type_scale.body,
                                                                        )
                                                                        .color(dim_color),
                                                                )
                                                                .frame(false)
                                                                .min_size(egui::vec2(18.0, 18.0)),
                                                            )
                                                            .on_hover_text("Close");
                                                        if x.clicked() {
                                                            close_clicked = true;
                                                        }

                                                        ui.with_layout(
                                                            egui::Layout::left_to_right(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                // Status before the truncating
                                                                // title — otherwise the title
                                                                // eats the row and hides `?`.
                                                                if session.needs_input() {
                                                                    let tip = session
                                                                        .activity
                                                                        .as_deref()
                                                                        .unwrap_or(
                                                                            "Waiting for answer",
                                                                        );
                                                                    ui.add(
                                                                        egui::Label::new(
                                                                            egui::RichText::new(
                                                                                "?",
                                                                            )
                                                                            .size(
                                                                                theme
                                                                                    .type_scale
                                                                                    .body,
                                                                            )
                                                                            .strong()
                                                                            .color(
                                                                                theme
                                                                                    .palette
                                                                                    .warning,
                                                                            ),
                                                                        )
                                                                        .selectable(false),
                                                                    )
                                                                    .on_hover_text(tip);
                                                                } else if session.has_activity() {
                                                                    let size = (theme
                                                                        .type_scale
                                                                        .body
                                                                        * 0.7)
                                                                        .clamp(10.0, 14.0);
                                                                    let color = if active {
                                                                        theme.palette.accent_fg
                                                                    } else {
                                                                        theme.palette.accent
                                                                    };
                                                                    ui.add(
                                                                        egui::Spinner::new()
                                                                            .size(size)
                                                                            .color(color),
                                                                    )
                                                                    .on_hover_text(
                                                                        session
                                                                            .activity
                                                                            .as_deref()
                                                                            .unwrap_or(
                                                                                "Working…",
                                                                            ),
                                                                    );
                                                                } else if !session.alive {
                                                                    vidya::status_dot(
                                                                        ui, &theme, false,
                                                                    )
                                                                    .on_hover_text("Exited");
                                                                }
                                                                // selectable(false): egui’s
                                                                // default selectable labels
                                                                // take click sense and steal
                                                                // selection from the parent.
                                                                ui.add(
                                                                    egui::Label::new(
                                                                        egui::RichText::new(
                                                                            &session.title,
                                                                        )
                                                                        .size(
                                                                            theme.type_scale.body,
                                                                        )
                                                                        .color(title_color),
                                                                    )
                                                                    .truncate()
                                                                    .selectable(false),
                                                                );
                                                            },
                                                        );
                                                    },
                                                );
                                            });
                                        });

                                    let response = row.response.interact(egui::Sense::click());
                                    if close_clicked {
                                        close = Some(id);
                                    } else if response.clicked() {
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
                                        if ui.button("Close").clicked() {
                                            close = Some(id);
                                            ui.close_menu();
                                        }
                                    });

                                    // Nested Task rows — click opens the subagent chat when
                                    // Cursor created a resumable `isSubagent` session.
                                    let tasks: Vec<_> = session.subagents.clone();
                                    let parent_workspace = session.workspace.clone();
                                    if let Some(note) = &task_note {
                                        let fold = egui::Frame::NONE
                                            .inner_margin(egui::Margin {
                                                left: 4,
                                                right: 4,
                                                top: 2,
                                                bottom: 0,
                                            })
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Button::new(
                                                        egui::RichText::new(note)
                                                            .size(theme.type_scale.caption)
                                                            .color(
                                                                theme.palette.text_secondary,
                                                            ),
                                                    )
                                                    .frame(false),
                                                )
                                                .on_hover_text(if tasks_folded {
                                                    "Expand tasks"
                                                } else {
                                                    "Fold tasks"
                                                })
                                            })
                                            .inner;
                                        if fold.clicked() {
                                            toggle_tasks_fold = Some(id);
                                        }
                                        if !tasks_folded {
                                            ui.add_space(2.0);
                                            // Live + failed first (already sorted); cap long
                                            // completed tails so the sidebar stays scannable.
                                            let mut shown_done = 0usize;
                                            const MAX_DONE_ROWS: usize = 3;
                                            let total_done = tasks
                                                .iter()
                                                .filter(|s| s.status == SubagentStatus::Done)
                                                .count();
                                            for sub in &tasks {
                                                if sub.status == SubagentStatus::Done {
                                                    if shown_done >= MAX_DONE_ROWS {
                                                        continue;
                                                    }
                                                    shown_done += 1;
                                                }
                                                let label = task_label(sub);
                                                let tip = task_hover(sub);
                                                let can_open = sub.chat_id.is_some();
                                                let mut clicked = false;
                                                let mut hovered = false;
                                                egui::Frame::NONE
                                                    .inner_margin(egui::Margin {
                                                        left: 14,
                                                        right: 4,
                                                        top: 1,
                                                        bottom: 1,
                                                    })
                                                    .show(ui, |ui| {
                                                        ui.horizontal(|ui| {
                                                            vidya::status_dot(
                                                                ui,
                                                                &theme,
                                                                sub.status.is_live(),
                                                            );
                                                            let btn = ui
                                                                .add(
                                                                    egui::Button::new(
                                                                        egui::RichText::new(
                                                                            label,
                                                                        )
                                                                        .size(
                                                                            theme
                                                                                .type_scale
                                                                                .caption,
                                                                        )
                                                                        .color(
                                                                            theme
                                                                                .palette
                                                                                .text_secondary,
                                                                        ),
                                                                    )
                                                                    .frame(false)
                                                                    .wrap_mode(
                                                                        egui::TextWrapMode::Truncate,
                                                                    ),
                                                                )
                                                                .on_hover_text(tip);
                                                            clicked = btn.clicked();
                                                            hovered = btn.hovered();
                                                        });
                                                    });
                                                if can_open && hovered {
                                                    ui.ctx().set_cursor_icon(
                                                        egui::CursorIcon::PointingHand,
                                                    );
                                                }
                                                if clicked {
                                                    if let Some(chat_id) = sub.chat_id.clone() {
                                                        open_task = Some((
                                                            parent_workspace.clone(),
                                                            chat_id,
                                                            sub.title.clone(),
                                                        ));
                                                    } else {
                                                        select = Some(id);
                                                    }
                                                }
                                            }
                                            if total_done > MAX_DONE_ROWS {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(format!(
                                                            "    +{} more completed",
                                                            total_done - MAX_DONE_ROWS
                                                        ))
                                                        .size(theme.type_scale.caption)
                                                        .color(theme.palette.text_secondary),
                                                    )
                                                    .selectable(false),
                                                );
                                            }
                                        }
                                    }

                                    ui.add_space(theme.spacing.sm);
                                });
                            }

                            ui.add_space(theme.spacing.xs);
                        }
                    });
            });

        if let Some(id) = select {
            self.active = Some(id);
        }
        if let Some(id) = close {
            self.close_session(id);
        }
        if let Some(id) = toggle_tasks_fold {
            if let Some(session) = self.sessions.get_mut(&id) {
                session.tasks_folded = !session.tasks_folded;
            }
        }
        if let Some((workspace, chat_id, title)) = open_task {
            self.open_or_focus_task(ctx, workspace, chat_id, title);
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

        vidya::dialog("Rename tab", &theme)
            .default_width(360.0)
            .min_width(280.0)
            .show(ctx, |ui| {
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
            let title = title_case_words(dialog.draft.trim());
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

        vidya::dialog("New session", &theme)
            .default_size([420.0, 360.0])
            .min_width(320.0)
            .min_height(280.0)
            .show(ctx, |ui| {
                vidya::title_2(ui, &theme, "Launch cursor-agent");
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Workspace");
                vidya::text_field_singleline(ui, &theme, &mut draft.workspace);
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Model (optional)");
                vidya::text_field_singleline(ui, &theme, &mut draft.model);
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Initial prompt (optional)");
                // Leave room for checkboxes + error + footer; grow prompt with the window.
                let below = theme.spacing.control_height * 3.0
                    + theme.spacing.md * 2.0
                    + theme.spacing.sm * 3.0
                    + if draft.images.is_empty() { 0.0 } else { 72.0 }
                    + 28.0;
                let prompt_h = (ui.available_height() - below).max(theme.spacing.control_height * 3.0);
                let rows = ((prompt_h / (theme.type_scale.body * 1.5)).floor() as usize).max(3);
                vidya::text_field_multiline(ui, &theme, &mut draft.prompt, rows);
                if draft.images.is_empty() {
                    vidya::dim_label(ui, &theme, "Ctrl+V pastes a clipboard image");
                } else {
                    ui.add_space(theme.spacing.sm);
                    self.show_prompt_images(ui, &theme, &mut draft.images);
                }
                ui.add_space(theme.spacing.sm);
                vidya::checkbox(ui, &theme, &mut draft.trust, "Trust workspace (--trust)");
                vidya::checkbox(ui, &theme, &mut draft.force, "Force / yolo (--force)");

                if let Some(err) = &error {
                    ui.add_space(theme.spacing.sm);
                    ui.colored_label(theme.palette.destructive, err);
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    let busy = self.pending_spawn.is_some();
                    ui.add_enabled_ui(!busy, |ui| {
                        if vidya::primary_button(ui, &theme, if busy { "Starting…" } else { "Spawn" })
                            .clicked()
                        {
                            spawn = true;
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
            self.cancel_pending_spawn();
            for path in draft.images.drain(..) {
                self.prompt_image_textures.remove(&path);
                let _ = std::fs::remove_file(&path);
            }
        }
        if spawn {
            self.begin_spawn(ctx, draft.clone(), false);
            self.new_dialog = Some(draft);
            return;
        }
        if keep_open || self.pending_spawn.is_some() {
            self.new_dialog = Some(draft);
        } else {
            for path in draft.images.drain(..) {
                self.prompt_image_textures.remove(&path);
                let _ = std::fs::remove_file(&path);
            }
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

        // Fresh id so a previously auto-grown size in egui memory is not reused.
        vidya::dialog("Resume session", &theme)
            .id(egui::Id::new("manager_resume_dialog"))
            .default_size([400.0, 320.0])
            .min_width(300.0)
            .min_height(240.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    vidya::dim_label(ui, &theme, "Filter");
                    ui.add_space(theme.spacing.sm);
                    // Button first (RTL) so the text field's desired_width(available)
                    // does not steal the button's space and auto-grow the window.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if vidya::button(ui, &theme, "↻")
                            .on_hover_text("Refresh")
                            .clicked()
                        {
                            refresh = true;
                        }
                        let _ = vidya::text_field_singleline(ui, &theme, &mut dialog.filter);
                    });
                });
                ui.add_space(theme.spacing.xs);

                if dialog.loading {
                    vidya::dim_label(ui, &theme, "Loading saved chats…");
                    ui.add_space(theme.spacing.xs);
                }

                if let Some(err) = &dialog.load_error {
                    ui.colored_label(theme.palette.destructive, err);
                    ui.add_space(theme.spacing.xs);
                }

                // Model + checkboxes + buttons (+ optional error) below the list.
                let footer_reserve = theme.spacing.control_height * 3.0
                    + theme.spacing.sm * 4.0
                    + theme.spacing.xs * 2.0
                    + 28.0;
                let list_h = (ui.available_height() - footer_reserve).max(80.0);
                egui::ScrollArea::vertical()
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if dialog.loading {
                            return;
                        }
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
                                "{} · {}",
                                chat.workspace_label(),
                                chat.age_label(),
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
                                    theme.spacing.sm as i8,
                                    theme.spacing.xs as i8,
                                ))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    // Cap row height: with_layout alone claims the
                                    // ScrollArea's full available_height, stretching
                                    // the first item to fill the list.
                                    let row_size = egui::vec2(
                                        ui.available_width(),
                                        ui.spacing().interact_size.y,
                                    );
                                    ui.allocate_ui_with_layout(
                                        row_size,
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(meta)
                                                        .size(theme.type_scale.caption)
                                                        .color(if is_sel {
                                                            theme.palette.accent_fg
                                                        } else {
                                                            theme.palette.text_secondary
                                                        }),
                                                )
                                                .truncate(),
                                            );
                                            ui.with_layout(
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.add(egui::Label::new(title).truncate());
                                                },
                                            );
                                        },
                                    );
                                });
                            if row.response.interact(egui::Sense::click()).clicked() {
                                dialog.selected = Some(chat.id.clone());
                            }
                        }
                    });

                ui.add_space(theme.spacing.xs);
                ui.horizontal(|ui| {
                    vidya::dim_label(ui, &theme, "Model");
                    ui.add_space(theme.spacing.sm);
                    vidya::text_field_singleline(ui, &theme, &mut dialog.model);
                });
                ui.horizontal(|ui| {
                    vidya::checkbox(ui, &theme, &mut dialog.trust, "Trust");
                    ui.add_space(theme.spacing.sm);
                    vidya::checkbox(ui, &theme, &mut dialog.force, "Force");
                });

                if let Some(err) = &error {
                    ui.add_space(theme.spacing.xs);
                    ui.colored_label(theme.palette.destructive, err);
                }

                ui.add_space(theme.spacing.sm);
                ui.horizontal(|ui| {
                    let busy = self.pending_spawn.is_some();
                    let can_resume =
                        dialog.selected.is_some() && !dialog.loading && !busy;
                    ui.add_enabled_ui(can_resume, |ui| {
                        if vidya::primary_button(
                            ui,
                            &theme,
                            if busy { "Starting…" } else { "Resume" },
                        )
                        .clicked()
                        {
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
            self.resume_load_rx = None;
            self.cancel_pending_spawn();
        }
        if refresh {
            dialog.loading = true;
            dialog.load_error = None;
            self.resume_dialog = Some(dialog);
            self.spawn_error = None;
            self.start_resume_load(ctx);
            return;
        }
        if resume {
            match dialog.to_draft() {
                Ok(draft) => {
                    self.resume_dialog = Some(dialog);
                    self.begin_spawn(ctx, draft, true);
                    return;
                }
                Err(err) => {
                    self.spawn_error = Some(err);
                    self.resume_dialog = Some(dialog);
                    return;
                }
            }
        }
        if keep_open || self.pending_spawn.is_some() {
            self.resume_dialog = Some(dialog);
        }
    }

    fn show_central(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let term_theme = self.term_theme.clone();
        let term_font = self.term_font.clone();
        let active = self.active;
        // egui_term's set_focus(true) calls request_focus() every frame, which would
        // steal keys from dialogs (new session / resume filter fields).
        let term_focus = !self.dialog_open();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.palette.view_bg))
            .show(ctx, |ui| {
                if let Some(id) = active {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        // CentralPanel is already top-down — don't wrap in ui.vertical /
                        // ui.horizontal (those can shrink-wrap and collapse the PTY).
                        show_summary_panel(ui, &theme, session);

                        let scroll = ui.spacing().scroll;
                        let gap = ui.spacing().item_spacing.x;
                        let bar_w = scroll.bar_width
                            + scroll.bar_inner_margin
                            + scroll.bar_outer_margin
                            + gap;
                        let avail = ui.available_size();
                        let term_size =
                            egui::vec2((avail.x - bar_w).max(1.0), avail.y.max(1.0));

                        ui.allocate_ui_with_layout(
                            avail,
                            egui::Layout::left_to_right(egui::Align::Min),
                            |ui| {
                                let terminal =
                                    TerminalView::new(ui, &mut session.backend)
                                        .set_focus(session.alive && term_focus)
                                        .set_font(term_font.clone())
                                        .set_theme(term_theme.clone())
                                        .set_size(term_size);
                                let term_response = ui.add(terminal);
                                // egui_term ignores CSI ? 25 l and always paints the
                                // grid cursor. cursor-agent hides the real caret and
                                // draws its own, leaving a rogue block at the bottom.
                                cover_hidden_pty_cursor(
                                    ui,
                                    term_response.rect,
                                    &session.backend,
                                    &term_theme,
                                );
                                show_term_scrollbar(
                                    ui,
                                    &mut session.backend,
                                    id,
                                    term_size.y,
                                );
                            },
                        );
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
        // Dialog fields need normal egui text paste (images handled separately).
        if self.dialog_open() {
            return;
        }
        // Don't steal paste while we're injecting the new-session seed ourselves.
        if self
            .active
            .and_then(|id| self.sessions.get(&id))
            .is_some_and(|s| s.has_pending_composer_seed())
        {
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

    /// Ctrl/Cmd+V in the new-session dialog: attach clipboard images to the initial prompt.
    ///
    /// Same Key-release quirk as [`Self::handle_agent_paste`]: image-only clipboards never
    /// produce `Event::Paste`. On a successful image capture we drop Paste so text fields
    /// are not also filled; text-only clipboards are left alone for normal egui paste.
    fn handle_dialog_image_paste(&mut self, ctx: &egui::Context) {
        if self.new_dialog.is_none() {
            return;
        }

        let paste_key = ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command && !modifiers.shift && !modifiers.alt
                )
            })
        });
        if !paste_key {
            return;
        }

        let Some(path) = crate::clipboard::capture_clipboard_image() else {
            return;
        };

        ctx.input_mut(|i| {
            i.events
                .retain(|e| !matches!(e, egui::Event::Paste(_)));
        });

        if let Some(draft) = self.new_dialog.as_mut() {
            draft.images.push(path);
        }
    }

    fn show_prompt_images(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        images: &mut Vec<PathBuf>,
    ) {
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (idx, path) in images.iter().enumerate() {
                ui.group(|ui| {
                    ui.set_min_height(56.0);
                    ui.horizontal(|ui| {
                        if let Some(tex) = self.prompt_image_texture(ui.ctx(), path) {
                            let max = 48.0;
                            let size = tex.size_vec2();
                            let scale = (max / size.x).min(max / size.y).min(1.0);
                            ui.add(
                                egui::Image::new((tex.id(), size * scale))
                                    .maintain_aspect_ratio(true),
                            );
                        } else {
                            vidya::dim_label(
                                ui,
                                theme,
                                path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("image"),
                            );
                        }
                        if ui
                            .add(
                                egui::Button::new("×")
                                    .min_size(egui::vec2(theme.spacing.control_height * 0.6, theme.spacing.control_height * 0.6)),
                            )
                            .on_hover_text("Remove image")
                            .clicked()
                        {
                            remove = Some(idx);
                        }
                    });
                });
            }
        });
        if let Some(idx) = remove {
            let path = images.remove(idx);
            self.prompt_image_textures.remove(&path);
            let _ = std::fs::remove_file(&path);
        }
    }

    fn prompt_image_texture(
        &mut self,
        ctx: &egui::Context,
        path: &Path,
    ) -> Option<TextureHandle> {
        if let Some(tex) = self.prompt_image_textures.get(path) {
            return Some(tex.clone());
        }
        let bytes = std::fs::read(path).ok()?;
        let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let color = ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        let name = format!("prompt-img-{}", path.display());
        let tex = ctx.load_texture(name, color, egui::TextureOptions::LINEAR);
        self.prompt_image_textures
            .insert(path.to_path_buf(), tex.clone());
        Some(tex)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        vidya::apply(ctx, &self.theme);

        if ctx.input(|i| i.viewport().close_requested()) {
            self.sessions.clear();
            self.active = None;
            let _ = self.chat_watch_tx.send(Vec::new());
            self.last_chat_watch.clear();
            self.cancel_pending_spawn();
        }

        self.poll_pty_events();
        self.drain_resume_load();
        self.drain_pending_spawn(ctx);
        self.poll_subagents(ctx);
        self.progress_composer_seeds(ctx);
        // Before widgets read input, so we can steal Paste events / attach images.
        self.handle_dialog_image_paste(ctx);
        self.handle_agent_paste(ctx);
        self.show_header(ctx);
        self.show_sidebar(ctx);
        self.show_central(ctx);
        self.show_new_dialog(ctx);
        self.show_resume_dialog(ctx);
        self.show_rename_dialog(ctx);
    }
}

fn task_header_label(
    running: usize,
    failed: usize,
    done: usize,
    first_title: Option<&str>,
) -> String {
    if running > 0 {
        if running == 1 {
            if let Some(title) = first_title.filter(|t| !t.is_empty()) {
                return truncate_ui(title, 36);
            }
        }
        let mut parts = vec![format!("{running} running")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if done > 0 {
            parts.push(format!("{done} done"));
        }
        return parts.join(" · ");
    }
    if failed > 0 {
        let mut parts = vec![format!("{failed} failed")];
        if done > 0 {
            parts.push(format!("{done} done"));
        }
        return parts.join(" · ");
    }
    if done == 1 {
        if let Some(title) = first_title.filter(|t| !t.is_empty()) {
            return truncate_ui(title, 36);
        }
    }
    format!("{done} done")
}

fn task_label(sub: &crate::subagents::Subagent) -> String {
    // Status is already shown via the live/idle dot — keep the row to the
    // actionable description (kind only when it adds signal).
    match &sub.kind {
        Some(kind) if !kind.is_empty() && kind != "generalPurpose" => {
            format!("{} · {}", sub.title, short_kind(kind))
        }
        _ => sub.title.clone(),
    }
}

fn task_hover(sub: &crate::subagents::Subagent) -> String {
    let status = match sub.status {
        SubagentStatus::Running => "running",
        SubagentStatus::Done => "done",
        SubagentStatus::Failed => "failed",
    };
    let head = match &sub.kind {
        Some(kind) if !kind.is_empty() => format!("{status} · {kind}"),
        _ => status.to_string(),
    };
    if sub.chat_id.is_some() {
        format!("{head}\n{}\nClick to open", sub.title)
    } else {
        format!("{head}\n{}", sub.title)
    }
}

fn short_kind(kind: &str) -> &str {
    match kind {
        "best-of-n-runner" => "best-of-n",
        "security-review" => "security",
        "cursor-guide" => "guide",
        "generalPurpose" => "agent",
        other => other,
    }
}

fn truncate_ui(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Multi-line session summary above the terminal — goal, work items, closing note.
fn show_summary_panel(ui: &mut egui::Ui, theme: &Theme, session: &crate::session::AgentSession) {
    let live = session.has_activity();
    let waiting = session.needs_input();
    let status = if !session.alive {
        "Exited"
    } else if waiting {
        session
            .activity
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Waiting for input")
    } else if live {
        session
            .activity
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Working…")
    } else {
        "Idle"
    };

    let summary = session.summary.as_ref().filter(|s| !s.is_empty());
    let accent = if live {
        theme.palette.accent
    } else if waiting {
        theme.palette.warning
    } else {
        theme.palette.border
    };

    let max_h = (ui.available_height() * 0.38).clamp(96.0, 220.0);

    egui::Frame::NONE
        .fill(theme.palette.card_bg)
        .stroke(egui::Stroke::new(1.0_f32, theme.palette.border_soft))
        .inner_margin(egui::Margin::symmetric(
            theme.spacing.md as i8,
            theme.spacing.sm as i8,
        ))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_max_height(max_h);

            // Accent rule — marks this as session chrome, not a toast.
            let (rule, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 2.0),
                egui::Sense::hover(),
            );
            ui.painter()
                .rect_filled(rule, theme.spacing.radius_sm, accent);
            ui.add_space(theme.spacing.sm);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = theme.spacing.sm;
                vidya::status_dot(ui, theme, live || (waiting && session.alive));
                ui.label(
                    egui::RichText::new(status)
                        .size(theme.type_scale.caption)
                        .strong()
                        .color(if live {
                            theme.palette.accent
                        } else if waiting && session.alive {
                            theme.palette.warning
                        } else {
                            theme.palette.text_secondary
                        }),
                );
                if let Some(ws) = session.workspace.file_name().and_then(|n| n.to_str()) {
                    ui.label(
                        egui::RichText::new("·")
                            .size(theme.type_scale.caption)
                            .color(theme.palette.text_disabled),
                    );
                    ui.label(
                        egui::RichText::new(ws)
                            .size(theme.type_scale.caption)
                            .color(theme.palette.text_disabled),
                    );
                }
            });

            match summary {
                Some(summary) => paint_summary_body(ui, theme, summary),
                None => {
                    ui.add_space(theme.spacing.xs);
                    ui.label(
                        egui::RichText::new(if session.alive {
                            "Session just started — work will show up here."
                        } else {
                            "No summary for this session."
                        })
                        .size(theme.type_scale.body)
                        .italics()
                        .color(theme.palette.text_secondary),
                    );
                }
            }
        });
}

fn paint_summary_body(ui: &mut egui::Ui, theme: &Theme, summary: &SessionSummary) {
    if let Some(goal) = summary.goal.as_deref() {
        ui.add_space(theme.spacing.sm);
        ui.add(
            egui::Label::new(
                egui::RichText::new(goal)
                    .size(theme.type_scale.title_2)
                    .strong()
                    .color(theme.palette.text),
            )
            .wrap(),
        );
    }

    if !summary.lines.is_empty() {
        ui.add_space(theme.spacing.sm);
        let label_w = measure_kind_column(ui, theme, &summary.lines);
        for line in &summary.lines {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = theme.spacing.md;
                let kind_color = kind_color(theme, line.kind);
                ui.add_sized(
                    egui::vec2(label_w, theme.type_scale.body),
                    egui::Label::new(
                        egui::RichText::new(line.kind.label())
                            .size(theme.type_scale.caption)
                            .strong()
                            .color(kind_color),
                    ),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&line.text)
                            .size(theme.type_scale.body)
                            .color(theme.palette.text),
                    )
                    .wrap(),
                );
            });
        }
    }

    if let Some(note) = summary.note.as_deref() {
        ui.add_space(theme.spacing.sm);
        ui.add(
            egui::Label::new(
                egui::RichText::new(note)
                    .size(theme.type_scale.body)
                    .italics()
                    .color(theme.palette.text_secondary),
            )
            .wrap(),
        );
    }
}

fn measure_kind_column(
    ui: &egui::Ui,
    theme: &Theme,
    lines: &[crate::subagents::SummaryLine],
) -> f32 {
    let mut w = 0.0_f32;
    for line in lines {
        let galley = ui.fonts(|f| {
            f.layout_no_wrap(
                line.kind.label().to_string(),
                egui::FontId::proportional(theme.type_scale.caption),
                theme.palette.text,
            )
        });
        w = w.max(galley.size().x);
    }
    (w + 4.0).clamp(52.0, 88.0)
}

fn kind_color(theme: &Theme, kind: SummaryKind) -> egui::Color32 {
    match kind {
        SummaryKind::Edited => theme.palette.accent,
        SummaryKind::Ran => theme.palette.success,
        SummaryKind::Searched => theme.palette.warning,
        SummaryKind::Read => theme.palette.text_secondary,
        SummaryKind::Delegated => theme.palette.accent_hover,
        SummaryKind::Other => theme.palette.text_secondary,
    }
}

fn chat_poller_loop(
    watch_rx: Receiver<Vec<ChatWatch>>,
    snap_tx: Sender<(u64, ChatSnapshot)>,
    ctx: egui::Context,
) {
    let mut watches: Vec<ChatWatch> = Vec::new();
    loop {
        if watches.is_empty() {
            // Sleep until the UI publishes something to watch.
            match watch_rx.recv() {
                Ok(next) => watches = next,
                Err(_) => return,
            }
        } else {
            match watch_rx.recv_timeout(CHAT_POLL_INTERVAL) {
                // Watch list changed (e.g. new session) — fall through and poll
                // immediately instead of waiting another full interval.
                Ok(next) => watches = next,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(next) = watch_rx.try_recv() {
            watches = next;
        }
        if watches.is_empty() {
            continue;
        }

        for (id, chat_id, workspace) in &watches {
            let snap = subagents::poll_chat(chat_id, Some(workspace.as_path()));
            if snap_tx.send((*id, snap)).is_err() {
                return;
            }
        }
        ctx.request_repaint();
    }
}

fn term_font_id() -> FontId {
    FontId::new(
        TERM_FONT_SIZE,
        FontFamily::Name(TERM_FONT_NAME.into()),
    )
}

/// Paint over the PTY cursor cell when the app has hidden it (`TermMode::SHOW_CURSOR`
/// cleared via CSI `?25l`). egui_term always draws that cell as a solid block.
fn cover_hidden_pty_cursor(
    ui: &mut egui::Ui,
    term_rect: egui::Rect,
    backend: &egui_term::TerminalBackend,
    theme: &TerminalTheme,
) {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    let content = backend.last_content();
    if content
        .terminal_mode
        .contains(egui_term::TerminalMode::SHOW_CURSOR)
    {
        return;
    }

    let cell_w = content.terminal_size.cell_width as f32;
    let cell_h = content.terminal_size.cell_height as f32;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    let point = content.grid.cursor.point;
    let line_num = point.line.0 + content.grid.display_offset() as i32;
    if line_num < 0 {
        return;
    }

    // Match egui_term's cursor fill (+1.0 on each axis) so the block is fully covered.
    let cell = egui::Rect::from_min_size(
        egui::pos2(
            term_rect.min.x + cell_w * point.column.0 as f32,
            term_rect.min.y + cell_h * line_num as f32,
        ),
        egui::vec2(cell_w + 1.0, cell_h + 1.0),
    );
    if !term_rect.intersects(cell) {
        return;
    }

    ui.painter().rect_filled(
        cell,
        0.0,
        theme.get_color(Color::Named(NamedColor::Background)),
    );
}

/// Scrollback scrollbar for an egui_term PTY (egui_term has no built-in bar).
///
/// Alacritty's `display_offset` is 0 at the bottom (live edge) and `history`
/// at the top of scrollback. Positive [`BackendCommand::Scroll`] moves up.
fn show_term_scrollbar(
    ui: &mut egui::Ui,
    backend: &mut egui_term::TerminalBackend,
    session_id: u64,
    height: f32,
) {
    use alacritty_terminal::grid::Dimensions;

    // TerminalView already synced this frame; avoid a second term lock.
    let (history, display_offset, screen_lines, alt_screen) = {
        let content = backend.last_content();
        (
            content.grid.history_size(),
            content.grid.display_offset(),
            content.grid.screen_lines().max(1),
            content
                .terminal_mode
                .contains(egui_term::TerminalMode::ALT_SCREEN),
        )
    };

    let scroll = ui.spacing().scroll;
    let width = scroll.bar_width;
    ui.add_space(scroll.bar_outer_margin);
    let id = ui.id().with(("term_scrollbar", session_id));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
    ui.add_space(scroll.bar_inner_margin);

    let visuals = ui.visuals();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);

    // Alternate screen (full-screen TUI) has no scrollback to show.
    if alt_screen {
        return;
    }

    let (thumb_h, from_top) = if history == 0 {
        (height, 0.0)
    } else {
        let total = (history + screen_lines) as f32;
        let thumb_h =
            (height * (screen_lines as f32 / total)).max(scroll.handle_min_length);
        // 0 = top of history, 1 = live bottom.
        let from_top = (history - display_offset) as f32 / history as f32;
        (thumb_h, from_top)
    };
    let movable = (height - thumb_h).max(1.0);
    let thumb_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, rect.min.y + from_top * movable),
        egui::vec2(width, thumb_h),
    );

    let thumb_fill = if response.dragged() {
        visuals.widgets.active.bg_fill
    } else if response.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.widgets.inactive.bg_fill
    };
    painter.rect_filled(thumb_rect, width * 0.45, thumb_fill);

    if history == 0 {
        return;
    }

    let mut scroll_delta = 0_i32;

    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let rel = ((pos.y - rect.min.y - thumb_h * 0.5) / movable).clamp(0.0, 1.0);
            let desired_from_top = (rel * history as f32).round() as usize;
            let desired_offset = history.saturating_sub(desired_from_top.min(history));
            scroll_delta = desired_offset as i32 - display_offset as i32;
        }
    } else if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if pos.y < thumb_rect.min.y {
                scroll_delta = screen_lines as i32;
            } else if pos.y > thumb_rect.max.y {
                scroll_delta = -(screen_lines as i32);
            }
        }
    }

    if response.hovered() {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel.abs() > f32::EPSILON {
            // Match egui_term wheel: positive wheel → look newer (decrease offset).
            let lines = (wheel / TERM_FONT_SIZE).round() as i32;
            if lines != 0 {
                scroll_delta += -lines;
            }
        }
    }

    if scroll_delta != 0 {
        backend.process_command(BackendCommand::Scroll(scroll_delta));
        ui.ctx().request_repaint();
    }
}

/// Register JetBrains Mono under its own family so egui_term never picks up
/// vidya’s proportional `→` fallback on `FontFamily::Monospace`.
fn install_term_font(ctx: &egui::Context) {
    ctx.add_font(FontInsert::new(
        TERM_FONT_NAME,
        FontData::from_static(TERM_FONT_TTF),
        vec![InsertFontFamily {
            family: FontFamily::Name(TERM_FONT_NAME.into()),
            priority: FontPriority::Highest,
        }],
    ));
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
