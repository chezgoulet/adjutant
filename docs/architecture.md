# Architecture

Adjutant is a **thin core with thick plugins** (SPEC §2.1). Everything that is
not essential to the plugin system itself is a plugin. This document describes
how the pieces fit together; see [`plugin-development.md`](plugin-development.md)
to build one and [`SPEC.md`](../SPEC.md) for the full design.

## Components

```
┌──────────────────────────── adjutant (single binary) ────────────────────────────┐
│                                                                                   │
│  core (server/src)                                                                │
│    config.rs         layered config (defaults < file < env < CLI)                 │
│    db.rs             PgPool, core schema migrations, migration runner             │
│    schema.rs         per-plugin PostgreSQL roles (isolation)                      │
│    events.rs         in-process bus (fan-out); durability lives in core.events    │
│    host.rs           HostDb / HostEvents / HostHttp implementations               │
│    identity.rs       IdentityHub: plugin-registered identity providers            │
│    middleware.rs     request id, access log, rate limiting, CORS                  │
│    permissions.rs    identity extraction + route-level authorize()                │
│    plugin_runtime.rs discovery, loading, validation, registry, lifecycle          │
│    wasm.rs           wasmtime host + WasmPlugin adapter (sandbox)                 │
│    server.rs         Axum router, dynamic dispatch, admin lifecycle, error shape  │
│    cli.rs            new-plugin / validate-plugin / test-plugin                   │
│                                                                                   │
│  plugins (cdylib for native, or *.wasm for sandboxed)                             │
│    plugins/sdk           adjutant-sdk — the contract                              │
│    plugins/auth          sessions, argon2, OIDC, roles                            │
│    plugins/membership    roster, patrols, lodges, proficiencies, CSV import       │
│    plugins/missions      mission lifecycle, mentors, milestones, impact report    │
│    plugins/governance    motions, votes, amendments, quorum, minutes, Accords     │
│    plugins/examples/hello        native reference                                 │
│    wasm/examples/hello           sandboxed reference                              │
└───────────────────────────────────────────────────────────────────────────────────┘
                                   │
                              PostgreSQL (one database, one schema per plugin + core)
```

There is no Redis, no external search engine, and no separate cache (SPEC §2.6).
`core.events` and PostgreSQL full-text search cover those needs.

## Request path

Every non-core request takes the same route through the core:

1. **Middleware** (outer → inner): CORS → request id + structured access log →
   rate limiting → dynamic dispatch.
2. **Dispatch** (`server.rs::dynamic_dispatch`): resolves `METHOD path` against
   the live registry under a read lock, then releases the lock before running the
   handler (a slow plugin never blocks a reload).
3. **Identity**: plugin providers first (auth sessions via cookie/Bearer), the
   dev-header stub only if explicitly enabled and nothing else claimed the
   request.
4. **Permission gate**: the core checks the route's `required_permission`
   against the identity — never the plugin (SPEC §9).
5. **Handler**: the core builds a framework-neutral `PluginRequest` and calls the
   plugin. Native and WASM plugins are indistinguishable here.
6. **Response**: a `PluginResponse` (status, headers, body) is written back; an
   `SdkError` maps to a status via `SdkError::status()`.

## Plugin lifecycle

Per SPEC §5.2, enforced by `plugin_runtime`:

1. **Discovery** — scan `ADJUTANT_PLUGIN_DIR` for `*.so` and `*.wasm`.
2. **Load** — native: `dlopen` + `adjutant_plugin_create`, with an ABI handshake
   (`adjutant_sdk_abi`) *before* the factory; WASM: instantiate in wasmtime and
   read the guest manifest.
3. **Validate** — id shape/reserved names, `/api/{id}` namespace, path captures,
   permission references, duplicate routes.
4. **Skip uninstalled** — before any side effect.
5. **Credential + pool** — read the plugin's stored credential, build a small
   pool authenticated as its `adjutant_plugin_<id>` role. No credential → the
   load fails.
6. **Migrations** — run pending migrations **on that pool**, as the plugin role,
   inside the plugin's own schema.
7. **Register permissions** into `core.permissions`; upsert `core.plugins`.
8. **`init(ctx)`** — hand the plugin its `PluginContext`.
9. **Serve** — routes and event subscriptions.
10. **Disable / uninstall / reload** — hot, without rebuilding the router.
    Libraries are retired, never unloaded (a `cdylib`'s code may still be
    referenced by in-flight requests).

## Host-mediated I/O (the defining rule)

A native plugin links its **own** copy of every dependency. If it called `sqlx`
directly it would resolve *its* tokio runtime handle — which the server's runtime
never set — and abort the process. So **nothing that needs a runtime or a
connection is linked into a plugin**: DB, events, HTTP, permissions, and audit
cross the boundary as `Arc<dyn Host…>` trait objects implemented in the core. The
SDK depends on neither `sqlx` nor `tokio`. WASM plugins follow the same rule
through a single JSON host call.

## Trust model: native vs WASM

| | Native (`*.so`) | WASM (`*.wasm`) |
|---|---|---|
| Trust | trusted (same address space, no isolation) | sandboxed |
| Isolation | none (ABI must match; not a security boundary) | wasmtime: no FS, no network, memory cap, fuel |
| I/O | `Arc<dyn Host…>` trait objects | `adjutant_host_call` JSON import |
| Distribution | first-party / trusted | suitable for third-party |

Schema isolation (below) applies to both.

## Data model

- **`core.*`** — users, sessions, roles, permissions, role_permissions,
  user_roles, plugins, events, audit_log, schema_migrations.
- **`{plugin}.*`** — one PostgreSQL schema per plugin, **owned by the plugin's
  `adjutant_plugin_<id>` `LOGIN` role**. The host runs that plugin's SQL on a pool
  authenticated as that role, so the boundary is the identity of the connection,
  not a statement filter: the plugin cannot `SET ROLE`/`RESET ROLE` into anything
  else, and a query into another plugin's schema is denied by the database.
  Cross-`core` access is an explicit allowlist (`server/src/schema.rs`).
- **Plugin credentials** live in `core.plugins.db_secret` (never in `config`,
  which is handed to the plugin as `ctx.config`). Roles, schema ownership, the
  allowlist and passwords are created by `adjutant bootstrap-isolation`; the
  runtime never needs `CREATEROLE`.
- **Audit log** is append-only (triggers reject UPDATE/DELETE/TRUNCATE) and
  hash-chained; `core.audit_verify()` recomputes the chain.

See [`design/plugin-isolation.md`](design/plugin-isolation.md) for the threat
model and the escape probes that pin it.

## Permissions

Permissions are namespaced strings (`missions:approve`). Roles map to permissions
(`core.role_permissions`); users hold roles, scoped to a **troop**, **lodge** or
**patrol** (`core.user_roles.scope_type`/`scope_id`, SPEC §9.2). `scope_id` is
opaque text owned by the plugin (`NULL` = troop-wide).

A route declares its reach, and the core gate enforces it:

| Constructor | Gate requires | Use for |
|---|---|---|
| `get_protected` (and `post`/`put`/`patch`) | a grant **covering troop** | collections, admin, reference data |
| `*_protected_any_scope` | the permission at *some* scope; the **handler** checks the object | object routes (`/member?id=`, `/mission/{id}/approve`) |
| any `delete` | a troop-covering grant | destructive operations |

Coverage is hierarchical: a troop grant covers every scope; a lodge grant covers
that lodge **and the patrols declared inside it**; a patrol grant covers only
that patrol. The hierarchy is core-owned data (`core.scope_hierarchy`) declared by
the owning plugin and resolved by the core in memory (no plugin call during
authorization, cycles rejected). Edges are owned **per scope type**:
`core.scope_owners` is core-written and plugin-unreadable, and a trigger plus
`core.declare_scope_parent` refuse an edge a plugin does not own. A handler
decides an object route with
`PermissionService::has_in_scope` (or `reach`, which returns a 403 naming the
scope). `PermissionService::has_any_scope` is the core gate's unscoped branch and
is hidden from plugin authors. Self-access is an ownership check, not a scope.

## Identity

`IdentityHub` holds one provider per owner (plugin id). The auth plugin registers
a session provider at `init`; the core consults providers for every request. Dev
headers (`x-dev-user`/`x-dev-role`) are off by default and only act as a fallback
when enabled.

## Scheduler

Plugins declare periodic work (`Schedule`); the core runs it on one scheduler
(`server/src/scheduler.rs`) with the same discipline as a request: the plugin's
own pool/isolation role, a per-run timeout, one attempt per tick, and a durable
row in `core.scheduled_runs`. Schedules start on load and are aborted on
disable/uninstall/reload. Cadence is an interval, not cron; after downtime a due
schedule runs once and resumes (no backfill). `/api/plugins` shows each
schedule's last run, last error and next run.

## Versioning

The core and `adjutant-sdk` share a version. Plugins built against a different
SDK ABI are refused at load. See [`sdk-compatibility.md`](sdk-compatibility.md).
