# Milestone 3 — SDK v0.1 + First Plugins

**Status:** complete (pending crates.io publication of the SDK — see exit criteria)
**Branch:** `feature/m3-sdk-and-plugins` (stacks on `feature/m2-core-server` / PR #2)

## Gates (all re-run on the final commit)

| Gate | Result |
|---|---|
| `cargo build --workspace` | 0 errors |
| `cargo test --workspace` | **47/47 passed** |
| `cargo clippy --workspace --all-targets` | **0 warnings** |
| `adjutant test-plugin` | **37/37 probes** against a fresh test DB |
| `docs/e2e_m3.py` (live, dev headers OFF) | **38/38 probes**, repeatable (run twice, exit 0) |

The E2E harness is the milestone's real proof: the server runs with
`ADJUTANT_DEV_HEADERS=false`, so the spoofable `x-dev-user` stub is off and every
identity in the run comes from a real session issued by the auth plugin.

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
   cast added. The failure was also non-atomic (user row written, session not),
   so register now answers 409 "already registered — log in instead" for a
   duplicate username instead of a bare 500.
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
      models, migrations) — proven live with a throwaway fixture, then removed
- [x] `adjutant test-plugin` runs a test server with mock permissions and a
      test database (37/37)
- [x] Auth plugin: OIDC login, session management, role enforcement — all via
      the SDK (proved end-to-end against a mock IdP)
- [x] Membership plugin: roster, OSG CSV import, proficiency tracking — via the SDK
- [x] Both plugins load, enable, disable, and uninstall through the core
- [x] Both plugins' routes enforce permissions correctly (with the stub off)
- [x] Both plugins' schemas are isolated; migrations run cleanly
- [x] **SDK verdict: the API held up.** Building auth and membership through it
      was not painful — the identity seam, `ctx.db`/`ctx.http`, and
      `route_handler` covered everything these two plugins needed, and the one
      real friction point (per-plugin `search_path`) was a core bug, not an SDK
      design flaw. No redesign required before M4.

## Next

Milestone 4: missions + governance plugins, and SDK v0.2 (route/event helpers,
test-harness improvements).
