# Agent Manager

Desktop GUI for running multiple interactive [`cursor-agent`](https://cursor.com) sessions. Themed with [vidya](../vidya) (egui), terminals via [`egui_term`](https://crates.io/crates/egui_term) (alacritty PTY).

## Requirements

- Rust toolchain
- `cursor-agent` or `agent` on `PATH` (or set `CURSOR_AGENT` to the binary path)
- Linux (Wayland or X11)

## Run

```bash
nix run                 # apps.default → cargo run + GL/Wayland libs
nix run .#manager
# or:
cargo run
nix develop             # same libs on LD_LIBRARY_PATH
```

Run from this checkout (sibling to `../vidya` — required by the path dependency).

**New session** opens a dialog for workspace, optional model / prompt, and `--trust` / `--force`. Each session is a full agent TUI in a tab; **Close** removes the active tab.

## Notes

- PTY I/O is handled by `egui_term` / alacritty’s tty (same role as `portable-pty`).
- Theme path-depends on `../vidya` at egui 0.31.
