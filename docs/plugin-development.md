# Plugin Development

How to build a plugin for Adjutant. This is written for the **stable core + SDK**
(Milestone 4). If you are writing your first plugin, read the
[Quick start](#quick-start), then [The host-mediated I/O rule](#the-host-mediated-io-rule),
then the reference sections you need.

See also: [`sdk-compatibility.md`](sdk-compatibility.md),
[`../CHANGELOG.md`](../CHANGELOG.md), SPEC §5.2 and §5.2a.

---

## Quick start

```bash
# 1. Scaffold a plugin crate (adds it to the workspace)
adjutant new-plugin gear_locker

# 2. Build it (produces target/debug/libadjutant_gear_locker.so)
cargo build -p adjutant-gear_locker

# 3. Validate the declaration — no database, no server
adjutant validate-plugin target/debug/libadjutant_gear_locker.so

# 4. Unit-test it (see adjutant_sdk::testing)
cargo test -p adjutant-gear_locker

# 5. Run the live route probes against a pristine test database
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin

# 6. Load it into a server
cp target/debug/libadjutant_gear_locker.so plugins-built/
ADJUTANT_PLUGIN_DIR=plugins-built cargo run -p adjutant-server
```

The scaffold is a compiling plugin: a manifest (the trait impl), two
permissions, one migration, and an open + a protected route. Start there.

## What a plugin is

A plugin is a Rust `cdylib` that implements the `AdjutantPlugin` trait and
exports it with `export_plugin!`. It owns:

- **a namespace** — its id (`[a-z][a-z0-9_]{0,30}`) is simultaneously its
  PostgreSQL schema and its URL prefix (`/api/{id}/…`);
- **permissions** it defines, and routes that may require them;
- **migrations** that run in its own schema;
- **event subscriptions**;
- optionally, an **identity provider** (only the auth plugin does this today).

The trait implementation *is* the manifest — id, version, permissions, routes,
and migrations are declared in code the compiler checks. (SPEC §5.2's
`manifest.json` is folded into the trait.)

```rust
use std::sync::OnceLock;
use adjutant_sdk::prelude::*;

pub struct GearLocker { ctx: OnceLock<PluginContext> }

#[async_trait]
impl AdjutantPlugin for GearLocker {
    fn id(&self) -> &str { "gear_locker" }
    fn name(&self) -> &str { "Gear Locker" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![Permission::new("gear_locker:read", "Read items")]
    }

    fn routes(&self) -> Vec<RouteDefinition> { /* … */ }
}

export_plugin!(GearLocker);
```

`Cargo.toml` must use `crate-type = ["cdylib", "rlib"]` — `cdylib` so the core
can load it, `rlib` so it can also be used as a test fixture. The scaffold emits
this for you.

## The host-mediated I/O rule

**Nothing that needs a runtime or a connection may be linked into a plugin.**
This is the load-bearing design rule of the whole SDK.

A plugin `cdylib` links its own copy of every dependency. If plugin code called
`sqlx` directly it would look up *its* Tokio thread-local runtime handle — which
the server's runtime never set — and abort the process
(`this functionality requires a Tokio context`). The same applies to `reqwest`.

So the SDK depends on **neither `sqlx` nor `tokio`**, and all I/O crosses the
boundary as `Arc<dyn Host…>` trait objects implemented in the core:

| You want | Use | Never link |
|---|---|---|
| Database | `ctx.db` (`HostDb`) | `sqlx` |
| HTTP | `ctx.http` (`HostHttp`) | `reqwest` |
| Events | `ctx.events` (`HostEvents`) | a message broker client |
| Permissions | `ctx.permissions` | — |
| Audit | `ctx.audit` | — |

Pure-CPU crates (`argon2`, `serde`, `jsonwebtoken`, `sha2`, …) are fine to link
— the auth plugin does.

## `PluginContext`: what you get at `init`

`init` is called exactly once, before `routes()`/`subscriptions()`. Most plugins
store the context in a `OnceLock` or clone it into their handlers.

| Field | Type | Purpose |
|---|---|---|
| `plugin_id` | `String` | Your id. |
| `db` | `DbHandle` | Schema-scoped queries; `ctx.db.table("items")` returns the quoted name. |
| `config` | `serde_json::Value` | Per-plugin config from `core.plugins.config`. |
| `events` | `EventBusHandle` | `publish`, `replay`. |
| `permissions` | `PermissionService` | In-handler permission checks. |
| `audit` | `AuditService` | Append to the tamper-evident audit log. |
| `identity` | `Arc<dyn IdentityRegistrar>` | Register an identity provider (advanced). |
| `http` | `Arc<dyn HostHttp>` | Outbound HTTP (OIDC discovery, webhooks). |

### Database

`DbHandle` runs every call inside a transaction with `search_path` set to your
schema, so bare table names resolve there. Core tables stay reachable as
`core.*`. Rows come back as JSON objects keyed by column name.

```rust
let rows = ctx.db.query(
    format!("SELECT id, name FROM {} ORDER BY id", ctx.db.table("items")),
    vec![],
).await?;
```

The core decodes json/jsonb, bool, int2/int4/int8, float4/float8, `text[]`, the
chrono date/time family, **uuid**, and text. Any other type (numeric, bytea,
inet, non-text arrays) must be **cast in SQL** (`amount::text`); an undecodable
column comes back as JSON `null` and the core logs a warning naming it.

### Bind values — the typed-null trap

`SqlValue` is a closed enum. The important subtlety is that **a NULL carries a
type**:

```rust
SqlValue::Null       // TEXT null
SqlValue::NullInt    // bigint null
SqlValue::NullBool   // boolean null
SqlValue::NullUuid   // uuid null
```

Passing `SqlValue::Null` to a bigint/bool/uuid column is a runtime error
(`column "patrol_id" is of type bigint but expression is of type text`). Do
**not** "fix" it with a cast on a bare parameter — `$2::bigint` makes Postgres
infer the parameter as text, so a non-null integer arrives as garbage. Use the
typed null.

`SqlValue::Uuid(String)` binds as a real uuid (no `::uuid` cast needed);
`SqlValue::IntArray(Vec<i64>)` and `TextArray(Vec<String>)` serve `= ANY($n)`.

### Events

```rust
ctx.events.publish("gear.checked_out", serde_json::json!({ "item_id": id })).await?;
```

Publishing persists to `core.events` first (durable) and then broadcasts to
in-process subscribers. Subscribe with a **prefix filter** (`"gear."`) or `"*"`
by returning `EventSubscription`s from `subscriptions()`. Subscriptions are
bound after `init`, so a handler may capture the context stored there.

### Audit

```rust
ctx.audit.log(req.identity.as_ref(), "checkout", "item", &id.to_string(), serde_json::json!({})).await?;
```

The audit log is append-only and hash-chained. While no auth plugin session
exists, the actor is recorded inside `details`; the FK column stays NULL.

### Outbound HTTP

OIDC discovery, token exchange, and any webhook you add go through `ctx.http`
(host-mediated, 15s timeout, no redirect-following). A JSON body is
`ctx.http.request(...).await?.json::<T>()`.

### Identity providers (advanced)

A plugin can answer "who is this request?" by registering an
`IdentityProvider` during `init`:

```rust
ctx.identity.register("auth", Arc::new(MyProvider { db: ctx.db.clone() }));
```

Providers are queried for every request before any fallback. With dev headers
off, the auth plugin is the only provider; the core refuses to disable or
uninstall the only enabled provider (409) to prevent an operator lockout.

## Routes

- Paths **must** live under `/api/{id}` (the core rejects namespace escapes at
  load).
- A segment may be a whole-segment capture: `/api/gear_locker/items/{id}`.
  Captures are percent-decoded exactly once before reaching your handler;
  literal routes match before templated ones; two templates of the same shape in
  one position are rejected as duplicates.
- Gate a route with a permission using the `*_protected` constructors. The
  **core** enforces it — never the plugin.

```rust
RouteDefinition::get_protected(
    "/api/gear_locker/items/{id}",
    "gear_locker:read",
    route_handler(move |req| {
        let ctx = ctx.clone();
        async move {
            let id = req.param("id").unwrap_or_default();
            // …
            PluginResponse::json(200, &serde_json::json!({ "item": row }))
        }
    }),
)
```

Methods: `get`, `post`, `put`, `patch`, `delete`, `head`, each with a matching
`*_protected` form. `PluginResponse` helpers: `json`, `text`, `empty`,
`no_content`, `redirect`, `created`, `error`, `with_header`.

Return an `SdkError` to short-circuit with the matching status:
`BadRequest` 400, `Unauthorized` 401, `Forbidden` 403, `NotFound` 404,
`Conflict` 409, `Db`/`Internal` 500.

## Scoped permissions (SPEC §9.2)

Permissions can be scoped to a troop, lodge, patrol, or person. The route gate
(`has`) checks the permission against any role the caller holds; an in-handler
check can require that the role's grant actually **covers** a scope:

```rust
use adjutant_sdk::prelude::*;

async fn approve(ctx: &PluginContext, req: &PluginRequest, lodge_id: &str) -> Result<(), SdkError> {
    let scope = Scope::lodge(lodge_id);
    if !ctx.permissions.has_in_scope(req.identity.as_ref(), "missions:approve", &scope).await {
        return Err(SdkError::Forbidden("not your lodge".into()));
    }
    Ok(())
}
```

- `Identity` carries `roles` (flat) and `grants` (`RoleGrant { role_id, scope }`).
  Build a troop-wide identity with `Identity::new(user_id, roles)`, or scoped
  ones with `Identity::from_grants(user_id, grants)`.
- `Scope::troop()` covers every scope; other scopes match by type and id
  exactly. The core does not model the lodge→patrol hierarchy, so coverage is
  intentionally flat.
- The auth plugin reads `core.user_roles(user_id, role_id, scope_type,
  scope_id)` and populates `grants` for the session identity.

## Migrations and schema

Return `Migration`s from `migrations()`. Versions start at 1 and must be unique.
Each runs once, in version order, inside a transaction with `search_path` set to
your schema, recorded in `core.schema_migrations`.

```rust
fn migrations(&self) -> Vec<Migration> {
    vec![Migration::new(
        1,
        "initial_schema",
        "CREATE TABLE IF NOT EXISTS items (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);",
    )]
}
```

A migration's SQL may contain multiple statements. (This is safe because the
runner uses `Executor::execute` on raw SQL; the *runtime* `HostDb` uses prepared
statements, which allow only one statement per call.)

## Schema isolation

Each plugin gets its own PostgreSQL schema **and** — when the database role can
manage roles (`CREATEROLE`/superuser) — its own `NOLOGIN` role
`adjutant_plugin_<id>`. Every `ctx.db` call runs inside `SET LOCAL ROLE`, so:

- bare table names resolve in your schema;
- you have full rights on your own schema;
- you can touch only an explicit allowlist of `core.*` tables (see
  `core_grants` in `server/src/schema.rs` — auth and membership have entries;
  other plugins get none);
- reaching into **another plugin's** schema fails with `permission denied`.

Migrations are authored by you and run as the base role at boot, so they may
create objects freely (and first-party plugins may alter core tables there). The
isolation applies to runtime queries, which is where plugin code executes. If
your plugin needs a core table, request it be added to `core_grants`; do not
assume `core.*` is open.

## Testing with `adjutant_sdk::testing`

Unit-test handlers and lifecycle without a database, server, or core:

```rust
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::*;

#[tokio::test]
async fn list_returns_rows() {
    let host = TestHost::new();
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "message": "hi" })]);
    let ctx = host.context("greetings");

    let list = route_handler(move |_req: PluginRequest| {
        let ctx = ctx.clone();
        async move {
            let rows = ctx.db.query("SELECT * FROM greetings", vec![]).await?;
            PluginResponse::json(200, &serde_json::json!({ "greetings": rows }))
        }
    });

    let resp = list(TestRequest::get("/api/greetings").build()).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(response_json(&resp)["greetings"][0]["message"], "hi");
    assert_eq!(host.db.query_count(), 1);
}
```

`TestRequest` builds requests (method, `json` body, `param`, `query_param`,
`header`, `identity`). `MockHttp` replays queued responses and fails on an
unexpected network call. `MockIdentity` records registered providers.

## Packaging, loading, and compatibility

- Build with `crate-type = ["cdylib", "rlib"]`.
- The core scans `ADJUTANT_PLUGIN_DIR` for `*.so` at boot and on reload.
- `export_plugin!` emits both `adjutant_plugin_create` and `adjutant_sdk_abi`;
  the core verifies the ABI before calling the factory and refuses stale builds.
- Native loading means **core and plugin share a toolchain and dependency
  versions** — this is not a security boundary. Only load trusted plugins until
  the WASM path lands (Milestone 4 W3).
- `adjutant validate-plugin <so>` is the fast, database-free gate. Run it in CI.
- See [`sdk-compatibility.md`](sdk-compatibility.md) for the version policy.

## Common traps (learned the hard way)

1. **Typed nulls** — use `NullInt`/`NullBool`/`NullUuid`, never `Null`, for
   non-text columns.
2. **`raw_sql` is not provably `Send`** — inside code reachable from a handler,
   use `Executor::execute(&mut *conn, sqlx::raw_sql(...))`, never the `async fn`
   wrapper (the HRTB bound breaks axum's `Handler`).
3. **Never hold a non-`Send` borrow across an `.await`** — a `libloading::Symbol`
   or a `slice::Iter` in the generator state poisons the future.
4. **One statement per `HostDb` call at runtime** — the host prepares
   statements; split multi-statement write batches.
5. **`search_path` is per call** — don't assume a bare name resolves outside a
   `ctx.db` call.
6. **Route namespace is enforced** — every path starts with `/api/{id}`.
7. **Permissions are namespaced and must be granted** — a route requiring a
   permission the plugin doesn't declare is rejected at load.
8. **Migration versions start at 1** and must be unique.

## Command reference

| Command | Purpose |
|---|---|
| `adjutant new-plugin <name>` | Scaffold `plugins/<name>/` and add it to the workspace. |
| `adjutant validate-plugin <so>` | Static validation; no database. |
| `adjutant test-plugin` | Boot against a pristine test DB and probe every route with mock permissions. |
| `adjutant serve` | Run the core server (default). |
