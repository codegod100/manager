//! OpenBao / Vault OIDC browser login (client callback mode).
//!
//! Matches `bao login -method=oidc` / `vault login -method=oidc`:
//! listen on `http://localhost:8250/oidc/callback`, open the provider URL,
//! then exchange `code`+`state` via `/v1/auth/{mount}/oidc/callback`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use url::Url;

const DEFAULT_MOUNT: &str = "oidc";
const DEFAULT_PORT: u16 = 8251;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);
const SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Agent Manager</title></head>
<body style="font-family: system-ui, sans-serif; margin: 2rem;">
<h1>Authentication successful</h1>
<p>You may close this window and return to Agent Manager.</p>
</body></html>"#;

fn error_html(summary: &str, detail: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Agent Manager</title></head>
<body style="font-family: system-ui, sans-serif; margin: 2rem;">
<h1>{summary}</h1>
<p>{detail}</p>
</body></html>"#,
        summary = html_escape(summary),
        detail = html_escape(detail),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Configuration for an interactive OIDC login.
#[derive(Debug, Clone)]
pub struct OidcLoginConfig {
    pub address: String,
    pub mount: String,
    pub role: String,
    pub listen_port: u16,
}

impl Default for OidcLoginConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            mount: DEFAULT_MOUNT.into(),
            role: String::new(),
            listen_port: DEFAULT_PORT,
        }
    }
}

impl OidcLoginConfig {
    pub fn from_env_defaults() -> Self {
        let mut cfg = Self::default();
        if let Ok(m) = std::env::var("BAO_OIDC_MOUNT").or_else(|_| std::env::var("VAULT_OIDC_MOUNT"))
        {
            let m = m.trim().trim_matches('/').to_string();
            if !m.is_empty() {
                cfg.mount = m;
            }
        }
        if let Ok(r) = std::env::var("BAO_OIDC_ROLE").or_else(|_| std::env::var("VAULT_OIDC_ROLE")) {
            cfg.role = r.trim().to_string();
        }
        if let Ok(p) = std::env::var("MANAGER_OIDC_PORT")
            .or_else(|_| std::env::var("BAO_OIDC_PORT"))
            .or_else(|_| std::env::var("VAULT_OIDC_PORT"))
        {
            if let Ok(port) = p.trim().parse::<u16>() {
                if port > 0 {
                    cfg.listen_port = port;
                }
            }
        }
        cfg
    }
}

/// Progress / result events from a background OIDC login.
#[derive(Debug, Clone)]
pub enum OidcLoginEvent {
    /// Authorization URL ready (browser open attempted).
    Ready {
        auth_url: String,
        /// Set when the OS browser launcher failed; URL can still be opened manually.
        browser_error: Option<String>,
    },
    /// Login finished with a Vault/OpenBao client token.
    Success { token: String },
    /// Login failed or was cancelled / timed out.
    Failed(String),
}

/// Handle for an in-flight OIDC login (cancel + poll).
pub struct OidcLogin {
    rx: Receiver<OidcLoginEvent>,
    cancel: Arc<AtomicBool>,
    /// Connect to this to unblock `accept` when cancelling.
    wake_addr: String,
}

impl OidcLogin {
    pub fn try_recv(&self) -> Option<OidcLoginEvent> {
        self.rx.try_recv().ok()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.wake_addr);
    }
}

impl Drop for OidcLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Start the OIDC browser login flow on a background thread.
pub fn start_oidc_login(config: OidcLoginConfig) -> Result<OidcLogin, String> {
    let address = config.address.trim().trim_end_matches('/').to_string();
    if address.is_empty() {
        return Err("Server address is required.".into());
    }
    let mount = {
        let m = config.mount.trim().trim_matches('/');
        if m.is_empty() {
            DEFAULT_MOUNT.to_string()
        } else {
            m.to_string()
        }
    };
    let role = config.role.trim().to_string();
    let port = if config.listen_port == 0 {
        DEFAULT_PORT
    } else {
        config.listen_port
    };

    // Build the HTTP client on the calling (UI) thread. Creating a reqwest
    // blocking client (and its Tokio runtime) from the callback thread is
    // unreliable on Android after the app has been backgrounded for browser login.
    let http = build_http_client()?;

    let listen_addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&listen_addr)
        .map_err(|e| format!("OIDC callback listen on {listen_addr} failed: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("OIDC listener setup failed: {e}"))?;

    let redirect_uri = format!("http://localhost:{port}/oidc/callback");
    let client_nonce = random_nonce();

    let auth_url =
        request_auth_url(&http, &address, &mount, &role, &redirect_uri, &client_nonce)?;
    let browser_error = open_browser(&auth_url).err();

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_bg = Arc::clone(&cancel);
    let wake_addr = listen_addr.clone();
    let http_bg = http.clone();

    let _ = tx.send(OidcLoginEvent::Ready {
        auth_url: auth_url.clone(),
        browser_error,
    });

    thread::spawn(move || {
        run_login_thread(
            listener,
            http_bg,
            address,
            mount,
            client_nonce,
            cancel_bg,
            tx,
            LOGIN_TIMEOUT,
        );
    });

    Ok(OidcLogin {
        rx,
        cancel,
        wake_addr,
    })
}

fn build_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        // Avoid HTTP/2 edge cases with long OIDC query strings on some stacks.
        .http1_only()
        // Browser login can take minutes; a pooled keep-alive to OpenBao is often
        // already closed by the LB and then fails with a vague "error sending request".
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| format!("HTTP client setup failed: {e}"))
}

fn format_reqwest_error(context: &str, err: reqwest::Error) -> String {
    // Put the root cause first — the URL is huge and used to bury TLS/connect details.
    let url = err
        .url()
        .map(|u| {
            let s = u.as_str();
            if s.len() > 96 {
                format!("{}…", &s[..96])
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();
    let mut parts = Vec::new();
    let mut src = std::error::Error::source(&err);
    while let Some(cause) = src {
        parts.push(cause.to_string());
        src = cause.source();
    }
    let cause = if parts.is_empty() {
        err.without_url().to_string()
    } else {
        parts.join(" → ")
    };
    if url.is_empty() {
        format!("{context}: {cause}")
    } else {
        format!("{context}: {cause} ({url})")
    }
}

#[allow(clippy::too_many_arguments)]
fn run_login_thread(
    listener: TcpListener,
    http: reqwest::blocking::Client,
    address: String,
    mount: String,
    client_nonce: String,
    cancel: Arc<AtomicBool>,
    tx: Sender<OidcLoginEvent>,
    timeout: Duration,
) {
    let started = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(OidcLoginEvent::Failed("OIDC login cancelled.".into()));
            return;
        }
        if started.elapsed() > timeout {
            let _ = tx.send(OidcLoginEvent::Failed(
                "Timed out waiting for OIDC provider.".into(),
            ));
            return;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if cancel.load(Ordering::SeqCst) {
                    let _ = tx.send(OidcLoginEvent::Failed("OIDC login cancelled.".into()));
                    return;
                }
                match handle_callback_connection(
                    stream,
                    &http,
                    &address,
                    &mount,
                    &client_nonce,
                ) {
                    Ok(token) => {
                        let _ = tx.send(OidcLoginEvent::Success { token });
                        return;
                    }
                    Err(CallbackConnError::WakeOnly) => continue,
                    Err(CallbackConnError::Failed(msg)) => {
                        let _ = tx.send(OidcLoginEvent::Failed(msg));
                        return;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = tx.send(OidcLoginEvent::Failed(format!("OIDC listener error: {e}")));
                return;
            }
        }
    }
}

enum CallbackConnError {
    /// Not a real OIDC callback (cancel wake-up, probe, wrong path).
    WakeOnly,
    Failed(String),
}

fn handle_callback_connection(
    mut stream: TcpStream,
    http: &reqwest::blocking::Client,
    address: &str,
    mount: &str,
    client_nonce: &str,
) -> Result<String, CallbackConnError> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return Err(CallbackConnError::WakeOnly);
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let (method, path_q, body) = match parse_http_request(&req) {
        Some(v) => v,
        None => return Err(CallbackConnError::WakeOnly),
    };

    // Cancellation wake-up is a bare TCP connect with no HTTP — or non-callback path.
    if !path_q.starts_with("/oidc/callback") {
        let _ = write_http_response(&mut stream, 404, "text/plain", b"not found");
        let _ = stream.shutdown(Shutdown::Both);
        return Err(CallbackConnError::WakeOnly);
    }

    let params = match method.as_str() {
        "GET" => query_params(path_q.split_once('?').map(|(_, q)| q).unwrap_or("")),
        "POST" => {
            let mut params = query_params(path_q.split_once('?').map(|(_, q)| q).unwrap_or(""));
            for (k, v) in form_params(&body) {
                params.insert(k, v);
            }
            // form_post mode: first POST code/id_token to OpenBao, then GET with state/code.
            if let Err(e) = oidc_callback_form_post(http, address, mount, &params, client_nonce) {
                let html = error_html("Login failed", &e);
                let _ = write_http_response(
                    &mut stream,
                    400,
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                );
                let _ = stream.shutdown(Shutdown::Both);
                return Err(CallbackConnError::Failed(e));
            }
            params.remove("id_token");
            params
        }
        _ => {
            let _ = write_http_response(&mut stream, 405, "text/plain", b"method not allowed");
            let _ = stream.shutdown(Shutdown::Both);
            return Err(CallbackConnError::WakeOnly);
        }
    };

    if let Some(err) = params.get("error") {
        let desc = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| err.clone());
        let html = error_html("Login failed", &desc);
        let _ = write_http_response(
            &mut stream,
            400,
            "text/html; charset=utf-8",
            html.as_bytes(),
        );
        let _ = stream.shutdown(Shutdown::Both);
        return Err(CallbackConnError::Failed(format!(
            "OIDC provider error: {desc}"
        )));
    }

    let state = params.get("state").cloned().unwrap_or_default();
    let code = params.get("code").cloned().unwrap_or_default();
    if state.is_empty() || code.is_empty() {
        let html = error_html("Login failed", "Missing state or code in callback.");
        let _ = write_http_response(
            &mut stream,
            400,
            "text/html; charset=utf-8",
            html.as_bytes(),
        );
        let _ = stream.shutdown(Shutdown::Both);
        return Err(CallbackConnError::Failed(
            "OIDC callback missing state or code.".into(),
        ));
    }

    // Exchange with OpenBao before answering the browser so a Chrome retry cannot
    // consume the authorization code twice. Keep the localhost socket open meanwhile.
    match oidc_callback_exchange(http, address, mount, &state, &code, client_nonce) {
        Ok(token) => {
            let _ = write_http_response(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                SUCCESS_HTML.as_bytes(),
            );
            let _ = stream.shutdown(Shutdown::Both);
            Ok(token)
        }
        Err(e) => {
            let html = error_html("Login failed", &e);
            let _ = write_http_response(
                &mut stream,
                400,
                "text/html; charset=utf-8",
                html.as_bytes(),
            );
            let _ = stream.shutdown(Shutdown::Both);
            Err(CallbackConnError::Failed(e))
        }
    }
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn parse_http_request(req: &str) -> Option<(String, String, String)> {
    let (head, body) = req.split_once("\r\n\r\n").unwrap_or((req, ""));
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    Some((method, path, body.to_string()))
}

fn query_params(query: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if let (Ok(k), Ok(v)) = (
            urlencoding_decode(k),
            urlencoding_decode(v),
        ) {
            if !k.is_empty() {
                map.insert(k, v);
            }
        }
    }
    map
}

fn form_params(body: &str) -> std::collections::BTreeMap<String, String> {
    query_params(body)
}

fn urlencoding_decode(s: &str) -> Result<String, ()> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                let b = u8::from_str_radix(hex, 16).map_err(|_| ())?;
                out.push(b);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn request_auth_url(
    http: &reqwest::blocking::Client,
    address: &str,
    mount: &str,
    role: &str,
    redirect_uri: &str,
    client_nonce: &str,
) -> Result<String, String> {
    let url = format!("{address}/v1/auth/{mount}/oidc/auth_url");
    let mut body = json!({
        "redirect_uri": redirect_uri,
        "client_nonce": client_nonce,
    });
    if !role.is_empty() {
        body["role"] = Value::String(role.to_string());
    }

    let response = http
        .post(&url)
        .header("X-Vault-Request", "true")
        .json(&body)
        .send()
        .map_err(|e| format_reqwest_error("OIDC auth_url request failed", e))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "OIDC auth_url failed (HTTP {}): {}",
            status.as_u16(),
            extract_errors(&text).unwrap_or(text.chars().take(300).collect())
        ));
    }

    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("auth_url parse error: {e}"))?;
    let auth_url = value
        .pointer("/data/auth_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if auth_url.is_empty() {
        return Err(format!(
            "Unable to authorize role \"{role}\" with redirect_uri \"{redirect_uri}\". Check OpenBao logs."
        ));
    }
    // Validate URL shape early so we fail before opening a junk browser tab.
    Url::parse(&auth_url).map_err(|e| format!("invalid auth_url from server: {e}"))?;
    Ok(auth_url)
}

fn oidc_callback_form_post(
    http: &reqwest::blocking::Client,
    address: &str,
    mount: &str,
    params: &std::collections::BTreeMap<String, String>,
    client_nonce: &str,
) -> Result<(), String> {
    let url = format!("{address}/v1/auth/{mount}/oidc/callback");
    let mut pairs = vec![("client_nonce", client_nonce.to_string())];
    for key in ["state", "code", "id_token"] {
        if let Some(v) = params.get(key) {
            pairs.push((key, v.clone()));
        }
    }
    let body = encode_form(&pairs);

    let response = http
        .post(&url)
        .header("X-Vault-Request", "true")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format_reqwest_error("OIDC form_post failed", e))?;
    if !response.status().is_success() {
        let text = response.text().unwrap_or_default();
        return Err(extract_errors(&text).unwrap_or_else(|| {
            text.chars().take(300).collect()
        }));
    }
    Ok(())
}

fn oidc_callback_exchange(
    http: &reqwest::blocking::Client,
    address: &str,
    mount: &str,
    state: &str,
    code: &str,
    client_nonce: &str,
) -> Result<String, String> {
    let url = format!("{address}/v1/auth/{mount}/oidc/callback");
    let mut last_err = None;
    // One retry covers transient connect resets after the app returns from the browser.
    for attempt in 0..2 {
        let response = match http
            .get(&url)
            .header("X-Vault-Request", "true")
            .query(&[
                ("state", state),
                ("code", code),
                ("client_nonce", client_nonce),
            ])
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(format_reqwest_error("OIDC callback failed", e));
                if attempt == 0 {
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
                break;
            }
        };
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "OIDC callback failed (HTTP {}): {}",
                status.as_u16(),
                extract_errors(&text).unwrap_or(text.chars().take(300).collect())
            ));
        }

        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("OIDC callback parse error: {e}"))?;
        return value
            .pointer("/auth/client_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "OIDC callback returned no client_token.".into());
    }
    Err(last_err.unwrap_or_else(|| "OIDC callback failed.".into()))
}

fn encode_form(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding_encode(k), urlencoding_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn extract_errors(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let errors = value.get("errors")?.as_array()?;
    let msgs: Vec<String> = errors
        .iter()
        .filter_map(|e| e.as_str().map(|s| s.to_string()))
        .collect();
    if msgs.is_empty() {
        None
    } else {
        Some(msgs.join("; "))
    }
}

fn random_nonce() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};
        let mut h = DefaultHasher::new();
        std::process::id().hash(&mut h);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .hash(&mut h);
        let n = h.finish().to_le_bytes();
        buf[..8].copy_from_slice(&n);
        buf[8..].copy_from_slice(&n);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn open_browser(url: &str) -> Result<(), String> {
    // `webbrowser` covers desktop launchers and Android (JNI ACTION_VIEW via ndk-context).
    webbrowser::open(url)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn query_params_decodes() {
        let p = query_params("state=abc%2Fdef&code=x+y");
        assert_eq!(p.get("state").map(String::as_str), Some("abc/def"));
        assert_eq!(p.get("code").map(String::as_str), Some("x y"));
    }

    #[test]
    fn parse_http_get() {
        let req = "GET /oidc/callback?code=1&state=2 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (m, p, b) = parse_http_request(req).unwrap();
        assert_eq!(m, "GET");
        assert!(p.starts_with("/oidc/callback?"));
        assert!(b.is_empty());
    }

    #[test]
    fn oidc_exchange_against_mock() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"auth":{"client_token":"s.testid"}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let http = build_http_client().unwrap();
        let token = oidc_callback_exchange(
            &http,
            &format!("http://{addr}"),
            "oidc",
            "st",
            "cd",
            "nonce",
        )
        .unwrap();
        assert_eq!(token, "s.testid");
    }

    #[test]
    fn auth_url_against_mock() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = r#"{"data":{"auth_url":"https://example.com/authorize?x=1"}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let http = build_http_client().unwrap();
        let url = request_auth_url(
            &http,
            &format!("http://{addr}"),
            "oidc",
            "dev",
            "http://localhost:8250/oidc/callback",
            "nonce",
        )
        .unwrap();
        assert_eq!(url, "https://example.com/authorize?x=1");
    }

    #[test]
    fn full_login_callback_roundtrip() {
        // Fake OpenBao: auth_url then callback → client_token.
        let bao = TcpListener::bind("127.0.0.1:0").unwrap();
        let bao_addr = bao.local_addr().unwrap();
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = bao.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.contains("auth_url") {
                    r#"{"data":{"auth_url":"https://example.com/authorize?x=1"}}"#
                } else {
                    r#"{"auth":{"client_token":"s.oidc-token"}}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        // Pick a free local callback port.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let login = start_oidc_login(OidcLoginConfig {
            address: format!("http://{bao_addr}"),
            mount: "oidc".into(),
            role: "dev".into(),
            listen_port: port,
        })
        .unwrap();

        // Drain Ready.
        let mut saw_ready = false;
        for _ in 0..50 {
            match login.try_recv() {
                Some(OidcLoginEvent::Ready { .. }) => {
                    saw_ready = true;
                    break;
                }
                Some(OidcLoginEvent::Failed(e)) => panic!("unexpected fail before callback: {e}"),
                Some(OidcLoginEvent::Success { .. }) => panic!("success too early"),
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
        assert!(saw_ready);

        // Simulate IdP redirect to the local callback.
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = "GET /oidc/callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        client.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = client.read_to_string(&mut resp);
        assert!(resp.contains("Authentication successful"), "{resp}");

        let mut token = None;
        for _ in 0..50 {
            match login.try_recv() {
                Some(OidcLoginEvent::Success { token: t }) => {
                    token = Some(t);
                    break;
                }
                Some(OidcLoginEvent::Failed(e)) => panic!("login failed: {e}"),
                Some(OidcLoginEvent::Ready { .. }) => {}
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(token.as_deref(), Some("s.oidc-token"));
    }
}
