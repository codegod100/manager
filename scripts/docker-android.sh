#!/usr/bin/env bash
# Run budtmo/docker-android on a Linux desktop for APK smoke tests.
#
#   ./scripts/docker-android.sh start    # emulator + http://localhost:6080 VNC
#   ./scripts/docker-android.sh wait     # until adb sees a booted device
#   ./scripts/docker-android.sh stop
#   ./scripts/docker-android.sh status
#   ./scripts/docker-android.sh logs
#
# Then: adb connect localhost:5555
#       just apk-release-x86 && just android-smoke android/target/release/apk/manager.apk
set -euo pipefail

IMAGE="${DOCKER_ANDROID_IMAGE:-budtmo/docker-android:emulator_13.0}"
CONTAINER="${DOCKER_ANDROID_CONTAINER:-manager-android-emulator}"
DEVICE="${EMULATOR_DEVICE:-Samsung Galaxy S10}"
ADB_PORT="${DOCKER_ANDROID_ADB_PORT:-5555}"
VNC_PORT="${DOCKER_ANDROID_VNC_PORT:-6080}"
WEB_VNC="${DOCKER_ANDROID_WEB_VNC:-true}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-600}"
ADB_SERIAL="localhost:${ADB_PORT}"

usage() {
  cat <<EOF
Usage: docker-android.sh <command>

Commands:
  start     Run emulator container (idempotent)
  wait      Wait until emulator is booted (needs adb on PATH)
  stop      Stop container
  rm        Remove container
  status    Show container state
  logs      Follow container logs

Env:
  DOCKER_ANDROID_IMAGE       default budtmo/docker-android:emulator_13.0
  DOCKER_ANDROID_CONTAINER   default manager-android-emulator
  EMULATOR_DEVICE            default Samsung Galaxy S10
  DOCKER_ANDROID_ADB_PORT      default 5555
  DOCKER_ANDROID_VNC_PORT      default 6080
  DOCKER_ANDROID_WEB_VNC       default true (browser UI on VNC port)
EOF
  exit 2
}

need() {
  command -v "$1" >/dev/null || {
    echo "missing: $1" >&2
    exit 1
  }
}

need_docker() {
  need docker
  if docker info >/dev/null 2>&1; then
    return 0
  fi
  if command -v sudo >/dev/null && sudo docker info >/dev/null 2>&1; then
    docker() { sudo docker "$@"; }
    return 0
  fi
  echo "docker daemon not running (start Docker Desktop or system docker)" >&2
  exit 1
}

warn_kvm() {
  if [[ ! -e /dev/kvm ]]; then
    echo "warning: /dev/kvm missing — emulator will be very slow or may fail" >&2
    echo "  Linux: enable KVM in BIOS; install qemu-kvm" >&2
    echo "  WSL2: see budtmo/docker-android README (nestedVirtualization)" >&2
  fi
}

container_running() {
  docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"
}

container_exists() {
  docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER"
}

cmd_start() {
  need_docker
  warn_kvm

  if container_running; then
    echo "already running: $CONTAINER" >&2
  elif container_exists; then
    echo "→ docker start $CONTAINER" >&2
    docker start "$CONTAINER"
  else
    echo "→ docker run $IMAGE" >&2
    docker run -d \
      --name "$CONTAINER" \
      --privileged \
      --device /dev/kvm \
      -p "${VNC_PORT}:6080" \
      -p "${ADB_PORT}:5555" \
      -p 5554:5554 \
      -e "EMULATOR_DEVICE=${DEVICE}" \
      -e "WEB_VNC=${WEB_VNC}" \
      -e APPIUM=false \
      -e AUTO_RECORD=false \
      -e ENFORCE_DEV_MODE=false \
      "$IMAGE"
  fi

  echo "" >&2
  echo "Emulator UI:  http://localhost:${VNC_PORT}" >&2
  echo "ADB:          adb connect ${ADB_SERIAL}" >&2
  echo "Boot status:  docker exec $CONTAINER cat device_status" >&2
}

cmd_wait() {
  need_docker
  if ! container_running; then
    echo "container not running — run: $0 start" >&2
    exit 1
  fi

  if command -v adb >/dev/null; then
    echo "→ adb connect ${ADB_SERIAL}" >&2
    adb disconnect "$ADB_SERIAL" >/dev/null 2>&1 || true
    adb connect "$ADB_SERIAL"
  fi

  local deadline=$((SECONDS + BOOT_TIMEOUT))
  if command -v adb >/dev/null; then
    adb -s "$ADB_SERIAL" wait-for-device
    until adb -s "$ADB_SERIAL" shell getprop sys.boot_completed 2>/dev/null | grep -q '^1$'; do
      if ((SECONDS >= deadline)); then
        echo "boot timed out after ${BOOT_TIMEOUT}s" >&2
        docker exec "$CONTAINER" cat device_status >&2 || true
        exit 1
      fi
      sleep 2
    done
    adb -s "$ADB_SERIAL" shell input keyevent 82 >/dev/null 2>&1 || true
    echo "emulator ready (${ADB_SERIAL})" >&2
    return 0
  fi

  echo "→ waiting via device_status (install adb for faster checks)" >&2
  until docker exec "$CONTAINER" cat device_status 2>/dev/null | grep -qiE 'ready|connected|healthy'; do
    if ((SECONDS >= deadline)); then
      echo "boot timed out after ${BOOT_TIMEOUT}s" >&2
      docker exec "$CONTAINER" cat device_status >&2 || true
      exit 1
    fi
    sleep 5
  done
  echo "emulator reports ready (check http://localhost:${VNC_PORT})" >&2
}

cmd_stop() {
  need_docker
  if container_running; then
    docker stop "$CONTAINER"
    echo "stopped $CONTAINER" >&2
  else
    echo "$CONTAINER is not running" >&2
  fi
}

cmd_rm() {
  need_docker
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  echo "removed $CONTAINER (if it existed)" >&2
}

cmd_status() {
  need_docker
  docker ps -a --filter "name=^${CONTAINER}$" --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
  if container_running; then
    docker exec "$CONTAINER" cat device_status 2>/dev/null || true
  fi
}

cmd_logs() {
  need_docker
  docker logs -f "$CONTAINER"
}

main() {
  local cmd="${1:-}"
  case "$cmd" in
    start) cmd_start ;;
    wait) cmd_wait ;;
    stop) cmd_stop ;;
    rm) cmd_rm ;;
    status) cmd_status ;;
    logs) cmd_logs ;;
    -h|--help|help) usage ;;
    *)
      [[ -n "$cmd" ]] && echo "unknown command: $cmd" >&2
      usage
      ;;
  esac
}

main "${1:-}"
