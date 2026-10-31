#!/usr/bin/env bash
# Restore an Adjutant deployment from a bundle produced by ./deploy/backup.sh.
#
#   ./deploy/restore.sh <dump|dump.age> --yes
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
#   `errors ignored` is the dangerous part: the old script reported success, and
#   the server then crash-looped on `password authentication failed for user
#   "adjutant_app"` — a message that points at a password when the actual fault
#   is a role that was never created.
#
#   This script now makes the order right instead of calling the error normal:
#   restore with `--no-owner --no-acl` (there is no role to own or grant to yet),
#   run `bootstrap-isolation` to create the roles and re-assert ownership and the
#   grant allowlist, then run `verify-isolation` — which checks every installed
#   plugin's role, its schema/table ownership, its allowlist, and a live
#   connection as that role. A restore that cannot answer the probes fails and
#   names the missing role or table; it never prints `ok`.
#
# **Encrypted bundles.** A `.dump.age` is decrypted with
# `ADJUTANT_BACKUP_AGE_IDENTITY` before anything is dropped. No identity means no
# restore — never drop a database whose bundle you cannot read. A plaintext
# `.dump` is accepted (older bundles exist) and is called out as credential
# material.
#
# **No plaintext dump is ever written to disk.** The decrypt is piped straight
# into `pg_restore -l` for validation and into `pg_restore` for the restore, so
# the clear-text archive (which holds every plugin role's password) never gets a
# path, not even a `chmod 600` one in `/tmp` that a `SIGKILL`, an OOM kill or a
# host crash could leave behind. The bundle is read twice, so it is decrypted
# twice; that is deliberate and cheaper than any on-disk intermediate.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE=(docker compose -f "$HERE/compose.proxy.yml")

DUMP="${1:-}"
ASSUME_YES="${2:-}"
[ -n "$DUMP" ] || { echo "usage: $0 <dump|dump.age> --yes" >&2; exit 2; }
[ -f "$DUMP" ] || { echo "FAIL: no such dump: $DUMP" >&2; exit 1; }
[ "$ASSUME_YES" = "--yes" ] || {
  echo "FAIL: this DROPS the current database. Re-run with --yes if that is what you want." >&2
  exit 2
}

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok — $*"; }
warn() { echo "  WARN — $*" >&2; }

: "${POSTGRES_PASSWORD:?set POSTGRES_PASSWORD (see deploy/README.md)}"
: "${ADJUTANT_APP_PASSWORD:?set ADJUTANT_APP_PASSWORD (see deploy/README.md)}"

# The bootstrap connection must NOT be the application role. The app's URL points
# at `adjutant_app`, which is exactly the role that does not exist yet on a clean
# host — connecting as it to create it is the chicken-and-egg that made the
# documented bring-up fail on its first real run.
ADMIN_URL="postgres://adjutant:${POSTGRES_PASSWORD}@postgres:5432/adjutant"

psql_admin() { "${COMPOSE[@]}" exec -T postgres psql -U adjutant -d "${1:-adjutant}" "${@:2}" </dev/null; }

age_decrypt() {
  local src="$1"
  local identity="$2"
  if command -v age >/dev/null 2>&1; then
    age --decrypt --identity "$identity" < "$src"
  else
    # `docker compose run` is how the deploy stack already runs one-off commands;
    # mounting the identity read-only keeps it out of the image and out of /app.
    local identity_abs
    identity_abs="$(readlink -f "$identity" 2>/dev/null || printf '%s' "$identity")"
    "${COMPOSE[@]}" run --rm -T --no-deps --entrypoint age \
      -v "$identity_abs:/run/adjutant-age-identity:ro" adjutant \
      --decrypt --identity /run/adjutant-age-identity < "$src"
  fi
}

# The archive as a stream on stdout. Every read of a bundle goes through this, so
# a decrypted copy never lands on disk: it exists only inside the pipe between
# this function and the `pg_restore` consuming it.
bundle_stream() {
  case "$DUMP" in
    *.age) age_decrypt "$DUMP" "$IDENTITY" ;;
    *) cat -- "$DUMP" ;;
  esac
}

echo "== 0. open the bundle before dropping anything =="
encrypted=0
IDENTITY=""
case "$DUMP" in
  *.age)
    encrypted=1
    IDENTITY="${ADJUTANT_BACKUP_AGE_IDENTITY:-}"
    [ -n "$IDENTITY" ] || fail \
      "$DUMP is encrypted; set ADJUTANT_BACKUP_AGE_IDENTITY to the age identity file. \
Refusing to drop the database for a bundle this host cannot read."
    [ -f "$IDENTITY" ] || fail \
      "ADJUTANT_BACKUP_AGE_IDENTITY=$IDENTITY does not exist. Refusing to drop the database."
    # Metadata check before the decrypt and before the DROP: a truncated or
    # overwritten file should be named as "not an age bundle", not surface as a
    # confusing decrypt error later.
    first_line=""
    IFS= read -r first_line < "$DUMP" || true
    [ "$first_line" = 'age-encryption.org/v1' ] \
      || fail "$DUMP does not start with the age header — refusing to drop the database for it"
    ;;
  *)
    warn "$DUMP is a PLAINTEXT bundle: it is credential material (every plugin role's password)."
    ;;
esac

# Validate the archive BEFORE dropping anything. The first run of this script got
# this wrong in a way worth keeping: it dropped the database, then handed
# pg_restore an empty stdin (a trailing `</dev/null` silently overrode the
# `< "$DUMP"` redirect, because the last redirect wins), and left an empty
# database behind with a perfectly good dump on disk. A restore script must fail
# before the destructive step, not after it.
#
# The decrypt is the first leg of the pipeline and `pg_restore -l` is the second;
# both statuses are fatal and mean different things. `set +e` is needed only
# because `pipefail` would otherwise abort before `PIPESTATUS` can be read.
set +e
bundle_stream | "${COMPOSE[@]}" exec -T postgres pg_restore -l >/dev/null 2>&1
read_rc=("${PIPESTATUS[@]}")
set -e
if [ "${read_rc[0]}" -ne 0 ]; then
  if [ "$encrypted" -eq 1 ]; then
    fail "cannot decrypt $DUMP with ADJUTANT_BACKUP_AGE_IDENTITY — refusing to drop the database for it"
  fi
  fail "cannot read $DUMP — refusing to drop the database for it"
fi
[ "${read_rc[1]}" -eq 0 ] \
  || fail "$DUMP is not a readable pg_restore archive — refusing to drop the database for it"
[ "$encrypted" -eq 0 ] || ok "decrypted the bundle with ADJUTANT_BACKUP_AGE_IDENTITY"
ok "the bundle is readable; safe to proceed"

echo "== 1. stop the application so nothing writes mid-restore =="
"${COMPOSE[@]}" stop adjutant </dev/null >/dev/null 2>&1 || true
ok "adjutant stopped"

echo "== 2. recreate the database =="
psql_admin postgres -c 'DROP DATABASE IF EXISTS adjutant' -c 'CREATE DATABASE adjutant OWNER adjutant' >/dev/null
ok "dropped and recreated"

echo "== 3. restore the dump (no owner, no ACL: neither exists yet) =="
# `--no-owner --no-acl` is the ordering fix, not a convenience. A role that does
# not exist yet has no ACL to restore, and ownership is re-established by
# bootstrap-isolation below. The errors that used to be "expected" here were the
# ACLs failing against roles that did not exist — after this flag they are real
# data loss, which is why the exit code and the literal `errors ignored` are both
# checked.
restore_log="$(mktemp "${TMPDIR:-/tmp}/adjutant-restore-log.XXXXXX")"
start=$(date +%s)
set +e
bundle_stream \
  | "${COMPOSE[@]}" exec -T postgres pg_restore -U adjutant -d adjutant --no-owner --no-acl 2>&1 \
  | tee "$restore_log" | tail -20
rc=("${PIPESTATUS[@]}")
set -e
elapsed_restore=$(( $(date +%s) - start ))

# First leg: the decrypt (or `cat` for an older plaintext bundle). Second leg:
# pg_restore itself. They are separate failures — a bad identity must not read as
# a bad archive, and a SQL error must not read as a decryption problem.
if [ "${rc[0]}" -ne 0 ]; then
  if [ "$encrypted" -eq 1 ]; then
    fail "cannot decrypt $DUMP with ADJUTANT_BACKUP_AGE_IDENTITY — the restore is partial, not successful."
  fi
  fail "cannot read $DUMP — the restore is partial, not successful."
fi
if [ "${rc[1]}" -ne 0 ]; then
  fail "pg_restore exited ${rc[1]} — the restore is partial, not successful. Re-run after reading the output above."
fi
if grep -q 'errors ignored' "$restore_log"; then
  fail "pg_restore reported 'errors ignored' — after --no-acl those errors are data loss, not pre-role noise. Read the output above."
fi
rm -f "$restore_log"
ok "pg_restore finished in ${elapsed_restore}s with no ignored errors"

echo "== 4. create the roles, ownership and grants (THE STEP A DUMP CANNOT DO) =="
start=$(date +%s)
boot_log="$(mktemp "${TMPDIR:-/tmp}/adjutant-bootstrap-log.XXXXXX")"
set +e
"${COMPOSE[@]}" run --rm -e ADJUTANT_DATABASE_URL="$ADMIN_URL" adjutant \
  bootstrap-isolation --app-role adjutant_app --app-password "$ADJUTANT_APP_PASSWORD" \
  </dev/null >"$boot_log" 2>&1
boot_rc=$?
set -e
tail -3 "$boot_log"
if [ "$boot_rc" -ne 0 ]; then
  tail -30 "$boot_log" >&2
  rm -f "$boot_log"
  fail "bootstrap-isolation failed (exit $boot_rc) — the roles, ownership or grants were not established"
fi
rm -f "$boot_log"
elapsed_bootstrap=$(( $(date +%s) - start ))
ok "bootstrap-isolation finished in ${elapsed_bootstrap}s"

echo "== 5. verify the isolation boundary before calling this a restore =="
# The runtime's own check: every installed plugin's role, schema/table ownership,
# allowlist grants, no privilege on an unlisted core table, and a live connection
# as the plugin role using the stored credential. It names the missing role or
# table, which is the difference between a diagnosable failure and another
# `password authentication failed`.
set +e
verify_log="$("${COMPOSE[@]}" run --rm -e ADJUTANT_DATABASE_URL="$ADMIN_URL" adjutant \
  verify-isolation --app-role adjutant_app </dev/null 2>&1)"
verify_rc=$?
set -e
if [ "$verify_rc" -ne 0 ]; then
  printf '%s\n' "$verify_log" >&2
  fail "the restore did not produce a working isolation boundary (exit $verify_rc). The missing role or table is named above."
fi
printf '%s\n' "$verify_log" | tail -5
ok "roles, ownership, grants and isolation probes pass"

echo "== 6. start the application =="
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

echo "== 7. readiness checks (the server answers and the grants exist) =="
grants="$(psql_admin adjutant -tAc "select count(*) from information_schema.role_table_grants where grantee like 'adjutant_plugin%'")"
[ "${grants:-0}" -gt 0 ] || fail "no plugin-role grants exist — the runtime will refuse to work"
ok "$grants plugin-role grant(s) present"

roles="$(psql_admin adjutant -tAc "select count(*) from pg_roles where rolname like 'adjutant%'")"
ok "$roles adjutant role(s) in the cluster"

echo
echo "restore complete. Mechanical time: $((elapsed_restore + elapsed_bootstrap))s"
echo "Verify the audit chain, which is hash-chained and the one thing a partial"
echo "restore can quietly corrupt: GET /api/audit/verify (admin only)."
