#!/usr/bin/env bash
# Restore an Adjutant deployment from a dump produced by ./deploy/backup.sh.
#
#   ./deploy/restore.sh <dump> --yes
#
# This DROPS the current database. `--yes` is required so it cannot be run by
# accident or by a loop that meant to do something else.
#
# Why this is more than `pg_restore` — and the whole reason this script exists
# rather than a paragraph in a README:
#
#   A `pg_dump` of a database contains the database. It does NOT contain the
#   cluster's roles, because roles are cluster-level objects. Adjutant runs every
#   plugin as its own role (`adjutant_plugin_<id>`) and the core as
#   `adjutant_app`, so a restore onto a clean host produces a database whose
#   grantee roles do not exist:
#
#     pg_restore: error: could not execute query: ERROR: role "adjutant_plugin_auth" does not exist
#     Command was: GRANT SELECT,INSERT,DELETE ON TABLE core.sessions TO adjutant_plugin_auth;
#     pg_restore: warning: errors ignored on restore: 6
#
#   `errors ignored` is the dangerous part: the restore reports success, and the
#   server then crash-loops on `password authentication failed for user
#   "adjutant_app"` — a message that points at a password when the actual fault
#   is a role that was never created.
#
#   `bootstrap-isolation` is what closes it. It creates the roles, transfers
#   ownership of the plugin schemas to them, hands the core schema to
#   `adjutant_app`, stores each plugin's credential and re-asserts the grant
#   allowlist. Measured at 1s on a 15-plugin deployment.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE=(docker compose -f "$HERE/compose.proxy.yml")

DUMP="${1:-}"
ASSUME_YES="${2:-}"
[ -n "$DUMP" ] || { echo "usage: $0 <dump> --yes" >&2; exit 2; }
[ -f "$DUMP" ] || { echo "FAIL: no such dump: $DUMP" >&2; exit 1; }
[ "$ASSUME_YES" = "--yes" ] || {
  echo "FAIL: this DROPS the current database. Re-run with --yes if that is what you want." >&2
  exit 2
}

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok — $*"; }

: "${POSTGRES_PASSWORD:?set POSTGRES_PASSWORD (see deploy/README.md)}"
: "${ADJUTANT_APP_PASSWORD:?set ADJUTANT_APP_PASSWORD (see deploy/README.md)}"

# The bootstrap connection must NOT be the application role. The app's URL points
# at `adjutant_app`, which is exactly the role that does not exist yet on a clean
# host — connecting as it to create it is the chicken-and-egg that made the
# documented bring-up fail on its first real run.
ADMIN_URL="postgres://adjutant:${POSTGRES_PASSWORD}@postgres:5432/adjutant"

psql_admin() { "${COMPOSE[@]}" exec -T postgres psql -U adjutant -d "${1:-adjutant}" "${@:2}" </dev/null; }

echo "== 0. stop the application so nothing writes mid-restore =="
"${COMPOSE[@]}" stop adjutant </dev/null >/dev/null 2>&1 || true
ok "adjutant stopped"

echo "== 1. recreate the database =="
# Validate the dump BEFORE dropping anything. The first run of this script got
# this wrong in a way worth keeping: it dropped the database, then handed
# pg_restore an empty stdin (a trailing `</dev/null` silently overrode the
# `< "$DUMP"` redirect, because the last redirect wins), and left an empty
# database behind with a perfectly good dump on disk. A restore script must fail
# before the destructive step, not after it.
"${COMPOSE[@]}" exec -T postgres pg_restore -l < "$DUMP" >/dev/null 2>&1 \
  || fail "$DUMP is not a readable pg_restore archive — refusing to drop the database for it"
ok "the dump is readable; safe to proceed"

psql_admin postgres -c 'DROP DATABASE IF EXISTS adjutant' -c 'CREATE DATABASE adjutant OWNER adjutant' >/dev/null
ok "dropped and recreated"

echo "== 2. restore the dump =="
start=$(date +%s)
"${COMPOSE[@]}" exec -T postgres pg_restore -U adjutant -d adjutant --no-owner < "$DUMP" 2>&1 | tail -20
# pg_restore exits non-zero on warnings, and "errors ignored" is expected here
# *before* the roles exist. The check that matters is step 3 and step 5: if the
# roles are missing, the server will not answer.
elapsed_restore=$(( $(date +%s) - start ))
ok "pg_restore finished in ${elapsed_restore}s"

echo "== 3. create the roles, ownership and grants (THE STEP A DUMP CANNOT DO) =="
start=$(date +%s)
"${COMPOSE[@]}" run --rm -e ADJUTANT_DATABASE_URL="$ADMIN_URL" adjutant \
  bootstrap-isolation --app-role adjutant_app --app-password "$ADJUTANT_APP_PASSWORD" </dev/null 2>&1 | tail -3
elapsed_bootstrap=$(( $(date +%s) - start ))
ok "bootstrap-isolation finished in ${elapsed_bootstrap}s"

echo "== 4. start the application =="
"${COMPOSE[@]}" up -d adjutant caddy </dev/null >/dev/null 2>&1

ready=0
for _ in $(seq 1 60); do
  # `-T` and an explicit `</dev/null`: `docker compose run` attaches stdin by
  # default, and when this script is itself being read from stdin (`bash -s`) the
  # container swallows the remainder of the script. The probe then appears to
  # work while everything after it silently does not happen. Same class of bug as
  # a trailing redirect overriding an earlier one — worth naming twice, because
  # both are invisible.
  if "${COMPOSE[@]}" run --rm -T --no-deps probe \
      "curl -s -o /dev/null -w '%{http_code}' http://caddy:8080/" </dev/null 2>/dev/null \
      | tr -d '\r' | grep -q 200; then
    ready=1; break
  fi
  sleep 2
done
if [ "$ready" != "1" ]; then
  echo "--- the app's own output (last 30 lines) ---" >&2
  "${COMPOSE[@]}" logs --tail 30 adjutant >&2 || true
  fail "the app is not answering after the restore — do not walk away from this state"
fi
ok "the app answers through the proxy"

echo "== 5. the restore is not complete until the roles can do their jobs =="
grants="$(psql_admin adjutant -tAc "select count(*) from information_schema.role_table_grants where grantee like 'adjutant_plugin%'")"
[ "${grants:-0}" -gt 0 ] || fail "no plugin-role grants exist — the runtime will refuse to work"
ok "$grants plugin-role grant(s) present"

roles="$(psql_admin adjutant -tAc "select count(*) from pg_roles where rolname like 'adjutant%'")"
ok "$roles adjutant role(s) in the cluster"

echo
echo "restore complete. Mechanical time: $((elapsed_restore + elapsed_bootstrap))s"
echo "Verify the audit chain, which is hash-chained and the one thing a partial"
echo "restore can quietly corrupt: GET /api/audit/verify (admin only)."
