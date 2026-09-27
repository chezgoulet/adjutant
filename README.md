# Adjutant — Development

> ## ⚠️ Alpha software — not ready for a troop's real records
>
> Adjutant is **alpha**. It is being built in the open, and it is not yet
> trustworthy with anything a troop would be upset to lose. Keep your own copy of
> anything that matters, and expect changes that break things with no migration
> path until we reach beta.
>
> **Every build we publish is unsigned, and will stay unsigned until beta.** We
> are not buying code-signing certificates for something this early, and we would
> rather say so plainly than let your operating system spring it on you.
> [What to do about that ↓](#unsigned-builds)

Sovereignty-first administration for democratic scout troops.
See [`SPEC.md`](SPEC.md) for the full specification.

## Stack

- **Server:** Rust + Axum, single binary, PostgreSQL
- **Plugins:** Rust `cdylib`s built against `adjutant-sdk`, loaded at boot
- **Client:** Flutter — the web, Android and all three desktop targets build
  today. **iOS is a target for support soon; it is not supported yet.**

## Repository layout

```
server/                     Core server (library + binary `adjutant`)
  src/                      config, db, events, host, identity, middleware,
                            permissions, plugin_runtime, server, cli
  tests/host_db.rs          DB-gated host-I/O + confinement probes (#[ignore]d)
plugins/sdk/                adjutant-sdk — the plugin contract
plugins/auth/               auth plugin (argon2, sessions, roles, OIDC)
plugins/membership/         membership plugin (roster, lodges, patrols, OSGi CSV)
plugins/missions/           missions plugin (6-stage lifecycle, mentors, impact report)
plugins/governance/         governance plugin (motions, votes, quorum, minutes, Accords)
plugins/examples/hello/     Prototype plugin proving the SDK end to end
scripts/probes.py           Committed M1+M2 probe harness (live server)
docs/e2e_m3.py              Committed M3 end-to-end harness (live server)
scripts/client-live-harness.sh  Boots a server and runs the live client harness
client/live/live_client_test.dart  The client against a real server (not mocked)
tools/mcp-bridge/           Stdio MCP bridge + probe: what an MCP host talks to
scripts/mcp-live-harness.sh  Boots a server and proves the MCP path (two identities)
docs/mcp-hermes.md          Wiring an MCP host (Hermes) to Adjutant's MCP plugin
docs/architecture.md        How the core, plugins, and sandbox fit together
docs/api-reference.md       Core + plugin HTTP API and the SDK surface
docs/plugin-development.md  Plugin author guide (start here to write one)
docs/plugin-roadmap.md      Plugin inventory, sequencing, and the v1.0 gate
docs/release-path.md        Ordered work from testing to v1.0 (stages, lanes, decisions)
docs/design/                Designs awaiting or carrying a decision (isolation, scopes)
docs/sdk-compatibility.md   SDK version + compatibility policy
docs/deployment.md          Deployment, upgrades, backup/restore
docs/releasing.md           Release + crates.io publishing runbook
docs/milestones/            Milestone evidence records
docs/evidence/              Committed probe transcripts
CONTRIBUTING.md             Dev setup, gates, and git flow
```

## Prerequisites

- Rust 1.96+ (the declared MSRV; developed on stable, see `rust-toolchain.toml`)
- PostgreSQL 14+ (18.6 in development) with `pgcrypto` available
- `psql` on `PATH` for the probe harnesses

## Getting started

```bash
# 1. database + role (the harnesses and tests assume this DSN)
psql -h 127.0.0.1 -p 5433 -U postgres \
  -c "CREATE ROLE adjutant LOGIN CREATEROLE" \
  -c "CREATE DATABASE adjutant_dev OWNER adjutant"
# port 5433 is this host's PostgreSQL; use 5432 or your own socket if different
# CREATEROLE (or superuser) is needed by `bootstrap-isolation`; the running
# server does not need it.

# 2. build (plugins land as .so files in target/debug)
cargo build --workspace

# 3. stage the plugins the server scans — every plugin on disk, not a hand list
#    (the old copy-three-.so-files step is how the release tarball came to
#    ship 3 of 14 plugins; this script asks the manifests instead)
python3 scripts/stage-plugins.py

# 4. create the per-plugin DB roles/credentials (once, and after adding a plugin)
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev \
  cargo run -p adjutant-server -- bootstrap-isolation --plugin-dir plugins-built

# 5. run (--allow-dev-headers is for local auth-less testing; dev only)
ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev \
  cargo run -p adjutant-server -- --allow-dev-headers
```

The server creates the `core` schema, runs core migrations, then each plugin's
migrations **on a pool authenticated as that plugin's own `adjutant_plugin_<id>`
role**, and seeds the bootstrap roles. `pgcrypto` is created by core migration 2,
so the connecting role must be allowed to create extensions. A plugin with no
stored credential refuses to load — run `bootstrap-isolation` first.

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
| `ADJUTANT_ALLOW_SUPERUSER` | **`false` by default.** The server refuses to boot on a PostgreSQL superuser connection (a plugin escape would be total compromise). Set `true` (or pass `--allow-superuser`) only for a throwaway database |
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

Each plugin gets its own PostgreSQL schema, **owned by its own `LOGIN` role**
`adjutant_plugin_<id>`, and `ctx.db` runs on a pool authenticated as that role —
so the boundary is the identity of the connection, not a statement filter. The
plugin has full rights in its own schema and only an explicit allowlist of
`core.*` tables; reaching into another plugin's schema fails with `permission
denied`, and `SET ROLE`/`RESET ROLE` cannot lift it out (SPEC §5.2, design
[`docs/design/plugin-isolation.md`](docs/design/plugin-isolation.md)). Migrations
run on that same role/pool, so they can only touch the plugin's own schema.

Roles, schema ownership and credentials are created once with
`adjutant bootstrap-isolation` (needs `CREATEROLE`); the runtime reads the
stored credential and needs no `CREATEROLE`. A plugin with no credential
refuses to load. The allowlist lives in `server/src/schema.rs` (`core_grants`);
a plugin needing another core table must add it there deliberately.

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

The compose file builds the server image for you, and the image carries every
plugin the workspace stages, so there is no Rust toolchain and no plugin staging
to do by hand. **There is no published image to pull yet** — the container image
is planned alongside the other per-platform artifacts in
[#111](https://github.com/chezgoulet/adjutant/issues/111) — so today the image is
built from this repository's `Dockerfile`.

See [`docs/deployment.md`](docs/deployment.md) for TLS, upgrades, and
backup/restore. **This is alpha software — see the note at the top of this file
before you point a troop at it.**

A tagged release (`v*` on `main`) publishes a Linux tarball and the client's web
bundle, and nothing per platform — see
[Releases, artifacts and unsigned builds](#releases-artifacts-and-unsigned-builds).

## Releases, artifacts and unsigned builds

Releases are cut by tagging `main` (`v*`). **What a tag produces today is two
files:**

- `adjutant-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` — the `adjutant` binary, every
  plugin library the workspace stages (**all fourteen**, plus the `hello_wasm`
  guest), `README.md` and `LICENSE`, with a `.sha256` beside it;
- `adjutant-client-vX.Y.Z-web.tar.gz` — the Flutter client built for the web, to
  serve as static files.

**Nothing else is packaged — not on a pull request, and not on a tag.** There is
no macOS build, no Windows build, no Android APK or `.aab`, no `.deb`, `.rpm`,
`.AppImage`, `.snap` or `.flatpak`, no `.dmg`, no `.msi`, and no published
container image. Building those per platform is
[#111](https://github.com/chezgoulet/adjutant/issues/111); until it lands, the
table below is the **plan** rather than a description of the tags that exist:

| Platform | Server (binary + all plugins) | Client |
|---|---|---|
| Linux | `.tar.gz` — **shipped today**; `.deb`, `.rpm` planned | web bundle — **shipped today**; `.deb`, `.rpm`, `.AppImage`, `.snap`, `.flatpak` planned |
| macOS | `.zip` planned | `.dmg`, `.zip` planned |
| Windows | `.zip` planned | `.msi`, `.zip` planned |
| Android | — | `.apk`, `.aab` planned |
| Web | — | a bundle to serve as static files — **shipped today** |
| Docker | a published image is planned | — |

Every artifact, when it exists, will carry all fourteen plugins rather than a
hand-written subset: the release derives the plugin set with
`scripts/stage-plugins.py` instead of naming files, which is the mistake the
tarball used to make.

Because a pull request packages nothing, a packaging fault surfaces at the tag
rather than in review — so the release job's own steps are exercised by hand
before a release is cut (`docs/release-path.md`, Stage 3.10).

### Unsigned builds

**Nothing we publish is signed, and nothing will be signed until beta.** A
certificate costs money and asserts a level of care this software has not earned
yet. Today there are two artifacts to receive unsigned (the Linux tarball and the
client's web bundle, neither of which any platform refuses); as the per-platform
builds land ([#111](https://github.com/chezgoulet/adjutant/issues/111)), each
platform will complain in its own way, and none of it will be a fault:

- **macOS** — Gatekeeper quarantines the download. Right-click → **Open**, or
  clear the flag yourself: `xattr -d com.apple.quarantine <file>`. The builds are
  neither signed nor notarised, so this is the expected path, not a defect to
  report.
- **Windows** — SmartScreen shows "Windows protected your PC". **More info** →
  **Run anyway**. Expect the same from the `.msi`.
- **Linux** — a `.tar.gz`, `.deb` or `.rpm` usually installs without a signature
  check, though some distributions warn. **`.snap` and `.flatpak` refuse it
  outright**: both formats expect a signature for an ordinary install, so you need
  the escape hatch (`snap install --dangerous`; `--no-gpg-verify` for flatpak).
  That inconvenience disappears when signing arrives at beta.
- **Android** — if you build the client yourself (`flutter build apk`) the
  platform will not install an unsigned APK at all, so Flutter signs it with a
  throwaway debug key. It installs from a file manager or Obtainium, but that key
  is not one we keep — so a properly signed build later may not upgrade over it
  cleanly. The `.aab` is for the Play Store, not for sideloading. Nothing Android
  is published yet: the APK is the artifact
  [#111](https://github.com/chezgoulet/adjutant/issues/111) exists to build.

If any of that surprises you, it is worth repeating: **this is alpha software, and
a warning from your operating system is the correct response to it.**

## Writing a plugin

See [`docs/plugin-development.md`](docs/plugin-development.md). The short loop
(**the crates are not published yet** — until they are, the `adjutant` CLI is the
one you built: `cargo build --workspace`, then `./target/debug/adjutant`):

```bash
adjutant new-plugin gear_locker
cargo build -p adjutant-gear_locker
adjutant validate-plugin target/debug/libadjutant_gear_locker.so   # no DB needed
cargo test -p adjutant-gear_locker                                  # adjutant_sdk::testing
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
```

## Tests

```bash
cargo test --workspace        # unit tests everywhere; the DB-gated tests are
                              # #[ignore]d, so they are reported as ignored,
                              # never as passed
cargo clippy --workspace --all-targets
# The DB-gated tests, explicitly. All live in server/tests/host_db.rs:
#   decode_covers_every_supported_type, bind_params_round_trips_every_variant
#   and the confinement probes (design docs/design/plugin-isolation.md §5):
#   probe_migration_cannot_create_a_core_table,
#   probe_do_block_cannot_set_role_and_write_core,
#   probe_plugin_sql_cannot_create_a_superuser_role,
#   probe_reset_role_is_inert_and_set_role_is_refused,
#   probe_cross_schema_read_is_denied, probe_unlisted_core_table_is_denied,
#   probe_every_plugin_connection_is_the_plugin_role
ADJUTANT_TEST_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev_test \
  cargo test -p adjutant-server --test host_db -- --ignored --nocapture
```

The confinement probes need a database role that can `CREATEROLE` (a throwaway
superuser container). `ADJUTANT_TEST_DATABASE_URL` must name a database ending
in `_test`; under `--ignored` a missing or unreachable database is a hard
failure, so CI cannot pass while the isolation proof silently does not run.

Three live harnesses prove the integration claims. Each drives a real server and
a real database, and each is committed because the earlier, uncommitted probe
transcripts could not be reproduced by anyone else. CI runs all of them
(`.github/workflows/ci.yml`) on every push and pull request.

```bash
# M1 + M2: drops/recreates and stages the hello plugin into the *_test database
# named by ADJUTANT_DATABASE_URL (it refuses a name that does not end in _test),
# boots its own server, runs m1_regression -> middleware -> lifecycle -> tamper.
# Writes docs/evidence/m2_probes.json. Needs port 8787 free.
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev_test \
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

The client has a live harness of its own, and it is the reason the client's
`flutter test` suite is not the whole v1.0 gate: those 104 tests drive a mock
`http.Client`, so a client that has drifted from the API still passes every one
of them. This one drives the product's `ApiClient` against a real server and
asserts on what the server returned. It creates its own database, bootstraps the
plugin roles into it, boots a server with the dev-header stub **off**, and fails
loudly — never skips — if the server is not there.

```bash
# Needs `cargo build --workspace` first, plus psql, python3 and flutter on PATH.
# Derives its database (`adjutant_client_live`) from ADJUTANT_TEST_DATABASE_URL;
# override with ADJUTANT_CLIENT_LIVE_DATABASE_URL. Port 8790, not 8787: it boots
# its own server rather than borrowing one, so a stale server cannot be mistaken
# for evidence. Set ADJUTANT_CLIENT_LIVE_LOG to keep the server's log.
scripts/client-live-harness.sh

# Or against a server you booted yourself:
cd client && ADJUTANT_LIVE_BASE=http://127.0.0.1:8790 \
  flutter test live/live_client_test.dart
```

## Development milestones

Tracked in [`SPEC.md` §15](SPEC.md); evidence per milestone in `docs/milestones/`,
and the order of the work remaining in
[`docs/release-path.md`](docs/release-path.md).

Milestones **1–4 are complete**, including the inserted core/SDK stabilization
gate the roadmap calls M3-S — see `docs/milestones/M3-sdk-and-plugins.md` for the
audit pass that corrected this repo's earlier evidence claims. **M6 is built**
(its five plugins load, are tested and are probed) with two boxes open by
decision: SDK v0.3, and announcements' delivery. **M5** is partially met and
**M7/M8** are open; both are ordered in the release path.

## Git flow

House standard ([chezgoulet-git-flow]): `testing` = integration target,
`main` = releases, `feature/*` branches from `testing`, PRs target `testing`.

[chezgoulet-git-flow]: https://github.com/chezgoulet/library

---

**Adjutant is alpha software.** Unsigned, unstable, and not ready for a troop's
real records — every build will say so again when your operating system stops you
from opening it. If you are considering putting real scouts into it, wait for
beta, or keep a backup you have actually restored.
