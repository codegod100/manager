#!/usr/bin/env bash
# Install Android SDK/NDK paths expected by scripts/build-apk.sh (CI + local).
set -euo pipefail

SDK="${ANDROID_HOME:-$HOME/.local/share/android-sdk}"
NDK="${ANDROID_NDK_HOME:-$HOME/.local/share/android-ndk-r29}"
NDK_ZIP="android-ndk-r29-linux.zip"
NDK_URL="https://dl.google.com/android/repository/${NDK_ZIP}"
CMDLINE_ZIP="commandlinetools-linux-11076708_latest.zip"
CMDLINE_URL="https://dl.google.com/android/repository/${CMDLINE_ZIP}"

mkdir -p "$SDK/cmdline-tools"

if [[ ! -d "$NDK" ]]; then
  echo "→ download NDK r29 → $NDK" >&2
  mkdir -p "$(dirname "$NDK")"
  tmp="$(mktemp)"
  wget -q "$NDK_URL" -O "$tmp"
  unzip -q "$tmp" -d "$(dirname "$NDK")"
  rm -f "$tmp"
  if [[ -d "$(dirname "$NDK")/android-ndk-r29" && "$NDK" != "$(dirname "$NDK")/android-ndk-r29" ]]; then
  # zip extracts as android-ndk-r29; keep the path build-apk.sh expects.
    if [[ ! -e "$NDK" ]]; then
      mv "$(dirname "$NDK")/android-ndk-r29" "$NDK"
    fi
  fi
fi

if [[ ! -x "$SDK/cmdline-tools/latest/bin/sdkmanager" ]]; then
  echo "→ download Android cmdline-tools" >&2
  tmp="$(mktemp -d)"
  wget -q "$CMDLINE_URL" -O "$tmp/cmdline.zip"
  unzip -q "$tmp/cmdline.zip" -d "$tmp"
  rm -rf "$SDK/cmdline-tools/latest"
  mv "$tmp/cmdline-tools" "$SDK/cmdline-tools/latest"
  rm -rf "$tmp"
fi

export ANDROID_HOME="$SDK"
export ANDROID_NDK_HOME="$NDK"
export ANDROID_NDK_ROOT="$NDK"
unset ANDROID_SDK_ROOT 2>/dev/null || true
export PATH="$SDK/cmdline-tools/latest/bin:$SDK/platform-tools:${HOME}/.cargo/bin:${PATH}"

yes | sdkmanager --licenses >/dev/null 2>&1 || true
sdkmanager \
  "platform-tools" \
  "platforms;android-34" \
  "build-tools;34.0.0" \
  >/dev/null

if ! command -v cargo-apk >/dev/null; then
  echo "→ cargo install cargo-apk" >&2
  cargo install cargo-apk --locked
fi

TARGET="${MANAGER_ANDROID_TARGET:-x86_64-linux-android}"
if ! rustc --print sysroot --target "$TARGET" >/dev/null 2>&1; then
  echo "→ rustup target add $TARGET" >&2
  rustup target add "$TARGET"
fi

echo "ANDROID_HOME=$ANDROID_HOME"
echo "ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
echo "cargo-apk $(cargo-apk --version 2>/dev/null || true)"
echo "rustc $(rustc --version)"
