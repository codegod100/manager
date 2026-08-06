# Agent Manager

Desktop GUI for running multiple interactive [`prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent) sessions. Themed with [vidya](https://tangled.org/nandi.uk/vidya) (egui), terminals via [`egui_term`](https://crates.io/crates/egui_term) (alacritty PTY).

![Agent Manager](assets/screenshot.png)

## Run

From this checkout (with sibling `../vidya`):

```bash
nix run                    # apps.default → cargo build + gtk-launch .desktop (icon)
nix run .#desktop          # same
nix run .#manager          # cargo build + run binary only (no .desktop)
nix run .#build            # cargo build --release only
nix run .#package          # pure store binary
nix run .#package-desktop  # pure package via its .desktop
```

`apps.default` / `apps.desktop` cargo-build, install `manager.desktop` + hicolor icons into **`$XDG_DATA_HOME`** (`~/.local/share` by default — required on Wayland/GNOME; a private share dir is invisible to the shell), refresh icon/desktop caches, then `gtk-launch manager`. The binary also embeds the PNG via `vidya::with_app_icon_id` (used on X11; Wayland window icons come from the FreeDesktop entry). Incremental builds go into `./target/`.

Optional interactive shell:

```bash
nix develop
cargo run --release
```

Packaged install (pure Nix derivation — may use remote builders):

```bash
nix build .#manager
./result/bin/manager
# also: result/share/applications/manager.desktop
#       result/share/icons/hicolor/.../apps/manager.{svg,png}
```

**New session** opens a dialog for workspace, optional model / prompt. It spawns an interactive `prime-agent` PTY with `--cwd <workspace>` (and optional `--model` / initial prompt). **Resume** lists past sessions from `~/.prime/agent/sessions` and spawns with `--resume <sessionId>`. **Cloud** still lists and launches [Cursor Cloud Agents](https://cursor.com/docs/cloud-agent) via the API (`CURSOR_API_KEY` from [cursor.com/dashboard/api](https://cursor.com/dashboard/api)) — watch-only tabs poll agent status and open `cursor.com/agents/<bc-id>` in the browser (no local PTY).

The left **Agents** sidebar groups sessions by workspace (folder name; hover for full path), shows status + title, and nests RLM child sessions under each parent (from `parentSession` links in session JSONL). Cloud agents show a ☁ marker. Click a session to focus its terminal; cloud tabs show a status panel instead. Click a child to open it with `--resume` in a new tab when it has its own session file. Right-click a session for rename. **Close** removes the active session.

**Paste images** with Ctrl+V in an active session (screenshot / image on the clipboard). The manager forwards `^V` to prime-agent, which attaches the image via `wl-paste` / `xclip`. In the **New session** dialog, Ctrl+V saves a clipboard image to a temp file; after spawn the manager pastes into the TUI composer, then types the initial prompt and submits.

## Android APK

Shell package (`uk.nandi.manager`) for phone / Waydroid install smoke tests. Full prime-agent PTY sessions stay on the desktop build (egui_term needs a Unix PTY).

```bash
nix develop
just apk-release          # aarch64 release → android/target/release/apk/manager.apk
just apk-release-x86      # x86_64 (Waydroid)
nix run .#apk -- --release --target aarch64-linux-android
```

`nix develop` / `nix run .#apk` provide a hermetic Android SDK+NDK, `cargo-apk`, JDK (`keytool`), and a rust-overlay toolchain with `aarch64-linux-android` / `x86_64-linux-android` targets. Override `ANDROID_HOME` / `ANDROID_NDK_HOME` if you prefer a host install.

## Requirements

- Rust toolchain (provided by `nix develop`)
- [`prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent) on `PATH` (or set `PRIME_AGENT` to an executable path)
- `CURSOR_API_KEY` for **Cloud** only (API key from [cursor.com/dashboard/api](https://cursor.com/dashboard/api))
- Linux (Wayland or X11)
- `wl-paste` / `wl-copy` (Wayland) or `xclip` (X11) on `PATH` for clipboard image paste (also in the flake)

Install prime-agent (example):

```bash
curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | sh
```

### Cursor API key (OIDC) — Cloud tabs

Agent Manager can load `CURSOR_API_KEY` for the Cursor Cloud Agents API via OpenBao:

1. **Utils → Cursor sign-in (OIDC)** (or the header **Cursor: sign in** button)
2. OIDC login to your OpenBao server (default `https://openbao.boxd.sh`, mount `oidc`)
3. Read `CURSOR_API_KEY` from KV `secret/data/ai-api-keys`

On success the OpenBao token is saved to `~/.bao-token` and `CURSOR_API_KEY` is exported. On next launch, a stored token is restored automatically when `CURSOR_API_KEY` is not already set. Local `prime-agent` PTYs use Prime Agent’s own `/login` / provider credentials, not this key.

| Variable | Purpose |
|----------|---------|
| `BAO_ADDR` / `VAULT_ADDR` | OpenBao server (default `https://openbao.boxd.sh`) |
| `BAO_TOKEN` / `VAULT_TOKEN` | Skip OIDC and use a token directly |
| `BAO_OIDC_MOUNT` / `BAO_OIDC_ROLE` | OIDC auth mount and role |
| `MANAGER_OIDC_PORT` | Local callback port (default `8251`) |
| `CURSOR_API_KEY` | If already set, OIDC sign-in is optional (Cloud only) |
| `PRIME_AGENT` | Optional path override for the prime-agent binary |
| `PRIME_AGENT_SESSION_DIR` | Optional override for session storage (default `~/.prime/agent/sessions`) |

## Nix

| Output | Role |
|--------|------|
| `apps.default` / `apps.desktop` | Devshell: cargo build + `gtk-launch` `.desktop` (icon) |
| `apps.manager` | Devshell: cargo build + run binary only |
| `apps.build` | Devshell: `cargo build --release` only |
| `apps.apk` | Hermetic cargo-apk build (`--release`, `--target`, …) |
| `apps.package` | Pure store binary |
| `apps.package-desktop` | Pure package via its `.desktop` |
| `packages.default` / `packages.manager` | Binary + `.desktop` + hicolor icons (PATH includes clipboard tools) |
| `packages.desktop` | Wrapper that launches the packaged `.desktop` |
| `packages.android-sdk` | Hermetic Android SDK+NDK used by APK builds |
| `devShells.default` | rust (android targets) + cargo-apk + SDK/NDK + clipboard |
| `assets/manager.svg` | App icon source |

Apps need a live checkout next to `../vidya`. The packaged `packages.manager` stages flake input `vidya` for pure builds. Install `prime-agent` on the host (not packaged by this flake).

## Notes

- PTY I/O is handled by `egui_term` / alacritty’s tty (same role as `portable-pty`).
- Theme path-depends on `../vidya` at egui 0.31.

## License

MIT
