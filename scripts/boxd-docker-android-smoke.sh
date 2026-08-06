#!/usr/bin/env bash
# Run on a boxd VM (KVM + Docker): docker-android → build manager x86 APK → install + launch.
#
# From your laptop (logged into boxd):
#   ./scripts/boxd-launch-docker-android.sh
#
# Already SSH'd into boxd:
#   ./scripts/boxd-docker-android-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKDIR="${BOXD_WORKDIR:-$HOME/manager-android-smoke}"
MANAGER_REPO="${MANAGER_REPO:-https://github.com/codegod100/manager.git}"
MANAGER_BRANCH="${MANAGER_BRANCH:-cursor/android-smoke-ci-a66f}"
VIDYA_REPO="${VIDYA_REPO:-https://tangled.org/nandi.uk/vidya}"

need() {
  command -v "$1" >/dev/null || {
    echo "missing: $1" >&2
    exit 1
  }
}

if [[ ! -e /dev/kvm ]]; then
  echo "error: /dev/kvm missing — docker-android needs KVM (use a boxd VM, not Cursor cloud)" >&2
  exit 1
fi

need docker
need git
if ! docker info >/dev/null 2>&1; then
  echo "error: docker daemon not running" >&2
  exit 1
fi

if [[ -f "$ROOT/scripts/docker-android.sh" && -f "$ROOT/scripts/build-apk.sh" ]]; then
  MANAGER_DIR="$ROOT"
  VIDYA_DIR="$(dirname "$ROOT")/vidya"
  if [[ ! -d "$VIDYA_DIR/.git" ]]; then
    git clone --depth 1 "$VIDYA_REPO" "$VIDYA_DIR"
  fi
else
  mkdir -p "$WORKDIR"
  MANAGER_DIR="$WORKDIR/manager"
  VIDYA_DIR="$WORKDIR/vidya"
  if [[ ! -d "$MANAGER_DIR/.git" ]]; then
    echo "→ clone manager ($MANAGER_BRANCH)" >&2
    git clone --depth 1 -b "$MANAGER_BRANCH" "$MANAGER_REPO" "$MANAGER_DIR"
  fi
  if [[ ! -d "$VIDYA_DIR/.git" ]]; then
    echo "→ clone vidya" >&2
    git clone --depth 1 "$VIDYA_REPO" "$VIDYA_DIR"
  fi
fi

cd "$MANAGER_DIR"
export DOCKER_ANDROID_WEB_VNC="${DOCKER_ANDROID_WEB_VNC:-true}"

echo "→ Android toolchain" >&2
./scripts/ci-setup-android.sh

echo "→ docker-android start" >&2
./scripts/docker-android.sh start

echo "→ waiting for emulator (can take several minutes on first boot)" >&2
./scripts/docker-android.sh wait

echo "→ build x86_64 debug APK" >&2
APK="$(./scripts/build-apk.sh --target x86_64-linux-android)"

echo "→ smoke test" >&2
./scripts/android-smoke.sh "$APK"

echo "" >&2
echo "✓ manager APK running in docker-android" >&2
echo "  VNC (on boxd):  http://localhost:6080" >&2
echo "  From laptop:    boxd machine proxy set-port <vm> --port 6080" >&2
echo "                  then open https://<vm>.boxd.sh" >&2
echo "  adb:            adb connect localhost:5555" >&2
