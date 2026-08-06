//! Bash → Nushell conversion via prime-agent (`--print`).

use crate::session::resolve_agent_binary;
use std::process::Command;

/// Convert a bash script/snippet to idiomatic Nushell using prime-agent print mode.
///
/// Safe to run off the UI thread. Uses `--print` (non-interactive; exits after reply).
pub fn bash_to_nushell(bash: &str) -> Result<String, String> {
    let bash = bash.trim();
    if bash.is_empty() {
        return Err("paste some bash first".into());
    }

    let agent = resolve_agent_binary()?;
    let prompt = format!(
        "Convert the following bash to idiomatic Nushell.\n\
         Output ONLY the Nushell code — no markdown fences, no commentary, no explanation.\n\
         Preserve comments when useful. Prefer native Nushell over `^bash` / `bash -c` wrappers.\n\
         \n\
         Bash:\n\
         ```bash\n\
         {bash}\n\
         ```"
    );

    let output = Command::new(&agent)
        .args(["--print", "--no-session", "--offline", &prompt])
        .output()
        .map_err(|e| format!("failed to run prime-agent: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(format!(
            "prime-agent convert failed ({}): {}",
            output.status,
            if detail.is_empty() {
                "no output"
            } else {
                &detail
            }
        ));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let cleaned = strip_code_fence(raw.trim());
    if cleaned.is_empty() {
        return Err("prime-agent returned empty Nushell".into());
    }
    Ok(cleaned)
}

/// Unwrap a single markdown code fence if the model wrapped the answer anyway.
fn strip_code_fence(s: &str) -> String {
    let trimmed = s.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    let rest = rest
        .strip_prefix("nushell")
        .or_else(|| rest.strip_prefix("nu"))
        .or_else(|| rest.strip_prefix("bash"))
        .or_else(|| rest.strip_prefix("sh"))
        .unwrap_or(rest);
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let body = rest
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(rest);
    body.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_plain_passthrough() {
        assert_eq!(strip_code_fence("print 'hi'"), "print 'hi'");
    }

    #[test]
    fn strip_nushell_fence() {
        assert_eq!(
            strip_code_fence("```nushell\nprint 'hi'\n```"),
            "print 'hi'"
        );
    }

    #[test]
    fn strip_bare_fence() {
        assert_eq!(strip_code_fence("```\nls\n```"), "ls");
    }
}
