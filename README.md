# Agent Manager

Desktop GUI for running multiple interactive [`cursor-agent`](https://cursor.com) sessions. Themed with [vidya](../vidya) (egui), terminals via [`egui_term`](https://crates.io/crates/egui_term) (alacritty PTY).

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

**New session** opens a dialog for workspace, optional model / prompt, and `--trust` / `--force`. **Resume** lists past chats from `~/.cursor/chats` and spawns with `--resume <chatId>`. Each session is a full agent TTY in a tab; **Close** removes the active tab.

**Paste images** with Ctrl+V in an active session (screenshot / image on the clipboard). The manager forwards `^V` to cursor-agent, which attaches the image via `wl-paste` / `xclip`.

## Requirements

- Rust toolchain
- `cursor-agent` or `agent` on `PATH` (or set `CURSOR_AGENT` to the binary path)
- Linux (Wayland or X11)
- `wl-paste` (Wayland) or `xclip` (X11) on `PATH` for clipboard image paste

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
