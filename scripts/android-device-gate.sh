#!/usr/bin/env bash
# The Android half of the v1.0 gate: the client's APK, installed on a device,
# driven against a real server.
#
# `flutter build apk` proves the client compiles. `flutter test` drives mocked
# wires on the host. Neither proves the thing a scout needs — that the APK
# installs on a phone, launches, reaches the server over the network, and signs
# in. #111 was that gap; this is the gate that closes it.
#
# It does four things, in order, and fails loudly at each:
#   1. a database for this run alone, and the server's roles bootstrapped into it
#   2. a server on the host, dev-header stub OFF, on a port nothing else holds
#   3. the APK built from this tree, installed on the device, launched, and
#      asserted to be the focused activity (the artifact half)
#   4. `integration_test/app_test.dart` on that device, which signs in through
#      the app's own screens and asserts on what the server returned (the
#      behaviour half)
#
# Nothing is skipped: no device, no database, no server, or a failed install is a
# non-zero exit with the log printed beside it.
#
#   scripts/android-device-gate.sh
#
# Inputs (all optional):
#   ADJUTANT_DEVICE_SERIAL       default: the only attached device
#   ADJUTANT_DEVICE_PORT         default 8790 (host side; the device reaches it
#                                as 10.0.2.2:$PORT under the emulator's NAT)
#   ADJUTANT_DEVICE_AVD          AVD to boot if no device is attached (default phonon_test)
#   ADJUTANT_DEVICE_DATABASE_URL a database to use; otherwise a throwaway
#                                `postgres:18-alpine` container on a free port
#   ADJUTANT_DEVICE_KEEP_EMULATOR=1  leave a started emulator running
#   ANDROID_HOME / JAVA_HOME / PATH  otherwise guessed from $HOME
#
# Requires: adb, flutter, cargo, python3, docker (unless a database URL is given).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT="${ADJUTANT_DEVICE_PORT:-8790}"
BASE="http://10.0.2.2:$PORT"
AVD="${ADJUTANT_DEVICE_AVD:-phonon_test}"
USER_NAME="${ADJUTANT_DEVICE_USER:-client-harness-chief}"
PASSWORD="${ADJUTANT_DEVICE_PASSWORD:-client-harness-bootstrap-pass}"
DB_PORT="${ADJUTANT_DEVICE_DB_PORT:-54329}"
DB_CONTAINER="adjutant-device-gate-db"
SERVER_BIN="${ADJUTANT_SERVER_BIN:-target/debug/adjutant}"
PLUGIN_DIR="${ADJUTANT_PLUGIN_DIR:-plugins-built}"
SERVER_LOG="$(mktemp -t adjutant-device-gate.XXXXXX.log)"
STARTED_EMULATOR=0
STARTED_DB=0

# The runner's own environment is not guaranteed to carry the Android toolchain
# (the installer's shell is not the job's shell), so the gate sets its own. These
# are the same paths the emulator unit uses.
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export JAVA_HOME="${JAVA_HOME:-$HOME/jdk-17.0.12+7}"
export PATH="$HOME/flutter/bin:$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$HOME/.cargo/bin:$PATH"

die() { echo "::error::$*" >&2; exit 2; }
step() { echo; echo "==> $*"; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is not on PATH — the gate cannot run without it"
}

for tool in adb flutter python3 cargo; do require "$tool"; done
[ -x "$ANDROID_HOME/platform-tools/adb" ] || die "adb is not at $ANDROID_HOME/platform-tools/adb"

# --- teardown -----------------------------------------------------------------
# Registered before anything is started, so a failure anywhere below still puts
# the host back the way it was found.
teardown() {
  local status=$?
  if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [ "$STARTED_DB" = 1 ]; then docker rm -f "$DB_CONTAINER" >/dev/null 2>&1 || true; fi
  if [ "$STARTED_EMULATOR" = 1 ] && [ "${ADJUTANT_DEVICE_KEEP_EMULATOR:-0}" != "1" ]; then
    echo "==> stopping the emulator this gate started"
    systemctl --user stop "${ADJUTANT_DEVICE_EMULATOR_UNIT:-phonon-emulator}" 2>/dev/null || \
      adb -s "${SERIAL:-}" emu kill >/dev/null 2>&1 || true
  fi
  if [ "$status" != 0 ]; then
    echo "--- server log ($SERVER_LOG) ---" >&2
    tail -40 "$SERVER_LOG" >&2 || true
  fi
  return $status
}
trap teardown EXIT

# --- 1. a device --------------------------------------------------------------
step "looking for a device"
SERIAL="${ADJUTANT_DEVICE_SERIAL:-}"
if [ -z "$SERIAL" ]; then
  SERIAL="$(adb devices | awk '/\tdevice$/ {print $1; exit}')"
fi
if [ -z "$SERIAL" ]; then
  echo "==> no device attached; starting the '$AVD' emulator"
  if systemctl --user start "${ADJUTANT_DEVICE_EMULATOR_UNIT:-phonon-emulator}" 2>/dev/null; then
    STARTED_EMULATOR=1
  else
    pkill -f "emulator -avd $AVD" 2>/dev/null || true
    nohup "$ANDROID_HOME/emulator/emulator" -avd "$AVD" -no-window -no-audio \
      -no-boot-anim -gpu swiftshader_indirect -no-snapshot >>"$SERVER_LOG" 2>&1 &
    STARTED_EMULATOR=1
  fi
fi

if [ -z "$SERIAL" ]; then
  # A wait loop that gives up quietly turns a dead emulator into a confusing
  # failure later; fail here instead, and say what was waited for.
  for _ in $(seq 1 90); do
    SERIAL="$(adb devices | awk '/\tdevice$/ {print $1; exit}')"
    [ -n "$SERIAL" ] && break
    sleep 2
  done
fi
[ -n "$SERIAL" ] || die "no Android device appeared — check 'systemctl --user status ${ADJUTANT_DEVICE_EMULATOR_UNIT:-phonon-emulator}' and 'adb devices' on this host"
echo "==> using device $SERIAL"

BOOTED=0
for _ in $(seq 1 90); do
  if [ "$(adb -s "$SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ]; then
    BOOTED=1; break
  fi
  sleep 2
done
[ "$BOOTED" = 1 ] || die "$SERIAL never finished booting (sys.boot_completed never became 1)"
echo "==> $SERIAL is booted"

# --- 2. a database, and a server on a port nothing else holds ------------------
step "preparing the database the gate runs against"
if [ -n "${ADJUTANT_DEVICE_DATABASE_URL:-}" ]; then
  DB_URL="$ADJUTANT_DEVICE_DATABASE_URL"
  echo "==> using the database the caller named"
else
  require docker
  docker rm -f "$DB_CONTAINER" >/dev/null 2>&1 || true
  echo "==> starting a throwaway postgres:18-alpine on 127.0.0.1:$DB_PORT"
  docker run -d --name "$DB_CONTAINER" -e POSTGRES_PASSWORD=adjutant -e POSTGRES_USER=adjutant \
    -e POSTGRES_DB=adjutant_gate -p "127.0.0.1:$DB_PORT:5432" postgres:18-alpine >/dev/null
  STARTED_DB=1
  DB_URL="postgres://adjutant:adjutant@127.0.0.1:$DB_PORT/adjutant_gate"
  READY=0
  for _ in $(seq 1 60); do
    if docker exec "$DB_CONTAINER" pg_isready -U adjutant -d adjutant_gate >/dev/null 2>&1; then READY=1; break; fi
    sleep 2
  done
  [ "$READY" = 1 ] || die "the gate's postgres never became ready"
fi

if [ ! -x "$SERVER_BIN" ]; then
  echo "==> building the workspace ($SERVER_BIN is missing)"
  cargo build --workspace
fi
[ -x "$SERVER_BIN" ] || die "$SERVER_BIN is still missing after a build"

if [ ! -d "$PLUGIN_DIR" ] || [ -z "$(ls -A "$PLUGIN_DIR" 2>/dev/null)" ]; then
  echo "==> staging plugins into $PLUGIN_DIR"
  python3 scripts/stage-plugins.py
fi

# The port must be ours: a server somebody else left running would make every
# probe below evidence about that process.
if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then
  die "something is already listening on 127.0.0.1:$PORT — refusing to run the gate against a server this script did not boot"
fi

step "bootstrapping the plugin roles and the first user"
"$SERVER_BIN" bootstrap-isolation --database-url "$DB_URL" --plugin-dir "$PLUGIN_DIR"

step "booting the server on 0.0.0.0:$PORT (dev headers OFF)"
# The stub is off so the session the device holds is one the server issued; the
# bind is 0.0.0.0 because the device reaches the host through the emulator's NAT
# (10.0.2.2), not through loopback.
ADJUTANT_DATABASE_URL="$DB_URL" \
ADJUTANT_PLUGIN_DIR="$PLUGIN_DIR" \
ADJUTANT_DEV_HEADERS=false \
ADJUTANT_RATE_MAX=0 \
ADJUTANT_ALLOW_SUPERUSER=true \
ADJUTANT_LOG=warn \
ADJUTANT_BIND="0.0.0.0:$PORT" \
  "$SERVER_BIN" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

UP=0
for _ in $(seq 1 150); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then break; fi
  if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then UP=1; break; fi
  sleep 0.2
done
if [ "$UP" != 1 ]; then
  echo "::error::the gate's server never opened 127.0.0.1:$PORT — its log follows" >&2
  cat "$SERVER_LOG" >&2
  exit 2
fi
echo "==> server is listening (pid $SERVER_PID); its log is $SERVER_LOG"

# The identity the gate signs in as. On a fresh database the first registration
# *is* the bootstrap chief (there is no seeded user and no default password to
# forget); on a database somebody already bootstrapped, the same identity is
# reused. Both paths are checked here rather than assumed, because "login failed"
# would otherwise be the only symptom of a database this gate did not create.
step "creating the gate's identity"
REGISTER=$(curl -s -o /tmp/adjutant-gate-register.json -w '%{http_code}' \
  -X POST "http://127.0.0.1:$PORT/api/auth/register" -H 'content-type: application/json' \
  -d "{\"username\":\"$USER_NAME\",\"password\":\"$PASSWORD\"}")
case "$REGISTER" in
  201) echo "==> registered $USER_NAME as the bootstrap chief" ;;
  403)
    LOGIN=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/api/auth/login" \
      -H 'content-type: application/json' \
      -d "{\"username\":\"$USER_NAME\",\"password\":\"$PASSWORD\"}")
    [ "$LOGIN" = "200" ] || die "the database is already bootstrapped and $USER_NAME cannot sign in ($LOGIN) — point ADJUTANT_DEVICE_DATABASE_URL at a fresh database, or set ADJUTANT_DEVICE_USER/PASSWORD to an identity it has"
    echo "==> $USER_NAME already exists; reusing that identity" ;;
  *) die "POST /api/auth/register answered $REGISTER — see the server log ($SERVER_LOG)" ;;
esac

# --- 3. the APK, on the device ------------------------------------------------
step "building the client's APK from this tree"
(cd client && flutter pub get && flutter build apk --debug)
APK="client/build/app/outputs/flutter-apk/app-debug.apk"
[ -f "$APK" ] || die "$APK was not produced"

step "installing it on $SERIAL and launching it (the artifact half)"
adb -s "$SERIAL" install -r "$APK" | tail -1
PACKAGE="org.chezgoulet.adjutant_client"
adb -s "$SERIAL" shell am force-stop "$PACKAGE" || true
adb -s "$SERIAL" shell am start -n "$PACKAGE/.MainActivity" >/dev/null
FOCUSED=0
for _ in $(seq 1 30); do
  if adb -s "$SERIAL" shell dumpsys window 2>/dev/null | grep -q "mCurrentFocus=.*$PACKAGE"; then FOCUSED=1; break; fi
  sleep 1
done
[ "$FOCUSED" = 1 ] || die "the app installed but never became the focused window — it launched and died, or the manifest changed"
echo "==> the APK installs and launches (mCurrentFocus is $PACKAGE)"

# --- 4. the app, driven against the server ------------------------------------
step "running the integration test on $SERIAL (the behaviour half)"
(cd client && flutter test integration_test/app_test.dart -d "$SERIAL" \
  --dart-define=ADJUTANT_DEVICE_BASE="$BASE" \
  --dart-define=ADJUTANT_DEVICE_USER="$USER_NAME" \
  --dart-define=ADJUTANT_DEVICE_PASSWORD="$PASSWORD")

echo
echo "==> device gate passed: $APK installed and driven on $SERIAL against $BASE"
