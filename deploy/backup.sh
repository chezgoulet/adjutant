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
# Retention: keeps ADJUTANT_BACKUP_KEEP_DAYS (default 14) of dumps, and prunes
# older ones. A backup directory that only grows is a disk-full incident with a
# delay on it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
COMPOSE=(docker compose -f "$HERE/compose.proxy.yml")

DEST="${1:-${ADJUTANT_BACKUP_DIR:-/var/backups/adjutant}}"
KEEP_DAYS="${ADJUTANT_BACKUP_KEEP_DAYS:-14}"

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok — $*"; }

command -v docker >/dev/null 2>&1 || fail "docker is not installed"

mkdir -p "$DEST" || fail "cannot create $DEST"
STAMP="$(date +%F-%H%M%S)"
OUT="$DEST/adjutant-$STAMP.dump"

echo "== dumping =="
start=$(date +%s)
# `-Fc` is the custom format: compressed, and restorable selectively with
# pg_restore. `-T` because there is no TTY here, and the dump goes to a file.
"${COMPOSE[@]}" exec -T postgres pg_dump -U adjutant -Fc adjutant > "$OUT" </dev/null \
  || fail "pg_dump failed"
elapsed=$(( $(date +%s) - start ))

[ -s "$OUT" ] || fail "the dump is empty — that is a failed backup, not a small one"
ok "wrote $OUT ($(stat -c %s "$OUT") bytes) in ${elapsed}s"

# A dump you cannot list is a dump you cannot trust. This also proves the file is
# a real archive rather than an error message that happened to be redirected.
# NOTE: no trailing `</dev/null` on this line — a later redirect overrides an
# earlier one, so appending it silently feeds pg_restore an empty stdin and the
# check fails on a perfectly good dump. It did, once.
"${COMPOSE[@]}" exec -T postgres pg_restore -l < "$OUT" >/dev/null 2>&1 \
  || fail "the dump is not readable by pg_restore — do not rely on it"
ok "pg_restore can read it"

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
done < <(find "$DEST" -maxdepth 1 -name 'adjutant-*.dump' -mtime "+${KEEP_DAYS}" -print)
ok "pruned $pruned dump(s) older than ${KEEP_DAYS} days"
echo
echo "backup complete: $OUT"
