#!/usr/bin/env bash
# The client half of the v1.0 gate: a real server, the real Flutter client.
#
# The `client` CI job runs `flutter test`, and every one of those tests drives a
# mock `http.Client`. That proves the client renders the shapes it expects; it
# cannot prove those shapes are the ones the server sends, and a client that has
# drifted from the API passes every mock. This script is the missing half.
#
# It creates a database for the harness alone, bootstraps the plugin roles into
# it, boots a server on it with the spoofable dev-header stub **off**, and runs
# `client/live/live_client_test.dart`, which asserts on what the server returns
# over a real socket.
#
# It fails loudly. If the server never comes up, or the port is already taken by
# something that is not this server, or the Flutter run reports a failure, the
# exit code is non-zero and the server's own log is printed beside it. Nothing
# here skips.
#
#   scripts/client-live-harness.sh
#
# Inputs (all optional except a database URL):
#   ADJUTANT_TEST_DATABASE_URL        admin URL to derive the harness DB from
#   ADJUTANT_CLIENT_LIVE_DATABASE_URL the harness database, stated directly
#   ADJUTANT_CLIENT_LIVE_PORT         default 8790
#   ADJUTANT_PLUGIN_DIR               default plugins-built
#   ADJUTANT_CLIENT_LIVE_LOG          default a temp file
#
# Requires: a built `./target/debug/adjutant`, `psql`, `python3`, and `flutter`
# on PATH.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT="${ADJUTANT_CLIENT_LIVE_PORT:-8790}"
BASE="http://127.0.0.1:$PORT"
PLUGIN_DIR="${ADJUTANT_PLUGIN_DIR:-plugins-built}"
SERVER="./target/debug/adjutant"
LOG="${ADJUTANT_CLIENT_LIVE_LOG:-$(mktemp -t adjutant-client-live.XXXXXX.log)}"

# The harness gets its own database. Reusing a shared one would make the
# bootstrap below — the first registration on a pristine database — depend on
# whatever a previous step happened to leave behind, and a fixture this harness
# did not create is not a fixture it can reason about.
ADMIN_URL="${ADJUTANT_TEST_DATABASE_URL:-${ADJUTANT_DATABASE_URL:-}}"
LIVE_URL="${ADJUTANT_CLIENT_LIVE_DATABASE_URL:-}"
if [ -z "$LIVE_URL" ]; then
  if [ -z "$ADMIN_URL" ]; then
    echo "::error::no database URL: set ADJUTANT_TEST_DATABASE_URL (the harness derives its own database from it) or ADJUTANT_CLIENT_LIVE_DATABASE_URL" >&2
    exit 2
  fi
  # Derive `…/adjutant_client_live` from the admin URL rather than writing a
  # second copy of the credential: a duplicated credential is one more place to
  # get it wrong.
  LIVE_URL="${ADMIN_URL%/*}/adjutant_client_live"
fi

if [ ! -x "$SERVER" ]; then
  echo "::error::$SERVER is missing — run \`cargo build --workspace\` first" >&2
  exit 2
fi
if [ ! -d "$PLUGIN_DIR" ] || [ -z "$(ls -A "$PLUGIN_DIR" 2>/dev/null)" ]; then
  echo "==> staging plugins into $PLUGIN_DIR"
  python3 scripts/stage-plugins.py
fi

# A port that is already serving something else would make the readiness wait
# below succeed against the wrong process, and every probe after it would be
# evidence about that process instead of this one.
if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then
  echo "::error::something is already listening on 127.0.0.1:$PORT — refusing to run the harness against a server this script did not boot" >&2
  exit 2
fi

echo "==> creating the harness database"
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS adjutant_client_live"
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE adjutant_client_live"

echo "==> bootstrapping plugin roles into it"
# The runtime never needs CREATEROLE: this admin step creates the roles and
# stores their credentials, exactly as the other live harnesses' steps do.
"$SERVER" bootstrap-isolation --database-url "$LIVE_URL" --plugin-dir "$PLUGIN_DIR"

echo "==> booting the server on $BASE (dev headers OFF)"
# ADJUTANT_DEV_HEADERS=false is the point: with the stub on, a caller could name
# its own identity with two headers, and the client's session would prove
# nothing. ADJUTANT_RATE_MAX=0 because this run issues a burst of requests.
ADJUTANT_DATABASE_URL="$LIVE_URL" \
ADJUTANT_PLUGIN_DIR="$PLUGIN_DIR" \
ADJUTANT_DEV_HEADERS=false \
ADJUTANT_RATE_MAX=0 \
ADJUTANT_ALLOW_SUPERUSER=true \
ADJUTANT_LOG=warn \
ADJUTANT_BIND="127.0.0.1:$PORT" \
  "$SERVER" >"$LOG" 2>&1 &
SERVER_PID=$!

teardown() {
  if kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap teardown EXIT

SERVER_UP=0
for _ in $(seq 1 150); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    break
  fi
  if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then
    SERVER_UP=1
    break
  fi
  sleep 0.2
done

if [ "$SERVER_UP" != "1" ]; then
  # A wait loop that gives up quietly turns a dead server into a confusing
  # network error from the harness. Fail here instead, beside the server's own
  # output in this log.
  echo "::error::the harness server never opened 127.0.0.1:$PORT — its log follows" >&2
  cat "$LOG" >&2
  exit 2
fi
echo "==> server is listening (pid $SERVER_PID); its log is $LOG"

echo "==> running the live client gate"
set +e
(
  cd client
  # The live harness is outside test/, so it is not part of the `client` job's
  # default run and needs its own resolve step here.
  flutter pub get
  ADJUTANT_LIVE_BASE="$BASE" flutter test --no-pub live/live_client_test.dart
)
STATUS=$?
set -e

if [ "$STATUS" != "0" ]; then
  echo "::error::the live client gate FAILED (exit $STATUS)" >&2
  echo "--- server log ($LOG) ---" >&2
  cat "$LOG" >&2
  exit "$STATUS"
fi

echo "==> live client gate passed against $BASE"
