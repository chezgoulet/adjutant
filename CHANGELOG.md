# Changelog

All notable changes to the **Adjutant SDK contract** (`adjutant-sdk`) and the
core that implements it. The SDK and core share a version (SPEC §13.9); the SDK
version is the contract version. See [`docs/sdk-compatibility.md`](docs/sdk-compatibility.md)
for the compatibility rules.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased] — Milestone 4 (Core & SDK Stabilization)

Targeting `adjutant-sdk` 0.2.0. Consolidates the M1–M3 contract before the first
crates.io publication.

### Added

- **Enforced schema isolation.** Each plugin gets a `NOLOGIN` PostgreSQL role
  (`adjutant_plugin_<id>`); its runtime database handle runs under
  `SET LOCAL ROLE` with full rights on its own schema and an explicit allowlist
  of `core.*` tables, so cross-plugin schema access is denied by the database
  (SPEC §5.2). Verified by a DB-backed test. Requires `CREATEROLE`/superuser;
  otherwise isolation is skipped with a warning. See `server/src/schema.rs`.
- **Scoped permissions (SPEC §9.2).** `ScopeType`, `Scope`, and `RoleGrant`;
  `Identity::new` (troop-wide), `Identity::from_grants`, and
  `Identity::roles_covering`; and `PermissionService::has_in_scope` for
  in-handler checks against a specific lodge/patrol/personal scope. The auth
  plugin now populates scoped grants from `core.user_roles`. Route-level gating
  is unchanged (`has`, any scope).
- **ABI handshake.** `export_plugin!` now also exports `adjutant_sdk_abi`, and
  the core resolves it *before* calling the plugin factory. A plugin built
  against a different SDK ABI is refused at load with an actionable error
  instead of running against a mismatched vtable. `SDK_ABI_VERSION` is the
  contract knob; bump it on any breaking change.
- **Public test harness** `adjutant_sdk::testing`: `TestHost`, `MockDb`,
  `MockEvents`, `MockHttp`, `MockIdentity`, and a `TestRequest` builder, so
  handlers and lifecycle can be unit-tested without a database or server.
- **`adjutant validate-plugin <so>`**: static validation (ABI, init against
  in-memory mocks, id, permissions, migrations, routes) with no database.
- **`SdkError` variants** `Unauthorized`, `Forbidden`, `NotFound`, `Conflict`,
  plus `SdkError::status()` — one error→HTTP mapping shared by core and plugins.
- **`Method::Patch` / `Method::Head`**, and `put_protected`,
  `patch`/`patch_protected`, `delete_protected`, and `head` constructors on
  `RouteDefinition` (previously only `get`/`post` had protected variants).
- **`PluginResponse` helpers**: `redirect`, `no_content`, `created`, and
  `with_header`.
- **`SqlValue::Uuid` / `NullUuid`** (bound as `uuid`, no `::uuid` cast needed)
  and **`SqlValue::IntArray`** for `= ANY($n)`. The host decodes `uuid` columns
  to their canonical string.

### Changed

- **The spoofable dev identity headers are now opt-in.** `ADJUTANT_DEV_HEADERS`
  defaults to `false`; enable it with `--allow-dev-headers`,
  `ADJUTANT_DEV_HEADERS=true`, or `[auth] allow_dev_headers = true`. The
  milestone harnesses set it explicitly.
- **Breaking (ABI 2):** `Identity` gained a `grants: Vec<RoleGrant>` field. Use
  `Identity::new` / `Identity::from_grants` rather than a struct literal.
  `SDK_ABI_VERSION` is now `2`.
- The core's plugin error mapping now uses `SdkError::status()` (a wrong
  password from a plugin is a `400`, not a `500`).
- Loader validation is factored into shared pure functions
  (`validate_declaration`, `validate_migrations`, `check_sdk_abi`) so the load
  path and `validate-plugin` enforce exactly the same rules.

## [0.1.0] — Milestones 1–3

The initial contract, validated by building the `auth` and `membership` plugins
with it. Core: plugin registry and lifecycle, per-plugin PostgreSQL schema,
Axum middleware stack, event bus with persistence and replay, tamper-evident
audit log, config precedence. SDK: `AdjutantPlugin`, `PluginContext`,
`HostDb`/`HostEvents`/`HostHttp`, the identity seam, routes with path captures,
migrations, permissions, and audit.
