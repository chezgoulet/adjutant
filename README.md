# Adjutant — Development

Sovereignty-first administration for democratic scout troops.
See [`SPEC.md`](SPEC.md) for the full specification.

## Stack

- **Server:** Rust + Axum, single binary, PostgreSQL
- **Plugins:** Rust `cdylib`s built against `adjutant-sdk`, loaded at boot
- **Client:** Flutter (all platforms) — not yet started (Milestone 5)

## Repository layout

```
server/                 Core server (binary: `adjutant`)
plugins/sdk/            adjutant-sdk — the plugin contract
plugins/examples/hello/ Prototype plugin proving the SDK end to end
docs/                   Architecture, plugin development, API reference
```

## Prerequisites

- Rust 1.75+ (workspace edition 2021)
- PostgreSQL 14+ (dev default: `postgres://adjutant@127.0.0.1:5433/adjutant_dev`)

## Build & test

```bash
cargo build --workspace          # core + sdk + plugins
cargo test  --workspace
```

## Run the prototype

```bash
# 1. build everything (plugins land as .so in target/debug)
cargo build --workspace

# 2. stage plugins into the plugin dir the server scans
mkdir -p plugins-built
cp target/debug/libadjutant_hello.so plugins-built/

# 3. start (env overrides: ADJUTANT_BIND, ADJUTANT_DATABASE_URL, ADJUTANT_PLUGIN_DIR, ADJUTANT_LOG)
ADJUTANT_PLUGIN_DIR=plugins-built cargo run -p adjutant-server
```

Smoke test (dev identity stub — `x-dev-user`/`x-dev-role`, replaced by the
auth plugin in Milestone 2):

```bash
curl -s localhost:8787/                                # health
curl -s localhost:8787/api/plugins                     # loaded plugins
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

## Development milestones

Tracked in [`SPEC.md` §15](SPEC.md). Milestone 1 = prototype validation
(core server compiles, plugin loads, routes dispatch, permissions enforce,
migrations run, events flow — all verified by tests).

## Git flow

House standard ([chezgoulet-git-flow]): `testing` = integration target,
`main` = releases, `feature/*` branches from `testing`, PRs target `testing`.

[chezgoulet-git-flow]: https://github.com/chezgoulet/library
