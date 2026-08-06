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

# docker-android emulator on desktop (VNC http://localhost:6080, adb localhost:5555)
docker-android:
    ./scripts/docker-android.sh start

docker-android-wait:
    ./scripts/docker-android.sh wait

docker-android-stop:
    ./scripts/docker-android.sh stop

docker-android-status:
    ./scripts/docker-android.sh status

# Full flow on boxd (laptop with boxd auth): boot VM + docker-android + APK smoke
boxd-docker-android:
    ./scripts/boxd-launch-docker-android.sh

# On a boxd VM already (SSH): docker-android + build + smoke
boxd-docker-android-smoke:
    ./scripts/boxd-docker-android-smoke.sh

# Build x86 debug APK, install, and launch on local docker-android
docker-android-smoke:
    ./scripts/docker-android.sh start
    ./scripts/docker-android.sh wait
    APK="$(./scripts/build-apk.sh --target x86_64-linux-android)"
    ./scripts/android-smoke.sh "$APK"

# Install + launch on adb device (docker-android: adb connect localhost:5555)
android-smoke apk:
    ./scripts/android-smoke.sh {{apk}}
