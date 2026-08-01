//! Discover Cursor Task/subagents for a bound chat id under `~/.cursor/chats`.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Running or finished subagent/Task under a parent chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: SubagentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Done,
    Failed,
}

impl SubagentStatus {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Snapshot of chat meta + nested subagents for one chat id.
#[derive(Debug, Clone, Default)]
pub struct ChatSnapshot {
    pub title: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub subagents: Vec<Subagent>,
}

/// Locate `~/.cursor/chats/*/<chat_id>/` by scanning workspace-hash dirs.
pub fn find_chat_dir(chat_id: &str) -> Option<PathBuf> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return None;
    }
    let root = chats_root()?;
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let ws = entry.path();
        if !ws.is_dir() {
            continue;
        }
        let candidate = ws.join(chat_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Poll meta + subagents/ + Task blobs for a chat id.
pub fn poll_chat(chat_id: &str) -> ChatSnapshot {
    let Some(dir) = find_chat_dir(chat_id) else {
        return ChatSnapshot::default();
    };

    let mut snap = ChatSnapshot::default();
    if let Some((title, updated)) = read_meta(&dir) {
        snap.title = title;
        snap.updated_at_ms = updated;
    }

    let mut by_id: std::collections::BTreeMap<String, Subagent> = std::collections::BTreeMap::new();

    for sub in list_subagents_dir(&dir) {
        by_id.insert(sub.id.clone(), sub);
    }
    for sub in scan_task_blobs(&dir) {
        by_id
            .entry(sub.id.clone())
            .and_modify(|existing| merge_subagent(existing, &sub))
            .or_insert(sub);
    }

    snap.subagents = by_id.into_values().collect();
    snap.subagents.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
    snap
}

fn chats_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cursor").join("chats"))
}

fn read_meta(chat_dir: &Path) -> Option<(Option<String>, Option<u64>)> {
    let text = fs::read_to_string(chat_dir.join("meta.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "Untitled")
        .map(str::to_string);
    let updated = value
        .get("updatedAtMs")
        .and_then(|v| v.as_u64())
        .or_else(|| value.get("createdAtMs").and_then(|v| v.as_u64()));
    Some((title, updated))
}

fn list_subagents_dir(chat_dir: &Path) -> Vec<Subagent> {
    let dir = chat_dir.join("subagents");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip sqlite sidecars / hidden junk.
        if name.starts_with('.') || name.ends_with("-shm") || name.ends_with("-wal") {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
            .to_string();
        if stem.is_empty() {
            continue;
        }

        let (title, kind, status) = inspect_subagent_file(&path, &stem);
        out.push(Subagent {
            id: stem,
            title,
            kind,
            status,
        });
    }
    out
}

fn inspect_subagent_file(path: &Path, stem: &str) -> (String, Option<String>, SubagentStatus) {
    let mut title = stem.to_string();
    let mut kind = None;
    let mut status = SubagentStatus::Running;

    // Prefer a sibling .json meta if present.
    let meta_path = path.with_extension("json");
    if meta_path.is_file() {
        if let Ok(text) = fs::read_to_string(&meta_path) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if let Some(t) = value
                    .get("title")
                    .or_else(|| value.get("description"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    title = t.to_string();
                }
                if let Some(k) = value
                    .get("subagentType")
                    .or_else(|| value.get("type"))
                    .or_else(|| value.get("kind"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    kind = Some(k.to_string());
                }
                if let Some(s) = value.get("status").and_then(|v| v.as_str()) {
                    status = parse_status(s);
                }
            }
        }
    }

    // jsonl / store-like files: skim first/last chunks for title + completion.
    if let Ok(bytes) = fs::read(path) {
        let text = String::from_utf8_lossy(&bytes);
        if title == stem {
            if let Some(t) = extract_description_from_text(&text) {
                title = t;
            }
        }
        if kind.is_none() {
            kind = extract_kind_from_text(&text);
        }
        if text_indicates_done(&text) {
            status = SubagentStatus::Done;
        } else if text_indicates_failed(&text) {
            status = SubagentStatus::Failed;
        } else if is_stale_file(path) && status == SubagentStatus::Running {
            // Quiet files that haven't grown recently are likely finished.
            status = SubagentStatus::Done;
        }
    }

    (truncate(&title, 48), kind, status)
}

fn scan_task_blobs(chat_dir: &Path) -> Vec<Subagent> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for name in ["store.db", "store.db-wal"] {
        let path = chat_dir.join(name);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        for sub in extract_tasks_from_bytes(&bytes) {
            if seen.insert(sub.id.clone()) {
                out.push(sub);
            }
        }
    }
    out
}

fn extract_tasks_from_bytes(bytes: &[u8]) -> Vec<Subagent> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();

    // IDE-style JSON tool calls.
    for (idx, _) in text.match_indices("\"toolName\":\"Task\"") {
        let window = surrounding(&text, idx, 1200);
        let id = extract_json_string(window, "toolCallId")
            .or_else(|| extract_json_string(window, "agentId"))
            .unwrap_or_else(|| format!("task-{idx}"));
        let title = extract_json_string(window, "description")
            .or_else(|| extract_json_string(window, "title"))
            .unwrap_or_else(|| "Task".into());
        let kind = extract_json_string(window, "subagentType")
            .or_else(|| extract_json_string(window, "subagent_type"));
        let status = if window.contains("\"status\":\"completed\"")
            || window.contains("\"subtype\":\"completed\"")
            || window.contains("tool-result")
        {
            SubagentStatus::Done
        } else if window.contains("\"status\":\"error\"")
            || window.contains("\"status\":\"failed\"")
            || window.contains("\"status\":\"aborted\"")
        {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Running
        };
        out.push(Subagent {
            id,
            title: truncate(&title, 48),
            kind,
            status,
        });
    }

    // stream-json / protobuf-ish readable fragments.
    for (idx, _) in text.match_indices("taskToolCall") {
        let window = surrounding(&text, idx, 800);
        let id = extract_json_string(window, "call_id")
            .or_else(|| extract_json_string(window, "toolCallId"))
            .unwrap_or_else(|| format!("taskToolCall-{idx}"));
        let title = extract_json_string(window, "description")
            .or_else(|| extract_json_string(window, "prompt"))
            .unwrap_or_else(|| "Subagent task".into());
        let kind = extract_json_string(window, "subagentType");
        let status = if window.contains("\"subtype\":\"completed\"")
            || window.contains("\"case\":\"success\"")
        {
            SubagentStatus::Done
        } else if window.contains("\"case\":\"error\"") || window.contains("rejected") {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Running
        };
        out.push(Subagent {
            id,
            title: truncate(&title, 48),
            kind,
            status,
        });
    }

    out
}

fn surrounding<'a>(text: &'a str, idx: usize, radius: usize) -> &'a str {
    let start = idx.saturating_sub(radius);
    let end = (idx + radius).min(text.len());
    // Stay on char boundaries.
    let start = text
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text
        .char_indices()
        .rev()
        .find(|(i, _)| *i <= end)
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(text.len());
    &text[start..end]
}

fn extract_json_string(window: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let start = window.find(&pattern)? + pattern.len();
    let rest = &window[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(match n {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '"' => '"',
                    '\\' => '\\',
                    other => other,
                });
            }
            continue;
        }
        if c == '"' {
            break;
        }
        out.push(c);
        if out.len() > 200 {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_description_from_text(text: &str) -> Option<String> {
    extract_json_string(text, "description")
        .or_else(|| extract_json_string(text, "title"))
        .or_else(|| extract_json_string(text, "prompt"))
}

fn extract_kind_from_text(text: &str) -> Option<String> {
    extract_json_string(text, "subagentType")
        .or_else(|| extract_json_string(text, "subagent_type"))
        .or_else(|| extract_json_string(text, "type"))
}

fn text_indicates_done(text: &str) -> bool {
    text.contains("\"status\":\"completed\"")
        || text.contains("\"subtype\":\"completed\"")
        || text.contains("turn_ended")
        || text.contains("\"type\":\"result\"")
}

fn text_indicates_failed(text: &str) -> bool {
    text.contains("\"status\":\"error\"")
        || text.contains("\"status\":\"failed\"")
        || text.contains("\"status\":\"aborted\"")
}

fn parse_status(s: &str) -> SubagentStatus {
    match s.trim().to_ascii_lowercase().as_str() {
        "running" | "in_progress" | "started" => SubagentStatus::Running,
        "error" | "failed" | "aborted" | "cancelled" => SubagentStatus::Failed,
        _ => SubagentStatus::Done,
    }
}

fn is_stale_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() > 90
}

fn merge_subagent(existing: &mut Subagent, incoming: &Subagent) {
    if existing.title == existing.id && incoming.title != incoming.id {
        existing.title = incoming.title.clone();
    }
    if existing.kind.is_none() {
        existing.kind = incoming.kind.clone();
    }
    // Prefer terminal statuses from either side.
    existing.status = match (existing.status, incoming.status) {
        (SubagentStatus::Failed, _) | (_, SubagentStatus::Failed) => SubagentStatus::Failed,
        (SubagentStatus::Done, _) | (_, SubagentStatus::Done) => SubagentStatus::Done,
        _ => SubagentStatus::Running,
    };
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Create an empty chat and return its id (stdout from `cursor-agent create-chat`).
pub fn create_chat(agent_bin: &str, workspace: &Path) -> Result<String, String> {
    let output = std::process::Command::new(agent_bin)
        .arg("create-chat")
        .current_dir(workspace)
        .output()
        .map_err(|e| format!("failed to run create-chat: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "create-chat failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        return Err("create-chat returned empty chat id".into());
    }
    // Basic UUID-ish sanity (cursor returns hyphenated ids).
    if id.len() < 8 || id.contains(char::is_whitespace) {
        return Err(format!("create-chat returned unexpected id: {id}"));
    }
    Ok(id)
}
