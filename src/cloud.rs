//! Cursor Cloud Agents API client and watch-only session state.
//!
//! Auth: set `CURSOR_API_KEY` (from [Cursor Dashboard → API Keys](https://cursor.com/dashboard/api)).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const API_BASE: &str = "https://api.cursor.com";

/// Summary row from `GET /v1/agents`.
#[derive(Debug, Clone)]
pub struct CloudAgentSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub url: String,
    pub repo_url: Option<String>,
    pub updated_at: Option<String>,
    pub latest_run_id: Option<String>,
}

/// Full agent record from `GET /v1/agents/{id}`.
#[derive(Debug, Clone)]
pub struct CloudAgentDetail {
    pub summary: CloudAgentSummary,
    pub model: Option<String>,
}

/// Run status from `GET /v1/agents/{id}/runs/{runId}`.
#[derive(Debug, Clone, Default)]
pub struct CloudRunSnapshot {
    pub status: String,
    pub result: Option<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
}

/// A watch-only cloud agent tab (no local PTY).
#[derive(Debug, Clone)]
pub struct CloudWatch {
    pub id: u64,
    pub title: String,
    pub bc_id: String,
    pub url: String,
    /// Local workspace context (for sidebar grouping).
    pub workspace: PathBuf,
    pub repo_url: Option<String>,
    pub agent_status: String,
    pub run_status: Option<String>,
    pub activity: Option<String>,
    pub summary: Option<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub title_locked: bool,
}

impl CloudWatch {
    pub fn new(id: u64, summary: CloudAgentSummary, workspace: PathBuf) -> Self {
        let activity = activity_label(&summary.status, None);
        Self {
            id,
            title: summary.name.clone(),
            bc_id: summary.id.clone(),
            url: summary.url.clone(),
            workspace,
            repo_url: summary.repo_url.clone(),
            agent_status: summary.status.clone(),
            run_status: None,
            activity,
            summary: None,
            branch: None,
            pr_url: None,
            title_locked: false,
        }
    }

    pub fn alive(&self) -> bool {
        is_live_agent_status(&self.agent_status)
            || self
                .run_status
                .as_deref()
                .is_some_and(is_live_run_status)
    }

    pub fn has_activity(&self) -> bool {
        self.alive()
    }

    pub fn apply_poll(&mut self, detail: &CloudAgentDetail, run: Option<&CloudRunSnapshot>) {
        self.agent_status = detail.summary.status.clone();
        if let Some(repo) = &detail.summary.repo_url {
            self.repo_url = Some(repo.clone());
        }
        if let Some(run) = run {
            self.run_status = Some(run.status.clone());
            self.branch = run.branch.clone();
            self.pr_url = run.pr_url.clone();
            if let Some(result) = &run.result {
                let trimmed = result.trim();
                if !trimmed.is_empty() {
                    self.summary = Some(truncate(result, 800));
                }
            }
        }
        self.activity = activity_label(
            &self.agent_status,
            self.run_status.as_deref(),
        );
        if !self.title_locked {
            let mut title = detail.summary.name.clone();
            if !self.alive() && !title.ends_with(" (finished)") {
                if is_terminal_run(self.run_status.as_deref()) {
                    title.push_str(" (finished)");
                }
            }
            self.title = title;
        }
    }

    pub fn set_user_title(&mut self, title: impl Into<String>) {
        let mut title = title.into().trim().to_string();
        if title.is_empty() {
            return;
        }
        if !self.alive() && !title.ends_with(" (finished)") {
            title.push_str(" (finished)");
        }
        self.title = title;
        self.title_locked = true;
    }
}

/// Resolve API key from `CURSOR_API_KEY`.
pub fn resolve_api_key() -> Result<String, String> {
    let key = std::env::var("CURSOR_API_KEY").map_err(|_| {
        "CURSOR_API_KEY is not set (use Sign in or cursor.com/dashboard/api)".to_string()
    })?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("CURSOR_API_KEY is empty".into());
    }
    Ok(key)
}

/// List cloud agents (newest first).
pub fn list_agents(api_key: &str, limit: usize) -> Result<Vec<CloudAgentSummary>, String> {
    let limit = limit.clamp(1, 100);
    let url = format!("{API_BASE}/v1/agents?limit={limit}&includeArchived=false");
    let value = api_get(api_key, &url)?;
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items.iter().filter_map(parse_agent_summary).collect())
}

/// Fetch one agent plus its latest run snapshot when available.
pub fn poll_agent(api_key: &str, bc_id: &str) -> Result<(CloudAgentDetail, Option<CloudRunSnapshot>), String> {
    let url = format!("{API_BASE}/v1/agents/{bc_id}");
    let value = api_get(api_key, &url)?;
    let summary = parse_agent_summary(&value).ok_or_else(|| format!("invalid agent payload for {bc_id}"))?;
    let detail = CloudAgentDetail {
        model: value
            .pointer("/model/id")
            .or_else(|| value.get("model"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        summary,
    };
    let run = match detail.summary.latest_run_id.as_deref() {
        Some(run_id) => get_run(api_key, bc_id, run_id).ok(),
        None => None,
    };
    Ok((detail, run))
}

/// Create a cloud agent and return its summary.
pub fn create_agent(
    api_key: &str,
    repo_url: &str,
    starting_ref: Option<&str>,
    prompt: &str,
    model: Option<&str>,
    name: Option<&str>,
) -> Result<CloudAgentSummary, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("prompt is required to start a cloud agent".into());
    }
    let repo_url = normalize_repo_url(repo_url)?;
    let mut body = serde_json::json!({
        "prompt": { "text": prompt },
        "repos": [{
            "url": repo_url,
            "startingRef": starting_ref.unwrap_or("main"),
        }],
        "autoCreatePR": false,
    });
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        body["model"] = serde_json::json!({ "id": model.trim() });
    }
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        body["name"] = serde_json::json!(truncate(name.trim(), 100));
    }
    let value = api_post(api_key, &format!("{API_BASE}/v1/agents"), &body)?;
    let agent = value
        .get("agent")
        .unwrap_or(&value);
    parse_agent_summary(agent).ok_or_else(|| "create-agent returned unexpected payload".into())
}

fn get_run(api_key: &str, bc_id: &str, run_id: &str) -> Result<CloudRunSnapshot, String> {
    let url = format!("{API_BASE}/v1/agents/{bc_id}/runs/{run_id}");
    let value = api_get(api_key, &url)?;
    Ok(parse_run_snapshot(&value))
}

/// Best-effort `git remote get-url origin` for a workspace directory.
pub fn git_remote_url(workspace: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &workspace.display().to_string(), "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Map a repo slug like `github.com/org/repo` to a local workspace when possible.
pub fn workspace_for_repo(repo_url: &str) -> PathBuf {
    let slug = repo_slug(repo_url);
    if let Ok(cwd) = std::env::current_dir() {
        if repo_slug(&cwd.to_string_lossy()) == slug {
            return cwd;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let parts: Vec<&str> = slug.split('/').collect();
        if parts.len() >= 2 {
            let guess = PathBuf::from(home)
                .join("code")
                .join(parts[parts.len() - 1]);
            if guess.is_dir() {
                return guess;
            }
        }
    }
    PathBuf::from(slug)
}

fn api_get(api_key: &str, url: &str) -> Result<Value, String> {
    let client = http_client()?;
    let response = client
        .get(url)
        .basic_auth(api_key, Some(""))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    parse_json_response(response)
}

fn api_post(api_key: &str, url: &str, body: &Value) -> Result<Value, String> {
    let client = http_client()?;
    let response = client
        .post(url)
        .basic_auth(api_key, Some(""))
        .header("Accept", "application/json")
        .json(body)
        .send()
        .map_err(|e| format!("POST {url} failed: {e}"))?;
    parse_json_response(response)
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client setup failed: {e}"))
}

fn parse_json_response(response: reqwest::blocking::Response) -> Result<Value, String> {
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("read response body: {e}"))?;
    if !status.is_success() {
        let detail = extract_error_message(&text).unwrap_or_else(|| text.trim().to_string());
        return Err(if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            format!("HTTP {}: {detail}", status.as_u16())
        });
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))
}

fn extract_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("message")
        .or_else(|| value.get("error"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_agent_summary(value: &Value) -> Option<CloudAgentSummary> {
    let id = value.get("id").and_then(|v| v.as_str())?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Cloud agent")
        .to_string();
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let url = value
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://cursor.com/agents/{id}"));
    let repo_url = value
        .get("repos")
        .and_then(|v| v.as_array())
        .and_then(|repos| repos.first())
        .and_then(|repo| repo.get("url").and_then(|v| v.as_str()))
        .map(str::to_string)
        .or_else(|| {
            value
                .pointer("/git/branches/0/repoUrl")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let updated_at = value
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let latest_run_id = value
        .get("latestRunId")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(CloudAgentSummary {
        id,
        name,
        status,
        url,
        repo_url,
        updated_at,
        latest_run_id,
    })
}

fn parse_run_snapshot(value: &Value) -> CloudRunSnapshot {
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let result = value
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let branch = value
        .pointer("/git/branches/0/branch")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let pr_url = value
        .pointer("/git/branches/0/prUrl")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    CloudRunSnapshot {
        status,
        result,
        branch,
        pr_url,
    }
}

fn normalize_repo_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("repository URL is required".into());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with("git@") {
        return Ok(raw.to_string());
    }
    if raw.contains('/') {
        return Ok(format!("https://{raw}"));
    }
    Err(format!("not a repository URL: {raw}"))
}

fn repo_slug(url: &str) -> String {
    let s = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some(rest) = s.strip_prefix("git@") {
        return rest.replacen(':', "/", 1);
    }
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    s.trim_start_matches("www.").to_string()
}

fn is_live_agent_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "ACTIVE" | "RUNNING" | "CREATING"
    )
}

fn is_live_run_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "CREATING" | "RUNNING" | "PENDING"
    )
}

fn is_terminal_run(status: Option<&str>) -> bool {
    status.is_some_and(|s| {
        matches!(
            s.to_ascii_uppercase().as_str(),
            "FINISHED" | "ERROR" | "CANCELLED" | "EXPIRED" | "FAILED"
        )
    })
}

fn activity_label(agent_status: &str, run_status: Option<&str>) -> Option<String> {
    if let Some(run) = run_status.filter(|s| is_live_run_status(s)) {
        return Some(format!("Run · {}", run.to_ascii_lowercase()));
    }
    if is_live_agent_status(agent_status) {
        return Some("Cloud agent running…".into());
    }
    if is_terminal_run(run_status) {
        return Some("Finished".into());
    }
    match agent_status.to_ascii_uppercase().as_str() {
        "ARCHIVED" => Some("Archived".into()),
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_normalizes_https() {
        assert_eq!(
            repo_slug("https://github.com/org/repo.git"),
            "github.com/org/repo"
        );
    }

    #[test]
    fn normalize_repo_url_adds_scheme() {
        assert_eq!(
            normalize_repo_url("github.com/org/repo").unwrap(),
            "https://github.com/org/repo"
        );
    }
}
