#!/usr/bin/env bash
# Simulate a long-press on docker-android / any adb device.
#
# egui treats a still press longer than ~0.8s as long_touched (secondary click).
# adb encodes that as a zero-distance swipe with a duration:
#
#   adb shell input swipe X Y X Y DURATION_MS
#
# Usage:
#   scripts/android-longpress.sh [x] [y] [duration_ms]
# Defaults: center-ish phone coords, 1000ms hold.
set -euo pipefail

ADB_SERIAL="${ADB_SERIAL:-localhost:5555}"
X="${1:-540}"
Y="${2:-900}"
DURATION_MS="${3:-1000}"

need() {
  command -v "$1" >/dev/null || {
    echo "missing: $1" >&2
    exit 1
  }
}
need adb

echo "→ long-press ($X,$Y) for ${DURATION_MS}ms on $ADB_SERIAL" >&2
adb -s "$ADB_SERIAL" shell input swipe "$X" "$Y" "$X" "$Y" "$DURATION_MS"
echo "ok"
