//! Discover Prime Agent sessions and nested RLM children.
//!
//! Sessions live as flat JSONL files under `~/.prime/agent/sessions/`
//! (override with `PRIME_AGENT_SESSION_DIR`). Each file starts with a
//! `{"type":"session",…}` header; child sessions record `parentSession`.

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Running or finished subagent under a parent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: SubagentStatus,
    /// Resumable session id when this child has its own session file.
    ///
    /// Clicking the row opens `--resume <chat_id>` (or focuses an existing tab).
    pub chat_id: Option<String>,
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

/// Snapshot of session meta + nested subagents for one session id.
#[derive(Debug, Clone, Default)]
pub struct ChatSnapshot {
    pub title: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub subagents: Vec<Subagent>,
    /// Live status for spinner / needs-input (e.g. "Thinking…").
    pub activity: Option<String>,
    /// Generated story paragraph for the summary panel.
    pub summary: Option<SessionSummary>,
}

/// Story for the summary panel — one casual narrative about the whole turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// Latest user ask (short); also drives tab auto-rename.
    pub goal: Option<String>,
    /// Cohesive narrator prose (not status, not tool history, not agent chatter).
    /// May include light markdown: `**bold**`, `*italic*`, `` `code` ``.
    pub prose: String,
}

impl SessionSummary {
    pub fn is_empty(&self) -> bool {
        self.prose.trim().is_empty() && self.goal.is_none()
    }

    /// Best short string for tab auto-rename.
    pub fn title_hint(&self) -> Option<&str> {
        self.goal
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let p = self.prose.trim();
                (!p.is_empty()).then_some(p)
            })
    }
}

/// Session storage root (`PRIME_AGENT_SESSION_DIR` or `~/.prime/agent/sessions`).
pub fn sessions_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PRIME_AGENT_SESSION_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Ok(dir) = std::env::var("PRIME_AGENT_CODING_AGENT_SESSION_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".prime").join("agent").join("sessions"))
}

/// Locate a session JSONL by full UUID, partial id, or absolute path.
pub fn find_session_file(session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let as_path = Path::new(session_id);
    if as_path.is_file() {
        return Some(as_path.to_path_buf());
    }
    let root = sessions_root()?;
    let direct = root.join(format!("{session_id}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    // Partial UUID match (prime-agent --resume accepts unambiguous prefixes).
    let entries = fs::read_dir(&root).ok()?;
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.starts_with(session_id) || stem.contains(session_id) {
            matches.push(path);
        }
    }
    if matches.len() == 1 {
        Some(matches.remove(0))
    } else {
        None
    }
}

/// After a new PTY starts, bind the newest session whose cwd matches `workspace`.
///
/// Prefers files modified at or after `not_before` (spawn wall time).
pub fn discover_newest_session(workspace: &Path, not_before: SystemTime) -> Option<String> {
    let root = sessions_root()?;
    let workspace = canonicalize_lossy(workspace);
    let not_before_ms = system_time_ms(not_before).saturating_sub(2_000);

    let mut best: Option<(u64, String)> = None;
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .map(system_time_ms)
            .unwrap_or(0);
        if mtime_ms < not_before_ms {
            continue;
        }
        let Some(header) = read_session_header(&path) else {
            continue;
        };
        if header.parent_session.is_some() {
            continue;
        }
        let Some(cwd) = header.cwd.as_deref() else {
            continue;
        };
        if canonicalize_lossy(Path::new(cwd)) != workspace {
            continue;
        }
        let id = header
            .id
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })?;
        match &best {
            Some((best_ms, _)) if *best_ms >= mtime_ms => {}
            _ => best = Some((mtime_ms, id)),
        }
    }
    best.map(|(_, id)| id)
}

/// Poll session meta + nested RLM children for a session id.
pub fn poll_chat(chat_id: &str, _workspace: Option<&Path>) -> ChatSnapshot {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return ChatSnapshot::default();
    }
    let Some(path) = find_session_file(chat_id) else {
        return ChatSnapshot::default();
    };

    let mut snap = ChatSnapshot::default();
    let parsed = parse_session_file(&path);
    snap.title = parsed.name.clone().or(parsed.first_user_goal.clone());
    snap.updated_at_ms = parsed.updated_at_ms.or_else(|| file_mtime_ms(&path));
    snap.activity = parsed.activity.clone();
    snap.summary = build_summary(&parsed);
    snap.subagents = list_child_sessions(&path, &parsed.id);
    snap
}

struct SessionHeader {
    id: Option<String>,
    cwd: Option<String>,
    parent_session: Option<String>,
}

struct ParsedSession {
    id: Option<String>,
    name: Option<String>,
    first_user_goal: Option<String>,
    last_user_goal: Option<String>,
    activity: Option<String>,
    updated_at_ms: Option<u64>,
    tool_events: usize,
    assistant_chars: usize,
    ended: bool,
}

fn read_session_header(path: &Path) -> Option<SessionHeader> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    Some(SessionHeader {
        id: value
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        cwd: value
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        parent_session: value
            .get("parentSession")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

fn parse_session_file(path: &Path) -> ParsedSession {
    let mut parsed = ParsedSession {
        id: None,
        name: None,
        first_user_goal: None,
        last_user_goal: None,
        activity: None,
        updated_at_ms: None,
        tool_events: 0,
        assistant_chars: 0,
        ended: true,
    };

    let Ok(file) = fs::File::open(path) else {
        return parsed;
    };
    let reader = BufReader::new(file);
    let mut last_ts_ms = None;
    let mut last_role: Option<String> = None;
    let mut agent_status: Option<String> = None;

    for line in reader.lines().flatten() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
            if let Some(ms) = parse_iso_ms(ts) {
                last_ts_ms = Some(ms);
            }
        }

        match entry_type {
            "session" => {
                parsed.id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            "session_info" => {
                if let Some(name) = value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    parsed.name = Some(name.to_string());
                }
            }
            "agent_status" => {
                if let Some(summary) = value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    agent_status = Some(summary.to_string());
                } else if let Some(status) = value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    agent_status = Some(status.to_string());
                }
            }
            "message" => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
                last_role = Some(role.to_string());
                match role {
                    "user" => {
                        if let Some(text) = message_text(message) {
                            let goal = sanitize_goal(&text);
                            if !goal.is_empty() {
                                if parsed.first_user_goal.is_none() {
                                    parsed.first_user_goal = Some(goal.clone());
                                }
                                parsed.last_user_goal = Some(goal);
                            }
                        }
                        parsed.ended = false;
                    }
                    "assistant" => {
                        if let Some(content) = message.get("content").and_then(|v| v.as_array()) {
                            for part in content {
                                match part.get("type").and_then(|v| v.as_str()) {
                                    Some("text") => {
                                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                            parsed.assistant_chars += t.chars().count();
                                        }
                                    }
                                    Some("toolCall") | Some("tool_use") => {
                                        parsed.tool_events += 1;
                                        parsed.ended = false;
                                        if let Some(name) =
                                            part.get("name").and_then(|v| v.as_str())
                                        {
                                            parsed.activity =
                                                Some(format_tool_activity(name, part));
                                        }
                                    }
                                    Some("thinking") => {
                                        parsed.ended = false;
                                        if parsed.activity.is_none() {
                                            parsed.activity = Some("Thinking…".into());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if message.get("stopReason").and_then(|v| v.as_str()) == Some("stop") {
                            parsed.ended = true;
                        }
                    }
                    "toolResult" => {
                        parsed.tool_events += 1;
                        parsed.ended = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    parsed.updated_at_ms = last_ts_ms.or_else(|| file_mtime_ms(path));
    if let Some(status) = agent_status {
        parsed.activity = Some(status);
    } else if parsed.ended {
        parsed.activity = match last_role.as_deref() {
            Some("user") => Some("Waiting for input".into()),
            _ => Some("Idle".into()),
        };
    } else if parsed.activity.is_none() {
        parsed.activity = Some("Working…".into());
    }
    parsed
}

fn list_child_sessions(parent_path: &Path, parent_id: &Option<String>) -> Vec<Subagent> {
    let Some(root) = sessions_root() else {
        return Vec::new();
    };
    let parent_path_str = parent_path.to_string_lossy();
    let parent_stem = parent_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut children = Vec::new();

    let Ok(entries) = fs::read_dir(root) else {
        return children;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == parent_path {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(header) = read_session_header(&path) else {
            continue;
        };
        let Some(parent) = header.parent_session.as_deref() else {
            continue;
        };
        let linked = parent == parent_path_str
            || Path::new(parent) == parent_path
            || parent_id
                .as_deref()
                .is_some_and(|id| parent.contains(id))
            || (!parent_stem.is_empty() && parent.contains(parent_stem));
        if !linked {
            continue;
        }
        let id = header
            .id
            .clone()
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| path.display().to_string());
        let child = parse_session_file(&path);
        let title = child
            .name
            .or(child.first_user_goal)
            .unwrap_or_else(|| "Subagent".into());
        let status = match child.activity.as_deref() {
            Some("Idle") | Some("Waiting for input") => SubagentStatus::Done,
            Some(a) if a.to_lowercase().contains("fail") || a.to_lowercase().contains("error") => {
                SubagentStatus::Failed
            }
            Some(_) if child.ended => SubagentStatus::Done,
            Some(_) => SubagentStatus::Running,
            None if child.ended => SubagentStatus::Done,
            None => SubagentStatus::Running,
        };
        children.push(Subagent {
            id: id.clone(),
            title: truncate(&title, 64),
            kind: Some("rlm".into()),
            status,
            chat_id: Some(id),
        });
    }

    children.sort_by(|a, b| a.title.cmp(&b.title));
    children
}

fn build_summary(parsed: &ParsedSession) -> Option<SessionSummary> {
    let goal = parsed
        .last_user_goal
        .clone()
        .or_else(|| parsed.first_user_goal.clone());
    let topic = goal
        .as_deref()
        .map(casual_topic)
        .unwrap_or_else(|| "this session".into());
    let prose = if parsed.ended {
        format!("Wrapped up on {topic}.")
    } else if parsed.tool_events > 0 {
        format!("Working through {topic} — tools in motion.")
    } else if parsed.assistant_chars > 0 {
        format!("Talking through {topic}.")
    } else if goal.is_some() {
        format!("Looking at {topic}.")
    } else {
        return None;
    };
    Some(SessionSummary { goal, prose })
}

fn message_text(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

fn sanitize_goal(text: &str) -> String {
    let mut s = text.trim().to_string();
    for marker in ["<user_query>", "</user_query>", "<user>", "</user>"] {
        s = s.replace(marker, "");
    }
    let s = s.trim();
    truncate(s, 80)
}

fn casual_topic(goal: &str) -> String {
    let g = goal.trim();
    for prefix in [
        "fix the ",
        "fix ",
        "add a ",
        "add ",
        "implement ",
        "create ",
        "update ",
        "refactor ",
    ] {
        if let Some(rest) = g.to_lowercase().strip_prefix(prefix) {
            // Preserve original casing of the remainder when lengths align poorly —
            // use the stripped lowercase form with a "that " prefix.
            return format!("that {rest}");
        }
    }
    format!("that {g}")
}

fn format_tool_activity(name: &str, part: &Value) -> String {
    let args = part
        .get("arguments")
        .or_else(|| part.get("input"))
        .cloned()
        .unwrap_or(Value::Null);
    match name {
        "ipython" | "IPython" => {
            if let Some(code) = args.get("code").and_then(|v| v.as_str()) {
                let first = code.lines().next().unwrap_or(code).trim();
                format!("Running {}", truncate(first, 40))
            } else {
                "Running IPython".into()
            }
        }
        "bash" | "Bash" | "Shell" => {
            if let Some(cmd) = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(|v| v.as_str())
            {
                format!("Running {}", truncate(cmd, 48))
            } else {
                format!("Running {name}")
            }
        }
        "read" | "Read" => {
            if let Some(path) = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(|v| v.as_str())
            {
                let name = Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path);
                format!("Reading {name}")
            } else {
                "Reading".into()
            }
        }
        other => format!("Using {other}"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .map(system_time_ms)
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn parse_iso_ms(ts: &str) -> Option<u64> {
    // Accept …Z or …±HH:MM; chrono isn't a dependency, so use a light parse via
    // `date` is overkill — prefer file mtime when this fails.
    // RFC3339-ish: 2024-12-03T14:00:00.000Z
    let ts = ts.trim();
    if ts.len() < 19 {
        return None;
    }
    let (date, rest) = ts.split_at(10);
    let time = rest.trim_start_matches('T');
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    let mut tparts = time.split(|c| c == ':' || c == '.' || c == 'Z' || c == '+' || c == '-');
    let hh: i64 = tparts.next()?.parse().ok()?;
    let mm: i64 = tparts.next()?.parse().ok()?;
    let ss: i64 = tparts.next()?.parse().ok()?;
    // Days from civil date (Howard Hinnant algorithm) → Unix seconds.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 } as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d as u64 - 1;
    let days = era * 146097 + doe as i64 - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some((secs.max(0) as u64) * 1000)
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_header_and_goal() {
        let dir = std::env::temp_dir().join(format!(
            "manager-prime-test-{}",
            system_time_ms(SystemTime::now())
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abcd-1234.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session","version":3,"id":"abcd-1234","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp/proj"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"session_info","id":"n1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","name":"Refactor auth"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:02.000Z","message":{{"role":"user","content":"fix the streaming bug","timestamp":1}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"message","id":"m2","parentId":"m1","timestamp":"2024-12-03T14:00:03.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"c1","name":"ipython","arguments":{{"code":"print(1)"}}}}],"stopReason":"toolUse","timestamp":2}}}}"#
        )
        .unwrap();

        std::env::set_var("PRIME_AGENT_SESSION_DIR", &dir);
        let snap = poll_chat("abcd-1234", None);
        let _ = fs::remove_dir_all(&dir);
        std::env::remove_var("PRIME_AGENT_SESSION_DIR");

        assert_eq!(snap.title.as_deref(), Some("Refactor auth"));
        assert!(
            snap.activity
                .as_deref()
                .is_some_and(|a| a.contains("Running") || a.contains("ipython") || a.contains("Working")),
            "activity={:?}",
            snap.activity
        );
        let summary = snap.summary.expect("summary");
        assert!(
            summary.prose.to_lowercase().contains("streaming")
                || summary.goal.as_deref() == Some("fix the streaming bug"),
            "{summary:?}"
        );
    }

    #[test]
    fn casual_topic_strips_imperatives() {
        assert_eq!(casual_topic("fix the streaming bug"), "that streaming bug");
        assert_eq!(casual_topic("add a summary bar"), "that summary bar");
    }

    #[test]
    fn child_sessions_link_via_parent() {
        let dir = std::env::temp_dir().join(format!(
            "manager-prime-child-{}",
            system_time_ms(SystemTime::now())
        ));
        fs::create_dir_all(&dir).unwrap();
        let parent = dir.join("parent-aaaa.jsonl");
        let child = dir.join("child-bbbb.jsonl");
        fs::write(
            &parent,
            r#"{"type":"session","version":3,"id":"parent-aaaa","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp/proj"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"parent work","timestamp":1}}
"#,
        )
        .unwrap();
        fs::write(
            &child,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": "child-bbbb",
                    "timestamp": "2024-12-03T14:00:02.000Z",
                    "cwd": "/tmp/proj",
                    "parentSession": parent.display().to_string(),
                }),
                r#"{"type":"session_info","id":"n1","parentId":null,"timestamp":"2024-12-03T14:00:03.000Z","name":"auth-reviewer"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:04.000Z","message":{"role":"user","content":"review auth","timestamp":1}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2024-12-03T14:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","timestamp":2}}"#
            ),
        )
        .unwrap();

        std::env::set_var("PRIME_AGENT_SESSION_DIR", &dir);
        let snap = poll_chat("parent-aaaa", None);
        let _ = fs::remove_dir_all(&dir);
        std::env::remove_var("PRIME_AGENT_SESSION_DIR");

        assert_eq!(snap.subagents.len(), 1);
        assert_eq!(snap.subagents[0].title, "auth-reviewer");
        assert_eq!(
            snap.subagents[0].chat_id.as_deref(),
            Some("child-bbbb")
        );
    }
}
