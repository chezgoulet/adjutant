# Milestone 1 — Prototype Validation ✓

**Status:** PASSED · 2026-09-21 · All 8 exit criteria verified against a live server.

Environment: Rust 1.96.1, PostgreSQL 18.6 (`adjutant_dev`), `debug` profile.
Artifacts: `cargo build --workspace`, `cargo test --workspace` (18 tests, 0 failed).

## Exit criteria (SPEC §15, Milestone 1)

| # | Criterion | Evidence |
|---|-----------|----------|
| 1 | Axum server compiles and responds to HTTP | `GET /` → `{"service":"adjutant","status":"ok"}` HTTP 200 |
| 2 | Plugin registers routes and handles requests through the core router | `GET /api/hello` → `Hello, Adjutant!` 200 (route owned by `hello` `.so`) |
| 3 | Plugin queries PostgreSQL through the core's connection pool | `GET /api/hello/greetings` → 200, rows returned from `hello.greetings` |
| 4 | Plugin publishes and subscribes to events through the event bus | `POST /api/hello/greet` → row in `core.events`; row in `hello.events_received` (subscriber ran) |
| 5 | Permissions declared by plugin, enforced by core middleware | no identity → 401; `scout` (no grant) → 403; `chief` (grant) → 200/201 |
| 6 | Database migrations run automatically when a plugin loads | `core.schema_migrations`: `core:1:core_schema`, `hello:0:create_schema`, `hello:1:create_greetings` |
| 7 | WASM path works **or** native-only fallback proven | Native-only proven: `libloading` + `#[no_mangle] adjutant_plugin_create`. `.so` has **0 tokio/sqlx dynamic symbols**, deps = `libc`, `libgcc_s` only (1.0 MB, down from 5.4 MB). WASM deferred per SPEC §14-R1. |
| 8 | "Hello world" plugin compiles, loads, and responds | `GET /api/plugins` → `{"id":"hello","version":"0.1.0","routes":3,"permissions":2,"enabled":true}` |

## Probe transcript (10/10)

```
1  health check                      PASS  {"service":"adjutant",...}                 200
2  plugin registry lists hello       PASS  {"plugins":[{"id":"hello",...}]}
3  open route (no auth)              PASS  Hello, Adjutant!                          200
4  protected, no identity            PASS  {"error":"authentication required"}       401
5  scout lacks hello:read            PASS  {"error":"insufficient permissions"}      403
6  chief hello:read                  PASS  {"greetings":[]}                          200
7  chief hello:write                 PASS  {"ok":true,"message":"first greeting"}    201
8  event persisted core.events       PASS  hello.greeted (source=hello)
9  greeting readable (db+schema)     PASS  [{id:1,message:"first greeting"}]
10 event delivered to subscriber     PASS  1
```

Post-run state: `audit_log=1` (`action=greet`), `role_grants=2`, server alive, no panics.

## Design decisions this milestone forced

These were not hypothetical — each was hit live, and the fix is now load-bearing.

### 1. Host-mediated I/O (the SDK's defining rule)

The plugin `cdylib` links its **own copy of every dependency**. First bring-up
crashed the server (`exit 134`) the moment a handler touched the database:

```
this functionality requires a Tokio context
fatal runtime error: Rust cannot catch foreign exceptions, aborting
```

Plugin-side `sqlx` resolves *its* tokio's thread-local runtime handle, which the
server's runtime never set. A panic in foreign code cannot unwind across the
boundary, so the whole process aborts.

**Rule:** nothing needing a runtime or a connection may be linked into a plugin.
DB, events, permissions, and audit cross as `Arc<dyn Host…>` trait objects
implemented in the core (`server/src/host.rs`). The SDK depends on neither
`sqlx` nor `tokio`. This is SPEC §14-R1 (the WASM host-API risk) solved for the
native path — and it is what the WASM host API must mirror later.

### 2. Plugin libraries must outlive the router

First crash (`exit 139`, SIGSEGV) was `build_app` holding the `PluginRegistry`
as a **local**: dropping it unloaded the `.so` while the router still held
handler `Arc`s into that code. The registry now lives in `AppState`, which every
dispatch closure captures — libraries outlive the router by construction.

### 3. SPEC §8.1 had invalid DDL

`PRIMARY KEY (user_id, role_id, COALESCE(scope_id, …))` — PostgreSQL forbids
expressions in PRIMARY KEY column lists. The schema failed to migrate on a fresh
database. Fixed in both `server/src/db.rs` and `SPEC.md`: `scope_id` is now
`NOT NULL DEFAULT zero-UUID`, PK is plain columns.

### 4. Fresh-database boot order

`core.schema_migrations` was created before the `core` schema existed. Now
`CREATE SCHEMA IF NOT EXISTS core` runs ahead of migration bookkeeping.

### 5. Bootstrap role grants are a post-load step

Plugins register permissions *during* load; roles are seeded *before* it. A
grant step must run **after** plugins load, or `chief` holds roles with zero
permissions (every protected route 403s). Placeholder until the auth plugin
(Milestone 2) replaces static roles.

## Verification procedure (reproducible)

```bash
cargo build --workspace && cargo test --workspace
psql -h 127.0.0.1 -p 5433 -U adjutant -d postgres \
  -c 'DROP DATABASE IF EXISTS adjutant_dev;' -c 'CREATE DATABASE adjutant_dev;'
mkdir -p plugins-built && cp target/debug/libadjutant_hello.so plugins-built/
ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5433/adjutant_dev \
  ./target/debug/adjutant
# then the 10 probes in README.md
```

## Next

Milestone 2 (Core Server): plugin enable/disable/hot-reload, CORS + rate
limiting middleware, config surface — then Milestone 3 (SDK v0.1 + auth plugin).
