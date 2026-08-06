# Agent Manager

Desktop GUI for running multiple interactive [`cursor-agent`](https://cursor.com) sessions. Themed with [vidya](https://tangled.org/nandi.uk/vidya) (egui), terminals via [`egui_term`](https://crates.io/crates/egui_term) (alacritty PTY).

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

**New session** opens a dialog for workspace, optional model / prompt, and `--trust` / `--force`. It runs `cursor-agent create-chat`, then spawns the PTY with `--resume <chatId>` so the session is bound to Cursor’s chat store. **Resume** lists past chats from `~/.cursor/chats` and spawns with `--resume <chatId>`. **Cloud** lists and launches [Cursor Cloud Agents](https://cursor.com/docs/cloud-agent) via the API (`CURSOR_API_KEY` from [cursor.com/dashboard/api](https://cursor.com/dashboard/api)) — watch-only tabs poll agent status and open `cursor.com/agents/<bc-id>` in the browser (no local PTY).

The left **Agents** sidebar groups sessions by workspace (folder name; hover for full path), shows status + title, and nests Tasks under each session (from `agent-transcripts/<chatId>/`, `subagents/`, and Task records in the chat store). Cloud agents show a ☁ marker. Click a session to focus its terminal; cloud tabs show a status panel instead. Click a Task to open its nested chat (`isSubagent`) in a new tab when Cursor created one, or focus the parent if not. Right-click a session for rename. **Close** removes the active session.

**Paste images** with Ctrl+V in an active session (screenshot / image on the clipboard). The manager forwards `^V` to cursor-agent, which attaches the image via `wl-paste` / `xclip`. In the **New session** dialog, Ctrl+V saves a clipboard image to a temp file; after spawn the manager types `@/path.png` into the TUI composer (so the agent attaches it synchronously), then types the initial prompt and submits. `--image` is headless-only, so interactive sessions cannot rely on it.

## Android APK

Shell package (`uk.nandi.manager`) for phone / Waydroid install smoke tests. Full cursor-agent PTY sessions stay on the desktop build (egui_term needs a Unix PTY).

```bash
nix develop
just apk-release          # aarch64 release → android/target/release/apk/manager.apk
just apk-release-x86      # x86_64 (Waydroid)
./scripts/build-apk.sh --release --target aarch64-linux-android
```

Needs Android NDK (`ANDROID_NDK_HOME`, default `~/.local/share/android-ndk-r29`), `cargo-apk`, and `rustup target add aarch64-linux-android` (and/or `x86_64-linux-android`).

## Requirements

- Rust toolchain
- `cursor-agent` on `PATH` (or set `CURSOR_AGENT` to an executable path; Cursor’s in-session `CURSOR_AGENT=1` flag is ignored)
- `CURSOR_API_KEY` for **Cloud** (API key from [cursor.com/dashboard/api](https://cursor.com/dashboard/api))
- Linux (Wayland or X11)
- `wl-paste` / `wl-copy` (Wayland) or `xclip` (X11) on `PATH` for clipboard image paste

### Cursor API key (OIDC)

Agent Manager can load `CURSOR_API_KEY` for spawned `cursor-agent` PTY children via OpenBao:

1. **Utils → Cursor sign-in (OIDC)** (or the header **Cursor: sign in** button)
2. OIDC login to your OpenBao server (default `https://openbao.boxd.sh`, mount `oidc`)
3. Read `CURSOR_API_KEY` from KV `secret/data/ai-api-keys`

On success the OpenBao token is saved to `~/.bao-token` and `CURSOR_API_KEY` is exported for child processes. On next launch, a stored token is restored automatically when `CURSOR_API_KEY` is not already set.

| Variable | Purpose |
|----------|---------|
| `BAO_ADDR` / `VAULT_ADDR` | OpenBao server (default `https://openbao.boxd.sh`) |
| `BAO_TOKEN` / `VAULT_TOKEN` | Skip OIDC and use a token directly |
| `BAO_OIDC_MOUNT` / `BAO_OIDC_ROLE` | OIDC auth mount and role |
| `MANAGER_OIDC_PORT` | Local callback port (default `8251`) |
| `CURSOR_API_KEY` | If already set, OIDC sign-in is optional |

## Nix

| Output | Role |
|--------|------|
| `apps.default` / `apps.desktop` | Devshell: cargo build + `gtk-launch` `.desktop` (icon) |
| `apps.manager` | Devshell: cargo build + run binary only |
| `apps.build` | Devshell: `cargo build --release` only |
| `apps.package` | Pure store binary |
| `apps.package-desktop` | Pure package via its `.desktop` |
| `packages.default` / `packages.manager` | Binary + `.desktop` + hicolor icons |
| `packages.desktop` | Wrapper that launches the packaged `.desktop` |
| `devShells.default` | rustc + cargo + egui runtime libs |
| `assets/manager.svg` | App icon source |

Apps need a live checkout next to `../vidya`. The packaged `packages.manager` stages flake input `vidya` for pure builds.

## Notes

- PTY I/O is handled by `egui_term` / alacritty’s tty (same role as `portable-pty`).
- Theme path-depends on `../vidya` at egui 0.31.

## License

MIT
