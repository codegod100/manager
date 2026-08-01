//! Read/write clipboard images the same way cursor-agent does (`wl-paste` / `xclip`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
struct ImageFormat {
    mime: &'static str,
    ext: &'static str,
    /// Magic-byte prefix used to reject mismatched clipboard payloads.
    magic: &'static [u8],
}

const FORMATS: &[ImageFormat] = &[
    ImageFormat {
        mime: "image/png",
        ext: "png",
        magic: b"\x89PNG\r\n\x1a\n",
    },
    ImageFormat {
        mime: "image/jpeg",
        ext: "jpg",
        magic: b"\xff\xd8\xff",
    },
    ImageFormat {
        mime: "image/gif",
        ext: "gif",
        magic: b"GIF8",
    },
    ImageFormat {
        mime: "image/webp",
        ext: "webp",
        magic: b"RIFF",
    },
];

/// Save the clipboard image (if any) under `$TMPDIR/manager-clipboard/` and return its path.
pub fn capture_clipboard_image() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("manager-clipboard");
    fs::create_dir_all(&dir).ok()?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();

    for (i, cmd) in clipboard_commands().into_iter().enumerate() {
        let Ok(output) = Command::new(cmd.program).args(cmd.args).output() else {
            continue;
        };
        if !output.status.success() || output.stdout.is_empty() {
            continue;
        }
        if !output.stdout.starts_with(cmd.format.magic) {
            continue;
        }
        // WebP: RIFF....WEBP
        if cmd.format.ext == "webp"
            && (output.stdout.len() < 12 || &output.stdout[8..12] != b"WEBP")
        {
            continue;
        }
        let path = dir.join(format!("clip-{stamp}-{i}.{}", cmd.format.ext));
        if fs::write(&path, &output.stdout).is_ok() {
            return Some(path);
        }
    }
    None
}

/// Put an image file on the system clipboard so cursor-agent can attach it via `^V`.
pub fn set_clipboard_image(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Some(format) = detect_format(&bytes) else {
        return false;
    };

    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();

    if wayland
        && pipe_to(
            "wl-copy",
            &["--type", format.mime],
            &bytes,
        )
    {
        return true;
    }
    if x11
        && pipe_to(
            "xclip",
            &[
                "-selection",
                "clipboard",
                "-t",
                format.mime,
                "-i",
                "-loops",
                "1",
            ],
            &bytes,
        )
    {
        return true;
    }
    false
}

fn detect_format(bytes: &[u8]) -> Option<&'static ImageFormat> {
    FORMATS.iter().find(|f| {
        if !bytes.starts_with(f.magic) {
            return false;
        }
        if f.ext == "webp" {
            bytes.len() >= 12 && &bytes[8..12] == b"WEBP"
        } else {
            true
        }
    })
}

fn pipe_to(program: &str, args: &[&str], bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return false;
    };
    use std::io::Write;
    if stdin.write_all(bytes).is_err() {
        let _ = child.kill();
        return false;
    }
    drop(stdin);
    // wl-copy's parent exits after forking; xclip may stay until the agent pastes.
    // Never block the UI thread on that.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    true
}

struct ClipboardCommand {
    program: &'static str,
    args: Vec<&'static str>,
    format: &'static ImageFormat,
}

fn clipboard_commands() -> Vec<ClipboardCommand> {
    let mut cmds = Vec::new();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();

    if wayland {
        for format in FORMATS {
            cmds.push(ClipboardCommand {
                program: "wl-paste",
                args: vec!["--type", format.mime],
                format,
            });
        }
    }
    if x11 {
        for format in FORMATS {
            cmds.push(ClipboardCommand {
                program: "xclip",
                args: vec!["-selection", "clipboard", "-t", format.mime, "-o"],
                format,
            });
        }
    }
    cmds
}
