#!/usr/bin/env bash
# Back up an Adjutant deployment's database.
#
#   ./deploy/backup.sh [backup-dir]
#
# Everything Adjutant knows lives in PostgreSQL — including the plugin schemas,
# the roles' grants and the audit chain — so this one dump is the whole backup.
# It is *not* the whole restore: roles are cluster-level objects and a database
# dump never carries them. See `restore.sh` and the note in
# `docs/deployment.md` § Backup and restore.
#
# **The dump is credential material.** `core.plugins.db_secret` holds every
# plugin role's password, and `pg_dump -Fc` compresses but does not encrypt. The
# bundle is therefore encrypted to an age *recipient* before it is written to its
# final name; the private key must not live on this server. With no recipient
# configured the backup FAILS (fail closed), unless
# `ADJUTANT_BACKUP_ALLOW_PLAINTEXT=true` explicitly accepts a `.dump`.
#
# Retention: keeps ADJUTANT_BACKUP_KEEP_DAYS (default 14) of bundles, and prunes
# older ones. A backup directory that only grows is a disk-full incident with a
# delay on it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE=(docker compose -f "$HERE/compose.proxy.yml")

DEST="${1:-${ADJUTANT_BACKUP_DIR:-/var/backups/adjutant}}"
KEEP_DAYS="${ADJUTANT_BACKUP_KEEP_DAYS:-14}"
RECIPIENT="${ADJUTANT_BACKUP_AGE_RECIPIENT:-}"
IDENTITY="${ADJUTANT_BACKUP_AGE_IDENTITY:-}"
ALLOW_PLAINTEXT="${ADJUTANT_BACKUP_ALLOW_PLAINTEXT:-}"

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok — $*"; }
warn() { echo "  WARN — $*" >&2; }

command -v docker >/dev/null 2>&1 || fail "docker is not installed"

plaintext_allowed=0
case "${ALLOW_PLAINTEXT,,}" in
  true|1) plaintext_allowed=1 ;;
esac

if [ -z "$RECIPIENT" ] && [ "$plaintext_allowed" -ne 1 ]; then
  fail "refusing to write an unencrypted backup: a pg_dump contains every plugin \
role's password in clear text. Set ADJUTANT_BACKUP_AGE_RECIPIENT to the age public \
key (the private key must not live on this server), or set \
ADJUTANT_BACKUP_ALLOW_PLAINTEXT=true to accept a plaintext bundle deliberately."
fi

mkdir -p "$DEST" || fail "cannot create $DEST"
STAMP="$(date +%F-%H%M%S)"
if [ -n "$RECIPIENT" ]; then
  OUT="$DEST/adjutant-$STAMP.dump.age"
else
  OUT="$DEST/adjutant-$STAMP.dump"
  warn "writing a PLAINTEXT bundle ($OUT): it is credential material — every plugin role's password."
fi

# Run `age` from the host when it is installed there, otherwise from the runtime
# image that ships it. The private key never reaches this script unless the
# operator explicitly supplies ADJUTANT_BACKUP_AGE_IDENTITY for a read-back check.
age_encrypt() {
  if command -v age >/dev/null 2>&1; then
    age --recipient "$RECIPIENT"
  else
    "${COMPOSE[@]}" run --rm -T --no-deps --entrypoint age adjutant \
      --recipient "$RECIPIENT"
  fi
}

age_decrypt() {
  local src="$1"
  if command -v age >/dev/null 2>&1; then
    age --decrypt --identity "$IDENTITY" < "$src"
  else
    local identity_abs
    identity_abs="$(readlink -f "$IDENTITY" 2>/dev/null || printf '%s' "$IDENTITY")"
    "${COMPOSE[@]}" run --rm -T --no-deps --entrypoint age \
      -v "$identity_abs:/run/adjutant-age-identity:ro" adjutant \
      --decrypt --identity /run/adjutant-age-identity < "$src"
  fi
}

echo "== dumping =="
start=$(date +%s)
set +e
if [ -n "$RECIPIENT" ]; then
  "${COMPOSE[@]}" exec -T postgres pg_dump -U adjutant -Fc adjutant </dev/null \
    | age_encrypt > "$OUT"
  rc=("${PIPESTATUS[@]}")
else
  "${COMPOSE[@]}" exec -T postgres pg_dump -U adjutant -Fc adjutant </dev/null > "$OUT"
  rc=("$?")
fi
set -e
[ "${rc[0]}" -eq 0 ] || { rm -f "$OUT"; fail "pg_dump failed"; }
if [ -n "$RECIPIENT" ] && [ "${rc[1]:-1}" -ne 0 ]; then
  rm -f "$OUT"
  fail "age encryption failed"
fi
elapsed=$(( $(date +%s) - start ))

[ -s "$OUT" ] || { rm -f "$OUT"; fail "the bundle is empty — that is a failed backup, not a small one"; }
ok "wrote $OUT ($(stat -c %s "$OUT") bytes) in ${elapsed}s"

# A bundle you cannot inspect is a bundle you cannot trust. An encrypted bundle
# cannot be listed without the identity; check the format instead, and do the
# full `pg_restore -l` read-back when the operator has supplied the identity.
if [ -n "$RECIPIENT" ]; then
  head -n 1 "$OUT" | grep -qx 'age-encryption.org/v1' \
    || fail "$OUT is not an age file — do not rely on it"
  ok "the bundle is age-encrypted (header verified)"
  if [ -n "$IDENTITY" ]; then
    [ -f "$IDENTITY" ] || fail "ADJUTANT_BACKUP_AGE_IDENTITY=$IDENTITY does not exist"
    if age_decrypt "$OUT" | "${COMPOSE[@]}" exec -T postgres pg_restore -l >/dev/null 2>&1; then
      ok "decrypted with ADJUTANT_BACKUP_AGE_IDENTITY and pg_restore can read it"
    else
      fail "the identity could not decrypt the bundle, or pg_restore cannot read it"
    fi
  else
    echo "  note — not read back: set ADJUTANT_BACKUP_AGE_IDENTITY to verify the archive now"
  fi
else
  # NOTE: no trailing `</dev/null` on this line — a later redirect overrides an
  # earlier one, so appending it silently feeds pg_restore an empty stdin and the
  # check fails on a perfectly good dump. It did, once.
  "${COMPOSE[@]}" exec -T postgres pg_restore -l < "$OUT" >/dev/null 2>&1 \
    || fail "the dump is not readable by pg_restore — do not rely on it"
  ok "pg_restore can read it"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$OUT" > "$OUT.sha256"
  ok "checksum written to $OUT.sha256"
fi

echo
echo "== retention: keeping ${KEEP_DAYS} days =="
pruned=0
while IFS= read -r old; do
  rm -f "$old" "$old.sha256"
  pruned=$((pruned + 1))
done < <(find "$DEST" -maxdepth 1 \( -name 'adjutant-*.dump' -o -name 'adjutant-*.dump.age' \) -mtime "+${KEEP_DAYS}" -print)
ok "pruned $pruned bundle(s) older than ${KEEP_DAYS} days"
echo
echo "backup complete: $OUT"
