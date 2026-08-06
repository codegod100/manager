#!/usr/bin/env bash
# From your laptop: boot a boxd VM and run manager APK in docker-android there.
#
# Prereqs: boxd CLI + auth (`boxd auth login` or export BOXD_TOKEN)
#
#   ./scripts/boxd-launch-docker-android.sh
#   BOXD_MACHINE=my-android ./scripts/boxd-launch-docker-android.sh
set -euo pipefail

export PATH="${HOME}/.local/bin:${PATH}"
BOXD_MACHINE="${BOXD_MACHINE:-manager-android}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TIMEOUT="${BOXD_EXEC_TIMEOUT:-7200}"

need() {
  command -v "$1" >/dev/null || {
    echo "missing: $1 — install: curl -fsSL https://boxd.sh/downloads/install.sh | sh" >&2
    exit 1
  }
}
need boxd

if ! boxd machine list --json 2>/dev/null | grep -q "\"name\":\"${BOXD_MACHINE}\""; then
  echo "→ boxd machine new $BOXD_MACHINE" >&2
  boxd machine new "$BOXD_MACHINE" --json
else
  echo "→ boxd machine start $BOXD_MACHINE (if stopped)" >&2
  boxd machine start "$BOXD_MACHINE" --json 2>/dev/null || true
fi

echo "→ run smoke script on $BOXD_MACHINE (timeout ${TIMEOUT}s)" >&2
boxd machine exec "$BOXD_MACHINE" --timeout "$TIMEOUT" -- \
  "bash -s" <"$ROOT/scripts/boxd-docker-android-smoke.sh"

echo "" >&2
echo "→ expose emulator VNC on https://${BOXD_MACHINE}.boxd.sh" >&2
boxd machine proxy set-port "$BOXD_MACHINE" --port 6080 2>/dev/null || \
  echo "  (run manually: boxd machine proxy set-port $BOXD_MACHINE --port 6080)" >&2

echo "" >&2
echo "Open https://${BOXD_MACHINE}.boxd.sh for the emulator UI." >&2
echo "SSH:  boxd connect $BOXD_MACHINE   or   ssh ${BOXD_MACHINE}.boxd" >&2
