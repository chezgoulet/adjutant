# Milestone 4 — Missions + Governance + SDK v0.2

**Status:** Complete · 2026-09-25 · branch `testing`
**Roadmap note:** SPEC §15's M4. Not to be confused with
[`M4-core-sdk-stabilization.md`](M4-core-sdk-stabilization.md), the inserted
stabilization gate the roadmap calls **M3-S** (§2 of
[`../plugin-roadmap.md`](../plugin-roadmap.md)).

## Goal

Expand the SDK and build the two plugins the Accords are made of: the mission
lifecycle (Art 8) and the troop's governance machinery (Art 5/9/12/17).

## Exit criteria (SPEC §15 M4)

| Criterion | Evidence |
|---|---|
| Missions plugin: full 6-stage lifecycle, mentor matching, Lodge Commander approval | `plugins/missions/` — 22 routes, 7 tables; `docs/e2e_m4.py` probes 01–21 walk request → review → approval → execution → debrief → report → completed, the approval guard, the mentor assignment and the ranked suggestions, the out-of-order refusals, and the Article 8 appeal |
| Governance plugin: motion lifecycle, voting, amendments, Accords versioning | `plugins/governance/` — 28 routes, 6 tables; probes 22–53 cover a second, debate, per-voter votes, quorum, a tally, implementation, friendly and formal amendments, minutes, and version N from a passed Congress motion |
| SDK v0.2: route helpers, event helpers, test harness improvements | `plugins/sdk/src/lib.rs` — `PluginRequest::{int_param, query_required, query_int, query_bool}`, `DbHandle::{query_one, exists}`, the `event_type` vocabulary + typed payloads and publishers, `TestRequest::identity_grants`, `MockDb::{queried_sql, assert_executed, last_execute_params, last_query_params}`, `MockEvents::{payloads, assert_published, assert_none}`. Additive: `SDK_ABI_VERSION` stays **4**. |
| Event bus integration: `mission.completed`, `motion.passed` | probes 54–58 read `/api/events/recent` and assert the payloads (`mission.completed` carries the mission, its stage and its impact; `motion.passed` carries the tally, the body and the threshold) |
| Both plugins built with the SDK, validating the API | Both are `adjutant-sdk` consumers with no `sqlx`/`tokio` linked; the SDK gained only what building them actually needed |

## Evidence

Recorded 2026-09-25 on `testing`, PostgreSQL **18.6**.

| Gate | Result |
|---|---|
| `cargo test --workspace` | **163 passed / 0 failed / 18 ignored** — auth 16, governance 21, hello 2, membership 12, missions 22, sdk 29 + 4 doc-tests, server 57; `host_db` 14 and `pool_lifecycle` 1 are the DB-gated `#[ignore]`d tests |
| `cargo test -p adjutant-server --test host_db --test pool_lifecycle -- --ignored` | **15 passed / 0 failed** (against a real database) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **0 warnings** |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| `adjutant validate-plugin` (missions, governance) | accepted |
| `adjutant test-plugin` (5 plugins staged, pristine DB) | **51/51 probes passed** (43 capture routes skipped, not probed) |
| `python3 docs/e2e_m4.py` (live server, real database) | **64/64 lifecycle probes passed** |

The migrations were applied by the plugin roles themselves on first load; the
resulting schemas are 7 tables in `missions` and 6 in `governance`
(`information_schema.tables`), recorded in `core.schema_migrations`.

## What the milestone decided (and why)

These are judgement calls the SPEC left open; they are behaviour now, so they are
written down rather than implied.

1. **A second is enough to vote; debate is optional.** `POST …/close` accepts a
   motion in `seconded`, `debate`, or `voting`. In a seven-scout patrol,
   "seconded, any discussion?, all in favour?" is the real flow, and a plugin
   that insists on a formal debate-opening step gets worked around.
2. **Quorum fails closed.** A meeting with no `expected_voters` and no fixed
   requirement has a required quorum of `0`, and `quorum_met` calls that *not
   met*. A meeting that silently has no quorum rule is how a body votes itself a
   mandate it does not have. Congress is `one_third_registered` (the rule the 3rd
   Congress locked); the Troop Council is `majority_members`.
3. **A vote is recorded once, by the database.** A unique index on
   `(motion_id, COALESCE(amendment_id, 0), voter)` means a re-vote is a `409`,
   not a silent replacement — a tally that can be rewritten after the fact is not
   a tally.
4. **Voting and seconding require presence.** A motion tied to a meeting is
   decided by the people in the room; a member not on the attendance list is
   refused with the route that fixes it.
5. **An Accords version comes from a passed Congress motion** (Art 17), and only
   one version per motion. A Troop Council motion that amends the Accords still
   needs the Congress, which is what the Accords say.
6. **Rejections carry guidance, and appeals need a seconder** (Art 8). A
   rejection without guidance is a `400`; an appeal without a seconding Council
   member is a `400`; overturning returns the mission to `execution` with the
   Council recorded as the approver.
7. **Friendly amendments are the mover's to accept; formal ones are voted and
   applied.** Either way the motion's text changes, because the record should
   show what was decided, not what was first proposed.
8. **`governance:manage` and `missions:mentor` extend SPEC §9.1's taxonomy.**
   Running a meeting (attendance, quorum, closing a vote, minutes, adopting a
   version) is not the same authority as proposing a motion; mentoring needs a
   permission a plain scout can hold without holding `missions:update` over every
   mission. Neither widens an existing permission.
9. **Neither plugin touches `core.*`.** They keep to their own schema, so neither
   appears in `core_grants` — the strictest possible answer. Display names are the
   client's business; the plugins store the opaque ids they are given.
10. **`mission.completed` fires when the approver signs the report off**, which is
    the end of the six-stage path — not at debrief, which is what the SPEC's one
    comment line (`"mission.completed — Mission debrief submitted"`) might suggest.
    A debrief that nobody accepted is not a completed mission.

## Scope notes

- Eight extra tables beyond SPEC §7.3/§7.4's required ones, each carrying a stated
  responsibility: `missions.{mentor_profiles, progress_notes, mission_appeals,
  mission_stage_log}` and `governance.{meetings, attendance}` (plus nothing else).
  The stage trail exists because "Approval and Consent (logged with TC)" is a
  requirement of the pathway (Art 8), and the audit log alone is not readable
  from inside the plugin's schema.
- `docs/e2e_m4.py` runs in CI as its own step (a second server with the dev-header
  stub ON). It is a *domain* ladder; M3's `docs/e2e_m3.py` remains the identity
  ladder.

## Not in this milestone

- **The third test in the v1.0 rule** ("usable by a scout without SQL or curl") —
  that needs the Flutter client (M5) and the fresh-machine harness.
- **The Archive plugin is not yet subscribed to `mission.completed` /
  `motion.passed`.** The events and their payloads are published and asserted; the
  consumer (M6 §7.8) is what makes the pair valuable, and it is the next milestone
  that reads the bus.
- **Notifications (#46)** — unaffected by this milestone.
- **Calendar's own quorum calculation (M5 §7.7).** Governance computes quorum from
  its own meetings; SPEC also gives the calendar plugin "quorum calculation for
  Congress". That duplication needs settling when M5 starts: the meeting (and its
  attendance) belongs to governance, so calendar should ask governance rather than
  keep a second tally.

## How to reproduce the evidence

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
mkdir -p plugins-built && cp target/debug/libadjutant_{hello,auth,membership,missions,governance}.so plugins-built/
ADJUTANT_DATABASE_URL=postgres://…/adjutant_dev_test \
  ./target/debug/adjutant bootstrap-isolation --database-url "$ADJUTANT_DATABASE_URL" --plugin-dir plugins-built
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
ADJUTANT_PLUGIN_DIR=plugins-built ADJUTANT_DEV_HEADERS=true ADJUTANT_RATE_MAX=0 \
ADJUTANT_ALLOW_SUPERUSER=true ADJUTANT_BIND=127.0.0.1:8788 ./target/debug/adjutant &
ADJUTANT_E2E_BASE=http://127.0.0.1:8788 python3 docs/e2e_m4.py
```
