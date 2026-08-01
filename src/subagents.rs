//! Discover Cursor Task/subagents for a bound chat id.
//!
//! Sources (merged by id):
//! - `~/.cursor/chats/*/<chat_id>/meta.json` + legacy `subagents/`
//! - Task records in that chat’s `store.db` / WAL
//! - `~/.cursor/projects/*/agent-transcripts/<chat_id>/` (jsonl Task tool_use + `subagents/`)

use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Running or finished subagent/Task under a parent chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: SubagentStatus,
    /// Resumable Cursor chat id when this Task has its own `isSubagent` chat.
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

/// Snapshot of chat meta + nested subagents for one chat id.
#[derive(Debug, Clone, Default)]
pub struct ChatSnapshot {
    pub title: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub subagents: Vec<Subagent>,
    /// Live status for spinner / needs-input (e.g. "Thinking…", "Waiting for input").
    pub activity: Option<String>,
    /// Structured retrospective of work done this turn.
    pub summary: Option<SessionSummary>,
}

/// Past-tense session recap for the summary panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// Latest user ask (short).
    pub goal: Option<String>,
    /// Discrete work items (edited files, commands, …).
    pub lines: Vec<SummaryLine>,
    /// Closing assistant prose when the turn has wound down.
    pub note: Option<String>,
}

impl SessionSummary {
    pub fn is_empty(&self) -> bool {
        self.goal.is_none() && self.lines.is_empty() && self.note.is_none()
    }

    /// Best short string for tab auto-rename.
    pub fn title_hint(&self) -> Option<&str> {
        self.goal
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.note.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| self.lines.first().map(|l| l.text.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    pub kind: SummaryKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    Edited,
    Ran,
    Searched,
    Read,
    Delegated,
    Other,
}

impl SummaryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Edited => "Edited",
            Self::Ran => "Ran",
            Self::Searched => "Searched",
            Self::Read => "Read",
            Self::Delegated => "Delegated",
            Self::Other => "Also",
        }
    }
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

/// Poll meta + nested Task/subagents for a chat id.
///
/// `workspace` narrows agent-transcript lookup to the matching project slug when set.
pub fn poll_chat(chat_id: &str, workspace: Option<&Path>) -> ChatSnapshot {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return ChatSnapshot::default();
    }

    let mut snap = ChatSnapshot::default();
    let mut by_id: BTreeMap<String, Subagent> = BTreeMap::new();

    if let Some(dir) = find_chat_dir(chat_id) {
        if let Some((title, updated)) = read_meta(&dir) {
            snap.title = title;
            snap.updated_at_ms = updated;
        }
        for sub in list_subagents_dir(&dir.join("subagents")) {
            upsert(&mut by_id, sub);
        }
        for sub in scan_task_blobs(&dir) {
            upsert(&mut by_id, sub);
        }
        for sub in scan_linked_subagent_chats(&dir, workspace) {
            upsert(&mut by_id, sub);
        }
    }

    let transcript_dirs = find_transcript_dirs(chat_id, workspace);
    for transcript_dir in &transcript_dirs {
        for sub in list_subagents_dir(&transcript_dir.join("subagents")) {
            upsert(&mut by_id, sub);
        }
        for sub in scan_transcript_jsonl(transcript_dir, chat_id) {
            upsert(&mut by_id, sub);
        }
    }

    // Prefer chat_id as the map key once known so Task-tool rows merge into
    // the resumable subagent chat instead of sitting beside it.
    let mut merged: BTreeMap<String, Subagent> = BTreeMap::new();
    for sub in by_id.into_values() {
        let key = sub
            .chat_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| sub.id.clone());
        merged
            .entry(key)
            .and_modify(|existing| merge_subagent(existing, &sub))
            .or_insert(sub);
    }

    snap.subagents = collapse_task_rows(merged.into_values().collect())
        .into_iter()
        .filter(is_actionable_subagent)
        .collect();
    snap.subagents.sort_by(|a, b| {
        status_rank(a.status)
            .cmp(&status_rank(b.status))
            .then(a.title.cmp(&b.title))
            .then(a.id.cmp(&b.id))
    });

    let (activity, summary) = summarize_transcript(chat_id, &snap.subagents, transcript_dirs);
    snap.activity = activity;
    snap.summary = summary;
    snap
}

/// Live status + past-tense recap from the transcript tail.
fn summarize_transcript(
    chat_id: &str,
    subagents: &[Subagent],
    transcript_dirs: Vec<PathBuf>,
) -> (Option<String>, Option<SessionSummary>) {
    let parsed = transcript_dirs
        .iter()
        .find_map(|dir| parse_transcript_dir(dir, chat_id));

    let running: Vec<&Subagent> = subagents
        .iter()
        .filter(|s| s.status.is_live())
        .collect();

    let Some(parsed) = parsed else {
        let activity = if !running.is_empty() {
            Some(format_running_tasks(&running))
        } else {
            None
        };
        return (activity, None);
    };

    let mut activity = parsed.live_status;
    if activity
        .as_deref()
        .is_some_and(|s| s.starts_with("Delegating"))
        && !running.is_empty()
    {
        activity = Some(format_running_tasks(&running));
    } else if activity.is_none() && !running.is_empty() {
        activity = Some(format_running_tasks(&running));
    }

    (activity, parsed.summary)
}

fn format_running_tasks(running: &[&Subagent]) -> String {
    match running {
        [one] => truncate(&format!("Task · {}", one.title), 72),
        [one, ..] => truncate(
            &format!("{} tasks · {}", running.len(), one.title),
            72,
        ),
        [] => "Working…".into(),
    }
}

struct TranscriptDigest {
    live_status: Option<String>,
    summary: Option<SessionSummary>,
}

fn parse_transcript_dir(transcript_dir: &Path, chat_id: &str) -> Option<TranscriptDigest> {
    let enc = encode_conversation_id(chat_id);
    let candidates = [
        transcript_dir.join(format!("{enc}.jsonl")),
        transcript_dir.join(format!("{chat_id}.jsonl")),
        transcript_dir.join(format!("{enc}.txt")),
    ];
    for path in candidates {
        if let Some(digest) = digest_jsonl_tail(&path) {
            return Some(digest);
        }
    }
    None
}

/// Scan the last ~128 KiB of a transcript for live status + a retrospective summary.
fn digest_jsonl_tail(path: &Path) -> Option<TranscriptDigest> {
    let Ok(mut file) = fs::File::open(path) else {
        return None;
    };
    let Ok(meta) = file.metadata() else {
        return None;
    };
    let size = meta.len();
    if size == 0 {
        return None;
    }
    const TAIL: u64 = 128 * 1024;
    let start = size.saturating_sub(TAIL);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return None;
    }
    let lines: Vec<&str> = buf.lines().collect();
    let start_idx = if start > 0 { 1 } else { 0 };
    if start_idx >= lines.len() {
        return None;
    }

    let mut last_tool: Option<String> = None;
    let mut last_assistant_text: Option<String> = None;
    let mut last_role: Option<&'static str> = None;
    let mut turn_ended = false;

    // Retrospective: aggregate tools since the latest user message.
    let mut user_goal: Option<String> = None;
    let mut edited: Vec<String> = Vec::new();
    let mut ran: Vec<String> = Vec::new();
    let mut searched: Vec<String> = Vec::new();
    let mut tasks: Vec<String> = Vec::new();
    let mut read_count: usize = 0;
    let mut other: Vec<String> = Vec::new();

    for line in &lines[start_idx..] {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if value.get("type").and_then(|v| v.as_str()) == Some("turn_ended") {
            turn_ended = true;
            last_tool = None;
            last_role = Some("ended");
            continue;
        }

        let role = value.get("role").and_then(|v| v.as_str());
        let content = value
            .pointer("/message/content")
            .or_else(|| value.get("content"));
        let Some(parts) = content.and_then(|c| c.as_array()) else {
            continue;
        };

        match role {
            Some("user") => {
                turn_ended = false;
                last_tool = None;
                last_assistant_text = None;
                last_role = Some("user");
                // New turn — reset retrospective buckets.
                edited.clear();
                ran.clear();
                searched.clear();
                tasks.clear();
                other.clear();
                read_count = 0;
                user_goal = parts.iter().find_map(|p| {
                    if p.get("type").and_then(|v| v.as_str()) != Some("text") {
                        return None;
                    }
                    let t = p.get("text").and_then(|v| v.as_str())?;
                    extract_user_goal(t)
                });
            }
            Some("assistant") => {
                turn_ended = false;
                last_role = Some("assistant");
                let mut tool_in_msg = None;
                let mut text_in_msg = None;
                for part in parts {
                    match part.get("type").and_then(|v| v.as_str()) {
                        Some("tool_use") => {
                            record_tool_for_summary(
                                part,
                                &mut edited,
                                &mut ran,
                                &mut searched,
                                &mut tasks,
                                &mut other,
                                &mut read_count,
                            );
                            if let Some(label) = format_tool_activity(part) {
                                tool_in_msg = Some(label);
                            }
                        }
                        Some("text") => {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                let t = collapse_ws(t);
                                if !t.is_empty() {
                                    text_in_msg = Some(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(t) = tool_in_msg {
                    last_tool = Some(t);
                    last_assistant_text = None;
                } else if let Some(t) = text_in_msg {
                    last_tool = None;
                    last_assistant_text = Some(t);
                }
            }
            _ => {}
        }
    }

    let live_status = if turn_ended || last_role == Some("ended") {
        Some("Waiting for input".into())
    } else if let Some(tool) = last_tool {
        Some(tool)
    } else if last_role == Some("user") {
        Some("Thinking…".into())
    } else if let Some(ref text) = last_assistant_text {
        Some(truncate(&first_clause(text), 72))
    } else {
        None
    };

    let summary = build_session_summary(
        user_goal.as_deref(),
        &edited,
        &ran,
        &searched,
        &tasks,
        &other,
        read_count,
        last_assistant_text
            .as_deref()
            .filter(|_| turn_ended || last_role == Some("ended")),
    );

    Some(TranscriptDigest {
        live_status,
        summary,
    })
}

fn extract_user_goal(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let goal = if let Some(inner) = trimmed
        .split("<user_query>")
        .nth(1)
        .and_then(|s| s.split("</user_query>").next())
    {
        collapse_ws(inner)
    } else {
        // Skip system/timestamp wrappers; take first real line.
        let without_ts = if let Some(rest) = trimmed.split("</timestamp>").last() {
            rest.trim()
        } else {
            trimmed
        };
        collapse_ws(without_ts)
    };
    if goal.is_empty() || goal.starts_with('<') {
        None
    } else {
        Some(truncate(&goal, 140))
    }
}

fn record_tool_for_summary(
    part: &Value,
    edited: &mut Vec<String>,
    ran: &mut Vec<String>,
    searched: &mut Vec<String>,
    tasks: &mut Vec<String>,
    other: &mut Vec<String>,
    read_count: &mut usize,
) {
    let Some(name) = part.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let input = part.get("input").unwrap_or(&Value::Null);
    match name {
        "Read" => {
            *read_count += 1;
        }
        "Write" | "StrReplace" | "EditNotebook" => {
            let path = input
                .get("path")
                .or_else(|| input.get("target_notebook"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            push_unique(edited, basename(path));
        }
        "Delete" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("file");
            push_unique(edited, format!("−{}", basename(path)));
        }
        "Shell" | "Bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("command");
            push_unique(ran, short_command(cmd));
        }
        "Grep" => {
            let pat = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("…");
            push_unique(searched, truncate(pat, 28));
        }
        "Glob" => {
            let g = input
                .get("glob_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("…");
            push_unique(searched, truncate(g, 28));
        }
        "Task" => {
            let desc = input
                .get("description")
                .or_else(|| input.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            push_unique(tasks, truncate(desc, 36));
        }
        "WebSearch" | "WebFetch" => {
            push_unique(other, "Looked up web".into());
        }
        "AskQuestion" => {}
        "TodoWrite" | "AwaitShell" | "Await" => {}
        other_name => {
            push_unique(other, format!("Used {other_name}"));
        }
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.iter().any(|x| x == &item) {
        list.push(item);
    }
}

fn build_session_summary(
    user_goal: Option<&str>,
    edited: &[String],
    ran: &[String],
    searched: &[String],
    tasks: &[String],
    other: &[String],
    read_count: usize,
    closing_text: Option<&str>,
) -> Option<SessionSummary> {
    let mut lines: Vec<SummaryLine> = Vec::new();

    if !edited.is_empty() {
        lines.push(SummaryLine {
            kind: SummaryKind::Edited,
            text: join_and(edited, 5),
        });
    }
    if !ran.is_empty() {
        let label_text = join_and(ran, 3);
        lines.push(SummaryLine {
            kind: SummaryKind::Ran,
            text: if looks_like_check(ran) && !edited.is_empty() {
                format!("verified with {label_text}")
            } else {
                label_text
            },
        });
    }
    if !searched.is_empty() {
        lines.push(SummaryLine {
            kind: SummaryKind::Searched,
            text: join_and(searched, 3),
        });
    }
    if read_count > 0 {
        lines.push(SummaryLine {
            kind: SummaryKind::Read,
            text: if read_count == 1 {
                "1 file".into()
            } else {
                format!("{read_count} files")
            },
        });
    }
    if !tasks.is_empty() {
        lines.push(SummaryLine {
            kind: SummaryKind::Delegated,
            text: join_and(tasks, 3),
        });
    }
    for o in other.iter().take(3) {
        lines.push(SummaryLine {
            kind: SummaryKind::Other,
            text: o.clone(),
        });
    }

    let note = closing_text
        .map(|t| truncate(&first_clause(t), 220))
        .filter(|c| c.chars().count() >= 12);

    let goal = user_goal
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(s, 140));

    let summary = SessionSummary {
        goal,
        lines,
        note,
    };
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

fn looks_like_check(ran: &[String]) -> bool {
    ran.iter().any(|c| {
        let l = c.to_lowercase();
        l.contains("check")
            || l.contains("test")
            || l.contains("build")
            || l.contains("lint")
            || l.contains("clippy")
            || l.contains("tsc")
            || l.contains("pytest")
            || l.contains("npm test")
            || l.contains("cargo t")
    })
}

fn join_and(items: &[String], max: usize) -> String {
    let shown = if items.len() > max {
        max
    } else {
        items.len()
    };
    let slice = &items[..shown];
    let mut s = match slice {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        many => {
            let last = many.last().unwrap();
            let head = many[..many.len() - 1].join(", ");
            format!("{head}, and {last}")
        }
    };
    if items.len() > max {
        s.push_str(&format!(" (+{})", items.len() - max));
    }
    s
}

fn format_tool_activity(part: &Value) -> Option<String> {
    let name = part.get("name").and_then(|v| v.as_str())?;
    let input = part.get("input").unwrap_or(&Value::Null);

    let label = match name {
        "Shell" | "Bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("command");
            format!("Running {}", short_command(cmd))
        }
        "Read" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("file");
            format!("Reading {}", basename(path))
        }
        "Write" | "StrReplace" | "EditNotebook" => {
            let path = input
                .get("path")
                .or_else(|| input.get("target_notebook"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            format!("Editing {}", basename(path))
        }
        "Delete" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("file");
            format!("Deleting {}", basename(path))
        }
        "Grep" => {
            let pat = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("…");
            format!("Searching {}", truncate(pat, 40))
        }
        "Glob" => {
            let g = input
                .get("glob_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("…");
            format!("Finding {}", truncate(g, 40))
        }
        "WebSearch" => {
            let q = input
                .get("search_term")
                .or_else(|| input.get("query"))
                .and_then(|v| v.as_str())
                .unwrap_or("web");
            format!("Web · {}", truncate(q, 48))
        }
        "WebFetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("url");
            format!("Fetching {}", truncate(url, 48))
        }
        "Task" => {
            let desc = input
                .get("description")
                .or_else(|| input.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("subagent");
            format!("Delegating · {}", truncate(desc, 48))
        }
        "AwaitShell" | "Await" => "Waiting on command…".into(),
        "AskQuestion" => {
            let title = input
                .get("title")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    input
                        .get("questions")
                        .and_then(|q| q.as_array())
                        .and_then(|qs| qs.first())
                        .and_then(|q| q.get("prompt"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("question");
            format!("Waiting for answer · {}", truncate(title, 40))
        }
        "TodoWrite" => "Updating todos".into(),
        "GenerateImage" => "Generating image".into(),
        "CallMcpTool" => {
            let tool = input
                .get("toolName")
                .or_else(|| input.get("tool_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("MCP");
            format!("MCP · {}", truncate(tool, 40))
        }
        other => format!("Using {other}"),
    };
    Some(truncate(&label, 72))
}

fn short_command(cmd: &str) -> String {
    let one_line = cmd.lines().next().unwrap_or(cmd).trim();
    // Drop noisy wrappers; keep the gist.
    let cleaned = one_line
        .trim_start_matches("cd ")
        .split(" && ")
        .last()
        .unwrap_or(one_line)
        .trim();
    truncate(cleaned, 48)
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_clause(s: &str) -> String {
    let s = s.trim();
    for sep in [". ", "! ", "? ", "\n"] {
        if let Some((head, _)) = s.split_once(sep) {
            let head = head.trim();
            if head.chars().count() >= 12 {
                return head.to_string();
            }
        }
    }
    s.to_string()
}

fn upsert(by_id: &mut BTreeMap<String, Subagent>, sub: Subagent) {
    by_id
        .entry(sub.id.clone())
        .and_modify(|existing| merge_subagent(existing, &sub))
        .or_insert(sub);
}

fn chats_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cursor").join("chats"))
}

fn projects_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cursor").join("projects"))
}

/// Cursor project dir slug: non-alnum → `-`, collapse, trim edges.
fn project_slug(workspace: &Path) -> String {
    let raw = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let s = raw.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Encode conversation id the same way cursor-agent’s `sh()` does.
fn encode_conversation_id(id: &str) -> String {
    let mut t = urlencoding_light(id);
    t = t.replace('%', "_");
    if t.len() > 200 {
        t.truncate(200);
    }
    t
}

fn urlencoding_light(s: &str) -> String {
    // UUIDs and typical chat ids need no escaping; fall back for odd ids.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn find_transcript_dirs(chat_id: &str, workspace: Option<&Path>) -> Vec<PathBuf> {
    let enc = encode_conversation_id(chat_id);
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |dir: PathBuf| {
        if dir.is_dir() && seen.insert(dir.clone()) {
            out.push(dir);
        }
    };

    if let Some(ws) = workspace {
        // Known workspace → one project dir (avoid scanning every Cursor project).
        if let Some(root) = projects_root() {
            push(root.join(project_slug(ws)).join("agent-transcripts").join(&enc));
        }
    } else if let Some(root) = projects_root() {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("agent-transcripts").join(&enc);
                push(candidate);
            }
        }
    }

    out
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

fn read_is_subagent(chat_dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(chat_dir.join("meta.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value.get("isSubagent").and_then(|v| v.as_bool()) == Some(true)
}

fn read_is_subagent_id(chat_id: &str) -> bool {
    find_chat_dir(chat_id).is_some_and(|dir| read_is_subagent(&dir))
}

/// Cursor marks subagent agent ids as `$<uuid>` inside parent store blobs.
fn extract_dollar_uuids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 37 <= bytes.len() {
        if bytes[i] == b'$' && is_uuid_at(bytes, i + 1) {
            let id = text[i + 1..i + 37].to_string();
            if !out.iter().any(|x| x == &id) {
                out.push(id);
            }
            i += 37;
            continue;
        }
        i += 1;
    }
    out
}

fn is_uuid_at(bytes: &[u8], start: usize) -> bool {
    // 8-4-4-4-12 hex with hyphens
    const PATTERN: [u8; 36] = [
        1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1,
    ];
    if start + 36 > bytes.len() {
        return false;
    }
    for (offset, kind) in PATTERN.iter().enumerate() {
        let b = bytes[start + offset];
        if *kind == 0 {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Nested `isSubagent` chats referenced from the parent store (`$uuid` markers).
fn scan_linked_subagent_chats(parent_dir: &Path, workspace: Option<&Path>) -> Vec<Subagent> {
    let mut store_text = String::new();
    for name in ["store.db", "store.db-wal"] {
        let path = parent_dir.join(name);
        if let Ok(bytes) = fs::read(&path) {
            store_text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    if store_text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in extract_dollar_uuids(&store_text) {
        if !seen.insert(id.clone()) || !read_is_subagent_id(&id) {
            continue;
        }
        let mut sub = match subagent_from_chat(&id, workspace) {
            Some(s) => s,
            None => continue,
        };
        if let Some(title) = title_near_dollar_uuid(&store_text, &id) {
            sub.title = title;
        }
        out.push(sub);
    }
    out
}

fn title_near_dollar_uuid(text: &str, chat_id: &str) -> Option<String> {
    let marker = format!("${chat_id}");
    let idx = text.find(&marker)?;
    // Task description usually sits in the same result blob as `$agentId`.
    let window = surrounding(text, idx, 2500);
    extract_json_string(window, "description")
        .or_else(|| extract_json_string(window, "title"))
        .filter(|t| is_meaningful_task_title(t))
        .map(|t| truncate(&t, 48))
        .or_else(|| {
            // Protobuf / binary blobs store the short description as a raw string.
            window.lines().find_map(|line| {
                let line = line.trim();
                if is_meaningful_task_title(line) && line.chars().count() <= 64 {
                    Some(truncate(line, 48))
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            // Binary blob: description may not be on its own line — scan for a
            // short printable run before the marker.
            let before = &window[..window.find(&marker).unwrap_or(0)];
            let mut best: Option<String> = None;
            for run in extract_printable_runs(before) {
                if is_meaningful_task_title(&run) && run.chars().count() <= 64 {
                    best = Some(truncate(&run, 48));
                }
            }
            best
        })
}

fn extract_printable_runs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_graphic() || c == ' ' {
            cur.push(c);
        } else if !cur.is_empty() {
            let t = cur.trim().to_string();
            if t.len() >= 3 {
                out.push(t);
            }
            cur.clear();
        }
    }
    let t = cur.trim().to_string();
    if t.len() >= 3 {
        out.push(t);
    }
    out
}

fn subagent_from_chat(chat_id: &str, workspace: Option<&Path>) -> Option<Subagent> {
    let dir = find_chat_dir(chat_id)?;
    if !read_is_subagent(&dir) {
        return None;
    }
    let (meta_title, _) = read_meta(&dir).unwrap_or((None, None));
    let transcript_dirs = find_transcript_dirs(chat_id, workspace);
    let digest = transcript_dirs
        .iter()
        .find_map(|d| parse_transcript_dir(d, chat_id));
    let title = meta_title
        .or_else(|| {
            transcript_dirs
                .iter()
                .find_map(|d| first_user_goal_from_dir(d, chat_id))
        })
        .filter(|t| is_meaningful_task_title(t))
        .unwrap_or_else(|| "Subagent".into());

    let status = if let Some(ref d) = digest {
        match d.live_status.as_deref() {
            Some("Waiting for input") => SubagentStatus::Done,
            Some(s) if s.starts_with("Waiting for answer") => SubagentStatus::Running,
            Some(_) => SubagentStatus::Running,
            None => {
                if is_stale_file(&dir.join("store.db")) {
                    SubagentStatus::Done
                } else {
                    SubagentStatus::Running
                }
            }
        }
    } else if is_stale_file(&dir.join("store.db")) {
        SubagentStatus::Done
    } else {
        SubagentStatus::Running
    };

    Some(Subagent {
        id: chat_id.to_string(),
        title: truncate(&title, 48),
        kind: None,
        status,
        chat_id: Some(chat_id.to_string()),
    })
}

fn first_user_goal_from_dir(transcript_dir: &Path, chat_id: &str) -> Option<String> {
    let enc = encode_conversation_id(chat_id);
    let candidates = [
        transcript_dir.join(format!("{enc}.jsonl")),
        transcript_dir.join(format!("{chat_id}.jsonl")),
        transcript_dir.join(format!("{enc}.txt")),
    ];
    for path in candidates {
        if let Some(goal) = first_user_goal_from_jsonl(&path) {
            return Some(goal);
        }
    }
    None
}

fn first_user_goal_from_jsonl(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().flatten() {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let content = value
            .pointer("/message/content")
            .or_else(|| value.get("content"))?;
        let parts = content.as_array()?;
        for part in parts {
            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let t = part.get("text").and_then(|v| v.as_str())?;
            if let Some(goal) = extract_user_goal(t) {
                // Prefer a short first line / clause as the sidebar title.
                return Some(truncate(&first_clause(&goal), 48));
            }
        }
    }
    None
}

fn list_subagents_dir(dir: &Path) -> Vec<Subagent> {
    let entries = match fs::read_dir(dir) {
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
        let id = sanitize_id(&stem);
        let chat_id = find_chat_dir(&id)
            .and_then(|dir| read_is_subagent(&dir).then_some(id.clone()));
        out.push(Subagent {
            id,
            title,
            kind,
            status,
            chat_id,
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

/// Parse parent agent transcript jsonl for `Task` tool_use entries.
fn scan_transcript_jsonl(transcript_dir: &Path, chat_id: &str) -> Vec<Subagent> {
    let enc = encode_conversation_id(chat_id);
    let candidates = [
        transcript_dir.join(format!("{enc}.jsonl")),
        transcript_dir.join(format!("{chat_id}.jsonl")),
        transcript_dir.join(format!("{enc}.txt")),
    ];

    let mut by_id: BTreeMap<String, Subagent> = BTreeMap::new();
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines().flatten() {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(content) = value
                .pointer("/message/content")
                .or_else(|| value.get("content"))
            else {
                continue;
            };
            let Some(parts) = content.as_array() else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                    continue;
                }
                if part.get("name").and_then(|v| v.as_str()) != Some("Task") {
                    continue;
                }
                let input = part.get("input").cloned().unwrap_or(Value::Null);
                let title = input
                    .get("description")
                    .or_else(|| input.get("title"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Task");
                let kind = input
                    .get("subagent_type")
                    .or_else(|| input.get("subagentType"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let id = part
                    .get("id")
                    .or_else(|| part.get("tool_use_id"))
                    .and_then(|v| v.as_str())
                    .map(sanitize_id)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| sanitize_id(&format!("task-{}", title)));
                // Transcripts rarely include a completion marker on the same
                // part; treat as done if the file is stale, else running.
                let status = if is_stale_file(&path) {
                    SubagentStatus::Done
                } else {
                    SubagentStatus::Running
                };
                upsert(
                    &mut by_id,
                    Subagent {
                        id,
                        title: truncate(title, 48),
                        kind,
                        status,
                        chat_id: None,
                    },
                );
            }
        }
    }
    by_id.into_values().collect()
}

fn scan_task_blobs(chat_dir: &Path) -> Vec<Subagent> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

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
        // Skip the available-tools enum lists that also contain the token.
        if window.contains("\"EditNotebook\"") && window.contains("\"AwaitShell\"") {
            // Still allow real calls that happen to sit near a tools list:
            // require args/description nearby.
            if !window.contains("\"args\":{") && !window.contains("\"description\":\"") {
                continue;
            }
        }
        let id = extract_json_string(window, "toolCallId")
            .or_else(|| extract_json_string(window, "agentId"))
            .map(|s| sanitize_id(&s))
            .unwrap_or_else(|| format!("task-{idx}"));
        let title = extract_json_string(window, "description")
            .or_else(|| extract_json_string(window, "title"))
            .filter(|t| is_meaningful_task_title(t));
        let Some(title) = title else {
            continue;
        };
        let kind = extract_json_string(window, "subagentType")
            .or_else(|| extract_json_string(window, "subagent_type"));
        let status = status_from_task_window(window);
        let chat_id = extract_dollar_uuids(window)
            .into_iter()
            .find(|id| read_is_subagent_id(id));
        out.push(Subagent {
            id: chat_id.clone().unwrap_or(id),
            title: truncate(&title, 48),
            kind,
            status,
            chat_id,
        });
    }

    // stream-json / protobuf-ish readable fragments.
    // Cursor often dumps its own minified UI/source into store.db; those
    // mention `taskToolCall` dozens of times without being real invocations.
    for (idx, _) in text.match_indices("taskToolCall") {
        let window = surrounding(&text, idx, 800);
        if is_js_noise_task_window(window) {
            continue;
        }
        let title = match extract_json_string(window, "description")
            .or_else(|| extract_json_string(window, "prompt"))
            .filter(|t| is_meaningful_task_title(t))
        {
            Some(t) => t,
            None => continue,
        };
        let id = extract_json_string(window, "call_id")
            .or_else(|| extract_json_string(window, "toolCallId"))
            .map(|s| sanitize_id(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| sanitize_id(&format!("task-{}", title)));
        let kind = extract_json_string(window, "subagentType")
            .or_else(|| extract_json_string(window, "subagent_type"));
        let status = status_from_task_window(window);
        let chat_id = extract_dollar_uuids(window)
            .into_iter()
            .find(|id| read_is_subagent_id(id));
        out.push(Subagent {
            id: chat_id.clone().unwrap_or(id),
            title: truncate(&title, 48),
            kind,
            status,
            chat_id,
        });
    }

    out
}

fn is_js_noise_task_window(window: &str) -> bool {
    // Minified React/TS source + switch cases that mention the tool name.
    window.contains("case\"taskToolCall\"")
        || window.contains("case\\\"taskToolCall\\\"")
        || window.contains("case\"taskToolCall\":")
        || window.contains("\"taskToolCall\"!==")
        || window.contains("\\\"taskToolCall\\\"!==")
        || window.contains("taskToolCall\\\"!==")
        || window.contains("taskToolCall\"!==")
        || window.contains("void 0")
        || window.contains("yield this")
        || window.contains("(0,r.jsx)")
        || window.contains(".map((")
        || window.contains("function ")
        || window.contains("return null")
}

fn is_meaningful_task_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() || t.len() < 3 {
        return false;
    }
    !matches!(
        t.to_ascii_lowercase().as_str(),
        "subagent task"
            | "subagent"
            | "task"
            | "running subagent"
            | "delegating"
            | "undefined"
            | "null"
    )
}

fn is_actionable_subagent(sub: &Subagent) -> bool {
    is_meaningful_task_title(&sub.title)
}

/// Fold Task-tool rows into linked `isSubagent` chats when titles overlap.
fn collapse_task_rows(subs: Vec<Subagent>) -> Vec<Subagent> {
    let mut with_chat: Vec<Subagent> = Vec::new();
    let mut orphans: Vec<Subagent> = Vec::new();
    for sub in subs {
        if sub.chat_id.is_some() {
            with_chat.push(sub);
        } else {
            orphans.push(sub);
        }
    }
    for orphan in orphans {
        if let Some(host) = with_chat
            .iter_mut()
            .find(|h| titles_overlap(&h.title, &orphan.title))
        {
            // Prefer the short Task description over a long prompt snippet.
            let prefer_orphan_title = is_meaningful_task_title(&orphan.title)
                && orphan.title.chars().count() < host.title.chars().count();
            merge_subagent(host, &orphan);
            if prefer_orphan_title {
                host.title = orphan.title;
            }
            if host.kind.is_none() {
                host.kind = orphan.kind;
            }
        } else {
            with_chat.push(orphan);
        }
    }
    with_chat
}

fn titles_overlap(a: &str, b: &str) -> bool {
    let a = a.trim().to_ascii_lowercase();
    let b = b.trim().to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    // Short Task descriptions often appear inside the full subagent prompt.
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    short.len() >= 8 && long.contains(short.as_str())
}

fn status_rank(status: SubagentStatus) -> u8 {
    match status {
        SubagentStatus::Running => 0,
        SubagentStatus::Failed => 1,
        SubagentStatus::Done => 2,
    }
}

fn status_from_task_window(window: &str) -> SubagentStatus {
    if window.contains("\"status\":\"completed\"")
        || window.contains("\"subtype\":\"completed\"")
        || window.contains("\"case\":\"success\"")
    {
        SubagentStatus::Done
    } else if window.contains("\"status\":\"error\"")
        || window.contains("\"status\":\"failed\"")
        || window.contains("\"status\":\"aborted\"")
        || window.contains("\"case\":\"error\"")
    {
        SubagentStatus::Failed
    } else {
        SubagentStatus::Running
    }
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
    if !is_meaningful_task_title(&existing.title) && is_meaningful_task_title(&incoming.title) {
        existing.title = incoming.title.clone();
    } else if existing.title == existing.id
        && incoming.title != incoming.id
        && is_meaningful_task_title(&incoming.title)
    {
        existing.title = incoming.title.clone();
    }
    if existing.kind.is_none() {
        existing.kind = incoming.kind.clone();
    }
    if existing.chat_id.is_none() {
        existing.chat_id = incoming.chat_id.clone();
    }
    if let Some(chat_id) = existing.chat_id.clone() {
        // Prefer the resumable chat id as the stable row id.
        if existing.id != chat_id {
            existing.id = chat_id;
        }
    }
    // Prefer terminal statuses from either side.
    existing.status = match (existing.status, incoming.status) {
        (SubagentStatus::Failed, _) | (_, SubagentStatus::Failed) => SubagentStatus::Failed,
        (SubagentStatus::Done, _) | (_, SubagentStatus::Done) => SubagentStatus::Done,
        _ => SubagentStatus::Running,
    };
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '-' } else { c })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_slug_matches_cursor() {
        assert_eq!(
            project_slug(Path::new("/home/nandi/code/manager")),
            "home-nandi-code-manager"
        );
    }

    #[test]
    fn poll_known_task_chat_finds_description() {
        let chat = "0c42e075-6bbb-4f4c-9a60-e507bdd3b011";
        let snap = poll_chat(chat, Some(Path::new("/home/nandi/code/sleek")));
        assert!(
            snap.subagents
                .iter()
                .any(|s| s.title.contains("Explore message architecture")),
            "expected Task from transcript/store, got: {:?}",
            snap.subagents
        );
    }

    #[test]
    fn linked_subagent_chat_is_resumable() {
        // Parent launched Task "Find search click bugs"; Cursor created an
        // isSubagent chat referenced as `$ad7163aa-…` in the parent store.
        let parent = "c9f2933a-8f0a-41ca-bac2-e3f6d54f3e70";
        let child = "ad7163aa-01d9-4536-8b8f-ac2e42d599c2";
        let snap = poll_chat(parent, Some(Path::new("/home/nandi/code/sleek")));
        let sub = snap
            .subagents
            .iter()
            .find(|s| s.chat_id.as_deref() == Some(child))
            .unwrap_or_else(|| panic!("expected linked subagent {child}, got: {:?}", snap.subagents));
        assert!(
            sub.title.to_lowercase().contains("search")
                || sub.title.to_lowercase().contains("click"),
            "expected task-ish title, got {:?}",
            sub.title
        );
    }

    #[test]
    fn extract_dollar_uuid_from_marker() {
        let ids = extract_dollar_uuids(
            "prefix$ad7163aa-01d9-4536-8b8f-ac2e42d599c2 suffix default2$35b14132-7289-4a4f-b6e0-1981aafc1f49",
        );
        assert!(ids.contains(&"ad7163aa-01d9-4536-8b8f-ac2e42d599c2".into()));
        assert!(ids.contains(&"35b14132-7289-4a4f-b6e0-1981aafc1f49".into()));
    }

    #[test]
    fn image_paste_chat_has_no_js_noise_tasks() {
        // This chat's store.db embeds Cursor UI source mentioning taskToolCall
        // dozens of times — none are real Task invocations.
        let chat = "9929dbe7-8f72-483b-9911-8f9c6ac41c13";
        let snap = poll_chat(chat, Some(Path::new("/home/nandi/code/manager")));
        assert!(
            snap.subagents.is_empty(),
            "expected no subagents, got: {:?}",
            snap.subagents
        );
    }

    #[test]
    fn rejects_js_noise_and_placeholder_titles() {
        assert!(is_js_noise_task_window(
            r#"case"taskToolCall":return"Running subagent";default:return"Working…""#
        ));
        assert!(is_js_noise_task_window(
            r#"if("taskToolCall"!==e.call.tool.case)return null"#
        ));
        assert!(!is_meaningful_task_title("Subagent task"));
        assert!(!is_meaningful_task_title("Task"));
        assert!(is_meaningful_task_title("Explore message architecture"));
    }

    #[test]
    fn format_tool_activity_read() {
        let part = serde_json::json!({
            "type": "tool_use",
            "name": "Read",
            "input": { "path": "/home/nandi/code/manager/src/app.rs" }
        });
        assert_eq!(
            format_tool_activity(&part).as_deref(),
            Some("Reading app.rs")
        );
    }

    #[test]
    fn format_tool_activity_shell() {
        let part = serde_json::json!({
            "type": "tool_use",
            "name": "Shell",
            "input": { "command": "cd /tmp && cargo check 2>&1 | tail -20" }
        });
        assert_eq!(
            format_tool_activity(&part).as_deref(),
            Some("Running cargo check 2>&1 | tail -20")
        );
    }

    #[test]
    fn digest_jsonl_tail_builds_retrospective_summary() {
        let dir = std::env::temp_dir().join(format!(
            "manager-summary-test-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chat.jsonl");
        let body = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nadd a summary bar\n</user_query>"}]}}
{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x/app.rs"}},{"type":"tool_use","name":"StrReplace","input":{"path":"/x/app.rs"}},{"type":"tool_use","name":"Shell","input":{"command":"cargo check"}}]}}
{"type":"turn_ended","status":"success"}
"#;
        fs::write(&path, body).unwrap();
        let digest = digest_jsonl_tail(&path);
        let _ = fs::remove_dir_all(&dir);
        let digest = digest.expect("digest");
        assert_eq!(digest.live_status.as_deref(), Some("Waiting for input"));
        let summary = digest.summary.expect("summary");
        assert_eq!(summary.goal.as_deref(), Some("add a summary bar"));
        assert!(
            summary
                .lines
                .iter()
                .any(|l| l.kind == SummaryKind::Edited && l.text.contains("app.rs")),
            "{:?}",
            summary.lines
        );
        assert!(
            summary.lines.iter().any(|l| {
                l.kind == SummaryKind::Ran
                    && (l.text.contains("cargo check") || l.text.contains("verified with"))
            }),
            "{:?}",
            summary.lines
        );
        assert!(
            summary.lines.iter().any(|l| l.kind == SummaryKind::Read),
            "{:?}",
            summary.lines
        );
    }

    #[test]
    fn build_session_summary_structured_lines() {
        let summary = build_session_summary(
            Some("add a summary bar"),
            &["app.rs".into()],
            &["cargo check".into()],
            &[],
            &[],
            &[],
            1,
            None,
        )
        .expect("summary");
        assert_eq!(summary.goal.as_deref(), Some("add a summary bar"));
        assert_eq!(summary.lines[0].kind, SummaryKind::Edited);
        assert_eq!(summary.lines[0].text, "app.rs");
        assert_eq!(summary.lines[1].kind, SummaryKind::Ran);
        assert!(summary.lines[1].text.contains("cargo check"));
        assert_eq!(summary.lines[2].kind, SummaryKind::Read);
    }
}
