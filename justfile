# Agent Manager
#   nix develop
#   just apk-release          # phone aarch64 release APK
#   just apk-release-x86      # Waydroid x86_64 release APK

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Desktop (existing flake apps)
run:
    nix run .#desktop

build:
    nix run .#build

# Android APK via cargo-apk (needs NDK + rustup android target)
apk *args:
    ./scripts/build-apk.sh {{args}}

apk-release:
    ./scripts/build-apk.sh --release --target aarch64-linux-android

apk-release-x86:
    ./scripts/build-apk.sh --release --target x86_64-linux-android
