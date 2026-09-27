# Milestone 6 — Finance + Equipment + Archive + Conflicts + Announcements

SPEC §15 M6 is the operational batch: the plugins a troop touches every day
(money, gear, announcements) and the two that carry the Accords into software —
searchable history, and the conflict pathway the Accords define. It is also the
milestone where the plugin API stops being a promise and becomes eleven plugins
built against it.

**Status (2026-09-26, `testing` at `150d024`):** five of the seven exit boxes are
met, one is partial and one is not delivered — **SDK v0.3** (not) and
**announcements' delivery** (partial, deferred by decision, #46). The eleven
plugins exist, load, and are probed. The figures in the table below are that
day's run and are left as recorded; for scale today, **fourteen** libraries are
staged (thirteen plugins plus `hello`), the route ladder reports **145/145** and
the workspace suite **564 passed / 0 failed**. The milestone's *client* intent is
now largely in the app — dues, equipment, announcements and governance all have
surfaces — but none of it has §1's third test yet, because no client path is
exercised against a live server in CI. See
[`../plugin-roadmap.md`](../plugin-roadmap.md) §3 and
[`../release-path.md`](../release-path.md) Stage 1.

> **Correction (2026-09-27).** Two of the sentences above are no longer true, and
> neither is being rewritten — a record's figures are dated
> (`plugin-roadmap.md` §7), so the current state is added beside them:
>
> 1. **SDK v0.3 is delivered**, after this record was written. `permissions!` and
>    `migrations!` are in `plugins/sdk/src/lib.rs` with compile-time traps at the
>    invocation, `adjutant-conflicts` is converted as the proof (its migration
>    versions, names and SQL byte-identical, so no database re-runs anything), and
>    `testing::assert_routes_gate_declared` covers the invariant the macros cannot
>    express. Additive: `SDK_ABI_VERSION` remains **4**. So M6 has **one** box
>    open — announcements' delivery — not two.
> 2. **No client path is exercised against a live server in CI** was true when
>    written and is now false: `scripts/client-live-harness.sh` is a step inside
>    `verify` and reported `🎉 10 tests passed` against a booted server with the
>    dev-header stub off, on `testing` at `75981e5` (run `36288096546`). The
>    workspace suite on that head is **573 passed / 0 failed / 65 ignored**, and
>    M5 now has a record of its own:
>    [`M5-mcp-and-flutter-mvp.md`](M5-mcp-and-flutter-mvp.md).

## Goal

> Build the operational plugins that scouts use daily, and the two that carry the
> Accords — searchable history and the conflict pathway. (SPEC §15)

## Exit criteria (SPEC §15 M6)

- [x] **Finance plugin** — fund tracking, transaction recording, budget vs.
  actuals, sliding-scale dues (`finance.*`, 19 routes, 6 permissions).
- [x] **Equipment plugin** — inventory, checkout/checkin, maintenance schedules
  (`equipment.items`, `equipment.checkouts`, 16 routes, 4 permissions).
- [x] **Archive plugin** — Congress proceedings, minutes, full-text search,
  timeline, decision → policy → mission tracking (`archive.*`, 13 routes, 5
  permissions).
- [x] **Conflicts plugin** — staged pathway, case management, stage transitions,
  anti-dropout nudges (`conflicts.cases`, `conflicts.stage_log`, 11 routes, 4
  permissions).
- [~] **Announcements plugin** — creation, read receipts, categories, push
  notifications (`announcements.*`, 12 routes, 4 permissions). Three of the four
  are built; **delivery is deferred**, deliberately and visibly — see
  [Not delivered](#not-delivered-at-this-milestone).
- [x] **SDK v0.3** — permission macros, migration helpers. **Delivered
  2026-09-27**, after this record was written: `permissions!` and `migrations!`
  in `plugins/sdk/src/lib.rs`, `adjutant-conflicts` converted as the proof,
  `SDK_ABI_VERSION` still 4. See the correction above.
- [x] **All plugins built with the SDK, validating the API** — twelve libraries
  in the workspace (the eleven plugins plus `hello`), every one accepted by
  `validate-plugin` and probed by `test-plugin`.

The deliverable's "eleven plugins covering the full operational scope" now
exists: auth, membership, missions, governance, calendar, mcp, finance,
equipment, archive, conflicts, announcements.

## Evidence

Recorded 2026-09-25 on `testing`, PostgreSQL **18**.

| Gate | Result |
|---|---|
| `cargo test --workspace` | **435 passed / 0 failed** |
| `cargo test -p adjutant-server --test host_db --test pool_lifecycle -- --ignored` | **15 passed / 0 failed** (against a real database) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **0 warnings** |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| `adjutant bootstrap-isolation --plugin-dir plugins-built` | 12 plugin roles, 12 schemas |
| `adjutant validate-plugin` (every library) | **12/12 accepted** |
| `adjutant test-plugin` (12 plugins staged, pristine DB) | **120/120 probes passed** (90 capture routes skipped, not probed) |
| `python3 docs/e2e_m3.py` (live server, dev headers **off**) | **52/52 probes passed** |
| `python3 docs/e2e_m4.py` (live server, dev headers on) | **64/64 lifecycle probes passed** |
| Client: `flutter analyze` / `flutter test` / `flutter build web --release` | clean / **24 passed** / built |

The five plugin migrations were applied by the plugin roles themselves on first
load (recorded in `core.schema_migrations`), and the live route ladder probes
every non-capture route with the real gate in front of it.

## What the milestone decided (and why)

**A conflict case is private by its own visibility list, not by role.** The
pathway needed the strongest statement in the system, so `conflicts` has no
`conflicts:read` at all: `read_own` admits a caller to the route and the handler
decides per case, against `party_ids` and `facilitator_ids`. `conflicts:manage`
staffs a case without reading it, and a manage holder cannot appoint themselves
as its facilitator. A case is deliberately not modelled as a troop or Lodge
scope, because "the troop" is exactly who must not see it.

**Immutability that is enforced rather than promised.** The archive refuses
`UPDATE`/`DELETE` on `records` at the database level (a `BEFORE UPDATE OR DELETE`
trigger), and a correction is a new record that supersedes the old one. The
conflict ledger enforces the same property on `stage_log`. In both cases the
missing route is good manners; the trigger is the guarantee.

**The books cannot drift.** Money is an integer number of cents and there is no
float in finance at all; a balance is derived from the ledger rather than stored;
and a transfer is two legs written by one statement, because the host mediates
every call and a plugin cannot open a transaction across them.

**An item is one physical thing.** Three identical tents are three rows, which is
what makes "who has the good tent" and "which tent came back damaged" answerable.
Checkout/checkin is a state machine enforced twice — a `409` naming the holder,
and a partial unique index as the backstop a race cannot get past.

**Announcements does not pretend to deliver.** No push provider is wired anywhere
in Adjutant, so the plugin records the announcement, publishes
`announcement.published` with everything a sender would need, and says
`"delivery": "deferred"` rather than "sent". The core now records notifications
per recipient (`core.notifications`, #46 slice 1), but that plugin and this one
are not wired together: an announcement is addressed to a scope, a notification to
one user. See `docs/design/notifications.md`.

**A plugin that needs host data asks for a method, not a wider grant.** Two read
routes in `announcements` resolved their audience by querying
`core.role_permissions` directly and answered `500` — a plugin role has no
privileges on that table, and it should not. The choice was a `core_grants` entry
or asking properly; a table-level `SELECT` would hand a plugin the whole
role → permission map for the troop in order to answer a question about the
caller's own roles. The SDK grew a **batched permission lookup**
(`PermissionService::scopes_for`) instead — one round trip, no grant, additive
(no ABI change), and it answers only from the grants the caller already holds, so
asking cannot widen reach.

**Namespaces match the plugin id.** `announcements` was the one plugin whose
permissions read `announcement:*` (singular). Renamed to `announcements:*` to
match every other plugin; pre-release, with no users, so the break costs nothing
but a decision. `core.role_permissions` rows under the old ids are inert and can
be deleted:

```sql
DELETE FROM core.role_permissions WHERE permission_id LIKE 'announcement:%';
DELETE FROM core.permissions      WHERE id            LIKE 'announcement:%';
```

## Scope notes

- **Authority is troop-scope for the batch**, except where the domain says
  otherwise: conflicts is object-level (standing in a case), and announcements
  distinguishes *authority* (coverage, as everywhere) from *audience* (the
  caller's own grants, resolved in one batched lookup, so a Lodge reader does not
  see a troop-wide announcement).
- **CI now stages and validates every library.** Before this milestone the plugin
  directory and the `validate-plugin` list named only the five original plugins,
  so the CI ladder never probed calendar, mcp or anything from M6 — which is how
  two `500`s could sit on `testing` unnoticed. Both lists are current.
- Deposits, equipment and archive carry no Lodge-scoped authority beyond what
  their domains state; where a route is destructive the SDK requires a
  troop-covering grant regardless of the constructor used.

## Not delivered at this milestone

Two exit criteria are not met. Neither is hidden by the checkboxes above.

1. **SDK v0.3 (permission macros, migration helpers) — delivered 2026-09-27, after
   this section was written.** `permissions!` and `migrations!` are in
   `plugins/sdk/src/lib.rs`; `adjutant-conflicts` is converted as the proof, with
   its migration versions, names and SQL byte-identical so no database re-runs
   anything; `SDK_ABI_VERSION` stays 4, and the SDK's version moves with the
   release rather than with the macros. The question this section posed — land in
   M7, or amend SPEC to drop the criterion — is therefore moot: it landed in M6.
   The paragraph below is kept as written, because a record's figures are dated.
2. **Push notification delivery.** Deliberately deferred, not overlooked: the core
   had no delivery channel (`docs/design/bg.md` §4 names it the missing
   capability), so `announcements` builds the seam and stops at it. The core now
   has the *record* half — per-recipient notifications with a per-user read state
   (#46 slice 1, `docs/design/notifications.md`) — but the transports (Web Push,
   device push, email) are still deferred, and a delivery plugin subscribing to
   `announcement.published` is a later slice.

## How to reproduce the evidence

The ladder, in order. Each gate is captured to a file so a failure cannot hide
behind a pipeline's exit code.

```bash
export ADJUTANT_DATABASE_URL=postgres://$USER@127.0.0.1:55432/adjutant_dev
export ADJUTANT_TEST_DATABASE_URL=postgres://$USER@127.0.0.1:55432/adjutant_dev_test
export ADJUTANT_ALLOW_SUPERUSER=true

cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test -p adjutant-server --test host_db --test pool_lifecycle -- --ignored

# Stage every library in ONE pass, by name: a glob also picks up stale
# artifacts in target/debug, and mixing fresh with old makes the ABI handshake
# refuse libraries that look like plugin bugs.
rm -rf plugins-built && mkdir -p plugins-built && cp target/debug/libadjutant_*.so plugins-built/
./target/debug/adjutant bootstrap-isolation --database-url "$ADJUTANT_DATABASE_URL" --plugin-dir plugins-built
for so in plugins-built/*.so; do ./target/debug/adjutant validate-plugin "$so"; done
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin   # wipes *_test
```

The two e2e harnesses need **different servers**, which is the easiest way to
misread a green run as red:

```bash
# m4: dev headers ON, base URL from the environment.
ADJUTANT_PLUGIN_DIR=plugins-built ADJUTANT_DEV_HEADERS=true ADJUTANT_RATE_MAX=0 \
  ADJUTANT_ALLOW_SUPERUSER=true ADJUTANT_BIND=127.0.0.1:8799 ./target/debug/adjutant &
ADJUTANT_E2E_BASE=http://127.0.0.1:8799 python3 docs/e2e_m4.py

# m3: port 8787 is hardcoded in the script and it asserts the dev-header stub is
# OFF, because it proves the session/OIDC path rather than the stub.
ADJUTANT_PLUGIN_DIR=plugins-built ADJUTANT_DEV_HEADERS=false ADJUTANT_RATE_MAX=0 \
  ADJUTANT_ALLOW_SUPERUSER=true ADJUTANT_BIND=127.0.0.1:8787 ./target/debug/adjutant &
python3 docs/e2e_m3.py
```

Probe readiness on an **open** route (`GET /api/hello`); `/api/plugins` requires
`core:admin` and answers `401` forever, so a `curl -f` loop against it will skip
every script while the server is healthy.
