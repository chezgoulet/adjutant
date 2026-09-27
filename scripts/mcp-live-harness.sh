#!/usr/bin/env bash
# The MCP gate: a real MCP client, Adjutant's real MCP routes, and the audit rows.
#
# `adjutant-mcp` exposes the MCP surface as HTTP + JSON and says in its own
# documentation that the JSON-RPC facade a host speaks is "a client concern".
# `tools/mcp-bridge/adjutant_mcp_bridge.py` is that facade; this script proves the
# whole path with it, against a server it booted itself:
#
#   1. a **chief** connects through the bridge, sees every tool their grants
#      reach, and calls one — the API answers, and `mcp.invocations` records `ok`;
#   2. a **scout** holding only `mcp:connect`, `mcp:invoke` and `membership:read`
#      connects through the same bridge, sees **one** tool rather than eleven,
#      and is **refused** when they ask for a tool by name anyway — recorded as
#      `denied`, the most security-relevant row in that table.
#
# That pair is the point: "tools exposed, permissions filtered, invocations
# logged" (SPEC §7.10) is a comparison between two callers, not a count for one.
#
# If `hermes` is on PATH it also registers the bridge with a **real MCP host** in
# a scratch HERMES_HOME and runs `hermes mcp test`, which is the "a Hermes agent
# can connect" half. When `hermes` is absent that half reports SKIP — loudly, and
# it is not a pass: read the line before you read the exit code.
#
# Inputs (all optional except a database URL):
#   ADJUTANT_TEST_DATABASE_URL        admin URL to derive the harness DB from
#   ADJUTANT_MCP_LIVE_DATABASE_URL    the harness database, stated directly
#   ADJUTANT_MCP_LIVE_PORT            default 8791
#   ADJUTANT_PLUGIN_DIR               default plugins-built
#   ADJUTANT_MCP_PYTHON               interpreter with the `mcp` SDK for the probe
#                                     (default: the Hermes venv if it exists)
#
# Requires: a built `./target/debug/adjutant`, `psql`, `python3`, `curl`.
# The probe needs the `mcp` Python SDK; the step below says how to get it.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT="${ADJUTANT_MCP_LIVE_PORT:-8791}"
BASE="http://127.0.0.1:$PORT"
PLUGIN_DIR="${ADJUTANT_PLUGIN_DIR:-plugins-built}"
SERVER="${ADJUTANT_SERVER:-./target/debug/adjutant}"
BRIDGE="tools/mcp-bridge/adjutant_mcp_bridge.py"
PROBE="tools/mcp-bridge/live_probe.py"
LOG="${ADJUTANT_MCP_LIVE_LOG:-$(mktemp -t adjutant-mcp-live.XXXXXX.log)}"
PROOF_HOME="${ADJUTANT_MCP_PROOF_HOME:-$(mktemp -d -t adjutant-mcp-proof.XXXXXX)}"
TMP="$(mktemp -d -t adjutant-mcp-harness.XXXXXX)"

CHIEF_USER="mcp-harness-chief"
CHIEF_PASS="mcp-harness-bootstrap-pass"
SCOUT_USER="mcp-harness-scout"
SCOUT_PASS="mcp-harness-scout-pass"
SCOUT_ROLE="mcp_harness_scout"

ADMIN_URL="${ADJUTANT_TEST_DATABASE_URL:-${ADJUTANT_DATABASE_URL:-}}"
LIVE_URL="${ADJUTANT_MCP_LIVE_DATABASE_URL:-}"
if [ -z "$LIVE_URL" ]; then
  if [ -z "$ADMIN_URL" ]; then
    echo "::error::no database URL: set ADJUTANT_TEST_DATABASE_URL (the harness derives its own database from it) or ADJUTANT_MCP_LIVE_DATABASE_URL" >&2
    exit 2
  fi
  LIVE_URL="${ADMIN_URL%/*}/adjutant_mcp_live"
fi
# The probe uses the SDK; the Hermes venv carries it on a developer machine, and
# CI installs it into the system python. Prefer whichever actually has it.
PROBE_PY="${ADJUTANT_MCP_PYTHON:-}"
if [ -z "$PROBE_PY" ]; then
  if /home/robot/.hermes/hermes-agent/venv/bin/python3 -c "import mcp" 2>/dev/null; then
    PROBE_PY="/home/robot/.hermes/hermes-agent/venv/bin/python3"
  else
    PROBE_PY="python3"
  fi
fi
if ! "$PROBE_PY" -c "import mcp" 2>/dev/null; then
  echo "::error::$PROBE_PY has no \`mcp\` package — install it (pip install 'mcp>=1.0') or point ADJUTANT_MCP_PYTHON at an interpreter that has it" >&2
  exit 2
fi

if [ ! -x "$SERVER" ]; then
  echo "::error::$SERVER is missing — run \`cargo build --workspace\` first" >&2
  exit 2
fi
if [ ! -d "$PLUGIN_DIR" ] || [ -z "$(ls -A "$PLUGIN_DIR" 2>/dev/null)" ]; then
  echo "==> staging plugins into $PLUGIN_DIR"
  python3 scripts/stage-plugins.py
fi

# A port already serving something else would make every probe below evidence
# about that process instead of this one.
if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then
  echo "::error::something is already listening on 127.0.0.1:$PORT — refusing to run against a server this script did not boot" >&2
  exit 2
fi

cleanup() {
  if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP" "$PROOF_HOME"
}
trap cleanup EXIT

echo "==> creating the harness database"
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS adjutant_mcp_live"
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE adjutant_mcp_live"

echo "==> bootstrapping plugin roles into it"
"$SERVER" bootstrap-isolation --database-url "$LIVE_URL" --plugin-dir "$PLUGIN_DIR"

# The plugin relays to the API over the core's mediated HTTP client, so it has to
# be told **this** server's address. It defaults to the core's default bind
# (8787); a harness on another port, or a deployment behind a proxy, must set it
# — this is the same `mcp.base_url` key the plugin documents.
echo "==> pointing the mcp plugin at $BASE"
psql "$LIVE_URL" -v ON_ERROR_STOP=1 -q \
  -c "UPDATE core.plugins SET config = jsonb_set(config, '{base_url}', '\"$BASE\"'::jsonb), updated_at = now() WHERE id = 'mcp'"

echo "==> booting the server on $BASE (dev headers OFF)"
ADJUTANT_DATABASE_URL="$LIVE_URL" \
ADJUTANT_PLUGIN_DIR="$PLUGIN_DIR" \
ADJUTANT_DEV_HEADERS=false \
ADJUTANT_RATE_MAX=0 \
ADJUTANT_ALLOW_SUPERUSER=true \
ADJUTANT_LOG=warn \
ADJUTANT_BIND="127.0.0.1:$PORT" \
  "$SERVER" >"$LOG" 2>&1 &
SERVER_PID=$!

SERVER_UP=0
for _ in $(seq 1 150); do
  kill -0 "$SERVER_PID" 2>/dev/null || break
  if python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),1); sys.exit(0)" 2>/dev/null; then
    SERVER_UP=1
    break
  fi
  sleep 0.2
done
if [ "$SERVER_UP" != "1" ]; then
  echo "::error::the harness server never opened 127.0.0.1:$PORT — its log follows" >&2
  cat "$LOG" >&2
  exit 2
fi
echo "==> server is listening (pid $SERVER_PID); its log is $LOG"

echo "==> registering the bootstrap chief, and a scout with three permissions"
"$PROBE_PY" - "$BASE" "$LIVE_URL" "$TMP" "$CHIEF_USER" "$CHIEF_PASS" "$SCOUT_USER" "$SCOUT_PASS" "$SCOUT_ROLE" <<'PY'
import json, sys, urllib.error, urllib.request

base, live, tmp, chief_user, chief_pass, scout_user, scout_pass, scout_role = sys.argv[1:9]


def call(method, path, body=None, token=None):
    request = urllib.request.Request(
        base + path,
        data=(json.dumps(body).encode() if body is not None else None),
        method=method,
    )
    if body is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, json.loads(response.read().decode() or "{}")
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read().decode() or "{}")


status, body = call("POST", "/api/auth/register", {"username": chief_user, "password": chief_pass})
if status == 403:
    # The database was already bootstrapped by an earlier run.
    status, body = call("POST", "/api/auth/login", {"username": chief_user, "password": chief_pass})
if status not in (200, 201):
    raise SystemExit(f"bootstrap chief: {status} {body}")
chief = body["token"]
open(f"{tmp}/session-chief", "w").write(chief)

status, _ = call("POST", "/api/auth/users", {"username": scout_user, "password": scout_pass, "roles": []}, chief)
if status not in (200, 201) and status != 409:
    raise SystemExit(f"create scout: {status}")

with open(f"{tmp}/sql", "w") as sql:
    sql.write(f"""
INSERT INTO core.roles (id, display_name, description)
VALUES ('{scout_role}', 'Scout (MCP harness)', 'Permission-filtering proof role')
ON CONFLICT (id) DO NOTHING;
INSERT INTO core.role_permissions (role_id, permission_id)
SELECT '{scout_role}', id FROM core.permissions
WHERE id IN ('mcp:connect', 'mcp:invoke', 'membership:read')
ON CONFLICT DO NOTHING;
""")
print(f"prepared: chief={chief_user} scout={scout_user} role={scout_role}")
PY

psql "$LIVE_URL" -v ON_ERROR_STOP=1 -q -f "$TMP/sql"

"$PROBE_PY" - "$BASE" "$TMP" "$CHIEF_USER" "$CHIEF_PASS" "$SCOUT_USER" "$SCOUT_PASS" "$SCOUT_ROLE" <<'PY'
import json, sys, urllib.error, urllib.request

base, tmp, chief_user, chief_pass, scout_user, scout_pass, scout_role = sys.argv[1:8]


def call(method, path, body=None, token=None):
    request = urllib.request.Request(
        base + path,
        data=(json.dumps(body).encode() if body is not None else None),
        method=method,
    )
    if body is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, json.loads(response.read().decode() or "{}")
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read().decode() or "{}")


chief = open(f"{tmp}/session-chief").read().strip()
status, _ = call("POST", "/api/auth/roles", {"username": scout_user, "roles": [scout_role]}, chief)
if status != 200:
    raise SystemExit(f"assign scout role: {status}")
status, body = call("POST", "/api/auth/login", {"username": scout_user, "password": scout_pass})
if status != 200:
    raise SystemExit(f"scout login: {status} {body}")
open(f"{tmp}/session-scout", "w").write(body["token"])
print("sessions minted for both identities")
PY

export ADJUTANT_BRIDGE="$REPO_ROOT/$BRIDGE"
export ADJUTANT_BASE_URL="$BASE"
export ADJUTANT_CLIENT="adjutant-mcp-live-harness"

echo "==> chief: every tool their grants reach, and a real call"
ADJUTANT_SESSION_FILE="$TMP/session-chief" ADJUTANT_CLIENT="mcp-harness-chief" \
  "$PROBE_PY" "$PROBE" chief

echo "==> scout: one tool visible, and a refusal when they ask for another"
ADJUTANT_SESSION_FILE="$TMP/session-scout" ADJUTANT_CLIENT="mcp-harness-scout" \
ADJUTANT_EXPECT_FEWER_THAN=11 \
  "$PROBE_PY" "$PROBE" scout

echo "==> the audit trail records both outcomes"
AUDIT="$(psql "$LIVE_URL" -tAc "
  SELECT tool || ' ' || status FROM mcp.invocations
   WHERE tool IN ('membership_list_members', 'missions_create_mission')
     AND status IN ('ok', 'denied')
   ORDER BY id")"
echo "$AUDIT"
if ! grep -q "^membership_list_members ok$" <<<"$AUDIT"; then
  echo "::error::no \`ok\` invocation row for the chief's call — the call did not reach the API" >&2
  exit 1
fi
if ! grep -q "^missions_create_mission denied$" <<<"$AUDIT"; then
  echo "::error::no \`denied\` invocation row for the scout's refused call — a refusal nobody records is a refusal nobody can audit" >&2
  exit 1
fi
echo "PASS  audit trail: membership_list_members ok, missions_create_mission denied"

if command -v hermes >/dev/null 2>&1; then
  echo "==> a real MCP host: registering the bridge with Hermes in $PROOF_HOME"
  HERMES_HOME="$PROOF_HOME" printf 'y\n' | HERMES_HOME="$PROOF_HOME" hermes mcp add adjutant \
    --env "ADJUTANT_BASE_URL=$BASE" "ADJUTANT_SESSION_FILE=$TMP/session-chief" \
          ADJUTANT_CLIENT=hermes-mcp-test \
    --command python3 --args "$REPO_ROOT/$BRIDGE" >"$TMP/hermes-add.log" 2>&1 || {
      echo "::error::registering the bridge with Hermes failed — its output follows" >&2
      cat "$TMP/hermes-add.log" >&2
      exit 1
    }
  HERMES_HOME="$PROOF_HOME" hermes mcp test adjutant >"$TMP/hermes-test.log" 2>&1 || {
    echo "::error::\`hermes mcp test adjutant\` failed — its output follows" >&2
    cat "$TMP/hermes-test.log" >&2
    exit 1
  }
  TOOLS="$(grep -cE '^    [a-z_]+ +' "$TMP/hermes-test.log" || true)"
  echo "PASS  hermes connects and discovers tools through the bridge ($TOOLS tool lines)"
else
  echo "SKIP  a real MCP host was NOT exercised on this machine — \`hermes\` is not on PATH."
  echo "SKIP  The protocol path above was driven by the \`mcp\` SDK client instead; do not read this run as the Hermes half."
fi

echo "==> MCP gate passed against $BASE"
