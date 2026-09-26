# The release path — ordered work from `testing` to v1.0

**Status:** the plan of record as of **2026-09-26**, `testing` at `c5698f5`.
**Owner:** the same rule as [`plugin-roadmap.md`](plugin-roadmap.md) §7 — this
document is edited in the same PR that changes a stage's state. The seven owner
decisions it is built on are recorded in that document's §6.

[`SPEC.md`](../SPEC.md) §15 owns the milestones and their exit criteria;
[`plugin-roadmap.md`](plugin-roadmap.md) owns which plugins exist and in what
tier; [`releasing.md`](releasing.md) owns how a release is cut. **This document
owns the order**: what to do next, what proves it, what it closes, and what it is
waiting on.

---

## Where the work starts from

Not from zero, and the difference matters for sequencing:

- `testing` is 124 commits ahead of `main` (`ed2cb24`, tagged **v0.2.0**) and
  green on every run since. `main` has not moved since the release.
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
- Both crate names (`adjutant-sdk`, `adjutant-server`) are **free on crates.io**
  and the repository is **public**, so publication is a decision, not a hurdle.

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

**Proves it:** `testing` green without a re-run across a full day.

---

## Stage 1 — the day-one release (M5's remainder)

**Exit criterion:** SPEC §15 M5 — seven boxes now that iOS is deferred.

1. **The client-in-CI harness — the missing half of the v1.0 gate.** §1 test 3
   requires the path to be "exercised by the fresh-machine harness in CI". Today
   the client's tests run against a **mock** HTTP client and the end-to-end
   harnesses drive the API, not the app: **no client path is exercised against a
   live server in CI at all.** One harness — the Flutter app driven against a
   booted server, on the existing `adjutant test-plugin` rail — converts every
   client-covered plugin's "usable" claim from a demonstration into a gate, and
   makes Stage 5.1 mechanical. Highest-leverage item on this page.
2. **The MCP connect-and-interact proof** (M5 box 2). The plugin loads and is
   tested; what is missing is a real Hermes agent connecting through it with
   permissions filtered and invocations logged.
3. **Offline mode** (M5 box 5) — the client caches locally and syncs. The largest
   single client item here, and it needs one decision before it starts rather
   than during: local storage (drift/sqlite) or a plain cache.
4. **Android and Web** (M5 box 4). Web builds in CI; Android reaches the 161st
   through Obtainium. **iOS is deferred entirely** by owner decision — no runner,
   no distribution path, revisit after 1.0 — and SPEC's box now says so.
5. **The UI/UX review** (M5 box 7). Christopher authors it, as with the other
   deep review docs; the 48 dp finding that put Governance in Settings rather
   than a ninth navigation destination is its first entry.
6. **Deploy for the 161st** (M5 box 8). Blocked on Stage 3.1 (the proxy-fronted
   deployment proven) and Stage 3.2 (the restore drill) — *not* on anything in
   the application. The host is chosen **after** 3.1, deliberately: the owner's
   decision is to pick a host against a finished, exercised deployment rather
   than an aspiration.

**Proves it:** the seven boxes checked in a new `M5` record, each with its run id,
and the client harness green on `testing`.

---

## Stage 2 — M6's open boxes

1. **SDK v0.3** — permission macros and migration helpers (SPEC §15 M6). The SDK
   is the product; this is the last additive step before the 1.0 freeze, and it
   carries the version move (Stage 5).
2. **Announcements delivery — as channels, not a channel.** The owner's answer is
   *all of them*: in-app (already there through the record and the client's
   badge), **email**, **ntfy**, and native app notifications, with FCM/APNs only
   if a platform ever forces it. The model already exists — `core.notifications`
   carries `delivery_channel` and `delivery_state` as facts about a transport,
   with one value today — so this is a vocabulary and a worker, not a new table.
   Email and ntfy first: the House runs both mail and an ntfy instance, so no
   vendor enters the stack, and ntfy is the UnifiedPush path GrapheneOS users
   expect. Closes the last `[~]` in M6 and the substance of #46.
3. **The third test's remaining client surfaces** — `finance`'s treasurer view
   (ledger, budgets), `archive`'s search and the timeline, and `conflicts`'
   **party-facing surface: the case list and the stage timeline** (owner decision
   — a pathway nobody can see is a pathway nobody uses). `equipment`,
   `announcements` and the member half of `finance` already have surfaces and need
   only Stage 1.1 to make them gated.

**Proves it:** M6's boxes all checked, and the surfaces covered by the Stage 1.1
harness.

---

## Stage 3 — M7 production hardening (the gate on a real deployment)

SPEC's eight boxes: two are met (**Docker Compose**, **CI/CD**); six are open.
Order within the stage is by dependency, not by size:

1. **#49 — the proxy-fronted deployment, proven.** *Re-scoped by owner decision:
   the application terminates no TLS and never will.* It is deployed alongside
   Caddy, nginx or Traefik — `docs/deployment.md` already documents exactly this —
   and it honours `x-forwarded-for` only from a peer named in
   `ADJUTANT_TRUSTED_PROXIES` (empty by default, which ignores the header
   entirely; the M2 unconditional-trust defect is fixed and has two tests behind
   it). **Authored, and awaiting a host:** [`deploy/`](../deploy/README.md) holds
   the complete stack (Caddy as the default, Traefik for The House's pattern, the
   static-address trust wiring) and `deploy/verify.sh` runs the three proofs —
   no published port for the app, the real client as the rate-limit key, and a
   forged `x-forwarded-for` from an untrusted peer refused. **The proof needs a
   host with Docker**, which the authoring machine is not (no Docker, no sudo) and
   the two that are were unreachable when this was written (SSH key refused;
   TrueNAS API 401). Until `verify.sh` runs green and its transcript is recorded,
   this box is not met.
2. **#54 — backup and restore, proven by a drill.** A backup nobody has restored
   is a hypothesis. Pairs with 3.1 as the deployment gate.
3. **The host decision**, now against a working deployment rather than a plan —
   and it follows 3.1, because the deployment is what tells us what the host has
   to be.
4. **#51 — the security audit** (OWASP Top 10 + dependency scanning). The House's
   `aegis-*` audit modules are the tool; run it **after** Stage 4, so it measures
   the shape that ships.
5. **#50 — performance at 100+ concurrent users.**
6. **#52 — the admin guide and the user guide.** The user guide is also the
   dependency of the community plugin guide in Stage 5.
7. **#53 — CONTRIBUTING.md, code of conduct, PR process.** Small, but a
   precondition of Stage 5's community contribution, not a courtesy.
8. **The image is never built in CI, so the deployment is ungraded.** The
   workflow builds the workspace and stages the plugin libraries, but nothing
   builds the `Dockerfile` — which is how an image shipping **three plugins of
   fourteen** stayed green until a human read it (#88). The gate is cheap: build
   the image and assert the plugin count inside it matches the workspace's
   `cdylib` count, the same derivation `scripts/stage-plugins.py` already uses.
   Folded in here rather than beside the probe steps because it needs a Docker
   runner, not a database.

**Proves it:** M7's eight boxes checked, with the drill transcript, the proxy
proof and the audit report committed as evidence.

---

## Stage 4 — the integrity residuals

Each is small, and each is a correctness or honesty defect rather than a backlog
wish. None should be carried into a release as a "known issue" while it is an
afternoon's fix.

- **#78 — the notification record's identity fields are mutable by whoever holds
  `UPDATE`.** The sharpest: an integrity hole in what we just shipped, and it sits
  under the channel work of Stage 2.2.
- **#72 — the receipts read routes' ownership branch is proven only by the mock
  host.** A gate claiming more than it proves.
- **#71 — a correction's carry-over claim is wrong for `tax_statement` and
  `issued_on`.** A document that overstates.

**#74** (a receipt does not yet follow an online payment automatically) is *not*
on this list: it is a feature, and it belongs with Stage 2's finance work.

---

## Stage 5 — M8, v1.0

0. **Reserve the crate names.** Publish `adjutant-sdk` and then
   `adjutant-server` at the current `0.2.x`, with `sdk-compatibility.md` stating
   that the API promise begins at 1.0. Both names are free today; the only thing
   this waits on is the owner's crates.io token (see below). Five minutes, and it
   unblocks SPEC M3's publication box now rather than at the end.
1. **§1's three tests for all eleven Tier A plugins.** Tests 1 and the test-2
   coverage clauses are met by every plugin today; the gaps are test 2's
   `>= 1.0.0` version bar, test 3's CI-exercised path (Stage 1.1), and the last
   client surfaces (Stage 2.3). Plugin versions move with the release.
2. **SDK v1.0 published** with a stable API; the reservation above makes this the
   second publish of the same crate, not the first.
3. **The documentation site live**, the **community plugin guide published**, and
   the **F-Droid listing** — the last needs the privacy and licence pass
   (`app-privacy-policy-authoring` exists for exactly this; the client ships
   through Obtainium and has no Play listing).
4. **Tier B: Meshcore and ATAK are deferred post-1.0 by dated decision**
   (`plugin-roadmap.md` §3 and §6.2), which discharges the inventory's "shipped
   or explicitly deferred" rule for both. The community suites (§7.15) each still
   need the same treatment or a shipped integration.

> **There is no adoption criterion.** SPEC §15 M8 no longer counts troops using
> Adjutant (owner decision, 2026-09-26). The one criterion left that depends on
> someone outside The House is *1+ community plugin contributed* — kept for now
> because it tests our docs, gates and contribution path rather than someone
> else's goodwill. Flagged here so it is a visible exception to "we do not gate
> ourselves on other people's choices", not an oversight.

**Proves it:** a `M8` record with the three tests per plugin, the published
crates, the live docs, and the F-Droid listing evidenced rather than asserted.

---

## The release decision

- **Interim — cut `v0.3.0` ("day one") when it is actually deployable:** Stage 1's
  boxes, plus Stage 3.1's proxy-fronted proof and Stage 3.2's restore drill. The
  owner's preference is that this is a *deployment-shaped* gate — the 161st runs a
  tag they can also host — not a date and not a branch.
- **Final — `v1.0.0`** when Stage 5's list is done. See
  [`releasing.md`](releasing.md) for the mechanics.

## The one action the owner owes

**The crates.io token.** Generate one, add it as the repository secret the
publish workflow expects (`CARGO_REGISTRY_TOKEN`, per `releasing.md`
§One-time setup), and Stage 5.0 can run the same day. Nothing else on this page
is blocked behind a decision — the seven are answered, and they are recorded in
`plugin-roadmap.md` §6.

## Lanes (how to run it without colliding)

Four lanes run concurrently; they touch different files, so they can go in
parallel while every PR passes the same gate. The only hard sequence is inside a
lane.

- **A — client.** The Stage 1.1 harness first (it gates every other claim), then
  `finance`'s treasurer view, then `archive`, then `conflicts`' case list and
  timeline, then offline mode.
- **B — core and SDK.** Stage 0's #76 and #83, then SDK v0.3, then #78, #72, #71.
- **C — ops and deployment.** Stage 3.1's proxy-fronted proof, then #54's restore
  drill, then the host decision, then #51, #50, #52, #53.
- **D — integration.** The MCP↔Hermes proof, then the notification channels
  (email, ntfy) with #46 and #78 folded in.

Merge discipline, unchanged: one concern per PR; merge `testing` into the branch
before opening it; a red check is worked, not re-run past — and if it is a flake,
it is filed with the same-SHA contrast that proves it.
