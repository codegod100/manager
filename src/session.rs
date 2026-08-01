//! Resolve and spawn cursor-agent sessions into PTYs via egui_term.

use crate::subagents::{self, Subagent};
use egui_term::{BackendSettings, PtyEvent, TerminalBackend};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Draft fields for the new-session / resume dialogs.
#[derive(Debug, Clone)]
pub struct NewSessionDraft {
    pub workspace: String,
    pub model: String,
    pub prompt: String,
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
    /// Cursor chat id (`create-chat` or resume); used for title + subagent polling.
    pub chat_id: String,
    /// Nested Task/subagents discovered under this chat.
    pub subagents: Vec<Subagent>,
    /// When true, ignore OSC / PTY title updates so a user rename sticks.
    pub title_locked: bool,
}

impl AgentSession {
    pub fn spawn(
        id: u64,
        ctx: egui::Context,
        pty_tx: Sender<(u64, PtyEvent)>,
        draft: &NewSessionDraft,
    ) -> Result<Self, String> {
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
        let args = build_args(&draft, &workspace);
        let title = draft
            .tab_title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                workspace
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("agent")
                    .to_string()
            });

        let backend = TerminalBackend::new(
            id,
            ctx,
            pty_tx,
            BackendSettings {
                shell,
                args,
                working_directory: Some(workspace.clone()),
            },
        )
        .map_err(|e| format!("failed to spawn agent PTY: {e}"))?;

        Ok(Self {
            id,
            title,
            workspace,
            backend,
            alive: true,
            chat_id,
            subagents: Vec::new(),
            title_locked: false,
        })
    }

    /// Refresh title (if unlocked) and subagent list from Cursor's chat store.
    pub fn poll_chat_state(&mut self) -> bool {
        let snap = subagents::poll_chat(&self.chat_id);
        let mut changed = false;

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
            self.subagents = snap.subagents;
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

    /// Derive a tab title from Cursor chat meta and/or visible terminal text.
    pub fn auto_rename_from_content(&mut self) {
        if let Some(title) = suggest_title_from_content(self) {
            self.set_user_title(title);
        }
    }
}

/// Prefer Cursor's chat title for this session; fall back to terminal text.
fn suggest_title_from_content(session: &mut AgentSession) -> Option<String> {
    if let Some(title) = lookup_chat_title_for_session(session) {
        return Some(title);
    }
    title_from_terminal(&mut session.backend)
}

fn lookup_chat_title_for_session(session: &AgentSession) -> Option<String> {
    let chats = list_saved_chats().ok()?;
    if let Some(chat) = chats.iter().find(|c| c.id == session.chat_id) {
        let title = chat.title.trim();
        if !title.is_empty() && title != "Untitled" {
            return Some(title.to_string());
        }
    }

    let ws = normalize_path(&session.workspace);
    chats.into_iter().find_map(|c| {
        let cwd = c.cwd.as_deref()?;
        if normalize_path(Path::new(cwd)) != ws {
            return None;
        }
        let title = c.title.trim();
        if title.is_empty() || title == "Untitled" {
            None
        } else {
            Some(title.to_string())
        }
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

fn push_terminal_line(lines: &mut Vec<String>, buf: &str) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        lines.push(trimmed.to_string());
    }
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

/// `CURSOR_AGENT` env, else `cursor-agent` on PATH.
///
/// Does not fall through to bare `agent` — that name is often another CLI (e.g. grok).
pub fn resolve_agent_binary() -> Result<String, String> {
    if let Ok(path) = std::env::var("CURSOR_AGENT") {
        let path = path.trim();
        if !path.is_empty() {
            return Ok(path.to_string());
        }
    }

    if which("cursor-agent").is_some() {
        return Ok("cursor-agent".into());
    }

    Err(
        "cursor-agent not found (set CURSOR_AGENT or install cursor-agent on PATH)".into(),
    )
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
    let prompt = draft.prompt.trim();
    if !prompt.is_empty() {
        args.push(prompt.to_string());
    }

    args
}
