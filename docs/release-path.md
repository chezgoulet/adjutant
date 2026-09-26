# The release path — ordered work from `testing` to v1.0

**Status:** the plan of record as of **2026-09-26**, `testing` at `150d024`.
**Owner:** the same rule as [`plugin-roadmap.md`](plugin-roadmap.md) §7 — this
document is edited in the same PR that changes a stage's state.

[`SPEC.md`](../SPEC.md) §15 owns the milestones and their exit criteria;
[`plugin-roadmap.md`](plugin-roadmap.md) owns which plugins exist and in what
tier; [`releasing.md`](releasing.md) owns how a release is cut. **This document
owns the order**: what to do next, what proves it, what it closes, and what it is
waiting on.

---

## Where the work starts from

Not from zero, and the difference matters for sequencing:

- `testing` is 122 commits ahead of `main` (`ed2cb24`, tagged **v0.2.0**), green
  on fifteen consecutive CI runs. `main` has not moved since the release.
- The `verify` job is 32 steps and every number below is a log line from it:
  build, clippy `-D warnings`, docs `-D warnings`, `cargo-deny`,
  **564 unit and integration tests**, eight DB-backed probe suites that connect
  *as each plugin's own database role* (host/pool-lifecycle, notifications,
  stripe and store outbox, store catalogue, finance routes/receipts/dues), the
  M1+M2 ladder **67/67**, the route ladder **145/145**, `e2e_m3.py` **52/52** (dev
  headers off), `e2e_m4.py` **64/64**, the WASM guest, the scaffolder, and
  `validate-plugin`. A separate `client` job runs `flutter analyze` and **104
  tests**; an `msrv` job holds the declared 1.96 floor.
- Fourteen plugin libraries (thirteen plugins plus `hello`) are staged and probed.
- Records exist for M1, M2, M3, M3-S, M4, M6. **M5, M7 and M8 have no record file
  yet** — their criteria are SPEC §15's, and this document is where the work
  against them is ordered.

**The standing rule.** Every item below lands the House way: a branch from
`testing`, a PR, green CI, merged. Evidence is a log line or a run id, never a
narration. A local green is not the branch's gate.

---

## Stage 0 — the gate's own honesty (do first; it is small)

Two items, because they protect everything after them and both are one sitting:

1. **#76 — pin the Flutter version in CI.** The `client` job follows the stable
   channel, so a local green can differ from the branch's gate for reasons nobody
   chose.
2. **#83 — the scheduler flake** (`host_db::probe_scheduler_runs_records_and_stops`).
   A gate that needs a re-run teaches people to wave red through. This is a work
   item, not noise.

**Proves it:** `testing` green without a re-run for a full day, twice over.

---

## Stage 1 — the day-one release (M5's remainder)

**Exit criterion:** SPEC §15 M5, all eight boxes. This is the shortest path to a
troop running on the software, so it goes first.

1. **The client-in-CI harness — the missing half of the v1.0 gate.** §1 test 3
   requires the path to be "exercised by the fresh-machine harness in CI". Today
   the client's tests run against a **mock** HTTP client and the end-to-end
   harnesses drive the API, not the app: **no client path is exercised against a
   live server in CI at all.** One harness — the Flutter app driven against a
   booted server, on the existing `adjutant test-plugin` rail — converts every
   client-covered plugin's "usable" claim from a demonstration into a gate. It is
   the single highest-leverage item on this page, and it makes Stage 5.1
   mechanical.
2. **The MCP connect-and-interact proof** (M5 box 2). The plugin loads and is
   tested; what is missing is a real Hermes agent connecting through it with
   permissions filtered and invocations logged.
3. **Offline mode** (M5 box 5) — the client caches locally and syncs. The largest
   single client item on this list, and it needs one decision: local storage
   (drift/sqlite) or a plain cache. Recommend deciding before starting rather than
   discovering it mid-flow.
4. **Android, iOS, Web** (M5 box 4). Web builds in CI today and Android reaches
   the 161st through Obtainium; **iOS needs a decision** — a build host or a dated
   deferral recorded in `plugin-roadmap.md` §3, not silence.
5. **The UI/UX review** (M5 box 7). Christopher authors it, as with the other
   deep review docs; the 48 dp finding that put Governance in Settings rather
   than a ninth navigation destination is its first recorded entry.
6. **Deploy to The House for the 161st** (M5 box 8). Blocked on Stage 3's TLS
   (#49) and restore drill (#54), or on an explicit accepted-risk decision. Also
   needs the host named — Thelio or the NAS — and an answer on whether a second
   idle/idle-capable service is wanted at all.

**Proves it:** the eight boxes checked in a new `M5` record, each with its run id,
and the client harness green on `testing`.

---

## Stage 2 — M6's open boxes

1. **SDK v0.3** — permission macros and migration helpers (SPEC §15 M6). The SDK
   is the product; this is the last additive step before the 1.0 freeze, and it
   carries the version move (Stage 5.4).
2. **Announcements delivery** — the one M6 box marked partial. Blocked on the
   decision in **#46**: which channel (email, push, both), and to whom. Wiring the
   plugin to `core.notify` is small once that is decided; until then "records, does
   not deliver" is the honest state and the plugin already says so.
3. **The third test for the M6 batch** — `finance`'s treasurer view (ledger,
   budgets) and `archive`'s search and timeline are the two client surfaces that
   remain from #56. `conflicts` needs a dated exception in `plugin-roadmap.md` §3
   (a private case between two people may belong in a conversation, not a screen).
   `equipment`, `announcements` and the member half of `finance` already have
   surfaces and only need Stage 1.1 to make them gated.

**Proves it:** M6's boxes all checked or explicitly excepted, and the surfaces
covered by the Stage 1.1 harness.

---

## Stage 3 — M7 production hardening (the gate on a real deployment)

SPEC's eight boxes: two are met (**Docker Compose**, **CI/CD**); six are open
issues. Order within the stage is by dependency, not by size:

1. **#49 — HTTPS/TLS** for a self-hosted deployment. Nothing else in this stage
   can be exercised by a real user before this, and it is what unblocks Stage 1.6.
2. **#54 — backup and restore, proven by a drill.** A backup nobody has restored
   is a hypothesis. This pairs with #49 as the deployment gate.
3. **#51 — the security audit** (OWASP Top 10 + dependency scanning). The House's
   `aegis-*` audit modules are the tool; run it **after** Stage 4, so the audit
   measures the shape that ships rather than one about to change.
4. **#50 — performance at 100+ concurrent users.**
5. **#52 — the admin guide and the user guide.** The user guide is also the
   dependency of the community plugin guide in Stage 5.
6. **#53 — CONTRIBUTING.md, code of conduct, PR process.** Small, but it is a
   precondition of Stage 5.5's community contribution, not a courtesy.

**Proves it:** M7's eight boxes checked, with the drill transcript and the audit
report committed as evidence.

---

## Stage 4 — the integrity residuals

Each is small, and each is a correctness or honesty defect rather than a backlog
wish. None of them should be carried into a release as a "known issue" while it is
an afternoon's fix.

- **#78 — the notification record's identity fields are mutable by whoever holds
  `UPDATE`.** The sharpest of these: an integrity hole in what we just shipped.
- **#72 — the receipts read routes' ownership branch is proven only by the mock
  host.** A gate claiming more than it proves.
- **#71 — a correction's carry-over claim is wrong for `tax_statement` and
  `issued_on`.** A document that overstates.
- **#76 and #83** move to Stage 0 because they protect the gate itself.

**#74** (a receipt does not yet follow an online payment automatically) is *not*
on this list: it is a feature, and it belongs with Stage 2's finance work.

---

## Stage 5 — M8, v1.0

1. **§1's three tests for all eleven Tier A plugins.** Test 1 and the test-2
   coverage clauses are met by every plugin today; the gaps are test 2's
   `>= 1.0.0` version bar, test 3's CI-exercised path (Stage 1.1), and the two
   missing client surfaces (Stage 2.3). The plugin versions move with the release.
2. **SDK v1.0 published** — **blocked on a decision only the owner can make: the
   crates.io token.** This is the highest-leverage open decision in the project,
   because SPEC M3's "the SDK is the product" claim and M8's community-contribution
   criterion both sit behind it. Nothing else on this page is blocked by a
   decision that is this cheap to make.
3. **The documentation site live**, the **community plugin guide published**, and
   the **F-Droid listing** — the last needs the privacy and licence pass
   (`app-privacy-policy-authoring` exists for exactly this; the client already has
   an Obtainium channel and no Play listing).
4. **3+ troops using Adjutant in production** and **1+ community plugin**. Both are
   downstream of 1–3 and both need the wording question settled first: "three
   troops we know" or "three independent operators" (`plugin-roadmap.md` §6.3).

**Proves it:** a `M8` record with the three tests per plugin, the published crate,
the live docs, and the two non-code criteria evidenced rather than asserted.

---

## The release decision

- **Interim — recommend cutting `v0.3.0` ("day one") from `testing` when Stage 1
  passes.** `main` has been still while 122 commits accumulated, and the 161st
  should run a tag rather than a branch. The M5 record is its release note.
- **Final — `v1.0.0`** when Stage 5's list is done. See
  [`releasing.md`](releasing.md) for the mechanics.

## Decisions only the owner can make

These block work; they are not preferences to be inferred:

1. **The crates.io token** (Stage 5.2) — gates SDK publication, the community
   plugin criterion, and the "SDK is the product" claim.
2. **v0.3.0 interim release, or hold for v1.0?**
3. **Announcements: the plugin owns delivery, or the client does** (#46,
   `plugin-roadmap.md` §6.1) — gates the channel choice and the wiring.
4. **`conflicts` in the client: build it, or record a dated exception?**
5. **iOS: a build host, or a dated deferral?**
6. **Tier B sizing (Meshcore, ATAK) and the meaning of "3+ troops"**
   (`plugin-roadmap.md` §6.2, §6.3).
7. **The deployment host and its TLS path** (#49) — Thelio or the NAS, and where
   the certificate comes from.

## Lanes (how to run it without colliding)

Four lanes run concurrently; they touch different files, so they can go in
parallel while every PR passes the same gate. The only hard sequence is inside a
lane.

- **A — client.** Stage 1.1 harness first (it gates everything else's claim), then
  `finance`'s treasurer view, then `archive`, then offline mode.
- **B — core and SDK.** Stage 0's #76 and #83, then SDK v0.3, then #78, #72, #71.
- **C — ops and deployment.** #49 TLS, then #54 restore drill, then #51 audit,
  #50 performance, #52 docs, #53 process.
- **D — integration.** The MCP↔Hermes proof, then the announcements/#46 decision
  and wiring, then the client-in-CI harness's server side if it lands there.

Merge discipline, unchanged: one concern per PR; merge `testing` into the branch
before opening it; a red check is worked, not re-run past — and if it is a flake,
it is filed with the same-SHA contrast that proves it.
