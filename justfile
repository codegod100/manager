# Agent Manager
#   nix develop                 # desktop: host rust + prime-agent (slim)
#   nix develop .#android       # SDK/NDK + cargo-apk (large)
#   just apk-release            # phone aarch64 release APK
#   just apk-release-x86        # Waydroid x86_64 release APK
#   nix run .#apk -- --release  # hermetic Android toolchain without enter shell

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Desktop (existing flake apps)
run:
    nix run .#desktop

build:
    nix run .#build

# Android APK via cargo-apk (needs nix develop .#android, or use nix run .#apk)
apk *args:
    ./scripts/build-apk.sh {{args}}

apk-release:
    ./scripts/build-apk.sh --release --target aarch64-linux-android

apk-release-x86:
    ./scripts/build-apk.sh --release --target x86_64-linux-android

# Install + launch on adb device (docker-android: adb connect localhost:5555)
android-smoke apk:
    ./scripts/android-smoke.sh {{apk}}
