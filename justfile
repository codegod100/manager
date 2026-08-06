# Agent Manager
#   nix develop                 # SDK/NDK + cargo-apk (+ prime-agent on PATH)
#   just apk-release            # phone aarch64 release APK
#   just apk-release-x86        # Waydroid x86_64 release APK
#   nix run .#apk -- --release  # same hermetic toolchain without enter shell

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Desktop (existing flake apps)
run:
    nix run .#desktop

build:
    nix run .#build

# Android APK via cargo-apk (flake SDK/NDK + rust-overlay android targets)
apk *args:
    ./scripts/build-apk.sh {{args}}

apk-release:
    ./scripts/build-apk.sh --release --target aarch64-linux-android

apk-release-x86:
    ./scripts/build-apk.sh --release --target x86_64-linux-android

# Install + launch on adb device (docker-android: adb connect localhost:5555)
android-smoke apk:
    ./scripts/android-smoke.sh {{apk}}
