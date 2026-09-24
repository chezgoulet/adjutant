# Deployment

How to run Adjutant, upgrade it safely, and back it up. See the
[README](../README.md) for the environment variable reference and the
[plugin guide](plugin-development.md) for plugin development.

Adjutant is a single Rust binary plus plugin `.so` files and a PostgreSQL
database. There is no Redis, no MinIO, and no separate cache (SPEC §2.6).

---

## Quick start (Docker Compose)

```bash
# 1. Pick a database password (required — there is no insecure default).
export POSTGRES_PASSWORD="$(openssl rand -hex 24)"

# 2. Build and start (server + PostgreSQL)
docker compose up -d

# 3. Check it is up
curl -s localhost:8787/          # {"service":"adjutant","status":"ok"}
```

On first boot the server creates the `core` schema, runs core migrations,
loads the bundled plugins (`hello`, `auth`, `membership`), runs each plugin's
migrations in its own schema, and seeds the bootstrap roles.

Postgres is **not** exposed to the host; only the server is. The first user to
`POST /api/auth/register` while `core.users` is empty becomes `chief`.

## Bare metal

```bash
cargo build --release --workspace          # produces target/release/adjutant + *.so
mkdir -p plugins-built
cp target/release/libadjutant_*.so plugins-built/

ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5432/adjutant \
  ./target/release/adjutant serve
```

The database role must be able to create extensions (`pgcrypto`) and — for
schema isolation (SPEC §5.2) — create roles: give it `CREATEROLE` or make it a
superuser. Without that, plugins run as the base role and the core logs a
warning.

```sql
CREATE ROLE adjutant LOGIN CREATEROLE;
CREATE DATABASE adjutant OWNER adjutant;
```

## Putting it behind TLS

Terminate TLS at a reverse proxy (Caddy, nginx, Traefik) and forward to the
server's port. Two settings matter:

- `ADJUTANT_TRUSTED_PROXIES` — comma-separated proxy IPs allowed to set
  `x-forwarded-for`. With this set, rate limiting keys on the real client;
  otherwise the header is ignored and every request behind the proxy shares one
  bucket.
- `ADJUTANT_CORS` — comma-separated allowed browser origins (`*` for any). Empty
  means same-origin only.

**Keep `ADJUTANT_DEV_HEADERS` off** (the default). When on, anyone who can reach
the port can claim any role by setting `x-dev-user`/`x-dev-role`.

## Upgrading

Adjutant applies migrations automatically at boot, so an upgrade is: stop, swap
the binary/plugins, start.

```bash
# 1. Back up first (see below).
# 2. Get the new release (binary + plugins), then:
docker compose down
docker compose build --pull
docker compose up -d
```

Notes:

- Core migrations and plugin migrations are recorded in
  `core.schema_migrations` and run once, in order. They are additive; there are
  no down-migrations, so **restore from backup is the rollback path**.
- Plugin `.so` files must be rebuilt against the same SDK ABI as the core. A
  stale plugin is refused at load with an explicit ABI-mismatch error (run
  `adjutant validate-plugin <so>` to check before deploying).
- If a release bumps the SDK ABI, rebuild and re-copy all plugins.

## Backup and restore

Everything lives in PostgreSQL. Back up with `pg_dump`:

```bash
# Backup (custom format, restorable selectively)
docker compose exec -T postgres \
  pg_dump -U adjutant -Fc adjutant > adjutant-$(date +%F).dump
```

Restore into a fresh database:

```bash
# Stop the server first so nothing writes mid-restore.
docker compose stop adjutant

# Recreate the database and restore.
docker compose exec -T postgres psql -U adjutant -d postgres \
  -c 'DROP DATABASE IF EXISTS adjutant' -c 'CREATE DATABASE adjutant OWNER adjutant'
docker compose exec -T postgres \
  pg_restore -U adjutant -d adjutant --no-owner < adjutant-2026-09-24.dump

docker compose start adjutant
```

The audit log is append-only and hash-chained; the chain is verified by
`GET /api/audit/verify` (admin only) after a restore.

> A physical volume snapshot (`pgdata`) also works but requires a consistent
> Postgres shutdown. `pg_dump` is the portable option.

## Releases

Tagged releases (`v*`) publish a tarball containing the `adjutant` binary, the
bundled plugin `.so` files, `README.md`, and `LICENSE` (see
`.github/workflows/release.yml`). Verify a plugin before loading it:

```bash
adjutant validate-plugin libadjutant_your_plugin.so
```
