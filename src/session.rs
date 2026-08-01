//! Resolve and spawn cursor-agent sessions into PTYs via egui_term.

use crate::subagents::{self, ChatSnapshot, SessionSummary, Subagent};
use egui_term::{BackendCommand, BackendSettings, PtyEvent, TerminalBackend};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long after PTY Wakeup to keep the session-tab spinner up.
const ACTIVITY_HOLD: Duration = Duration::from_millis(2500);

/// Agent binary path resolved once (avoids `which` on the UI thread).
static AGENT_BIN: OnceLock<Result<String, String>> = OnceLock::new();

/// Everything needed to open a PTY after off-thread `create-chat` / validation.
#[derive(Debug, Clone)]
pub struct PreparedSession {
    pub shell: String,
    pub args: Vec<String>,
    pub workspace: PathBuf,
    pub chat_id: String,
    pub title: String,
    /// Images + prompt to inject into the interactive composer after the TUI is up.
    ///
    /// `--image` only works for headless prompts, so interactive sessions paste via
    /// clipboard + `^V` instead (see [`AgentSession::progress_composer_seed`]).
    pub composer_seed: Option<ComposerSeed>,
}

/// Initial composer contents deferred until the agent TUI can accept input.
#[derive(Debug, Clone)]
pub struct ComposerSeed {
    pub images: Vec<PathBuf>,
    pub prompt: String,
}

/// In-progress injection of a [`ComposerSeed`] into a live PTY.
#[derive(Debug)]
pub struct ComposerSeedInject {
    images: Vec<PathBuf>,
    prompt: String,
    /// How many images have had clipboard+`^V` sent (await `[Image #N]` before the next).
    pastes_sent: usize,
    /// After all pastes attach: 0 = type prompt, 1 = submit.
    step: usize,
    next_at: Instant,
    /// When the inject was armed (so we can stop waiting on "Starting…").
    started_at: Instant,
}

/// Draft fields for the new-session / resume dialogs.
#[derive(Debug, Clone)]
pub struct NewSessionDraft {
    pub workspace: String,
    pub model: String,
    pub prompt: String,
    /// Clipboard images pasted into the initial prompt (injected into the TUI via `^V`).
    pub images: Vec<PathBuf>,
    pub trust: bool,
    pub force: bool,
    /// When set, spawn with `--resume <id>` (initial prompt is ignored).
    pub resume_chat_id: Option<String>,
    /// Tab title override (e.g. saved chat title when resuming).
    pub tab_title: Option<String>,
}

impl Default for NewSessionDraft {
    fn default() -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        Self {
            workspace: cwd,
            model: String::new(),
            prompt: String::new(),
            images: Vec::new(),
            trust: true,
            force: false,
            resume_chat_id: None,
            tab_title: None,
        }
    }
}

/// A past cursor-agent chat from `~/.cursor/chats`.
#[derive(Debug, Clone)]
pub struct SavedChat {
    pub id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub updated_at_ms: u64,
}

impl SavedChat {
    pub fn age_label(&self) -> String {
        format_age(self.updated_at_ms)
    }

    pub fn workspace_label(&self) -> String {
        match &self.cwd {
            Some(cwd) if !cwd.is_empty() => cwd.clone(),
            _ => "(no workspace)".into(),
        }
    }
}

/// One live (or exited) agent PTY tab.
pub struct AgentSession {
    pub id: u64,
    pub title: String,
    pub workspace: PathBuf,
    pub backend: TerminalBackend,
    pub alive: bool,
    /// Direct child PID of the PTY shell (`cursor-agent`), if known.
    ///
    /// Used to kill the agent *before* dropping [`TerminalBackend`]: egui_term's
    /// subscriber only exits on `Event::Exit` (sent when the child exits). Dropping
    /// first sends `Shutdown` and busy-spins the subscriber thread forever.
    pub child_pid: Option<u32>,
    /// Cursor chat id (`create-chat` or resume); used for title + subagent polling.
    pub chat_id: String,
    /// Nested Task/subagents discovered under this chat.
    pub subagents: Vec<Subagent>,
    /// Live status for spinner / needs-input (e.g. "Thinking…").
    pub activity: Option<String>,
    /// Casual narrator story of this turn for the summary panel.
    pub summary: Option<SessionSummary>,
    /// Latest chat meta title from the poller (kept even when [`Self::title_locked`]).
    pub meta_title: Option<String>,
    /// When true, hide nested Task rows in the sidebar.
    pub tasks_folded: bool,
    /// When true, ignore OSC / PTY title updates so a user rename sticks.
    pub title_locked: bool,
    /// Last PTY `Wakeup` (holds the tab spinner briefly between transcript polls).
    last_activity: Option<Instant>,
    /// Paste images + type prompt into the interactive composer once the TUI is ready.
    composer_seed: Option<ComposerSeedInject>,
    /// True while the pointer is dragging a text selection in this session's terminal.
    /// Used to auto-copy on release (egui_term has no built-in automatic_copy).
    pub term_select_dragging: bool,
    /// Primary was pressed while the pointer was over this terminal (may lose hover mid-drag).
    pub term_select_armed: bool,
}

impl AgentSession {
    /// Validate draft + `create-chat` if needed. Safe to run off the UI thread
    /// (`create-chat` can take multiple seconds).
    pub fn prepare(draft: &NewSessionDraft) -> Result<PreparedSession, String> {
        let workspace = PathBuf::from(draft.workspace.trim());
        if draft.workspace.trim().is_empty() {
            return Err("workspace path is required".into());
        }
        if !workspace.is_dir() {
            return Err(format!(
                "workspace is not a directory: {}",
                workspace.display()
            ));
        }

        let shell = resolve_agent_binary()?;
        let chat_id = match draft
            .resume_chat_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(id) => id.to_string(),
            None => subagents::create_chat(&shell, &workspace)?,
        };
        let mut draft = draft.clone();
        draft.resume_chat_id = Some(chat_id.clone());

        // Interactive TUI ignores `--image` (headless-only). When images were pasted in
        // the new-session dialog, withhold the CLI prompt too and inject both via PTY.
        let title_from_prompt = sanitize_auto_title(draft.prompt.trim());
        let composer_seed = if draft.images.is_empty() {
            None
        } else {
            let seed = ComposerSeed {
                images: std::mem::take(&mut draft.images),
                prompt: std::mem::take(&mut draft.prompt),
            };
            Some(seed)
        };

        let args = build_args(&draft, &workspace);
        let title = draft
            .tab_title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(title_from_prompt)
            .unwrap_or_else(|| {
                workspace
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("agent")
                    .to_string()
            });

        Ok(PreparedSession {
            shell,
            args,
            workspace,
            chat_id,
            title,
            composer_seed,
        })
    }

    /// Open the PTY on the UI thread after [`Self::prepare`].
    pub fn spawn_prepared(
        id: u64,
        ctx: egui::Context,
        pty_tx: Sender<(u64, PtyEvent)>,
        prepared: PreparedSession,
    ) -> Result<Self, String> {
        let parent = std::process::id();
        let before = direct_child_pids(parent);
        let backend = TerminalBackend::new(
            id,
            ctx,
            pty_tx,
            BackendSettings {
                shell: prepared.shell,
                args: prepared.args,
                working_directory: Some(prepared.workspace.clone()),
            },
        )
        .map_err(|e| format!("failed to spawn agent PTY: {e}"))?;
        let after = direct_child_pids(parent);
        let child_pid = after.difference(&before).next().copied();

        Ok(Self {
            id,
            title: prepared.title,
            workspace: prepared.workspace,
            backend,
            alive: true,
            child_pid,
            chat_id: prepared.chat_id,
            subagents: Vec::new(),
            activity: Some("Starting…".into()),
            summary: None,
            meta_title: None,
            tasks_folded: false,
            title_locked: false,
            last_activity: Some(Instant::now()),
            composer_seed: prepared.composer_seed.map(|seed| {
                let started_at = Instant::now();
                ComposerSeedInject {
                    images: seed.images,
                    prompt: seed.prompt,
                    pastes_sent: 0,
                    step: 0,
                    // Give the interactive TUI time to mount the composer before we paste.
                    next_at: started_at + Duration::from_millis(1500),
                    started_at,
                }
            }),
            term_select_dragging: false,
            term_select_armed: false,
        })
    }

    /// True while a deferred new-session image/prompt inject is still running.
    pub fn has_pending_composer_seed(&self) -> bool {
        self.composer_seed.is_some()
    }

    /// Advance clipboard-`^V` + prompt injection for images pasted on the new-session screen.
    ///
    /// Returns how long to wait before the next step (for `request_repaint_after`).
    ///
    /// Puts each image on the system clipboard and sends `^V` (same path as a manual
    /// Ctrl+V). Waits until `[Image #N]` appears in the TUI before typing the prompt or
    /// submitting, so the agent's async paste handler can finish.
    pub fn progress_composer_seed(&mut self) -> Option<Duration> {
        let Some(inject) = self.composer_seed.as_mut() else {
            return None;
        };
        if !self.alive {
            self.clear_composer_seed();
            return None;
        }

        let now = Instant::now();
        if now < inject.next_at {
            return Some(inject.next_at.saturating_duration_since(now));
        }

        // Hold off while the agent is still booting the composer (cap so a stuck
        // "Starting…" label cannot block the seed forever).
        if inject.pastes_sent == 0
            && inject.step == 0
            && self.activity.as_deref() == Some("Starting…")
            && inject.started_at.elapsed() < Duration::from_secs(8)
        {
            inject.next_at = Instant::now() + Duration::from_millis(250);
            return Some(Duration::from_millis(250));
        }

        let img_count = inject.images.len();
        let attached = terminal_image_token_count(&mut self.backend);

        // Send next clipboard paste once prior attaches have landed.
        if inject.pastes_sent < img_count && attached >= inject.pastes_sent {
            let path = inject.images[inject.pastes_sent].clone();
            if crate::clipboard::set_clipboard_image(&path) {
                self.backend
                    .process_command(BackendCommand::Write(vec![0x16]));
            }
            inject.pastes_sent += 1;
            let wait = Duration::from_millis(50);
            inject.next_at = Instant::now() + wait;
            return Some(wait);
        }

        // Wait until every paste shows as `[Image #N]` in the TUI.
        if attached < img_count {
            if inject.started_at.elapsed() < Duration::from_secs(10) {
                let wait = Duration::from_millis(100);
                inject.next_at = Instant::now() + wait;
                return Some(wait);
            }
            // Timed out — leave the composer open rather than submitting without images.
            let _ = self.composer_seed.take();
            return None;
        }

        if inject.step == 0 {
            let prompt = inject.prompt.trim();
            if !prompt.is_empty() {
                self.backend
                    .process_command(BackendCommand::Write(prompt.as_bytes().to_vec()));
                inject.step = 1;
                inject.next_at = Instant::now() + Duration::from_millis(80);
                return Some(Duration::from_millis(80));
            }
            // Image-only: leave the composer open. Keep files — agent may still
            // reference paths until the user submits (/tmp reclaims later).
            let _ = self.composer_seed.take();
            return None;
        }

        // Prompt typed: submit. Keep clip files in case anything still references them.
        self.backend
            .process_command(BackendCommand::Write(vec![b'\r']));
        let _ = self.composer_seed.take();
        None
    }

    fn clear_composer_seed(&mut self) {
        // Drop the seed but keep image files — paste/selectedImages may still
        // reference paths until blob ingest finishes.
        let _ = self.composer_seed.take();
    }

    /// Record PTY output so the sidebar spinner stays up between chat polls.
    pub fn bump_activity(&mut self) {
        self.last_activity = Some(Instant::now());
    }

    /// Alive and waiting on the user (prompt / follow-up).
    pub fn needs_input(&self) -> bool {
        self.alive
            && self
                .activity
                .as_deref()
                .is_some_and(activity_needs_input)
    }

    /// Mid-turn work: live Tasks, non-idle activity label, or recent PTY output.
    pub fn has_activity(&self) -> bool {
        if !self.alive || self.needs_input() {
            return false;
        }
        if self.subagents.iter().any(|s| s.status.is_live()) {
            return true;
        }
        match self.activity.as_deref() {
            None | Some("Idle") | Some("Waiting for input") => self
                .last_activity
                .is_some_and(|t| t.elapsed() < ACTIVITY_HOLD),
            Some(_) => true,
        }
    }

    /// Apply a snapshot produced off-thread by [`subagents::poll_chat`].
    pub fn apply_chat_snapshot(&mut self, snap: ChatSnapshot) -> bool {
        let mut changed = false;

        // Always keep the polled meta title so auto-rename can use it even when
        // the displayed tab title is locked.
        if let Some(title) = snap.title.clone() {
            if self.meta_title.as_ref() != Some(&title) {
                self.meta_title = Some(title);
                changed = true;
            }
        }

        if !self.title_locked {
            if let Some(title) = snap.title {
                let next = if self.alive {
                    title
                } else if title.ends_with(" (exited)") {
                    title
                } else {
                    format!("{title} (exited)")
                };
                if self.title != next {
                    self.title = next;
                    changed = true;
                }
            }
        }

        if self.subagents != snap.subagents {
            let had_live = self.subagents.iter().any(|s| s.status.is_live());
            let has_live = snap.subagents.iter().any(|s| s.status.is_live());
            self.subagents = snap.subagents;
            // Collapse the list once everything finishes so the sidebar stays tidy.
            if had_live && !has_live && !self.subagents.is_empty() {
                self.tasks_folded = true;
            }
            // Expand when new live work appears.
            if has_live && !had_live {
                self.tasks_folded = false;
            }
            changed = true;
        }
        if self.activity != snap.activity {
            self.activity = snap.activity;
            changed = true;
        }
        if self.summary != snap.summary {
            self.summary = snap.summary;
            changed = true;
        }
        changed
    }

    /// Set tab title from a user action (manual or auto-rename).
    pub fn set_user_title(&mut self, title: impl Into<String>) {
        let mut title = title.into().trim().to_string();
        if title.is_empty() {
            return;
        }
        if !self.alive && !title.ends_with(" (exited)") {
            title.push_str(" (exited)");
        }
        self.title = title;
        self.title_locked = true;
    }

    /// Derive a tab title from already-polled chat state and/or visible terminal text.
    pub fn auto_rename_from_content(&mut self) {
        if let Some(title) = suggest_title_from_content(self) {
            self.set_user_title(title);
        }
    }
}

/// Prefer poller state (meta title, then summary); fall back to terminal text.
///
/// Does not scan `~/.cursor/chats` on the UI thread — that data arrives via
/// [`AgentSession::apply_chat_snapshot`].
fn suggest_title_from_content(session: &mut AgentSession) -> Option<String> {
    if let Some(title) = session
        .meta_title
        .as_deref()
        .and_then(meaningful_meta_title)
    {
        return Some(title.to_string());
    }
    if let Some(title) = session.summary.as_ref().and_then(|s| s.title_hint()).and_then(sanitize_auto_title) {
        return Some(title);
    }
    title_from_terminal(&mut session.backend)
}

fn meaningful_meta_title(title: &str) -> Option<&str> {
    let title = title.trim();
    if title.is_empty() || title == "Untitled" {
        None
    } else {
        Some(title)
    }
}

fn title_from_terminal(backend: &mut TerminalBackend) -> Option<String> {
    let content = backend.sync();
    let mut lines: Vec<String> = Vec::new();
    let mut current_line: Option<i32> = None;
    let mut buf = String::new();

    for indexed in content.grid.display_iter() {
        let line = indexed.point.line.0;
        if current_line != Some(line) {
            if current_line.is_some() {
                push_terminal_line(&mut lines, &buf);
            }
            buf.clear();
            current_line = Some(line);
        }
        if indexed.c != '\0' {
            buf.push(indexed.c);
        }
    }
    if current_line.is_some() {
        push_terminal_line(&mut lines, &buf);
    }

    lines
        .into_iter()
        .find_map(|line| sanitize_auto_title(&line))
}

/// How many `[Image #N]` tokens are visible in the agent TUI.
fn terminal_image_token_count(backend: &mut TerminalBackend) -> usize {
    let content = backend.sync();
    let mut text = String::new();
    let mut current_line: Option<i32> = None;
    for indexed in content.grid.display_iter() {
        let line = indexed.point.line.0;
        if current_line.is_some() && current_line != Some(line) {
            text.push('\n');
        }
        current_line = Some(line);
        if indexed.c != '\0' {
            text.push(indexed.c);
        }
    }
    text.matches("[Image #").count()
}

fn push_terminal_line(lines: &mut Vec<String>, buf: &str) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        lines.push(trimmed.to_string());
    }
}

/// Only structured questions (AskQuestion), not every idle “Waiting for input”.
fn activity_needs_input(activity: &str) -> bool {
    activity.starts_with("Waiting for answer")
}

fn sanitize_auto_title(line: &str) -> Option<String> {
    let mut s = line.trim().to_string();
    // Strip common TUI bullets / prompt chrome.
    for prefix in ["› ", "> ", "❯ ", "• ", "- ", "* ", "$ "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim().to_string();
            break;
        }
    }
    // Skip tiny / decorative noise.
    let alnum = s.chars().filter(|c| c.is_alphanumeric()).count();
    if alnum < 4 || s.len() < 4 {
        return None;
    }
    if s.chars().all(|c| !c.is_alphanumeric()) {
        return None;
    }
    const MAX: usize = 48;
    if s.chars().count() > MAX {
        let truncated: String = s.chars().take(MAX.saturating_sub(1)).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(s)
    }
}

/// Title-Case Each Word for manual renames.
pub fn title_case_words(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut out: String = first.to_uppercase().collect();
                    out.extend(chars.flat_map(|c| c.to_lowercase()));
                    out
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `CURSOR_AGENT` env (path override), else `cursor-agent` on PATH.
///
/// Cursor itself sets `CURSOR_AGENT=1` inside agent sessions as a boolean flag —
/// that must not be treated as a binary path. Does not fall through to bare
/// `agent` — that name is often another CLI (e.g. grok).
///
/// Result is cached — safe to call from background threads; does not block the
/// UI after the first resolution.
pub fn resolve_agent_binary() -> Result<String, String> {
    AGENT_BIN
        .get_or_init(resolve_agent_binary_uncached)
        .clone()
}

fn resolve_agent_binary_uncached() -> Result<String, String> {
    if let Ok(path) = std::env::var("CURSOR_AGENT") {
        let path = path.trim();
        if looks_like_agent_binary(path) {
            return Ok(path.to_string());
        }
    }

    if which("cursor-agent").is_some() {
        return Ok("cursor-agent".into());
    }

    Err(
        "cursor-agent not found (set CURSOR_AGENT to a binary path or install cursor-agent on PATH)"
            .into(),
    )
}

/// True when `CURSOR_AGENT` looks like an executable override, not Cursor's `=1` flag.
fn looks_like_agent_binary(path: &str) -> bool {
    if path.is_empty() || path == "0" || path == "1" {
        return false;
    }
    Path::new(path).is_file() || which(path).is_some()
}

/// Scan `~/.cursor/chats` for resumable sessions (newest first).
pub fn list_saved_chats() -> Result<Vec<SavedChat>, String> {
    let root = chats_root().ok_or_else(|| "HOME is unset; cannot find ~/.cursor/chats".to_string())?;
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut chats = Vec::new();
    let workspace_dirs = fs::read_dir(&root).map_err(|e| format!("read {}: {e}", root.display()))?;
    for ws_entry in workspace_dirs.flatten() {
        let ws_path = ws_entry.path();
        if !ws_path.is_dir() {
            continue;
        }
        let chat_dirs = match fs::read_dir(&ws_path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for chat_entry in chat_dirs.flatten() {
            let chat_path = chat_entry.path();
            if !chat_path.is_dir() {
                continue;
            }
            let meta_path = chat_path.join("meta.json");
            if !meta_path.is_file() {
                continue;
            }
            let Some(chat) = parse_saved_chat(&chat_path, &meta_path) else {
                continue;
            };
            chats.push(chat);
        }
    }

    chats.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    Ok(chats)
}

fn chats_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cursor").join("chats"))
}

fn parse_saved_chat(chat_path: &Path, meta_path: &Path) -> Option<SavedChat> {
    let text = fs::read_to_string(meta_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;

    // Empty shells (created but never prompted) are not useful to resume.
    if value.get("hasConversation").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    // Nested Task chats are opened from the parent session's task rows.
    if value.get("isSubagent").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }

    let id = chat_path.file_name()?.to_str()?.to_string();
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Untitled")
        .to_string();
    let cwd = value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let updated_at_ms = value
        .get("updatedAtMs")
        .and_then(|v| v.as_u64())
        .or_else(|| value.get("createdAtMs").and_then(|v| v.as_u64()))
        .unwrap_or(0);

    Some(SavedChat {
        id,
        title,
        cwd,
        updated_at_ms,
    })
}

fn format_age(updated_at_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let age = Duration::from_millis(now_ms.saturating_sub(updated_at_ms));
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// PIDs whose parent is `ppid` (Linux `/proc`).
fn direct_child_pids(ppid: u32) -> HashSet<u32> {
    let mut out = HashSet::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if parent_pid(pid) == Some(ppid) {
            out.insert(pid);
        }
    }
    out
}

fn parent_pid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Force-kill a PTY child so alacritty emits `Event::Exit` (egui_term subscriber exits).
pub fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

fn which(name: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn build_args(draft: &NewSessionDraft, workspace: &Path) -> Vec<String> {
    let mut args = Vec::new();

    args.push("--workspace".into());
    args.push(workspace.display().to_string());

    if draft.trust {
        args.push("--trust".into());
    }
    if draft.force {
        args.push("--force".into());
    }

    let model = draft.model.trim();
    if !model.is_empty() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    if let Some(chat_id) = draft.resume_chat_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        args.push("--resume".into());
        args.push(chat_id.to_string());
    }

    // Initial prompt still applies when resuming a freshly created empty chat.
    // Images are not passed here: `--image` is headless-only; see ComposerSeed.
    let prompt = draft.prompt.trim();
    if !prompt.is_empty() {
        args.push(prompt.to_string());
    }

    args
}
