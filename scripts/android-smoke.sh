#!/usr/bin/env bash
# Install APK on docker-android (or any adb device) and verify NativeActivity launch.
set -euo pipefail

APK="${1:?usage: android-smoke.sh <apk>}"
PACKAGE="${ANDROID_SMOKE_PACKAGE:-uk.nandi.manager}"
ADB_SERIAL="${ADB_SERIAL:-localhost:5555}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-600}"

need() {
  command -v "$1" >/dev/null || {
    echo "missing: $1" >&2
    exit 1
  }
}
need adb

adb_connect() {
  # Local adb emulators are already listed; only network devices need connect.
  if [[ "$ADB_SERIAL" == emulator-* ]]; then
    return 0
  fi
  if [[ "$ADB_SERIAL" == *:* ]]; then
    adb disconnect "$ADB_SERIAL" >/dev/null 2>&1 || true
    adb connect "$ADB_SERIAL"
  fi
}

wait_booted() {
  local deadline=$((SECONDS + BOOT_TIMEOUT))
  adb -s "$ADB_SERIAL" wait-for-device
  until adb -s "$ADB_SERIAL" shell getprop sys.boot_completed 2>/dev/null | grep -q '^1$'; do
    if ((SECONDS >= deadline)); then
      echo "emulator boot timed out after ${BOOT_TIMEOUT}s" >&2
      adb -s "$ADB_SERIAL" shell getprop sys.boot_completed >&2 || true
      exit 1
    fi
    sleep 2
  done
  adb -s "$ADB_SERIAL" shell input keyevent 82 >/dev/null 2>&1 || true
}

install_apk() {
  adb -s "$ADB_SERIAL" install -r "$APK"
}

launch_app() {
  adb -s "$ADB_SERIAL" shell monkey -p "$PACKAGE" -c android.intent.category.LAUNCHER 1
}

verify_running() {
  local deadline=$((SECONDS + 90))
  until adb -s "$ADB_SERIAL" shell pidof "$PACKAGE" >/dev/null 2>&1; do
    if ((SECONDS >= deadline)); then
      echo "process $PACKAGE not running after launch" >&2
      adb -s "$ADB_SERIAL" logcat -d | tail -120 >&2
      return 1
    fi
    sleep 1
  done

  if adb -s "$ADB_SERIAL" logcat -d | grep 'AndroidRuntime: FATAL EXCEPTION' | grep -q "$PACKAGE"; then
    echo "FATAL exception for $PACKAGE in logcat" >&2
    adb -s "$ADB_SERIAL" logcat -d | grep -A20 'AndroidRuntime: FATAL EXCEPTION' >&2
    return 1
  fi

  if ! adb -s "$ADB_SERIAL" logcat -d | grep -q 'android_main start'; then
    echo "warning: did not see manager android_main start in logcat" >&2
  fi

  echo "smoke ok: $PACKAGE running on $ADB_SERIAL"
}

echo "→ adb connect $ADB_SERIAL" >&2
adb_connect
echo "→ wait for emulator boot" >&2
wait_booted
echo "→ install $APK" >&2
install_apk
adb -s "$ADB_SERIAL" logcat -c
echo "→ launch $PACKAGE" >&2
launch_app
verify_running
