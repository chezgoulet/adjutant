# Adjutant — Development

Sovereignty-first administration for democratic scout troops.
See [`SPEC.md`](SPEC.md) for the full specification.

## Stack

- **Server:** Rust + Axum, single binary, PostgreSQL
- **Plugins:** Rust `cdylib`s built against `adjutant-sdk`, loaded at boot
- **Client:** Flutter (all platforms) — not yet started (Milestone 5)

## Repository layout

```
server/                     Core server (library + binary `adjutant`)
  src/                      config, db, events, host, identity, middleware,
                            permissions, plugin_runtime, server, cli
  tests/host_db.rs          DB-backed host-I/O tests (skip without a database)
plugins/sdk/                adjutant-sdk — the plugin contract
plugins/auth/               auth plugin (argon2, sessions, roles, OIDC)
plugins/membership/         membership plugin (roster, lodges, patrols, OSGi CSV)
plugins/examples/hello/     Prototype plugin proving the SDK end to end
scripts/probes.py           Committed M1+M2 probe harness (live server)
docs/e2e_m3.py              Committed M3 end-to-end harness (live server)
docs/plugin-development.md  Plugin author guide (start here to write one)
docs/sdk-compatibility.md   SDK version + compatibility policy
docs/milestones/            Milestone evidence records
docs/evidence/              Committed probe transcripts
```

## Prerequisites

- Rust 1.88+ (the declared MSRV; developed on stable, see `rust-toolchain.toml`)
- PostgreSQL 14+ (18.6 in development) with `pgcrypto` available
- `psql` on `PATH` for the probe harnesses

## Getting started

```bash
# 1. database + role (the harnesses and tests assume this DSN)
psql -h 127.0.0.1 -p 5433 -U postgres \
  -c "CREATE ROLE adjutant LOGIN CREATEROLE" \
  -c "CREATE DATABASE adjutant_dev OWNER adjutant"
# port 5433 is this host's PostgreSQL; use 5432 or your own socket if different
# CREATEROLE (or superuser) lets the core create one role per plugin for schema
# isolation; without it, plugins run as the base role with a boot warning.

# 2. build (plugins land as .so files in target/debug)
cargo build --workspace

# 3. stage the plugins the server scans
mkdir -p plugins-built
cp target/debug/libadjutant_hello.so \
   target/debug/libadjutant_auth.so \
   target/debug/libadjutant_membership.so plugins-built/

# 4. run (--allow-dev-headers is for local auth-less testing; dev only)
ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev \
  cargo run -p adjutant-server -- --allow-dev-headers
```

The server creates the `core` schema, runs core migrations, then each plugin's
migrations in its own schema (`hello`, `auth`, `membership`), and seeds the
bootstrap roles. `pgcrypto` is created by core migration 2, so the connecting role
must be allowed to create extensions.

## Configuration

Precedence is defaults < TOML file (./adjutant.toml or `--config`) < environment
< CLI flags.

| Variable | Meaning |
|---|---|
| `ADJUTANT_BIND` | listen address (default `127.0.0.1:8787`) |
| `ADJUTANT_DATABASE_URL` | PostgreSQL DSN |
| `ADJUTANT_PLUGIN_DIR` | directory scanned for plugin `*.so` at boot/reload |
| `ADJUTANT_LOG` | tracing filter (default `info,adjutant_server=debug`) |
| `ADJUTANT_LOG_FORMAT` | `pretty` \| `json` |
| `ADJUTANT_RATE_MAX`, `ADJUTANT_RATE_WINDOW` | fixed window per client IP; `0` disables |
| `ADJUTANT_TRUSTED_PROXIES` | comma-separated peer IPs allowed to set `x-forwarded-for`. **Empty by default — the header is otherwise ignored**, because any client can send it |
| `ADJUTANT_CORS` | comma-separated origins, `*` for any |
| `ADJUTANT_MAX_BODY` | request body cap in bytes |
| `ADJUTANT_DEV_HEADERS` | **`false` by default.** Set `true` (or pass `--allow-dev-headers`) to enable the spoofable `x-dev-user`/`x-dev-role` identity stub for development. Only consulted when no plugin identity provider answered |
| `ADJUTANT_TEST_DATABASE_URL` | overrides the derived `_test` database for `adjutant test-plugin` and the DB-backed tests |

### Two operational hazards worth reading before deploying

- **Dev headers are off by default — and spoofable when on.** With
  `ADJUTANT_DEV_HEADERS=true` (or `--allow-dev-headers`), anyone who can reach
  the port can claim any role by setting two headers. Keep it off (the default)
  and rely on the auth plugin's sessions. The milestone harnesses assert this
  for you.
- **Disabling the last identity provider is refused.** With dev headers off the
  auth plugin is the only source of identity, so disabling or uninstalling it
  would make every authenticated route — including the admin route that would
  undo it — unreachable until a restart. The core now answers `409` with a hint
  instead of allowing the lockout; recovery from a manual `core.plugins` edit is
  still a restart, so register a second provider before removing the first.

### Schema isolation

Each plugin gets its own PostgreSQL schema and (when the database role can
manage roles) its own `NOLOGIN` role `adjutant_plugin_<id>`. At runtime the
plugin's database handle runs every query under `SET LOCAL ROLE`, with full
rights on its own schema and an explicit allowlist of `core.*` tables — so a
query that reaches into another plugin's schema fails with `permission denied`
(SPEC §5.2). Migrations still run as the base role (some, like auth's,
deliberately alter `core.users`). Requires `CREATEROLE` or superuser; otherwise
the core logs a warning and runs plugins unisolated. The allowlist lives in
`server/src/schema.rs` (`core_grants`); a plugin needing another core table must
add it there deliberately.

## Smoke test

`GET /api/plugins` and `/api/events/recent` are admin-gated, so they need an
identity. Start the server with dev headers enabled for the smoke test
(`--allow-dev-headers`, or `ADJUTANT_DEV_HEADERS=true`), then:

```bash
curl -s localhost:8787/                                # health
curl -s -H 'x-dev-user: christopher' -H 'x-dev-role: chief' \
     localhost:8787/api/plugins                        # registry (admin)
curl -s localhost:8787/api/hello                       # open route
curl -s localhost:8787/api/hello/greetings             # 401 — needs hello:read
curl -s -H 'x-dev-user: christopher' -H 'x-dev-role: chief' \
     localhost:8787/api/hello/greetings                # 200 — chief has all
curl -s -H 'x-dev-user: christopher' -H 'x-dev-role: chief' \
     -H 'content-type: application/json' \
     -d '{"message":"first greeting"}' \
     localhost:8787/api/hello/greet                    # 201, publishes hello.greeted
curl -s -H 'x-dev-user: christopher' -H 'x-dev-role: chief' \
     localhost:8787/api/events/recent                  # persisted event visible
```

## Deployment

```bash
export POSTGRES_PASSWORD="$(openssl rand -hex 24)"
docker compose up -d          # server + PostgreSQL
```

See [`docs/deployment.md`](docs/deployment.md) for TLS, upgrades, and
backup/restore. Tagged releases (`v*`) publish a tarball with the binary and
bundled plugins.

## Writing a plugin

See [`docs/plugin-development.md`](docs/plugin-development.md). The short loop:

```bash
adjutant new-plugin gear_locker
cargo build -p adjutant-gear_locker
adjutant validate-plugin target/debug/libadjutant_gear_locker.so   # no DB needed
cargo test -p adjutant-gear_locker                                  # adjutant_sdk::testing
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
```

## Tests

```bash
cargo test --workspace        # unit tests everywhere + 2 DB-backed host tests
                              # (those two print SKIPPED unless
                              #  ADJUTANT_TEST_DATABASE_URL is set)
cargo clippy --workspace --all-targets
ADJUTANT_TEST_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev_test \
  cargo test -p adjutant-server --test host_db
```

Two live harnesses prove the integration claims. Both drive a real server and a
real database, and both are committed because the earlier, uncommitted probe
transcripts could not be reproduced by anyone else. CI runs all of them
(`.github/workflows/ci.yml`) on every push and pull request.

```bash
# M1 + M2: resets adjutant_dev, stages the hello plugin, boots its own server,
# runs m1_regression -> middleware -> lifecycle -> tamper. Writes
# docs/evidence/m2_probes.json. Needs port 8787 free.
python3 scripts/probes.py

# adjutant test-plugin: pristine _test database, probes every registered route
# with mock permissions. Requires CREATE DATABASE rights.
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin

# M3 end-to-end: expects a server already running with ALL plugins staged and
# the identity stub OFF, plus psql access and port 9099 free for the mock IdP.
# ~55 requests per run, so start the server with ADJUTANT_RATE_MAX=0 for
# back-to-back runs.
ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev \
ADJUTANT_DEV_HEADERS=false ADJUTANT_RATE_MAX=0 \
  ./target/debug/adjutant &
python3 docs/e2e_m3.py
```

## Development milestones

Tracked in [`SPEC.md` §15](SPEC.md); evidence per milestone in `docs/milestones/`.
Milestones 1–3 are complete (prototype validation, core server, SDK v0.1 + auth
and membership plugins) — see `docs/milestones/M3-sdk-and-plugins.md` for the
audit pass that corrected this repo's earlier evidence claims.

## Git flow

House standard ([chezgoulet-git-flow]): `testing` = integration target,
`main` = releases, `feature/*` branches from `testing`, PRs target `testing`.

[chezgoulet-git-flow]: https://github.com/chezgoulet/library
