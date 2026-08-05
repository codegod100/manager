#!/usr/bin/env bash
# Idempotent Cloud Agent bootstrap for the Agent Manager (manager) codebase.
#
# Runs from the repository root after checkout. Prepares everything needed to
# build and run the egui/eframe desktop GUI with plain cargo (no Nix required):
#   - system libraries eframe links against (Wayland/X11/GL/Vulkan) + SVG icons
#   - a Rust toolchain new enough for edition 2024 transitive deps (>= 1.85)
#   - the sibling `../vidya` checkout that Cargo.toml path-depends on
#   - the cursor-agent CLI the app spawns at runtime
#   - a release build of the binary
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# 1. System libraries. eframe (glow/wgpu) needs GL/Vulkan + Wayland/X11 client
#    libs at runtime; librsvg2-bin (rsvg-convert) renders the app icon.
log "Installing system libraries (egui runtime + SVG icons)"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
  libxkbcommon0 libxkbcommon-x11-0 \
  libgl1 libglvnd0 libegl1 libgles2 libvulkan1 \
  libwayland-client0 libwayland-cursor0 libwayland-egl1 \
  libx11-6 libxcursor1 libxi6 libxrandr2 libxcb1 \
  librsvg2-bin libfontconfig1 fontconfig

# 2. Rust toolchain. The default image ships 1.83, but transitive deps
#    (e.g. wayland-protocols) require the stabilized edition-2024 feature (1.85+).
log "Ensuring a recent stable Rust toolchain"
rustup toolchain install stable --profile minimal --no-self-update
rustup default stable
rustc --version

# 3. Sibling vidya checkout. Cargo.toml declares `vidya = { path = "../vidya" }`;
#    relative to the repo root that resolves to /vidya. Track vidya's main branch
#    (the committed flake.lock pin is only used by pure `nix build`, and lags the
#    symbols the app's main branch consumes, e.g. vidya::dialog).
VIDYA_DIR="$(cd "$REPO_ROOT/.." && pwd)/vidya"
log "Preparing sibling vidya checkout at $VIDYA_DIR"
if [ ! -d "$VIDYA_DIR/.git" ]; then
  if ! mkdir -p "$VIDYA_DIR" 2>/dev/null; then
    sudo mkdir -p "$VIDYA_DIR"
    sudo chown -R "$(id -u):$(id -g)" "$VIDYA_DIR"
  fi
  git clone https://tangled.org/nandi.uk/vidya "$VIDYA_DIR"
fi
git -C "$VIDYA_DIR" fetch --quiet origin main || true
git -C "$VIDYA_DIR" checkout main
git -C "$VIDYA_DIR" pull --ff-only origin main || true
git -C "$VIDYA_DIR" --no-pager log --oneline -1

# 4. cursor-agent CLI. The manager spawns `cursor-agent` (create-chat / --resume)
#    for each session and resolves it from PATH. Install once and expose it on the
#    default PATH so GUI child processes can find it regardless of launcher.
if ! command -v cursor-agent >/dev/null 2>&1; then
  log "Installing cursor-agent CLI"
  curl https://cursor.com/install -fsSL | bash
fi
if [ -x "$HOME/.local/bin/cursor-agent" ] && [ ! -e /usr/local/bin/cursor-agent ]; then
  sudo ln -sf "$HOME/.local/bin/cursor-agent" /usr/local/bin/cursor-agent
fi
command -v cursor-agent && cursor-agent --version || true

# 5. Build. Warms the cargo cache and produces target/release/manager.
log "Building manager (release)"
cargo build --release
log "Done: $REPO_ROOT/target/release/manager"
