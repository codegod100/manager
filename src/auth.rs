//! OpenBao OIDC → Cursor API key for Cloud Agents API (watch-only tabs).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

const DEFAULT_ADDR: &str = "https://openbao.boxd.sh";
const KEYS_PATH: &str = "secret/data/ai-api-keys";
const CURSOR_KEY_FIELDS: &[&str] = &["CURSOR_API_KEY", "cursor_api_key", "CURSOR_TOKEN"];

#[derive(Debug)]
pub enum AuthError {
    Message(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Message(s) => write!(f, "{s}"),
        }
    }
}

impl From<String> for AuthError {
    fn from(s: String) -> Self {
        AuthError::Message(s)
    }
}

impl From<&str> for AuthError {
    fn from(s: &str) -> Self {
        AuthError::Message(s.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub initialized: bool,
    pub sealed: bool,
}

struct BaoClient {
    base_url: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl BaoClient {
    fn new(address: &str, token: &str) -> Result<Self, AuthError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| format!("HTTP client setup failed: {e}"))?;
        Ok(Self {
            base_url: address.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn send(&self, builder: reqwest::blocking::RequestBuilder) -> Result<Value, AuthError> {
        let response = builder.send().map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            return Err(extract_errors(&body)
                .or_else(|| {
                    if body.is_empty() {
                        status.canonical_reason().map(str::to_string)
                    } else {
                        Some(body.chars().take(300).collect())
                    }
                })
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()))
                .into());
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| format!("parse error: {e}").into())
    }

    fn health(&self) -> Result<Health, AuthError> {
        let response = self
            .http
            .get(self.url("sys/health"))
            .header("X-Vault-Token", &self.token)
            .query(&[
                ("standbyok", "true"),
                ("sealedcode", "200"),
                ("uninitcode", "200"),
            ])
            .send()
            .map_err(|e| format!("health check failed: {e}"))?;
        let body = response.text().unwrap_or_default();
        serde_json::from_str(&body).map_err(|e| format!("health parse error: {e}: {body}").into())
    }

    fn read_ai_keys(&self) -> Result<BTreeMap<String, String>, AuthError> {
        let value = self.send(
            self.http
                .get(self.url(KEYS_PATH))
                .header("X-Vault-Token", &self.token)
                .header("X-Vault-Request", "true"),
        )?;
        let data_map = value
            .pointer("/data/data")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut data = BTreeMap::new();
        for (k, v) in data_map {
            data.insert(k, value_to_string(&v));
        }
        Ok(data)
    }
}

/// OpenBao address (`BAO_ADDR` / `VAULT_ADDR`, else default).
pub fn resolve_bao_addr() -> String {
    std::env::var("BAO_ADDR")
        .or_else(|_| std::env::var("VAULT_ADDR"))
        .unwrap_or_else(|_| DEFAULT_ADDR.into())
        .trim()
        .trim_end_matches('/')
        .to_string()
}

/// Load a stored OpenBao token from env or `~/.bao-token`.
pub fn load_bao_token() -> String {
    if let Ok(t) = std::env::var("BAO_TOKEN").or_else(|_| std::env::var("VAULT_TOKEN")) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    for path in bao_token_paths() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}

fn bao_token_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok();
    [
        std::env::var("BAO_TOKEN_PATH")
            .ok()
            .map(PathBuf::from),
        home.as_ref()
            .map(|h| PathBuf::from(h).join(".vault-token")),
        home.as_ref().map(|h| PathBuf::from(h).join(".bao-token")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Persist OpenBao token for the next launch (`~/.bao-token`).
pub fn save_bao_token(token: &str) -> Result<(), AuthError> {
    let token = token.trim();
    if token.is_empty() {
        return Err("refusing to save empty token".into());
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is unset; cannot save token".to_string())?;
    let path = PathBuf::from(home).join(".bao-token");
    std::fs::write(&path, format!("{token}\n"))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    Ok(())
}

/// True when `CURSOR_API_KEY` is set in the environment.
pub fn has_cursor_api_key() -> bool {
    std::env::var("CURSOR_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Export `CURSOR_API_KEY` for Cloud Agents API calls.
pub fn apply_cursor_api_key(key: &str) {
    let key = key.trim();
    if !key.is_empty() {
        // SAFETY: called from the UI thread before spawning children.
        unsafe { std::env::set_var("CURSOR_API_KEY", key) };
    }
}

/// Human-readable auth status for the header.
pub fn cursor_auth_status_label() -> String {
    if has_cursor_api_key() {
        "Cursor: signed in".into()
    } else {
        "Cursor: not signed in".into()
    }
}

/// Fetch `CURSOR_API_KEY` from OpenBao `secret/data/ai-api-keys`.
pub fn fetch_cursor_api_key(address: &str, bao_token: &str) -> Result<String, AuthError> {
    let client = BaoClient::new(address, bao_token)?;
    let health = client.health()?;
    if health.sealed {
        return Err("OpenBao is sealed".into());
    }
    if !health.initialized {
        return Err("OpenBao is not initialized".into());
    }
    let keys = client.read_ai_keys()?;
    for field in CURSOR_KEY_FIELDS {
        if let Some(value) = keys.get(*field) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(format!(
        "no Cursor API key in {KEYS_PATH} (expected one of: {})",
        CURSOR_KEY_FIELDS.join(", ")
    )
    .into())
}

/// Load OpenBao token (if any), fetch Cursor key, export to env.
pub fn restore_cursor_auth() -> Result<(), AuthError> {
    if has_cursor_api_key() {
        return Ok(());
    }
    let bao_token = load_bao_token();
    if bao_token.is_empty() {
        return Err("no OpenBao token (sign in with OIDC or set BAO_TOKEN)".into());
    }
    let cursor_key = fetch_cursor_api_key(&resolve_bao_addr(), &bao_token)?;
    apply_cursor_api_key(&cursor_key);
    Ok(())
}

/// After OIDC: persist Bao token, fetch Cursor key, export to env.
pub fn complete_oidc_login(address: &str, bao_token: &str) -> Result<(), AuthError> {
    save_bao_token(bao_token)?;
    let cursor_key = fetch_cursor_api_key(address, bao_token)?;
    apply_cursor_api_key(&cursor_key);
    Ok(())
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn extract_errors(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    if let Some(errs) = value.get("errors").and_then(|e| e.as_array()) {
        let joined: Vec<_> = errs
            .iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect();
        if !joined.is_empty() {
            return Some(joined.join("; "));
        }
    }
    value
        .get("error")
        .and_then(|e| e.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_key_field_priority() {
        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        keys.insert("cursor_api_key".into(), "lower".into());
        keys.insert("CURSOR_API_KEY".into(), "upper".into());
        let picked = CURSOR_KEY_FIELDS
            .iter()
            .find_map(|field| keys.get(*field).map(String::as_str));
        assert_eq!(picked, Some("upper"));
    }
}
