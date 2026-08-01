//! Resolve and spawn cursor-agent sessions into PTYs via egui_term.

use egui_term::{BackendSettings, PtyEvent, TerminalBackend};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

/// Draft fields for the new-session dialog.
#[derive(Debug, Clone)]
pub struct NewSessionDraft {
    pub workspace: String,
    pub model: String,
    pub prompt: String,
    pub trust: bool,
    pub force: bool,
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
        let args = build_args(draft, &workspace);
        let title = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("agent")
            .to_string();

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
        })
    }
}

/// `CURSOR_AGENT` env, else `cursor-agent` / `agent` on PATH.
pub fn resolve_agent_binary() -> Result<String, String> {
    if let Ok(path) = std::env::var("CURSOR_AGENT") {
        let path = path.trim();
        if !path.is_empty() {
            return Ok(path.to_string());
        }
    }

    for name in ["cursor-agent", "agent"] {
        if which(name).is_some() {
            return Ok(name.to_string());
        }
    }

    Err(
        "cursor-agent not found (set CURSOR_AGENT or install agent on PATH)".into(),
    )
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

    let prompt = draft.prompt.trim();
    if !prompt.is_empty() {
        args.push(prompt.to_string());
    }

    args
}
