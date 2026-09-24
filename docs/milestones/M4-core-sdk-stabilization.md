# Milestone 4 — Core & SDK Stabilization

**Status:** In progress · opened 2026-09-23
**Branch:** `feature/m4-core-sdk-stabilization` (from `testing`; merges into `testing`)
**Roadmap note:** This milestone is inserted *before* SPEC §15's "M4 (missions +
governance)". The foundation must be stable, documented, and publishable before
anyone — first-party or third-party — starts writing plugins against it. The
SPEC M4 domain plugins follow once this milestone's exit criteria pass.

## Goal

Bring the core and `adjutant-sdk` to a completed, stable, and usable state so
that writing a plugin is a documented, testable, reproducible experience. No new
domain plugins are in scope. The SDK is the product; everything else is
infrastructure (SPEC §14-R2).

## Non-goals

- No missions/governance/finance/etc. plugins (SPEC M4+).
- No Flutter client (SPEC M5).
- No multi-tenancy, app-store distribution, or the full SPEC M7 hardening set.

## Branch & release flow (house standard)

- `testing` = integration target. All work branches from it and merges into it.
- `main` = deployable state; releases are cut from it. `testing` is merged into
  `main` only when a release is ready.
- W0 below performs the first release-gate merge: the `testing` history becomes
  the `main` baseline and is tagged.

## Baseline (W0 evidence)

Recorded 2026-09-23 on `testing` @ `19e65df` (pre-merge; `main` was 15 commits
behind and fast-forwardable).

| Gate | Result |
|---|---|
| `cargo build --workspace` | clean, 0 errors |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings |
| `cargo test --workspace` | **62 passed / 0 failed** (auth 14, hello 2, membership 3, sdk 7, server 34, host_db 2) |
| Live probe ladders (`scripts/probes.py`, `docs/e2e_m3.py`) | **Not run locally** — require PostgreSQL + `psql`, absent on the authoring host. CI (`.github/workflows/ci.yml`) owns these; they must be green on `main` after the W0 merge. |

Test breakdown matches `M3-sdk-and-plugins.md` (62/62), so the baseline is
consistent with the milestone record.

---

## Workstreams

Each workstream lists tasks and explicit exit criteria. A workstream is done only
when its exit criteria are evidenced (tests, harness output, or a committed
document), not when the code merely exists.

### W0 — Integrate & baseline ✓ (mostly)

- [x] Validate baseline on `testing` (build, clippy, 62 tests).
- [x] Merge `testing` → `main` as the v0.1 benchmark and tag `v0.1.0`.
- [x] Confirm the stale `main` README is replaced by the accurate dev README
      (carried in by the merge).
- [x] Confirm CI is green on `main` after the merge — run
      [35935007267](https://github.com/chezgoulet/adjutant/actions/runs/35935007267),
      all steps passed (build, clippy, tests, M1/M2 probe ladder, `test-plugin`,
      scaffolder-compiles, e2e).
- [x] Record the baseline above.

**Exit criteria:** `main` builds and its CI passes at the tagged commit; the
baseline numbers are recorded here. **Met** — `v0.1.0` @ `6a0f2d7`, CI green.

### W1 — Lock the SDK contract (target `adjutant-sdk` 0.2 → 1.0-rc) ✓

**Branch:** `feature/m4-w1-sdk-contract` + follow-ups
(`feature/m4-w1-identity-scope`, `feature/m4-w1-schema-isolation`,
`feature/m4-w1-testing-dogfood`). **Status: complete.**

- [x] Version handshake: `export_plugin!` emits an `adjutant_sdk_abi` symbol and
      the core verifies it against `SDK_ABI_VERSION` **before** the factory,
      rejecting stale builds with an actionable error. (Done via an exported
      symbol rather than a trait method, so a stale plugin cannot be read through
      a mismatched vtable before the check.)
- [x] Publish a SemVer / compatibility policy and a `CHANGELOG.md` for the SDK —
      `docs/sdk-compatibility.md`, `CHANGELOG.md`.
- [x] Ergonomics:
  - [x] `Method::Patch` and `Method::Head`.
  - [x] `_protected` constructors for `put`/`delete`, plus `patch`/`head`
        (all methods now have matching protected constructors).
  - [x] `SdkError` gains `NotFound`, `Conflict`, `Forbidden`, `Unauthorized`;
        central `SdkError::status()` used by the core.
  - [x] `PluginResponse` helpers: redirect, `204` (`no_content`), created,
        `with_header`.
  - [x] `SqlValue` gains `Uuid`/`NullUuid`/`IntArray`; the host decodes uuid
        columns. Numeric/bytea/non-text arrays remain cast-in-SQL, documented.
- [x] Decide the identity/scope question — **resolved: model scope now.**
      `ScopeType`/`Scope`/`RoleGrant`, `Identity::{new,from_grants,roles_covering}`,
      and `PermissionService::has_in_scope` added; auth populates scoped grants
      from `core.user_roles`. `SDK_ABI_VERSION` bumped to 2 (breaking: `Identity`
      gained `grants`). See `docs/plugin-development.md` §Scoped permissions.
- [x] Public `adjutant_sdk::testing`: `MockDb`/`MockEvents`/`MockHttp`/
      `MockIdentity`, `TestHost::context`, and a `TestRequest` builder, with a
      usage doctest. **Dogfooded:** the SDK's own tests, `hello`, and now `auth`
      (`session_identity` → scoped grants via `MockDb`) and `membership`
      (`upsert_from_import` → `MockDb`) all exercise it.
- [x] `adjutant validate-plugin`: static validation (ABI, init against mocks, id,
      permission references, migration versions, route namespace/captures/
      duplicates) with no database; wired into CI (incl. the scaffolded plugin).
- [x] Schema isolation — **enforced via per-plugin PostgreSQL roles.**
      `server/src/schema.rs`: a `NOLOGIN` role per plugin, `SET LOCAL ROLE` on
      every runtime `ctx.db` call, full rights on its own schema, and an explicit
      `core.*` allowlist. A DB-backed test proves cross-schema access is denied.
      Requires `CREATEROLE`/superuser; skips isolation with a warning otherwise.
- [x] Documentation: `docs/plugin-development.md` written.

**Exit criteria:** a plugin can be scaffolded, statically validated, unit-tested
with only the public SDK, and loaded; `auth` + `membership` pass using only the
public SDK + testing module. **Met.**

### W2 — Core hardening

- [x] Flip `ADJUTANT_DEV_HEADERS` default to `false`; make dev headers an
      explicit opt-in. Update harnesses and docs. (`--allow-dev-headers` flag +
      env + `[auth]` file; harnesses set it explicitly.)
- [x] Write the real actor into `core.audit_log.user_id` when the auth plugin is
      present (remove the `details`-embedded `user_id` workaround). Non-user
      identities keep a NULL FK and fall back to `details.user_id`.
- [x] Unify the API error envelope and status mapping across core and plugins.
      One `{"error": ...}` envelope; 5xx responses are generic (detail logged,
      not returned), 4xx carry the plugin message.
- [x] MSRV pin (`rust-toolchain.toml`), `cargo-deny`, and a `cargo doc`
      warnings gate. MSRV declared as 1.88; `deny.toml` + CI `cargo-deny`,
      `cargo doc -D warnings`, and an MSRV `cargo check` job.
- [x] Dockerfile + `docker-compose.yml` (server + PostgreSQL) and a documented
      upgrade / backup / restore story — `docs/deployment.md`; image built and
      smoke-tested against Postgres (health, open route, 401 on an admin route,
      schema isolation active).
- [x] GitHub release workflow (build, tag, artifacts) — `.github/workflows/release.yml`.

**Exit criteria:** a fresh host can deploy the tagged core with one documented
command; security defaults are safe; supply-chain and doc gates are enforced in CI.

### W3 — WASM host API prototype (SPEC §14-R1)

- [ ] A host-side `WasmPlugin` adapter implementing `AdjutantPlugin`, backed by
      `wasmtime`.
- [ ] Prototype ABI: guest exports a handle entry point; imports one generic
      `adjutant_host_call(method, payload_json) -> result_json` dispatched to
      db / events / permissions / audit / http. Reuses the SDK's JSON types.
      (Typed WIT / component model is the likely end state — documented.)
- [ ] Enforce: no filesystem, no network by default, memory cap, fuel/epoch CPU
      limit, per-call timeout.
- [ ] Compile `hello` (and a scaffolded plugin) to `wasm32-wasip1`, load, and
      serve through the same registry/routes/permissions pipeline.
- [ ] Tests: guest FS/network attempts fail; a runaway loop is interrupted;
      `test-plugin` can exercise a WASM plugin.
- [ ] Fallback documented: if async reentrancy/perf blocks the prototype, land
      the ABI spec and keep native-only, explicitly.

**Exit criteria:** native and WASM paths both proven; the sandbox limits are
tested, not asserted; the trusted-native vs untrusted-WASM distinction is
documented.

### W4 — Documentation & DX

- [ ] `docs/plugin-development.md`: lifecycle, the host-mediated I/O rule and
      why (the M1 abort story), every `PluginContext` service, routes/captures/
      permissions, migrations/schema, events, audit, the testing module,
      packaging/loading, compatibility, and the real traps (`SqlValue` typed
      nulls, `raw_sql` `!Send`, `search_path`).
- [ ] `docs/architecture.md`, `docs/api-reference.md` (core REST + SDK API),
      `docs/deployment.md`, and a WASM authoring note.
- [ ] Promote `plugins/examples/hello` into a proper reference example.
- [ ] `CONTRIBUTING.md`; `docs.rs` metadata; `cargo doc` published.

**Exit criteria:** the W6 fresh-machine test passes using documentation alone.

### W5 — Publication & CLI

- [ ] Publish `adjutant-sdk` to crates.io.
- [ ] Ship an installable CLI. Decision required: publish `adjutant-server`
      (bin `adjutant`, all subcommands) vs. split a lightweight `adjutant-cli`
      for `new-plugin`/`validate-plugin` (no database). Document the choice.
- [ ] Document git-tag pinning as the crates.io alternative.
- [ ] Define the third-party plugin distribution/loading story.

**Exit criteria:** `cargo install` yields a working CLI; `cargo add adjutant-sdk`
works; an author can follow the docs end-to-end.

### W6 — Stability gates & release

- [ ] CI covers `validate-plugin` on scaffold output, the SDK testing module
      used by shipping plugins, WASM sandbox tests, `cargo-deny`, `cargo doc`.
- [ ] Dogfood gate: `auth` + `membership` import only
      `adjutant_sdk::prelude` + `adjutant_sdk::testing`.
- [ ] Fresh-machine exit test: a third-party plugin scaffolded and built
      following only `docs/plugin-development.md` loads and passes
      `test-plugin` (native and WASM).
- [ ] Merge `testing` → `main`, tag `v0.2.0` (or `v1.0.0-rc1` if the contract is
      frozen).

**Exit criteria:** all gates green on `main`; the tag is cut; the "start writing
plugins" footing is real.

---

## Definition of done

- `main` holds a releasable state; releases tagged from it; `testing` is the
  integration target.
- `adjutant-sdk` is published with a documented SemVer/compat policy; an
  installable CLI ships beside it.
- A plugin author can: install the CLI → `new-plugin` → build →
  `validate-plugin` → unit-test with the public SDK testing module →
  `test-plugin` → load into a running core — using docs alone.
- `auth` + `membership` compile and pass against only the public SDK + testing
  module.
- Native and WASM paths both proven; sandbox limits tested.
- Gates green: build, test, clippy, all live harnesses, `cargo doc`,
  `cargo-deny`.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | **W3 async boundary** is the main technical unknown. | Timebox the prototype; keep the native-only fallback; land the ABI spec regardless. |
| 2 | **Identity/scope design** is a breaking contract decision. | Decide it in W1, before publishing. |
| 3 | **Once published, every SDK change is a SemVer commitment.** | Freeze and review the contract before W5. |
| 4 | **Local harness gap.** Live probes need PostgreSQL/`psql`, absent here. | CI is the authority for W0; do not claim a gate that was not run. |
| 5 | **Scope creep back into domain plugins.** | Non-goals above; the milestone gate is enforced. |

## Tracking

Update the checkboxes and the baseline table as work lands. Each workstream's
exit criteria are the review gates at its boundary; do not advance on intent.
