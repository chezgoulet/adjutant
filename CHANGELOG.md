# Changelog

All notable changes to the **Adjutant SDK contract** (`adjutant-sdk`) and the
core that implements it. The SDK and core share a version (SPEC §13.9); the SDK
version is the contract version. See [`docs/sdk-compatibility.md`](docs/sdk-compatibility.md)
for the compatibility rules.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased] — Missions + Governance + SDK v0.2

The SDK stays **0.2.0** and `SDK_ABI_VERSION` stays **4**: every addition below
is additive, so a plugin built against ABI 4 keeps loading unchanged. Rebuild a
first-party plugin to pick the helpers up.

### Added

- **SDK v0.2 route helpers.** `PluginRequest::int_param` (a whole-segment
  capture as an integer, with a missing capture reported as a plugin bug and a
  bad value as a client error), `query_required`, `query_int`, `query_bool`;
  `DbHandle::query_one` (the "fetch one or 404" path) and `DbHandle::exists`.
- **SDK v0.2 event helpers.** `adjutant_sdk::event_type` names the vocabulary
  SPEC §5.4 documents, so a publisher and a subscriber cannot disagree about a
  string; `MissionCompleted`, `MotionPassed`, and `MotionFailed` are the typed
  payloads, and `EventBusHandle::publish_mission_completed` /
  `publish_motion_passed` / `publish_motion_failed` publish them. A payload that
  will not serialize is an error, not a silently dropped event.
- **SDK v0.2 test-harness improvements.** `TestRequest::identity_grants` builds
  a caller with scoped grants — the shape every object route's handler checks and
  the troop-wide `identity` builder could not express. `MockDb` gains
  `queried_sql`, `assert_executed`, `last_execute_params`, and
  `last_query_params` (an `INSERT … RETURNING` is a *query* on this host);
  `MockEvents` gains `payloads`, `assert_published`, and `assert_none`, whose
  failures list what *was* published.
- **SDK v0.2 batched permission lookup.** `PermissionService::scopes_for`
  answers "which of these permissions does this caller hold, and at which
  scopes?" in **one** round trip, returning `PermissionScope` pairs. It is the
  batched form of `has_in_scope`, for a handler that must resolve a whole
  **audience** rather than check a single object — announcements resolves its
  read/manage audience with it, where one query replaces one per grant. It is
  additive (no ABI change), and because the answer is derived from the grants the
  caller already holds, asking cannot widen reach.
- **`adjutant-missions`** (SPEC §7.3, Accords Art 8): the six-stage lifecycle
  (`request → review → approval → execution → debrief → report`) as routes, each
  guarded by the stage the mission is actually in and each written to a stage
  trail and the audit log; the structured proposal form; mentor matching ranked
  on expertise and spare capacity; Lodge Commander approval with guidance,
  rejection, and the Article 8 appeal (a seconding Council member required,
  overturning returns the mission to execution); milestones, execution progress
  notes, debrief, report; and the cumulative Impact Report. Publishes
  `mission.created`, `mission.approved`, and `mission.completed`.
- **`adjutant-governance`** (SPEC §7.4, Accords Art 5/9/12/17): motions through
  `proposed → seconded → debate → voting → decided → implemented`; per-voter
  votes recorded once (the database enforces it) with the method the room used;
  friendly amendments accepted by the mover and formal ones tallied and applied
  to the motion text; meetings with attendance and a **fail-closed** quorum
  (one-third of registered scouts for a Congress, a majority of members for the
  Troop Council, or an explicit number) that a motion cannot be decided without;
  minutes drafted from the motion record; and Accords versions created only by a
  **passed Congress motion**, superseding the previous adopted one. Publishes
  `motion.proposed`, `motion.passed`, `motion.failed`, and `accords.adopted`.
- **`adjutant-mcp`** (SPEC §7.10): the permissions-aware MCP server Hermes
  connects to. `POST /api/mcp/connect` mints an audit-tracked session (only the
  token's SHA-256 is stored), `GET /api/mcp/tools` lists **only** the tools the
  caller's role and scope already reach, and `POST /api/mcp/invoke` re-checks
  the tool's own permission before calling the API route it names — with the
  caller's own credentials forwarded, so the core's gate decides a second time
  and the plugin can never exceed the invoking user's authority. Arguments are
  validated against each tool's published JSON Schema (unknown arguments are
  refused), and every invocation — `ok`, `error`, and every `denied` — is
  recorded in `mcp.invocations` and in `core.audit_log`;
  `GET /api/mcp/invocations` reads the trail back (own rows, or troop-wide with
  `mcp:audit`). Eleven tools cover membership, missions, governance, and
  calendar, and the catalogue is data: `tools.add` / `tools.override` /
  `tools.disable` in the plugin's config retarget or extend it without a
  rebuild.

- **`adjutant-stripe`** (SPEC §7.13): the payment path — Checkout session
  creation for dues, fundraising donations and event fees; `POST
  /api/stripe/webhook`, which verifies Stripe's HMAC signature over the raw body
  (300s replay window, `v0` refused) before it trusts a byte and writes nothing
  when the signature does not check out; idempotent recording at two layers
  (`stripe.webhook_events.event_id`, and `stripe.payments.payment_id` — the same
  key finance holds as `external_ref`, so a redelivery is a no-op and a double
  booking is impossible, and a delivery whose hand-off failed is retried by the
  next one); `stripe.checkout_sessions`, `stripe.webhook_events` and
  `stripe.payments` in its own schema; and `stripe:read` / `stripe:read_all` /
  `stripe:checkout` / `stripe:manage`. **The money path is where the boundary
  bites, and this plugin reports it rather than papering over it:** a webhook
  carries no Adjutant caller, so there is no credential to forward and the
  §2(b) "call its API as the caller" mechanism cannot be used on that path;
  finance's `payment.received` subscriber is therefore the mechanism (idempotent
  on the payment id), and it returns no answer. Every payment carries a
  `ledger_status`, `GET /api/stripe/unbooked` is the worklist of payments with
  no *confirmed* ledger entry, a six-hourly sweep publishes
  `stripe.ledger.unbooked` (a notice, never the write), and `POST
  /api/stripe/payment/{id}/book` makes the ledger write synchronous by
  forwarding the caller's own credential so finance's gate re-decides
  `finance:write`. Closing it structurally needs the pattern
  `docs/design/plugin-to-plugin.md` §3.2 deliberately leaves open.

### Notes for plugin authors

- Both plugins touch **no** `core.*` table: they keep to their own schema, so
  neither appears in `core_grants`. Display names are the client's business; the
  plugins store opaque member ids.
- `governance:manage` and `missions:mentor` extend SPEC §9.1's taxonomy — see
  their doc comments for why running a meeting is not the same authority as
  proposing a motion.

---

## [Unreleased] — Core scheduler

**Breaking:** `SDK_ABI_VERSION` is bumped to **4**. A plugin built against ABI 3
is refused at load with the existing actionable handshake error; rebuild it.

### Added

- **`AdjutantPlugin::schedules()` and `Schedule`** (#45; design
  `docs/design/core-and-plugin-boundary.md` §4). The core runs plugin-declared
  scheduled work with the same discipline as a request: on the plugin's own pool
  (its isolation role), with a per-run timeout and **one attempt per tick** (a
  failure is recorded, not retried), and one durable row per run in
  `core.scheduled_runs`. Schedules start on load and are aborted on
  disable/uninstall/reload. `schedule_handler(...)` wraps the closure;
  `PluginInfo`/`/api/plugins` show each schedule's last run, last error and next
  run. Cadence is an **interval** (`Duration`), not cron.

### Migration for plugin authors

Rebuild against SDK 0.2 / ABI 4. `schedules()` has a default (no schedules), so
an existing plugin compiles unchanged; add `fn schedules(&self) -> Vec<Schedule>`
that returns `Schedule::new(name, Duration, schedule_handler(|| async { … }))`
to run periodic work. Do **not** spawn your own thread.

---

## [Unreleased] — Scope enforcement

**Breaking:** `SDK_ABI_VERSION` is bumped to **3**. A plugin built against ABI 2
is refused at load with the existing actionable handshake error; rebuild it.

### Changed

- **A scope is a condition the gate checks, not a label on a grant**
  (SPEC §9.2; design `docs/design/scoped-permissions.md`).
  `RouteDefinition` gains `required_scope`: the ordinary `*_protected`
  constructors require a **troop-covering** grant; the new
  `*_protected_any_scope` constructors require the permission at *some* scope
  and put the object check in the handler. `delete` always requires troop
  coverage. The core gate passes the declared scope to `authorize` and logs a
  denial with the permission, required scope and the caller's grants.
- **`PermissionService::has` is gone.** The unscoped check is
  `#[doc(hidden)] has_any_scope` (the core gate's "any scope" branch); plugin
  authors write `has_in_scope(…, &Scope::troop())` for a troop-wide check, or
  the new `reach(identity, permission, scope)` which returns a ready 403.
- **`Identity`:** `grants` is the single source of truth; `roles` is a derived
  method. An identity payload without `grants` fails to deserialize.
- **`ScopeType::Personal` is removed.** Reading your own record is an ownership
  check, not a scope.
- **`core.user_roles.scope_id` is `TEXT NULL`** (was `UUID NOT NULL`), so a
  plugin's ids may be bigints, UUIDs or slugs. `NULL` means troop-wide; a
  non-troop scope requires an id and a troop scope requires `NULL`.
- **WASM host:** `permissions.has` is troop-only (kept for back-compat); added
  `permissions.has_in_scope` with `{"permission": …, "scope": {"type": …, "id": …}}`.

### Migration for plugin authors

Rebuild against SDK 0.2 / ABI 3 (`cargo build -p adjutant-your_plugin`;
`adjutant validate-plugin` catches a stale build). Replace any
`permissions.has(id, perm)` with `has_in_scope(id, perm, &Scope::troop())` or
`reach(...)`; remove any `ScopeType::Personal` use; a hand-built
`RouteDefinition` needs the new `required_scope` field (the constructors set it:
troop by default, `None` for `*_protected_any_scope`).

---

## [0.2.0] — 2026-09-24 — Milestone 4 (Core & SDK Stabilization)

Consolidates the M1–M3 contract and stabilizes the core/SDK before the first
crates.io publication. `SDK_ABI_VERSION` is **2**.

### Added

- **Publication prep.** `adjutant-sdk` and `adjutant-server` carry crates.io
  metadata (`readme`, `keywords`, `categories`, docs.rs config); the SDK passes
  `cargo publish --dry-run` and the server confirms the SDK-first publish order.
  See `docs/releasing.md`.
- **WASM plugin host (prototype, SPEC §14-R1).** Sandboxed plugins run in
  `wasmtime` with no preopened filesystem, no sockets, a 64 MiB memory cap, and
  a per-call fuel budget. A host `WasmPlugin` adapter implements the ordinary
  `AdjutantPlugin` trait, so registry/validation/permissions/dispatch are
  unchanged; the guest calls the core through one generic
  `adjutant_host_call` JSON import. Ships a guest helper crate
  (`adjutant-wasm-guest`), a `hello_wasm` example, and sandbox tests.
- **Deployment assets.** `Dockerfile` + `docker-compose.yml` (server +
  PostgreSQL, one command), `docs/deployment.md` (quick start, TLS, upgrade,
  backup/restore), and a tag-triggered release workflow that publishes a
  verified tarball (binary + bundled plugins).
- **Supply-chain and doc gates.** `deny.toml` (advisories, licenses, bans,
  sources), a `cargo doc -D warnings` gate, and an MSRV job, all in CI.
  `rust-toolchain.toml` plus `rust-version = "1.96"` declare the toolchain
  (wasmtime 49 raised the floor from 1.88).
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

- **Unified error envelope + no 5xx leakage.** Every core/plugin error response
  is `{"error": "..."}` built by one helper. `4xx` responses carry the plugin's
  message (the client's fault); `5xx` responses are a generic `internal error`
  with the detail logged, so SQL and driver messages never reach clients.
- **Audit attribution is real.** `AuditService` now writes a genuine
  `core.users` UUID to `core.audit_log.user_id`. Identities that are not users
  (the dev-header stub) keep a NULL FK and are recorded in `details.user_id`
  instead, so the actor is never lost.
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
