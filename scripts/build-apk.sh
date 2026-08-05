#!/usr/bin/env bash
# In-tree cargo-apk build for Agent Manager.
#   ./scripts/build-apk.sh              # debug, aarch64
#   ./scripts/build-apk.sh --release    # release (signed)
#   ./scripts/build-apk.sh --release --target x86_64-linux-android
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/android"
TARGET="${MANAGER_ANDROID_TARGET:-aarch64-linux-android}"
RELEASE=0
CARGO_RESTORE=0

usage() {
  cat >&2 <<'EOF'
Usage: build-apk.sh [--release] [--target <triple>]

  --release   cargo apk build --release (needs signing metadata)
  --target    default aarch64-linux-android (phones); x86_64-linux-android for Waydroid

Env:
  ANDROID_NDK_HOME           default ~/.local/share/android-ndk-r29
  ANDROID_HOME                 default ~/.local/share/android-sdk
  ANDROID_KEYSTORE_PATH        release keystore path (default ~/.android/manager-release.keystore)
  ANDROID_KEYSTORE_BASE64      base64-encoded keystore (CI)
  ANDROID_KEYSTORE_PASSWORD    keystore password (default android)
  ANDROID_KEY_ALIAS            key alias (default manager)
  ANDROID_KEY_PASSWORD         key password (default ANDROID_KEYSTORE_PASSWORD)
EOF
  exit 2
}

restore_cargo_toml() {
  if [[ "$CARGO_RESTORE" -eq 1 && -f "$APP/Cargo.toml.buildbak" ]]; then
    mv -f "$APP/Cargo.toml.buildbak" "$APP/Cargo.toml"
    CARGO_RESTORE=0
  fi
}
trap restore_cargo_toml EXIT

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) RELEASE=1; shift ;;
    --target)
      [[ $# -ge 2 ]] || usage
      TARGET="$2"
      shift 2
      ;;
    -h|--help) usage ;;
    *)
      echo "unknown arg: $1" >&2
      usage
      ;;
  esac
done

export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/.local/share/android-ndk-r29}"
export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/.local/share/android-sdk}"
unset ANDROID_SDK_ROOT 2>/dev/null || true
export PATH="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin:${ANDROID_HOME}/platform-tools:${HOME}/.cargo/bin:${PATH}"

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need cargo
need cargo-apk
need rustc

[[ -d "$ANDROID_NDK_HOME" ]] || {
  echo "Set ANDROID_NDK_HOME=$ANDROID_NDK_HOME" >&2
  exit 1
}

# Map target → clang triple helpers used by cargo-apk / cc crate.
case "$TARGET" in
  aarch64-linux-android)
    export CC_aarch64_linux_android="${CC_aarch64_linux_android:-aarch64-linux-android28-clang}"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER:-$CC_aarch64_linux_android}"
    export AR_aarch64_linux_android="${AR_aarch64_linux_android:-llvm-ar}"
    ;;
  x86_64-linux-android)
    export CC_x86_64_linux_android="${CC_x86_64_linux_android:-x86_64-linux-android28-clang}"
    export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="${CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER:-$CC_x86_64_linux_android}"
    export AR_x86_64_linux_android="${AR_x86_64_linux_android:-llvm-ar}"
    ;;
  *)
    echo "unsupported --target $TARGET" >&2
    exit 1
    ;;
esac

if ! rustc --print sysroot --target "$TARGET" >/dev/null 2>&1; then
  echo "error: rustc missing $TARGET" >&2
  echo "  rustup target add $TARGET" >&2
  exit 1
fi

ensure_release_signing() {
  local keystore="${ANDROID_KEYSTORE_PATH:-$HOME/.android/manager-release.keystore}"
  local storepass="${ANDROID_KEYSTORE_PASSWORD:-android}"
  local alias="${ANDROID_KEY_ALIAS:-manager}"
  local keypass="${ANDROID_KEY_PASSWORD:-$storepass}"

  if [[ -n "${ANDROID_KEYSTORE_BASE64:-}" ]]; then
    echo "decoding release keystore from ANDROID_KEYSTORE_BASE64" >&2
    mkdir -p "$(dirname "$keystore")"
    echo "$ANDROID_KEYSTORE_BASE64" | base64 -d >"$keystore"
  elif [[ ! -f "$keystore" ]]; then
    echo "generating release keystore at $keystore" >&2
    need keytool
    mkdir -p "$(dirname "$keystore")"
    keytool -genkeypair -v \
      -keystore "$keystore" \
      -alias "$alias" \
      -keyalg RSA -keysize 2048 -validity 10000 \
      -storepass "$storepass" -keypass "$keypass" \
      -dname "CN=Agent Manager, OU=nandi.uk, O=nandi, L=Unknown, ST=Unknown, C=US" \
      >/dev/null
  fi

  cp "$APP/Cargo.toml" "$APP/Cargo.toml.buildbak"
  CARGO_RESTORE=1
  # Strip any existing signing.release block, then inject the current keystore.
  awk '
    /^\[package\.metadata\.android\.signing\.release\]/ { skip=1; next }
    skip && /^\[/ { skip=0 }
    !skip { print }
  ' "$APP/Cargo.toml" >"$APP/Cargo.toml.tmp"
  cat >>"$APP/Cargo.toml.tmp" <<EOF

[package.metadata.android.signing.release]
path = "$keystore"
keystore_password = "$storepass"
key_alias = "$alias"
key_password = "$keypass"
EOF
  mv "$APP/Cargo.toml.tmp" "$APP/Cargo.toml"
}

profile=debug
apk_args=(build --target "$TARGET" -p manager-android --lib)
if [[ "$RELEASE" -eq 1 ]]; then
  profile=release
  apk_args+=(--release)
  ensure_release_signing
fi

echo "cargo apk ${apk_args[*]}  (in-tree → $APP)" >&2
echo "rustc $(rustc --version) | $(command -v rustc)" >&2

# cargo-apk stages NDK libs as mode 0555; make writable so rebuilds don't fail.
chmod -R u+w \
  "$APP/target/${profile}/apk" \
  "$APP/target/apk" \
  2>/dev/null || true

(
  cd "$APP"
  cargo apk "${apk_args[@]}" >&2
)

apk=""
for cand in \
  "$APP/target/${profile}/apk/manager.apk" \
  "$APP/target/${TARGET}/${profile}/apk/manager.apk"; do
  if [[ -f "$cand" ]]; then
    apk="$cand"
    break
  fi
done
if [[ -z "$apk" ]]; then
  apk="$(find "$APP/target" -type f -name 'manager.apk' ! -name '*-unaligned.apk' 2>/dev/null | head -1 || true)"
fi
if [[ -z "${apk:-}" || ! -f "$apk" ]]; then
  echo "APK not found under $APP/target" >&2
  exit 1
fi

echo "$apk"
ls -lh "$apk" >&2
