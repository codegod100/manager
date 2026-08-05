//! Main window: vidya chrome, session sidebar, embedded agent terminals.

use crate::auth::{
    complete_oidc_login, cursor_auth_status_label, fetch_cursor_api_key, has_cursor_api_key,
    load_bao_token, resolve_bao_addr, restore_cursor_auth,
};
use crate::oidc::{start_oidc_login, OidcLogin, OidcLoginConfig, OidcLoginEvent};
use crate::session::{
    kill_pid, list_saved_chats, title_case_words, AgentSession, NewSessionDraft, PreparedSession,
    SavedChat,
};
use crate::subagents::{self, ChatSnapshot, SubagentStatus};
use alacritty_terminal::selection::SelectionType;
use egui::text::{LayoutJob, TextFormat};
use egui::{ColorImage, FontData, FontDefinitions, FontFamily, FontId, TextureHandle};
use egui_term::{
    BackendCommand, Binding, BindingAction, ColorPalette, FontSettings, InputKind, PtyEvent,
    TerminalBackend, TerminalFont, TerminalMode, TerminalTheme, TerminalView,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use vidya::Theme;

/// JetBrains Mono — includes `→` at the same advance as ASCII, unlike egui’s
/// default monospace + vidya’s proportional symbol fallback (which overflows
/// into the block-cursor cell on cursor-agent’s “→ Add a follow-up” line).
const TERM_FONT_TTF: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
const TERM_FONT_NAME: &str = "jetbrains-mono";
const TERM_FONT_SIZE: f32 = 14.0;

/// DejaVu braille + Noto `⌕` — glyphs JetBrains + egui defaults lack
/// (cursor-agent spinner / Find icon). See `assets/NOTICE` /
/// `scripts/rebuild-term-symbols-ttf.sh`.
const TERM_SYMBOLS_TTF: &[u8] = include_bytes!("../assets/term-symbols.ttf");
const TERM_SYMBOLS_NAME: &str = "term-symbols";

/// How often the background chat poller re-reads store/transcripts.
const CHAT_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// How long to wait for `PtyEvent::Exit` after killing a child before giving up
/// and `mem::forget`ing the backend (avoids egui_term's Shutdown busy-spin).
const PTY_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Backend held after tab close until the PTY child exit delivers `Event::Exit`.
struct DrainingPty {
    id: u64,
    backend: TerminalBackend,
    since: Instant,
}

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

/// Split bash → Nushell util dialog.
struct ConvertDialog {
    bash: String,
    nushell: String,
    error: Option<String>,
    /// True while cursor-agent `--print` convert runs off-thread.
    converting: bool,
    /// Request focus on the bash field once when opened.
    focus: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMethod {
    Token,
    Oidc,
}

/// OpenBao OIDC / token sign-in for `CURSOR_API_KEY`.
struct AuthDialog {
    address: String,
    bao_token: String,
    show_token_field: bool,
    auth_method: AuthMethod,
    oidc_mount: String,
    oidc_role: String,
    oidc_login: Option<OidcLogin>,
    oidc_auth_url: String,
    status: String,
}

impl Default for ConvertDialog {
    fn default() -> Self {
        Self {
            bash: String::new(),
            nushell: String::new(),
            error: None,
            converting: false,
            focus: true,
        }
    }
}

/// One successful bash → Nushell conversion (newest first in [`App::convert_history`]).
#[derive(Clone)]
struct ConvertSnippet {
    bash: String,
    nushell: String,
}

const CONVERT_HISTORY_MAX: usize = 40;

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
    convert_dialog: Option<ConvertDialog>,
    spawn_error: Option<String>,
    pending_spawn: Option<PendingSpawn>,
    /// Bumped on each [`Self::begin_spawn`] / cancel so stale prepares are dropped.
    spawn_gen: u64,
    spawn_rx: Receiver<(u64, Result<PreparedSession, String>)>,
    spawn_tx: Sender<(u64, Result<PreparedSession, String>)>,
    resume_load_rx: Option<Receiver<Result<Vec<SavedChat>, String>>>,
    /// Bumped when starting / cancelling a bash→nu convert so stale results drop.
    convert_gen: u64,
    convert_rx: Receiver<(u64, Result<String, String>)>,
    convert_tx: Sender<(u64, Result<String, String>)>,
    /// Session-local convert history (newest first); survives dialog close.
    convert_history: Vec<ConvertSnippet>,
    term_theme: TerminalTheme,
    term_font: TerminalFont,
    /// Thumbnails for images pasted into the new-session prompt.
    prompt_image_textures: BTreeMap<PathBuf, TextureHandle>,
    /// Closed-but-not-yet-dropped backends waiting for `PtyEvent::Exit`.
    draining: Vec<DrainingPty>,
    /// Workspace paths whose session lists are folded in the sidebar.
    workspace_folded: BTreeSet<PathBuf>,
    auth_dialog: Option<AuthDialog>,
    /// One-shot restore from `~/.bao-token` on first frame.
    pending_auto_auth: bool,
    cursor_auth_label: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_term_font(&cc.egui_ctx);
        let (pty_tx, pty_rx) = mpsc::channel();
        let (chat_watch_tx, chat_watch_rx) = mpsc::channel::<Vec<ChatWatch>>();
        let (chat_snap_tx, chat_snap_rx) = mpsc::channel::<(u64, ChatSnapshot)>();
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let (convert_tx, convert_rx) = mpsc::channel();
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
            convert_dialog: None,
            spawn_error: None,
            pending_spawn: None,
            spawn_gen: 0,
            spawn_rx,
            spawn_tx,
            resume_load_rx: None,
            convert_gen: 0,
            convert_rx,
            convert_tx,
            convert_history: Vec::new(),
            term_theme: vidya_term_theme(),
            term_font: TerminalFont::new(FontSettings {
                font_type: term_font_id(),
            }),
            prompt_image_textures: BTreeMap::new(),
            draining: Vec::new(),
            workspace_folded: BTreeSet::new(),
            auth_dialog: None,
            pending_auto_auth: true,
            cursor_auth_label: cursor_auth_status_label(),
        }
    }

    fn dialog_open(&self) -> bool {
        self.new_dialog.is_some()
            || self.resume_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.convert_dialog.is_some()
            || self.auth_dialog.is_some()
    }

    fn open_auth_dialog(&mut self) {
        let oidc_defaults = OidcLoginConfig::from_env_defaults();
        let stored = load_bao_token();
        self.auth_dialog = Some(AuthDialog {
            address: resolve_bao_addr(),
            bao_token: stored.clone(),
            show_token_field: stored.is_empty(),
            auth_method: AuthMethod::Oidc,
            oidc_mount: oidc_defaults.mount,
            oidc_role: oidc_defaults.role,
            oidc_login: None,
            oidc_auth_url: String::new(),
            status: if has_cursor_api_key() {
                "CURSOR_API_KEY is already set in the environment.".into()
            } else {
                String::new()
            },
        });
    }

    fn cancel_oidc_login(dialog: &mut AuthDialog) {
        if let Some(login) = dialog.oidc_login.take() {
            login.cancel();
        }
        dialog.oidc_auth_url.clear();
    }

    fn start_oidc_login(&mut self) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        let address = dialog.address.clone();
        let mount = dialog.oidc_mount.clone();
        let role = dialog.oidc_role.clone();
        Self::cancel_oidc_login(dialog);
        let mut cfg = OidcLoginConfig::from_env_defaults();
        cfg.address = address;
        cfg.mount = mount;
        cfg.role = role;
        match start_oidc_login(cfg) {
            Ok(login) => {
                dialog.oidc_login = Some(login);
                dialog.status = "Waiting for OIDC login in browser…".into();
            }
            Err(e) => dialog.status = e,
        }
    }

    fn finish_bao_token_auth(&mut self, bao_token: String) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        let address = dialog.address.clone();
        dialog.bao_token = bao_token.clone();
        dialog.status = "Fetching Cursor API key…".into();
        match complete_oidc_login(&address, &bao_token) {
            Ok(()) => {
                self.cursor_auth_label = cursor_auth_status_label();
                dialog.status = "Cursor API key loaded.".into();
                dialog.show_token_field = false;
                dialog.oidc_login = None;
                dialog.oidc_auth_url.clear();
            }
            Err(e) => {
                dialog.status = e.to_string();
                dialog.show_token_field = true;
            }
        }
    }

    fn try_token_auth(&mut self) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        let address = dialog.address.trim().to_string();
        let token = dialog.bao_token.trim().to_string();
        if address.is_empty() {
            dialog.status = "Server address is required.".into();
            return;
        }
        if token.is_empty() {
            dialog.status = "OpenBao token is required.".into();
            dialog.show_token_field = true;
            return;
        }
        dialog.status = "Fetching Cursor API key…".into();
        match fetch_cursor_api_key(&address, &token) {
            Ok(cursor_key) => {
                crate::auth::apply_cursor_api_key(&cursor_key);
                if let Err(e) = crate::auth::save_bao_token(&token) {
                    dialog.status = format!("Key loaded but token save failed: {e}");
                } else {
                    dialog.status = "Cursor API key loaded.".into();
                }
                self.cursor_auth_label = cursor_auth_status_label();
                dialog.show_token_field = false;
            }
            Err(e) => {
                dialog.status = e.to_string();
                dialog.show_token_field = true;
            }
        }
    }

    fn poll_oidc_login(&mut self, ctx: &egui::Context) {
        let event = self
            .auth_dialog
            .as_ref()
            .and_then(|d| d.oidc_login.as_ref())
            .and_then(|login| login.try_recv());
        let Some(event) = event else {
            return;
        };
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        match event {
            OidcLoginEvent::Ready {
                auth_url,
                browser_error,
            } => {
                dialog.oidc_auth_url = auth_url;
                dialog.status = if let Some(err) = browser_error {
                    format!("Open this URL manually:\n{err}")
                } else {
                    "Complete login in your browser, then return here…".into()
                };
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            OidcLoginEvent::Success { token } => {
                dialog.oidc_login = None;
                dialog.oidc_auth_url.clear();
                self.finish_bao_token_auth(token);
            }
            OidcLoginEvent::Failed(msg) => {
                dialog.oidc_login = None;
                dialog.oidc_auth_url.clear();
                dialog.status = msg;
            }
        }
    }

    fn try_restore_cursor_auth(&mut self) {
        match restore_cursor_auth() {
            Ok(()) => self.cursor_auth_label = cursor_auth_status_label(),
            Err(_) => self.cursor_auth_label = cursor_auth_status_label(),
        }
    }

    fn poll_pty_events(&mut self, ctx: &egui::Context) {
        while let Ok((id, event)) = self.pty_rx.try_recv() {
            match event {
                PtyEvent::Exit => {
                    // Subscriber thread has exited; safe to Drop the backend now.
                    self.finish_draining(id);
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
        self.poll_draining_timeouts(ctx);
    }

    fn finish_draining(&mut self, id: u64) {
        self.draining.retain(|d| d.id != id);
    }

    fn poll_draining_timeouts(&mut self, ctx: &egui::Context) {
        let mut i = 0;
        while i < self.draining.len() {
            if self.draining[i].since.elapsed() >= PTY_DRAIN_TIMEOUT {
                let drained = self.draining.swap_remove(i);
                // Exit never arrived — leak the backend so Drop's Shutdown cannot
                // busy-spin egui_term's subscriber thread.
                std::mem::forget(drained.backend);
            } else {
                i += 1;
            }
        }
        if !self.draining.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
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

    fn begin_convert(&mut self, ctx: &egui::Context, bash: String) {
        self.convert_gen = self.convert_gen.wrapping_add(1);
        let gen = self.convert_gen;
        if let Some(dialog) = self.convert_dialog.as_mut() {
            dialog.converting = true;
            dialog.error = None;
        }
        let tx = self.convert_tx.clone();
        let convert_ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((gen, crate::convert::bash_to_nushell(&bash)));
            convert_ctx.request_repaint();
        });
    }

    fn cancel_convert(&mut self) {
        self.convert_gen = self.convert_gen.wrapping_add(1);
        if let Some(dialog) = self.convert_dialog.as_mut() {
            dialog.converting = false;
        }
    }

    fn drain_convert(&mut self) {
        loop {
            let Ok((gen, result)) = self.convert_rx.try_recv() else {
                return;
            };
            if gen != self.convert_gen {
                continue;
            }
            if self.convert_dialog.is_none() {
                continue;
            }
            let push = {
                let dialog = self.convert_dialog.as_mut().unwrap();
                dialog.converting = false;
                match result {
                    Ok(nushell) => {
                        let bash = dialog.bash.clone();
                        dialog.nushell = nushell.clone();
                        dialog.error = None;
                        Some((bash, nushell))
                    }
                    Err(err) => {
                        dialog.error = Some(err);
                        None
                    }
                }
            };
            if let Some((bash, nushell)) = push {
                self.push_convert_history(bash, nushell);
            }
        }
    }

    fn push_convert_history(&mut self, bash: String, nushell: String) {
        let bash = bash.trim().to_string();
        let nushell = nushell.trim().to_string();
        if bash.is_empty() || nushell.is_empty() {
            return;
        }
        self.convert_history
            .retain(|s| s.bash.trim() != bash || s.nushell.trim() != nushell);
        self.convert_history.insert(
            0,
            ConvertSnippet {
                bash,
                nushell,
            },
        );
        if self.convert_history.len() > CONVERT_HISTORY_MAX {
            self.convert_history.truncate(CONVERT_HISTORY_MAX);
        }
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
        if let Some(session) = self.sessions.remove(&id) {
            if session.alive {
                // Kill child first so alacritty emits Exit and egui_term's
                // subscriber breaks cleanly; only then Drop the backend.
                if let Some(pid) = session.child_pid {
                    kill_pid(pid);
                }
                self.draining.push(DrainingPty {
                    id,
                    backend: session.backend,
                    since: Instant::now(),
                });
            }
            // Already dead: subscriber already exited on Exit; Drop is safe.
        }
        if self.active == Some(id) {
            self.active = self
                .sessions
                .range(..id)
                .next_back()
                .map(|(k, _)| *k)
                .or_else(|| self.sessions.keys().next().copied());
        }
    }

    /// Tear down all PTYs without triggering egui_term Shutdown busy-spins.
    fn shutdown_all_sessions(&mut self) {
        let sessions = std::mem::take(&mut self.sessions);
        for (_, session) in sessions {
            if let Some(pid) = session.child_pid {
                kill_pid(pid);
            }
            // Forget backends: process is exiting; don't Drop → Shutdown race.
            std::mem::forget(session.backend);
        }
        for drained in self.draining.drain(..) {
            std::mem::forget(drained.backend);
        }
        self.active = None;
        let _ = self.chat_watch_tx.send(Vec::new());
        self.last_chat_watch.clear();
        self.cancel_pending_spawn();
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
        let mut open_convert = false;
        let mut open_auth = false;
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
                    ui.add_space(theme.spacing.sm);
                    let auth_label = if has_cursor_api_key() {
                        self.cursor_auth_label.clone()
                    } else {
                        "Cursor: sign in".to_string()
                    };
                    if vidya::button(ui, &theme, &auth_label).clicked() {
                        open_auth = true;
                    }
                    ui.add_space(theme.spacing.sm);
                    let utils_text = egui::RichText::new("Utils")
                        .size(theme.type_scale.body)
                        .color(theme.palette.button_fg);
                    ui.menu_button(utils_text, |ui| {
                        ui.set_min_width(180.0);
                        if ui.button("Cursor sign-in (OIDC)…").clicked() {
                            open_auth = true;
                            ui.close_menu();
                        }
                        if ui.button("Convert to nushell").clicked() {
                            open_convert = true;
                            ui.close_menu();
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
            self.convert_dialog = None;
            self.spawn_error = None;
        }
        if open_resume {
            self.resume_dialog = Some(ResumeDialog::loading());
            self.new_dialog = None;
            self.convert_dialog = None;
            self.spawn_error = None;
            self.start_resume_load(ctx);
        }
        if open_convert {
            self.convert_dialog = Some(ConvertDialog::default());
            self.new_dialog = None;
            self.resume_dialog = None;
            self.resume_load_rx = None;
            self.spawn_error = None;
        }
        if open_auth {
            self.open_auth_dialog();
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
        let mut toggle_workspace_fold: Option<PathBuf> = None;
        let mut open_task: Option<(PathBuf, String, String)> = None;

        egui::SidePanel::left("agents")
            .resizable(true)
            .default_width(260.0)
            .width_range(200.0..=400.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme.palette.view_bg)
                    .stroke(egui::Stroke::new(1.0_f32, theme.palette.border_soft))
                    .inner_margin(egui::Margin::symmetric(10, 10)),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
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
                            let ws_folded = self.workspace_folded.contains(workspace);
                            let chevron = if ws_folded { "▶" } else { "▼" };
                            let header = format!("{chevron} {ws_label}");
                            let fold = ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(header)
                                            .size(theme.type_scale.caption)
                                            .strong()
                                            .color(theme.palette.text_secondary),
                                    )
                                    .frame(false),
                                )
                                .on_hover_text(if ws_folded {
                                    format!(
                                        "Expand sessions\n{}",
                                        workspace.display()
                                    )
                                } else {
                                    format!(
                                        "Fold sessions\n{}",
                                        workspace.display()
                                    )
                                });
                            if fold.clicked() {
                                toggle_workspace_fold = Some(workspace.clone());
                            }
                            ui.add_space(theme.spacing.xs);

                            if ws_folded {
                                ui.add_space(theme.spacing.xs);
                                continue;
                            }

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
                                    // Frame::end allocates *after* children, so a full-row
                                    // Sense::click on the Frame sits on top of × and steals
                                    // the hit (egui only prefers thinner targets when they
                                    // aren't fully contained). Sense select on a rect that
                                    // excludes × instead.
                                    let mut close_rect = None;
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
                                                        close_rect = Some(x.rect);
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

                                    let mut select_rect = row.response.rect;
                                    if let Some(xr) = close_rect {
                                        select_rect.max.x = select_rect.max.x.min(xr.min.x);
                                    }
                                    let response = ui.interact(
                                        select_rect,
                                        ui.id().with("session_select"),
                                        egui::Sense::click(),
                                    );
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
        if let Some(workspace) = toggle_workspace_fold {
            if !self.workspace_folded.remove(&workspace) {
                self.workspace_folded.insert(workspace);
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

    fn show_convert_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.convert_dialog.take() else {
            return;
        };

        let theme = self.theme.clone();
        let mut convert = false;
        let mut close = false;
        let mut copy = false;
        let mut clear_history = false;
        let mut restore: Option<usize> = None;
        let mut keep_open = true;
        let converting = dialog.converting;
        let can_convert = !dialog.bash.trim().is_empty() && !converting;
        let can_copy = !dialog.nushell.trim().is_empty();
        let history = self.convert_history.clone();

        vidya::dialog("Convert to nushell", &theme)
            .id(egui::Id::new("manager_convert_dialog"))
            .default_size([720.0, 560.0])
            .min_width(480.0)
            .min_height(360.0)
            .show(ctx, |ui| {
                vidya::dim_label(
                    ui,
                    &theme,
                    "Paste bash on the left; Convert runs cursor-agent (ask mode).",
                );
                ui.add_space(theme.spacing.sm);

                let footer = theme.spacing.control_height
                    + theme.spacing.md
                    + if dialog.error.is_some() {
                        theme.type_scale.body + theme.spacing.sm
                    } else {
                        0.0
                    };
                let history_h = 140.0;
                let history_block = theme.type_scale.title_2
                    + theme.spacing.xs
                    + theme.spacing.sm
                    + history_h;
                let body_h = (ui.available_height() - footer - history_block)
                    .max(theme.spacing.control_height * 4.0);
                let rows = ((body_h / (theme.type_scale.body * 1.5)).floor() as usize).max(4);
                let min_col = (ui.available_width() * 0.35).clamp(180.0, 280.0);

                vidya::two_col(
                    ui,
                    &theme,
                    min_col,
                    |ui| {
                        ui.horizontal(|ui| {
                            vidya::title_2(ui, &theme, "Bash");
                        });
                        ui.add_space(theme.spacing.xs);
                        let resp = vidya::text_field_multiline(ui, &theme, &mut dialog.bash, rows);
                        if dialog.focus {
                            resp.request_focus();
                            dialog.focus = false;
                        }
                    },
                    |ui| {
                        ui.horizontal(|ui| {
                            vidya::title_2(ui, &theme, "Nushell");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if vidya::icon_button(ui, &theme, vidya::Icon::Copy, "Copy Nushell")
                                    .clicked()
                                    && can_copy
                                {
                                    copy = true;
                                }
                            });
                        });
                        ui.add_space(theme.spacing.xs);
                        ui.add_enabled_ui(!converting, |ui| {
                            vidya::text_field_multiline(ui, &theme, &mut dialog.nushell, rows);
                        });
                    },
                );

                ui.add_space(theme.spacing.sm);
                ui.horizontal(|ui| {
                    vidya::title_2(ui, &theme, "History");
                    if !history.is_empty() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if vidya::button(ui, &theme, "Clear").clicked() {
                                clear_history = true;
                            }
                        });
                    }
                });
                ui.add_space(theme.spacing.xs);
                egui::ScrollArea::vertical()
                    .id_salt("convert_history")
                    .max_height(history_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if history.is_empty() {
                            vidya::dim_label(ui, &theme, "Converted snippets show up here.");
                            return;
                        }
                        for (idx, snippet) in history.iter().enumerate() {
                            let bash_preview = first_line_preview(&snippet.bash, 48);
                            let nu_preview = first_line_preview(&snippet.nushell, 48);
                            let selected = dialog.bash.trim() == snippet.bash.trim()
                                && dialog.nushell.trim() == snippet.nushell.trim();
                            let label = format!("{bash_preview}  →  {nu_preview}");
                            let text = egui::RichText::new(label)
                                .size(theme.type_scale.caption)
                                .color(if selected {
                                    theme.palette.accent
                                } else {
                                    theme.palette.text
                                });
                            let resp = ui
                                .add(
                                    egui::Label::new(text)
                                        .sense(egui::Sense::click())
                                        .truncate(),
                                )
                                .on_hover_text(format!(
                                    "Bash:\n{}\n\nNushell:\n{}",
                                    snippet.bash, snippet.nushell
                                ));
                            if resp.clicked() && !converting {
                                restore = Some(idx);
                            }
                        }
                    });

                if let Some(err) = &dialog.error {
                    ui.add_space(theme.spacing.sm);
                    ui.colored_label(theme.palette.destructive, err);
                }

                if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !converting {
                    close = true;
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(can_convert, |ui| {
                        let label = if converting {
                            "Converting…"
                        } else {
                            "Convert"
                        };
                        if vidya::primary_button(ui, &theme, label).clicked() {
                            convert = true;
                        }
                    });
                    ui.add_enabled_ui(can_copy, |ui| {
                        if vidya::button(ui, &theme, "Copy").clicked() {
                            copy = true;
                        }
                    });
                    if vidya::button(ui, &theme, if converting { "Cancel" } else { "Close" })
                        .clicked()
                    {
                        close = true;
                    }
                });
            });

        if clear_history {
            self.convert_history.clear();
        }
        if let Some(idx) = restore {
            if let Some(snippet) = self.convert_history.get(idx).cloned() {
                dialog.bash = snippet.bash;
                dialog.nushell = snippet.nushell;
                dialog.error = None;
            }
        }
        if copy {
            ctx.copy_text(dialog.nushell.clone());
        }
        if convert {
            let bash = dialog.bash.clone();
            self.convert_dialog = Some(dialog);
            self.begin_convert(ctx, bash);
            return;
        }
        if close {
            self.cancel_convert();
            keep_open = false;
        }
        if keep_open {
            self.convert_dialog = Some(dialog);
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
                                1.0_f32,
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
                                // Drive selection ourselves before TerminalView paints.
                                // egui_term skips SelectStart when the PTY has MOUSE_MODE
                                // (Ink/cursor-agent), so drag-select would otherwise no-op.
                                let term_origin = ui.cursor().min;
                                let term_rect =
                                    egui::Rect::from_min_size(term_origin, term_size);
                                drive_term_selection(ui, session, term_rect);

                                // PageUp/Down scroll scrollback (egui_term only
                                // forwards those keys as CSI to the PTY).
                                let alt_screen = session
                                    .backend
                                    .last_content()
                                    .terminal_mode
                                    .contains(TerminalMode::ALT_SCREEN);
                                if session.alive && term_focus {
                                    handle_term_page_scroll(
                                        ui,
                                        &mut session.backend,
                                        alt_screen,
                                    );
                                }

                                let mut terminal =
                                    TerminalView::new(ui, &mut session.backend)
                                        .set_focus(session.alive && term_focus)
                                        .set_font(term_font.clone())
                                        .set_theme(term_theme.clone())
                                        .set_size(term_size);
                                // Don't also send PageUp/Down to the PTY while
                                // we own them for scrollback (primary screen).
                                if !alt_screen {
                                    terminal = terminal
                                        .add_bindings(term_page_scroll_bindings());
                                }
                                let term_response = ui.add(terminal);
                                auto_copy_term_selection(ui, session, &term_response);
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

    fn show_auth_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.auth_dialog.take() else {
            return;
        };

        let theme = self.theme.clone();
        let mut close = false;
        let mut start_oidc = false;
        let mut use_token = false;
        let oidc_busy = dialog.oidc_login.is_some();

        vidya::dialog("Cursor sign-in", &theme)
            .default_size([440.0, 420.0])
            .min_width(360.0)
            .show(ctx, |ui| {
                vidya::title_2(ui, &theme, "OpenBao → Cursor API key");
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(
                    ui,
                    &theme,
                    "OIDC login to OpenBao, then read CURSOR_API_KEY from secret/data/ai-api-keys.",
                );
                ui.add_space(theme.spacing.md);
                vidya::dim_label(ui, &theme, "OpenBao address");
                vidya::text_field_singleline(ui, &theme, &mut dialog.address);
                ui.add_space(theme.spacing.sm);
                vidya::dim_label(ui, &theme, "Auth method");
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(dialog.auth_method == AuthMethod::Token, "Token")
                        .clicked()
                    {
                        Self::cancel_oidc_login(&mut dialog);
                        dialog.auth_method = AuthMethod::Token;
                    }
                    if ui
                        .selectable_label(dialog.auth_method == AuthMethod::Oidc, "OIDC")
                        .clicked()
                    {
                        dialog.auth_method = AuthMethod::Oidc;
                    }
                });

                match dialog.auth_method {
                    AuthMethod::Token => {
                        if dialog.show_token_field {
                            ui.add_space(theme.spacing.sm);
                            vidya::dim_label(ui, &theme, "OpenBao token");
                            let tw = ui.available_width().max(1.0);
                            ui.add(
                                egui::TextEdit::singleline(&mut dialog.bao_token)
                                    .password(true)
                                    .desired_width(tw),
                            );
                        } else if !dialog.bao_token.is_empty() {
                            ui.add_space(theme.spacing.sm);
                            vidya::dim_label(ui, &theme, "Using stored OpenBao token");
                            if vidya::button(ui, &theme, "Use a different token").clicked() {
                                dialog.show_token_field = true;
                                dialog.bao_token.clear();
                            }
                        }
                    }
                    AuthMethod::Oidc => {
                        ui.add_space(theme.spacing.sm);
                        vidya::dim_label(ui, &theme, "OIDC mount");
                        vidya::text_field_singleline(ui, &theme, &mut dialog.oidc_mount);
                        ui.add_space(theme.spacing.sm);
                        vidya::dim_label(ui, &theme, "Role (optional)");
                        vidya::text_field_singleline(ui, &theme, &mut dialog.oidc_role);
                        if !dialog.oidc_auth_url.is_empty() {
                            ui.add_space(theme.spacing.sm);
                            vidya::dim_label(ui, &theme, "Authorization URL");
                            let tw = ui.available_width().max(1.0);
                            ui.add(
                                egui::TextEdit::multiline(&mut dialog.oidc_auth_url)
                                    .desired_width(tw)
                                    .desired_rows(3),
                            );
                        }
                    }
                }

                if !dialog.status.is_empty() {
                    ui.add_space(theme.spacing.sm);
                    ui.label(&dialog.status);
                }

                ui.add_space(theme.spacing.md);
                ui.horizontal(|ui| {
                    match dialog.auth_method {
                        AuthMethod::Oidc => {
                            ui.add_enabled_ui(!oidc_busy, |ui| {
                                let label = if oidc_busy {
                                    "Waiting for browser…"
                                } else {
                                    "Sign in with OIDC"
                                };
                                if vidya::primary_button(ui, &theme, label).clicked() {
                                    start_oidc = true;
                                }
                            });
                        }
                        AuthMethod::Token => {
                            if vidya::primary_button(ui, &theme, "Load Cursor key").clicked() {
                                use_token = true;
                            }
                        }
                    }
                    if vidya::button(ui, &theme, "Close").clicked() {
                        close = true;
                    }
                });
            });

        if start_oidc {
            self.auth_dialog = Some(dialog);
            self.start_oidc_login();
            return;
        }
        if use_token {
            self.auth_dialog = Some(dialog);
            self.try_token_auth();
            return;
        }
        if close {
            Self::cancel_oidc_login(&mut dialog);
        } else {
            self.auth_dialog = Some(dialog);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        vidya::apply(ctx, &self.theme);

        if self.pending_auto_auth {
            self.pending_auto_auth = false;
            self.try_restore_cursor_auth();
        }

        if self
            .auth_dialog
            .as_ref()
            .is_some_and(|d| d.oidc_login.is_some())
        {
            self.poll_oidc_login(ctx);
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            self.shutdown_all_sessions();
        }

        self.poll_pty_events(ctx);
        self.drain_resume_load();
        self.drain_pending_spawn(ctx);
        self.drain_convert();
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
        self.show_convert_dialog(ctx);
        self.show_auth_dialog(ctx);
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

/// First non-empty line of a snippet, truncated for history rows.
fn first_line_preview(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_ui(line, max)
}

/// Session story above the terminal — casual narrator prose about the whole turn.
fn show_summary_panel(ui: &mut egui::Ui, theme: &Theme, session: &crate::session::AgentSession) {
    let summary = session.summary.as_ref().filter(|s| !s.is_empty());
    let muted = mix_rgb(theme.palette.text_secondary, theme.palette.text, 0.45);
    let max_h = (ui.available_height() * 0.28).clamp(72.0, 160.0);

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

            let prose = match summary {
                Some(summary) if !summary.prose.trim().is_empty() => summary.prose.as_str(),
                _ if session.alive => "quiet so far — nothing cooking yet.",
                _ => "nothing happened in this session.",
            };
            let empty = summary.is_none_or(|s| s.prose.trim().is_empty());
            let color = if empty { muted } else { theme.palette.text };
            let job = summary_markdown_job(prose, theme, color, empty);

            egui::ScrollArea::vertical()
                .max_height(max_h - theme.spacing.sm * 2.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.add(egui::Label::new(job).wrap());
                });
        });
}

/// Lightweight markdown → [`LayoutJob`] for the summary panel.
///
/// Supports ATX headings (`#`–`###`), bullet / numbered lists, paragraphs,
/// and inline `**bold**`, `*italic*` / `_italic_`, and `` `code` ``.
fn summary_markdown_job(
    md: &str,
    theme: &Theme,
    color: egui::Color32,
    force_italic: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let body = theme.type_scale.title_3.max(theme.type_scale.body);
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;
    let mut first_block = true;

    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_end();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        if !first_block {
            append_formatted(&mut job, "\n\n", body_format(theme, color, body, force_italic));
        }
        first_block = false;

        if let Some((level, title)) = parse_atx_heading(line) {
            let size = match level {
                1 => theme.type_scale.title,
                2 => theme.type_scale.title_2,
                _ => theme.type_scale.title_3,
            };
            let fmt = TextFormat {
                font_id: FontId::proportional(size),
                color: if force_italic {
                    color
                } else {
                    theme.palette.text
                },
                italics: force_italic,
                line_height: Some(size * 1.3),
                ..Default::default()
            };
            append_inline_markdown(&mut job, title, theme, fmt, force_italic);
            i += 1;
            continue;
        }

        if let Some((marker, item)) = parse_list_item(line) {
            let fmt = body_format(theme, color, body, force_italic);
            append_formatted(&mut job, marker, fmt.clone());
            append_inline_markdown(&mut job, item, theme, fmt, force_italic);
            i += 1;
            while i < lines.len() {
                let next = lines[i].trim_end();
                if next.trim().is_empty() {
                    break;
                }
                let Some((marker, item)) = parse_list_item(next) else {
                    break;
                };
                append_formatted(
                    &mut job,
                    "\n",
                    body_format(theme, color, body, force_italic),
                );
                let fmt = body_format(theme, color, body, force_italic);
                append_formatted(&mut job, marker, fmt.clone());
                append_inline_markdown(&mut job, item, theme, fmt, force_italic);
                i += 1;
            }
            continue;
        }

        // Paragraph: join soft-wrapped continuation lines until blank / block.
        let mut para = line.trim_start().to_string();
        i += 1;
        while i < lines.len() {
            let next = lines[i].trim_end();
            if next.trim().is_empty()
                || parse_atx_heading(next).is_some()
                || parse_list_item(next).is_some()
            {
                break;
            }
            para.push(' ');
            para.push_str(next.trim());
            i += 1;
        }
        append_inline_markdown(
            &mut job,
            &para,
            theme,
            body_format(theme, color, body, force_italic),
            force_italic,
        );
    }

    if job.is_empty() {
        append_formatted(
            &mut job,
            md,
            body_format(theme, color, body, force_italic),
        );
    }
    job
}

fn body_format(
    _theme: &Theme,
    color: egui::Color32,
    size: f32,
    italic: bool,
) -> TextFormat {
    TextFormat {
        font_id: FontId::proportional(size),
        color,
        italics: italic,
        line_height: Some(size * 1.35),
        ..Default::default()
    }
}

fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let mut level = 0_u8;
    let bytes = trimmed.as_bytes();
    while (level as usize) < bytes.len() && bytes[level as usize] == b'#' && level < 3 {
        level += 1;
    }
    if level == 0 {
        return None;
    }
    let rest = &trimmed[level as usize..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some((level, rest.trim()))
}

fn parse_list_item(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        return Some(("•  ", rest));
    }
    // Ordered: "1. ", "12. "
    let mut digits = 0_usize;
    for b in trimmed.bytes() {
        if b.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 {
        let after = &trimmed[digits..];
        if let Some(rest) = after.strip_prefix(". ") {
            return Some(("•  ", rest));
        }
    }
    None
}

fn append_inline_markdown(
    job: &mut LayoutJob,
    text: &str,
    theme: &Theme,
    base: TextFormat,
    force_italic: bool,
) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();

    while i < chars.len() {
        // `code`
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !buf.is_empty() {
                    append_formatted(job, &buf, base.clone());
                    buf.clear();
                }
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                let mut code_fmt = base.clone();
                code_fmt.font_id = FontId::monospace(base.font_id.size * 0.92);
                code_fmt.color = theme.palette.accent;
                code_fmt.background = theme.palette.popover_bg;
                code_fmt.italics = false;
                append_formatted(job, &format!("\u{00a0}{code}\u{00a0}"), code_fmt);
                i += end + 2;
                continue;
            }
        }

        // **bold**
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                if !buf.is_empty() {
                    append_formatted(job, &buf, base.clone());
                    buf.clear();
                }
                let inner: String = chars[i + 2..end].iter().collect();
                let mut bold = base.clone();
                bold.color = theme.palette.text;
                bold.italics = force_italic;
                // Default UI fonts lack a bold face — nudge size for weight.
                bold.font_id = FontId::proportional(base.font_id.size + 0.5);
                append_inline_markdown(job, &inner, theme, bold, force_italic);
                i = end + 2;
                continue;
            }
        }

        // *italic* or _italic_ (single delimiter, non-empty, no newline)
        let italic_delim = match chars[i] {
            '*' if !(i + 1 < chars.len() && chars[i + 1] == '*') => Some('*'),
            '_' => Some('_'),
            _ => None,
        };
        if let Some(delim) = italic_delim {
            if let Some(rel) = chars[i + 1..].iter().position(|&c| c == delim) {
                let end = i + 1 + rel;
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.is_empty() && !inner.contains('\n') {
                    if !buf.is_empty() {
                        append_formatted(job, &buf, base.clone());
                        buf.clear();
                    }
                    let mut ital = base.clone();
                    ital.italics = true;
                    append_inline_markdown(job, &inner, theme, ital, true);
                    i = end + 1;
                    continue;
                }
            }
        }

        buf.push(chars[i]);
        i += 1;
    }
    if !buf.is_empty() {
        append_formatted(job, &buf, base);
    }
}

fn find_closing(chars: &[char], from: usize, delim: &[char]) -> Option<usize> {
    let n = delim.len();
    let mut i = from;
    while i + n <= chars.len() {
        if chars[i..i + n] == *delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn append_formatted(job: &mut LayoutJob, text: impl AsRef<str>, format: TextFormat) {
    job.append(text.as_ref(), 0.0, format);
}

fn mix_rgb(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8
    };
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
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

/// Auto-copy terminal selection when the user finishes a select gesture
/// (drag release, or double/triple-click word/line select).
///
/// egui_term has no `selection.automatic_copy`; we approximate alacritty's.
///
/// Important: `TerminalView` uses `Sense::click()` only, so `Response::hovered`
/// often goes false mid-drag. We arm on press-over-term and decide on release
/// from the actual selection range (and egui's drag/click flags).
fn auto_copy_term_selection(
    ui: &mut egui::Ui,
    session: &mut AgentSession,
    term_response: &egui::Response,
) {
    let (primary_released, decidedly_dragging) = ui.input(|i| {
        (
            i.pointer.primary_released(),
            i.pointer.is_decidedly_dragging(),
        )
    });

    if !primary_released {
        return;
    }

    let armed = session.term_select_armed;
    let dragged = session.term_select_dragging || decidedly_dragging;
    let multi_click = term_response.double_clicked() || term_response.triple_clicked();
    session.term_select_armed = false;
    session.term_select_dragging = false;

    if !armed {
        return;
    }

    // Force a sync so selectable_range matches SelectUpdate we applied this frame.
    session.backend.sync();

    let multi_cell = session
        .backend
        .last_content()
        .selectable_range
        .is_some_and(|r| r.start != r.end);

    if !dragged && !multi_click && !multi_cell {
        return;
    }

    let text = term_selection_text(&session.backend);
    if text.is_empty() {
        return;
    }
    ui.ctx().copy_text(text);
}

/// Start/update grid selection even when egui_term refuses (MOUSE_MODE).
///
/// Must run *before* `TerminalView` so `show()` paints the updated range.
fn drive_term_selection(
    ui: &mut egui::Ui,
    session: &mut AgentSession,
    term_rect: egui::Rect,
) {
    let (primary_pressed, primary_down, primary_released, decidedly_dragging, pointer_pos) =
        ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.is_decidedly_dragging(),
                i.pointer.interact_pos().or(i.pointer.hover_pos()),
            )
        });

    let Some(pos) = pointer_pos else {
        if !primary_down && !primary_released {
            session.term_select_armed = false;
            session.term_select_dragging = false;
        }
        return;
    };

    let over = term_rect.contains(pos);
    let rel = pos - term_rect.min;

    if primary_pressed && over {
        session.term_select_armed = true;
        session.term_select_dragging = false;
        session.backend.process_command(BackendCommand::SelectStart(
            SelectionType::Simple,
            rel.x,
            rel.y,
        ));
        return;
    }

    if !session.term_select_armed {
        return;
    }

    if primary_down {
        if decidedly_dragging {
            session.term_select_dragging = true;
        }
        // Keep updating while armed so MOUSE_MODE sessions still highlight.
        if session.term_select_dragging || decidedly_dragging || over {
            session.backend.process_command(BackendCommand::SelectUpdate(rel.x, rel.y));
        }
        return;
    }

    if primary_released {
        if decidedly_dragging {
            session.term_select_dragging = true;
        }
        // Final update so the release cell is included; double/triple-click is
        // handled afterward by TerminalView (Semantic/Lines SelectStart).
        if session.term_select_dragging || decidedly_dragging {
            session.backend.process_command(BackendCommand::SelectUpdate(rel.x, rel.y));
        }
        return;
    }

    // Button no longer down and we missed release (focus loss, etc.).
    session.term_select_armed = false;
    session.term_select_dragging = false;
}

/// Selected cells as a string, with newlines between grid rows and trailing
/// spaces stripped (egui_term's `selectable_content` concatenates with neither).
fn term_selection_text(backend: &egui_term::TerminalBackend) -> String {
    let content = backend.last_content();
    let Some(range) = content.selectable_range else {
        return String::new();
    };

    let mut lines: Vec<String> = Vec::new();
    let mut current: Option<(i32, String)> = None;

    for indexed in content.grid.display_iter() {
        if !range.contains(indexed.point) {
            continue;
        }
        let line = indexed.point.line.0;
        match current.as_mut() {
            Some((l, buf)) if *l == line => buf.push(indexed.c),
            Some(_) => {
                let (_, prev) = current.take().unwrap();
                lines.push(trim_trailing_ws(prev));
                current = Some((line, indexed.c.to_string()));
            }
            None => current = Some((line, indexed.c.to_string())),
        }
    }
    if let Some((_, prev)) = current {
        lines.push(trim_trailing_ws(prev));
    }

    let text = lines.join("\n");
    if text.chars().all(|c| c.is_whitespace()) {
        String::new()
    } else {
        text
    }
}

fn trim_trailing_ws(s: String) -> String {
    s.trim_end_matches(|c: char| c == ' ' || c == '\t').to_string()
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

/// Bindings that keep PageUp/Down out of the PTY on the primary screen so
/// [`handle_term_page_scroll`] can drive scrollback instead.
fn term_page_scroll_bindings() -> Vec<(Binding<InputKind>, BindingAction)> {
    let none = egui::Modifiers::NONE;
    let shift = egui::Modifiers::SHIFT;
    [
        (egui::Key::PageUp, none),
        (egui::Key::PageDown, none),
        (egui::Key::PageUp, shift),
        (egui::Key::PageDown, shift),
    ]
    .into_iter()
    .map(|(key, modifiers)| {
        (
            Binding {
                target: InputKind::KeyCode(key),
                modifiers,
                terminal_mode_include: TerminalMode::empty(),
                terminal_mode_exclude: TerminalMode::empty(),
            },
            BindingAction::Ignore,
        )
    })
    .collect()
}

/// PageUp/Down (and Shift+) scroll one screen of history when not in alt-screen.
///
/// egui_term sends bare PageUp/Down to the child and leaves Shift+PageUp/Down
/// unbound outside alt-screen, so scrollback keys never moved the viewport.
fn handle_term_page_scroll(
    ui: &mut egui::Ui,
    backend: &mut egui_term::TerminalBackend,
    alt_screen: bool,
) {
    use alacritty_terminal::grid::Dimensions;

    if alt_screen {
        return;
    }

    let (page_up, page_down) = ui.input(|i| {
        let mods = i.modifiers;
        // Leave Ctrl/Alt chords to egui_term (CSI with modifier params).
        if mods.ctrl || mods.alt || mods.command {
            return (false, false);
        }
        (
            i.key_pressed(egui::Key::PageUp),
            i.key_pressed(egui::Key::PageDown),
        )
    });
    if !page_up && !page_down {
        return;
    }

    let screen_lines = backend.last_content().grid.screen_lines().max(1);
    let scroll_delta = if page_up {
        screen_lines as i32
    } else {
        -(screen_lines as i32)
    };
    backend.process_command(BackendCommand::Scroll(scroll_delta));
    ui.ctx().request_repaint();
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
/// vidya’s proportional `→` fallback on `FontFamily::Monospace`, then attach
/// symbol/emoji supplements for cursor-agent icons (`⌕`, `🔍`, …).
fn install_term_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        TERM_FONT_NAME.to_owned(),
        FontData::from_static(TERM_FONT_TTF).into(),
    );
    fonts.font_data.insert(
        TERM_SYMBOLS_NAME.to_owned(),
        FontData::from_static(TERM_SYMBOLS_TTF).into(),
    );

    // Isolated family: JB first (mono advances), then our symbol subset, then
    // egui’s built-in emoji/Hack fallbacks. Do not use FontFamily::Monospace —
    // vidya installs a proportional symbol font there that breaks cell width.
    fonts.families.insert(
        FontFamily::Name(TERM_FONT_NAME.into()),
        vec![
            TERM_FONT_NAME.to_owned(),
            TERM_SYMBOLS_NAME.to_owned(),
            "NotoEmoji-Regular".to_owned(),
            "emoji-icon-font".to_owned(),
            "Hack".to_owned(),
        ],
    );

    ctx.set_fonts(fonts);
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
