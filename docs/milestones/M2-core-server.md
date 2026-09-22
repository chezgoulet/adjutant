# Milestone 2 — Core Server ✓

**Status:** PASSED · 2026-09-22 · Every exit criterion verified live.

Environment: Rust 1.96.1, clippy 0.1.96, PostgreSQL 18.6 (`adjutant_dev`,
reset to empty before the run so migration 2 executed on a fresh database).
Gates: `cargo clippy --workspace --all-targets` → **0 warnings**,
`cargo test --workspace` → **28 passed / 0 failed at commit `0cb6fff`** (56 today),
live probes → **63/63**, four batches (m1_regression, middleware, lifecycle,
tamper), reproducible and committed:

```
python3 scripts/probes.py        # writes docs/evidence/m2_probes.json
```

**Correction (2026-09-22, adversarial audit).** An earlier version of this document
claimed "live probes → 52/52 across four batches". That number was wrong and its
evidence was not in the repository: the four `m2_probes_*.json` transcripts were
gitignored (`.gitignore:6`, still are) and, where they existed on the authoring
machine, recorded 57 entries with 50 passes and 7 failures (`part1` R8/M4/M5/M6/M7
failed on a self-inflicted 429 window; `lifecycle` L15/L18 failed). The probes were
re-run through the committed harness; the defects the audit found behind those
failures are listed below.

## Exit criteria (SPEC §15, Milestone 2)

| # | Criterion | Evidence |
|---|-----------|----------|
| 1 | Plugin registry: load, enable, disable, uninstall, hot-reload | lifecycle batch (L1–L19b): disable → route 404 `plugin hello is disabled` + core routes unaffected + DB `enabled=false`; enable → 200; uninstall → 404, rows/schema survive (`data archived`), `retired_libraries ≥ 1`; reload → `reloaded: []` while uninstalled; clear the flag + reload → `reloaded: ["hello"]` with the route and its subscription live again |
| 2 | Database connection pool with per-plugin schema isolation | Unchanged from M1 + boot on fresh DB; plugin migrations still run in their own schema |
| 3 | Middleware stack: request ID, logging, CORS, rate limiting, auth, authorization, routing | `x-request-id` on every response incl. 429s; `adjutant::http` structured line per request (id, method, path, status, latency_ms) and a parseable JSON line in JSON mode; `access-control-allow-origin` echoed from `ADJUTANT_CORS='*'` on 200 **and** 429; 9th request in a process-clean window → 429 + `retry-after: 60`; 401/403/permission gates re-verified. Note: rate limiting keys on the direct peer; `x-forwarded-for` is only honoured for a peer listed in `ADJUTANT_TRUSTED_PROXIES` (empty by default) |
| 4 | Event bus: pub/sub, persistence, replay | `?since=<id>&limit=<n>` cursor replay (`cursor` field; `since=999` → empty page); publish persists *then* broadcasts with the real row id; subscriptions rebound after reload (L21: event published post-reload lands) |
| 5 | Configuration: file, environment, CLI | `config.rs`: defaults < TOML file < env < CLI, all three layers unit-tested in one precedence test; CLI accepts `--flag value` and `--flag=value`, rejects unknown flags; live run used env (`ADJUTANT_RATE_MAX`, `ADJUTANT_CORS`), JSON smoke used `ADJUTANT_LOG_FORMAT=json` |
| 6 | Structured logging with tracing | Pretty mode: one line per request with fields. JSON mode: `{"timestamp":…,"level":"INFO","fields":{…},"target":…}` — parsed as valid JSON |
| 7 | Audit log: append-only, tamper-evident | tamper batch (T1–T9): direct `UPDATE`/`DELETE` → `ERROR: core.audit_log is append-only`; rows untouched; SHA-256 hash chain (migration 2, `audit_chain_fill` trigger + advisory lock) — `/api/audit/verify` → `{"ok":true,"rows_checked":>0}`; **superuser bypass detected**: disabling the guard + tampering yields `{"ok":false,"first_bad":1}`; restore → `ok:true`; guard re-armed. Limits, stated plainly: the chain covers `id/action/resource_type/resource_id/details/source` — not `user_id` or `created_at`; the verifier now **fails closed** (500) if its query errors instead of reporting `{"ok":true,"rows_checked":0}`; and `audit_log.user_id` is written as NULL by the SDK, with the actor kept in `details` |
| 8 | Core traits compiled as `adjutant-sdk` | Unchanged; M2 added `Event.id`, `HostEvents::replay`, `SqlValue` to the SDK, core still implements `HostDb`/`HostEvents` |
| 9 | Compiles, tests pass, clippy clean | clippy: **0 warnings**; tests: **28/28** at this commit (52 workspace-wide today) |

## The hard bug of this milestone: `raw_sql` is not provably `Send`

**Symptom:** every axum handler that awaited `load_all` → `run_migration`
failed with `Handler<_, _> is not satisfied`, while identical-looking handlers
worked. The suggested `#[axum::debug_handler]` doesn't exist in axum 0.8.9.

**Bisect (probe handlers, registered then removed):**

| Probe | Body | Result |
|---|---|---|
| A | `query_as().fetch_optional(pool)` | ✅ compiles |
| B | `run_migration(...)` | ❌ Handler unsatisfied |
| C | `pool.acquire()` + parameterized query on held conn | ✅ compiles |
| D | `raw_sql(...).execute(&mut *conn)` | ❌ Handler unsatisfied |
| E | `Executor::execute(&mut *conn, raw_sql(...))` | ✅ compiles |

**Root cause:** `RawSql::execute` is an `async fn` generic over `E: Executor<'e>`.
rustc cannot prove that future `Send` (it reports *"implementation of `Executor`
is not general enough"* — the HRTB limit), so the enclosing handler future fails
axum's `Send` bound. `Executor::execute` returns a concrete
`BoxFuture = Pin<Box<dyn Future + Send>>`, which is trivially provable.

**Fix:** in `db.rs::run_migration`, call `Executor::execute(&mut *conn, sqlx::raw_sql(&script))`
directly. Documented in-place. **Rule: inside code reachable from a handler,
use `Executor::execute` for multi-statement SQL, never the `async fn` wrapper.**

Two related `!Send` traps found in the same hunt:
- A `libloading::Symbol` kept in scope across an await poisons the generator —
  now scoped to a block that ends before the first `.await`.
- A `slice::Iter<'_, Permission>` held across an await does the same permission
  rows are owned and consumed by value.

## Other findings

- **SEED had become multi-statement.** `sqlx::query` prepares a single statement
  (`cannot insert multiple commands into a prepared statement`) after adding the
  `core:admin` seed — split into `SEED_ROLES` + `SEED_PERMS`.
- **`core:admin` gating works without an auth plugin:** `chief` gets every
  permission via the post-load bootstrap grant (including `core:admin`);
  `scout` gets 403 on admin endpoints, anonymous gets 401.
- **Uninstall ↔ reload contract:** `uninstalled=true` rows are skipped at load
  *before* side effects; clearing the flag + reload reinstalls — routes and
  subscriptions come back, data never left.

## Probe artifacts

`scripts/probes.py` (committed) regenerates the four batches and writes
`docs/evidence/m2_probes.json` (committed). The old `m2_probes_*.json` files were
gitignored, which is why this milestone's evidence could not be checked by anyone
cloning the repo — that is the defect this section now closes.

Defects found by re-running the ladder (all fixed in the audit pass):

- **Reinstall was broken.** Uninstall set `enabled=false` in `core.plugins`, and
  the reload path reads `enabled` from that row — so clearing `uninstalled` and
  reloading loaded the plugin into a *disabled* state (routes 404) with nothing in
  the docs to say so. Uninstall no longer touches `enabled`.
- **Anonymous plugin inventory.** `GET /api/plugins` (and `/api/events/recent`)
  were ungated, exposing the full route/permission table to anyone; both now
  require `core:admin`, and the harness authenticates as its mock chief.
- **Audit verify failed open.** A query error returned `{"ok":true,"rows_checked":0}`;
  it now answers 500.
- **Lifecycle audit rows lost their actor.** `enable/disable/uninstall/reload`
  resolved identity from dev headers instead of the identity hub; all four now use
  the same resolution as request dispatch.
- **`x-forwarded-for` was trusted unconditionally**, letting any client reset its
  own rate-limit window; the header is now honoured only from a configured
  trusted proxy.

## Next

Milestone 3 (SDK v0.1 + first plugins): auth plugin replaces the
`x-dev-user`/`x-dev-role` stub, membership plugin — both built with the SDK to
dogfood it, per SPEC §5.2a.
