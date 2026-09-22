# Milestone 3 — SDK v0.1 + First Plugins

**Status:** complete (pending crates.io publication of the SDK — see exit criteria)
**Branch:** `feature/m3-sdk-and-plugins` (stacks on `feature/m2-core-server` / PR #2)

## Gates (all re-run on the final commit)

| Gate | Result |
|---|---|
| `cargo build --workspace` | 0 errors |
| `cargo test --workspace` | **56/56 passed** (2 are DB-backed and print SKIPPED without `ADJUTANT_TEST_DATABASE_URL`) |
| `cargo clippy --workspace --all-targets` | **0 warnings** (independently re-verified on a from-scratch build) |
| `adjutant test-plugin` | **36/36 probes passed, 3 skipped** (skips are open mutating routes — never executed, and no longer counted as passes) against a fresh test DB |
| `docs/e2e_m3.py` (live, dev headers OFF) | **49/49 probes**, repeatable (run twice back to back, exit 0) |
| `scripts/probes.py` (M1+M2 batches) | **63/63 probes**, committed transcript |

Counts changed in the 2026-09-22 audit pass because the harnesses were made
honest, not because the software moved: `docs/e2e_m3.py`'s `probe()` used an
`elif`, so every body assertion was skipped whenever the status matched (17
probes asserted a status code only); `test-plugin` counted three never-executed
"skipped" entries as passes and asserted only `!404 && !500` for its chief pass.

**The E2E harness now verifies its own premise.** Probe 0 sends dev-stub headers
and requires a 401/403 — i.e. it proves `ADJUTANT_DEV_HEADERS=false` instead of
assuming it. (The identity claims were sound by construction even before: the
harness never sends `x-dev-*` headers, so any identified request got its identity
from the auth plugin's session provider.)

Run it against a server started with `ADJUTANT_DEV_HEADERS=false` and, for a
back-to-back run, `ADJUTANT_RATE_MAX=0` — the harness issues ~55 requests per run
and would otherwise exhaust the 120/min window (it waits out one window and
retries rather than failing opaquely).

## What landed

**SDK (`adjutant-sdk` 0.1.0)**
- `HostHttp` — OIDC discovery/token exchange goes through the core's reqwest
  (a plugin linking its own copy would resolve an unset tokio reactor; same
  class of bug as plugin-side `sqlx` in M1).
- `PluginContext` gains `identity` and `http`; prelude exports `export_plugin`.
- Per-plugin schema contract documented and **implemented** (see bug 1).

**Identity seam (`server/src/identity.rs`)**
- Plugin-registered providers answer "who is this?" for every route in the core.
- Dev headers only as a gated fallback, and only when no provider claimed the
  request. Disabled/uninstalled plugins stop answering; hot-reload replaces the
  provider under its owner key without double-registering.

**Auth plugin** — local passwords (argon2), sessions (hashed tokens), role
assignment, `login`/`logout`/`me`/`register`/`users`, and OIDC:
`oidc/login` (state stored server-side, authorize endpoint from discovery) →
`oidc/callback` (code exchange, id_token verified HS256 or RS256-via-JWKS,
iss/aud/exp enforced) → session. Registers the core's identity provider.
`verify_id_token` now has direct unit tests (valid/expired/wrong-iss/wrong-aud,
plus RS256-through-JWKS with a committed test key) — the old test round-tripped a
token through `jsonwebtoken` and never called the function that decides trust.

**Membership plugin** — roster, lodges, patrols, proficiencies (+ completion
sign-off), stewards, member detail with proficiency/position aggregates, and
OSG CSV import (upsert by username, patrols auto-created, bad rows reported
rather than dropped).

**CLI** — `new-plugin` (scaffolds a plugin crate, manifest, migration, route,
and workspace member entry) and `test-plugin` (isolated `_test` database, mock
permissions, route probe ladder, graceful shutdown).

## Bugs found and fixed

Found by running the live E2E, not by unit tests. Listed in the order they
blocked the suite — the first one is the reason the earlier "37/37 test-plugin
pass" was misleading.

1. **`search_path` was never applied at runtime (systemic).** The SDK documents
   that the core pre-sets `search_path` so plugin queries can use bare table
   names; migrations did this, but runtime queries ran on a shared pool whose
   connections sat on `public`. Every unqualified plugin table was invisible:
   `relation "members"/"lodges"/"oidc_states" does not exist`. Fixed by giving
   each plugin a schema-scoped `CoreDb` (`CoreDb::for_plugin`) that runs each
   call inside a transaction with `SET search_path` — so a failed call cannot
   leak a search_path into the next plugin's queries.
2. **`text[]` columns decoded to `Null`.** `decode_value` tried jsonb → bool →
   i64 → f64 → string, so `ARRAY(SELECT role_id …)` matched nothing and returned
   null. Every role list in the system was silently empty, which made
   permission checks and admin routes fail for reasons nothing reported.
3. **`SdkError::BadRequest` mapped to 500.** The dispatch error arm sent every
   plugin error as an internal error; a wrong password is a 400.
4. **OIDC authorize URL was built from the discovery-path helper**, producing
   `…/.well-known/openid-configuration?response_type=code…`. Per OIDC the
   authorize endpoint comes from the discovery document; the URL builder is now
   a tested pure function (`discovery_url`).
5. **Two statements in one prepared statement** (`INSERT … ON CONFLICT; DELETE
   …`) — sqlx refuses; split. Same trap as the M2 SEED bug.
6. **`make_interval(hours => $3)`** with a bigint bind — PostgreSQL wants `int`;
   cast added (`$3::int`, `auth/src/lib.rs:193`). The same entry originally
   claimed register "now answers 409 'already registered — log in instead'",
   which was **not true of the code at the time** — no 409 and no such string
   existed, so a duplicate username surfaced as a 500. The claim is now true: a
   unique violation (`23505`) is mapped to 409 with that message on
   `POST /api/auth/register`, and to `409 username "x" is already taken` on the
   admin `POST /api/auth/users` route.
7. **Membership SQL typos**: `EXCLUDED.patrols_lodge` (no such column),
   `COALESCE($8, true)` where `$8` binds as text-null (needs `::boolean`), and
   `WHERE m.id = $1` binding a bigint id as text.
8. **IdP users had no local username**, making them unreachable to
   username-keyed admin routes (role grants, user list). OIDC users now get
   `preferred_username` → email local-part → subject, sanitized and
   deterministically disambiguated on collision.
9. **A plugin library could unload while the core still held trait objects into
   it** (identity providers registered during `init`). On a load *error* the
   library was dropped, and dropping the hub's provider afterwards called into
   unmapped code — SIGSEGV. Libraries are now parked for the process lifetime.
10. **`test-plugin` hung on shutdown**: reqwest keep-alive connections kept
    `axum::serve`'s graceful drain waiting. The harness now aborts the task
    (the production path still drains gracefully, proven in M2).

## Harness pitfalls worth remembering

- `pkill -f '<pattern>'` matches the shell running it — it killed its own cell
  mid-command, which is why a DB reset silently never ran. Use `pkill -x`.
- `DROP DATABASE` blocks while a server holds pooled connections: kill, then drop.
- `TRUNCATE` without `RESTART IDENTITY` leaves sequences climbing, so a
  repeatable test that asks for `id=1` fails on the second run.
- Never `cp` over a `.so` the running process has mapped — write a temp file and
  rename. Overwriting in place segfaulted the server (exit 139).

## Exit criteria

- [x] `adjutant-sdk` crate at v0.1 (workspace version 0.1.0) — **crates.io
      publication pending**: it needs a registry token/account decision from
      Christopher, so it is not claimed as done
- [x] `adjutant new-plugin` scaffolds a plugin project (manifest, routes,
      models, migrations) — verified live: the scaffold was generated into a
      scratch copy of the repo, compiled (`cargo build -p adjutant-gear_locker`),
      loaded by the real server, and exercised end to end:

      ```
      GET  /api/gear_locker/health   -> 200 {"ok":true,"plugin":"gear_locker"}
      GET  /api/gear_locker/items    -> 401 anonymous, 200 {"items":[]} as chief
      registry                       -> 2 routes, [gear_locker:read, gear_locker:manage]
      core.schema_migrations         -> gear_locker:0:create_schema, gear_locker:1:initial_schema
      SELECT count(*) FROM gear_locker.items -> 0   (bare name resolved via search_path)
      ```

      The scaffold-compiles step now runs in CI so the criterion cannot regress.
      (The old evidence for this line was a unit test asserting the generated
      strings, which never compiled anything.)
- [x] `adjutant test-plugin` runs a test server with mock permissions and a
      test database (36/36 probed, 3 open mutating routes reported as skipped)
- [x] Auth plugin: OIDC login, session management, role enforcement — all via
      the SDK (proved end-to-end against a mock IdP)
- [x] Membership plugin: roster, OSG CSV import, proficiency tracking — via the SDK
- [x] Both plugins load, enable, disable, and uninstall through the core —
      evidenced by e2e probes 39–48 for membership (disable → 404, auth keeps
      serving, enable → 200, uninstall → rows survive, clear flag + reload →
      route live again). **Auth's lifecycle is not exercised end-to-end on
      purpose:** with the dev-header stub off, the auth plugin is the only way to
      authenticate, so disabling it locks the operator out of the admin routes
      until a restart. That is a real operational hazard, documented in README,
      not something the harness should paper over.
- [x] Both plugins' routes enforce permissions correctly (with the stub off)
- [x] Both plugins' schemas are separate and migrations run cleanly
      *(reworded in the audit: nothing enforces isolation beyond a per-call
      `search_path`, and the shipped plugins deliberately write `core.*`
      tables — see `server/src/host.rs:92-106`)*
- [x] **SDK verdict: the API held up.** Building auth and membership through it
      was not painful — the identity seam, `ctx.db`/`ctx.http`, and
      `route_handler` covered everything these two plugins needed, and the one
      real friction point (per-plugin `search_path`) was a core bug, not an SDK
      design flaw. No redesign required before M4.

## Audit pass (2026-09-22) — what else changed

An adversarial review of the milestone docs, harnesses and test suite produced
these fixes (all verified by the gates above):

1. **`docs/e2e_m3.py` body assertions were dead** whenever the status matched
   (`elif` in `probe()`). Fixed; ~17 probes now check what their names claim.
2. **The re-import probe could not detect duplicates** (`updated + created == 3`
   passes for a duplicating importer). It now asserts row identity.
3. **The dev-headers precondition is asserted, not assumed** (probe 0).
4. **Missing lifecycle evidence** for the criterion above (probes 39–48).
5. **Test-plugin inflated its gate**: three auto-pass "skipped" entries are
   reported separately; the probe authenticates for the now-admin-gated
   `GET /api/plugins`.
6. **Four tests that could not fail** were removed (`assert_eq!(Null, Null)`, a
   bind test that asserted only the SQL string, two registry tests against an
   empty placeholder). Replaced by: a DB-backed `decode_value`/`bind_params`
   regression test that provably catches the `text[]`→`Null` bug (verified by
   reverting the fix in a scratch copy), and real registry tests covering
   Found/Disabled/NotFound, enable/disable, uninstall and reload generation swap.
7. **The OIDC verifier is now tested directly**, including the RS256/JWKS branch
   that the mock IdP never exercised.
8. **Audit verify fails closed**; lifecycle audit rows resolve the real actor.
9. **`GET /api/plugins` and `/api/events/recent` are admin-gated** (they exposed
   the full route/permission table anonymously).
10. **`x-forwarded-for` is only trusted from `ADJUTANT_TRUSTED_PROXIES`** —
    otherwise any client could reset its own rate-limit window.
11. **Reinstall actually reinstalls**: uninstall no longer clears `enabled`, so
    clearing `uninstalled` + reload brings the plugin back serving (this was
    broken and the old probes recorded the failure without explaining it).
12. **The auth-disable lockout is now prevented, not just documented**: with dev
    headers off, disabling/uninstalling the only enabled identity provider is
    refused with `409` and a hint, because it would make every authenticated
    route — including the admin route that would undo it — unreachable until a
    restart.
13. **Path captures landed** (`/api/missions/{id}` → `PluginRequest::params`),
    with literal-before-template matching, load-time validation of malformed
    captures, unit tests, and a live probe through the hello plugin's new
    `GET /api/hello/greetings/{id}` route. This is the routing shape M4's
    missions/governance plugins need; it arrived ahead of the SDK v0.2 bucket
    because it is a prerequisite, not a nicety.
14. **CI now enforces the gates** (`.github/workflows/ci.yml`): build, clippy
    with `-D warnings`, unit + DB-backed tests, the M1/M2 probe ladder,
    `test-plugin`, and the end-to-end harness against a PostgreSQL service.

## Next

Milestone 4: missions + governance plugins, and SDK v0.2 (route/event helpers,
test-harness improvements — including a public `adjutant_sdk::testing` module so
plugin authors can unit-test handlers without a live core).
