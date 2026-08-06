# Agent Manager
#   just android     # emulator + build APK + install + launch (docker-android)
#   just emu         # emulator only → http://localhost:6080

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Desktop
run:
    nix run .#desktop

build:
    nix run .#build

# Android APK (cargo-apk + NDK)
apk *args:
    ./scripts/build-apk.sh {{args}}

apk-release:
    ./scripts/build-apk.sh --release --target aarch64-linux-android

apk-release-x86:
    ./scripts/build-apk.sh --release --target x86_64-linux-android

# --- docker-android one-liners ---
emu:
    ./scripts/docker-android.sh up

android:
    ./scripts/docker-android.sh up
    APK="$(./scripts/build-apk.sh --target x86_64-linux-android)"
    ./scripts/android-smoke.sh "$APK"

# boxd: boot VM + full smoke (needs boxd auth on laptop)
boxd:
    ./scripts/boxd-launch-docker-android.sh

# --- optional granular ---
docker-android-stop:
    ./scripts/docker-android.sh stop

docker-android-status:
    ./scripts/docker-android.sh status
