# The release path — ordered work from `testing` to v1.0

**Status:** the plan of record as of **2026-09-27**, `testing` at `75981e5`.
**Owner:** the same rule as [`plugin-roadmap.md`](plugin-roadmap.md) §7 — this
document is edited in the same PR that changes a stage's state. The owner
decisions it is built on are recorded in that document's §6 (2026-09-26 and
2026-09-27).

> **Read the states, not the intent.** This document lagged its own rule once
> already: Stage 0's two items and Stage 1's harness all landed while this page
> still called them "do first" and "highest-leverage", so anyone reading it as
> status would have been wrong about the branch. Every state line below cites a
> commit, a run id or a log line; where a state is dated, the date is when it was
> read and not a claim about today.

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

## Stage 0 — the gate's own honesty — **both items MET**

Two items, because they protect everything after them and both were one sitting:

1. **#76 — pin the Flutter version in CI. — MET.** Both jobs that touch the
   client read the same pinned toolchain
   (`subosito/flutter-action@v2` with `flutter-version-file: client/pubspec.yaml`
   and `channel: stable`), so a local green and the branch's gate at least agree
   about which Flutter they mean. The release workflow now reads the same file,
   for the same reason.
2. **#83 — the scheduler flake** (`host_db::probe_scheduler_runs_records_and_stops`).
   **— MET**, merged as `fix/scheduler-probe-flake`. A gate that needs a re-run
   teaches people to wave red through, and this one no longer does.

**Proves it:** `testing` green without a re-run across a full day — and green on
run `36288096546` (`75981e5`) with no re-run.

---

## Stage 1 — the day-one release (M5's remainder)

**Exit criterion:** SPEC §15 M5 — **eight** boxes (this page said "seven now that
iOS is deferred", which was an arithmetic slip: iOS was a clause inside the
Android-and-Web box, so deferring it moved no box). As of 2026-09-27, three are
met, one is partial and four are open — the per-box state and its evidence are in
[`milestones/M5-mcp-and-flutter-mvp.md`](milestones/M5-mcp-and-flutter-mvp.md).
**Later the same day, item 3 below landed and the boxes read four met, one
partial, three open** (PR #121); the record carries both readings rather than
rewriting the first.

1. **The client-in-CI harness — the missing half of the v1.0 gate. — MET.**
   `scripts/client-live-harness.sh` derives its own database, bootstraps the
   plugin roles into it, boots a server with the spoofable dev-header stub
   **off**, and runs `client/live/live_client_test.dart` against it. On
   `testing` at `75981e5` (run `36288096546`, `verify`) the step reported
   `bootstrapped 14 plugin role(s)`, `booting the server on
   http://127.0.0.1:8790 (dev headers OFF)`, then nine probes — server health,
   the stub refusing a spoofed `x-dev-user` with `401`, `login`, `me`,
   `members`, `lodges`, `motions`, `plugins`, and a dead session refused — and
   `🎉 10 tests passed.` / `==> live client gate passed`. The `client` job beside
   it passed **131 tests**. This is the gate that converts every client-covered
   plugin's "usable" claim from a demonstration into a check, and it is what
   makes Stage 5.1 mechanical.
2. **The MCP connect-and-interact proof** (M5 box 2). The plugin loads and is
   tested; what is missing is a real Hermes agent connecting through it with
   permissions filtered and invocations logged.
3. **Offline mode** (M5 box 5) — the client caches locally and syncs. **— MET**
   (2026-09-27, PR #121). Built on the storage decision taken 2026-09-27: a plain
   durable read cache plus a queued write list, no schema on the device,
   drift/SQLite still the answer only if offline *queries* need one. Reads are
   cached per resource and served **only** when the server cannot be reached, so a
   cache is never preferred to a live answer and age alone never makes stale data
   look live; invalidation is the end of the session (signing out drops the cache
   and the queue). Writes are queued **in order** and replayed when the server
   answers again, and only where the server treats a repeat as the same act — a
   receipt, a dues tier, a plugin's enable/disable; a write the server would
   *append* (an order, a Checkout session, a draw) is refused in words instead of
   queued; and a queued write the server refuses (a 4xx) is parked with the
   server's own sentence and does not block the queue. `client/test/offline_test.dart`
   is 19 of the branch's **150** client tests (131 before).
4. **Android and Web** (M5 box 4). Web builds — proven on this host with the
   pinned toolchain (`flutter build web --release`, exit 0, 41 MB bundle) — and
   the release workflow now attaches the built bundle to a tag. Android reaches
   the 161st through Obtainium, but **no CI step builds it**, which is #111: the
   release workflow grows the Android artifact when that gate exists, rather than
   attaching an APK nobody built. **iOS is deferred entirely** by owner decision —
   no runner, no distribution path, revisit after 1.0 — and SPEC's box says so.
5. **The UI/UX review** (M5 box 7). Christopher authors it, as with the other
   deep review docs; the 48 dp finding that put Governance in Settings rather
   than a ninth navigation destination is its first entry.
6. **Deploy for the 161st** (M5 box 8). **The host decision is taken (owner,
   2026-09-27): hardware the 161st owns or is given.** That closes Stage 3.3 and
   leaves one open question this document must not answer by assumption — how
   that box is reached from outside. A Cloudflare Tunnel on the troop's hardware
   opens no ports and is the shape Stage 3.1's proof exercised; a port-forward
   with a certificate of their own is the other honest shape, and the choice
   belongs with the troop's network rather than with this repository. Everything
   else is unblocked: Stage 3.1 and 3.2 are met.
7. **Choose the plugin set — and make the choice real (#89, #90). — MET.**
   `enabled` decides what is loaded rather than what answers, the choice is
   recorded in the database with both doors (CLI and the first-run wizard)
   calling one implementation, and a plugin that is enabled for the first time
   migrates then. Merged as `#110` (`b3999f34`); both issues are closed.

**Proves it:** the eight M5 boxes checked in a new `M5` record, each with its run
id — the record exists as [`milestones/M5-mcp-and-flutter-mvp.md`](milestones/M5-mcp-and-flutter-mvp.md),
with **four boxes met (offline mode is the fourth, PR #121), one partial and
three open** — it read three/four before that PR, and both readings are in the
record. The plugin set is chosen on a fresh deployment and honoured by what the
server loads (#89/#90), and the client harness is green on `testing` — both done.
What this stage is still waiting on is items 2, 5, 6 and the Android half of
item 4.

---

## Stage 2 — M6's open boxes

1. **SDK v0.3** — permission macros and migration helpers (SPEC §15 M6). **— MET.**
   `permissions!` and `migrations!` are in `plugins/sdk/src/lib.rs`, each carrying
   compile-time traps at the invocation (a duplicate permission id, a duplicate or
   out-of-order migration version, a version below 1, an empty id or description),
   and `testing::assert_routes_gate_declared` turns the one invariant the macros
   cannot express into an assertion that names the route. Additive: no ABI bump,
   `SDK_ABI_VERSION` stays **4**. `adjutant-conflicts` is converted as the proof,
   with its migration versions, names and SQL byte-identical so no database
   re-runs anything. M6's record and SPEC's M5 box were written before this landed
   and said otherwise; both now carry the dated correction.
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

SPEC's eight boxes: one is met (**CI/CD**); the other seven are open.

**Docker Compose is written but has never started** — corrected 2026-09-26. The
roadmap counted it as met because the files exist; the first run on a real Docker
host showed otherwise: `postgres:18-alpine` exits immediately when its volume is
mounted at `/var/lib/postgresql/data`, because 18 moved `PGDATA` into a versioned
subdirectory and treats the old path as an unused mount. CI never saw it, because
the workflow's database is a GitHub Actions *service container* and not the
compose file. Fixed in #92 (the mount is now the parent directory) and folded into
this stage as item 9 rather than left in the "met" column, where it would have gone
on looking finished.
Order within the stage is by dependency, not by size:

1. **#49 — the proxy-fronted deployment, proven.** *Re-scoped by owner decision:
   the application terminates no TLS and never will.* It is deployed alongside
   Caddy, nginx or Traefik — `docs/deployment.md` already documents exactly this —
   and it honours `x-forwarded-for` only from a peer named in
   `ADJUTANT_TRUSTED_PROXIES` (empty by default, which ignores the header
   entirely; the M2 unconditional-trust defect is fixed and has two tests behind
   it). **MET on 2026-09-26:** `deploy/verify.sh` runs green on a real Docker host
   — no published port for the app, the app answering through the proxy, the real
   client as the rate-limit key (200 then 429 for one client, 200 for a second),
   and a forged `x-forwarded-for` from an untrusted peer refused. The transcript
   is in `deploy/README.md` § Status. Reaching it needed five fixes there, because
   the proofs had never been **runnable** — which is why "authored, not yet
   proven" went unchallenged for so long. **The public TLS path is still
   unproven:** a real name, ACME and the `:443` listener are not things
   `verify.sh` exercises, and they take more than a placeholder domain. That
   belongs to item 3's host decision, not to this box.
2. **#54 — backup and restore, proven by a drill.** A backup nobody has restored
   is a hypothesis. **MET on 2026-09-26:** `deploy/backup.sh` and
   `deploy/restore.sh` were written and then run end to end from a destroyed
   volume — `pg_restore` 2s, `bootstrap-isolation` 3s, **6s wall clock** for a
   242KB dump, after which the app answered through the proxy, 17 roles and 335
   plugin-role grants were back, and the marker row had survived. The measurement
   is in `docs/deployment.md` § Backup and restore, together with why the drill
   needed a script instead of a paragraph: a `pg_dump` carries the database and
   **not** the cluster's roles, `pg_restore` reports `errors ignored` while
   silently dropping six `GRANT`s, and the server then crash-loops on a password
   error whose real cause is a role that was never created. **RPO is not yet
   automated** — the cadence is documented and nothing takes the dump on a
   schedule; that belongs with item 3's host decision, since it is the host that
   would own the timer.
3. **The host decision — TAKEN (owner, 2026-09-27): hardware the 161st owns or is
   given.** Against a working deployment rather than a plan, as intended. Two
   consequences this document records rather than resolves: **RPO is still
   unautomated** (item 2) and now lands with the troop's box, since the host owns
   the timer; and **how that box is reached from outside is open** — a Cloudflare
   Tunnel on it (no ports opened, the shape item 1 proved) or a port-forward with
   a certificate of their own. The public TLS path is unproven in either shape,
   which is the same open thread item 1 names.
4. **#51 — the security audit** (OWASP Top 10 + dependency scanning). The House's
   `aegis-*` audit modules are the tool; run it **after** Stage 4, so it measures
   the shape that ships. **Partly in flight:** the dependency-scanning half is PR
   #113 (green; not yet merged). The OWASP audit itself is not started.
5. **#50 — performance at 100+ concurrent users.** Not started.
6. **#52 — the admin guide and the user guide.** Not started. The user guide is
   also the dependency of the community plugin guide in Stage 5.
7. **#53 — CONTRIBUTING.md, code of conduct, PR process.** **In PR #112** (green;
   not yet merged). Small, but a precondition of Stage 5's community
   contribution, not a courtesy.
8. **The image is never built in CI, so the deployment is ungraded. — STILL
   OPEN.** The workflow builds the workspace and stages the plugin libraries, but
   nothing builds the `Dockerfile` — which is how an image shipping **three
   plugins of fourteen** stayed green until a human read it (#88). **The same
   three-of-fourteen list was in `.github/workflows/release.yml`**, in the one
   file that builds what users download: fixed 2026-09-27 by deriving the set from
   `scripts/stage-plugins.py` and failing when the tarball carries fewer libraries
   than the staging step derived. The gate for the *image* is still the cheap
   thing to build here: build it and assert the plugin count inside matches the
   same derivation.
9. **The compose stack starts on a clean host.** Not a documentation task: the
   first real run is what found the Postgres 18 mount fault above, and the box
   should not be called met again on the strength of files existing. The evidence
   is a transcript — `docker compose up -d`, the app answering through the proxy,
   and `deploy/verify.sh`'s proofs — on a host that has never run Adjutant before.
10. **The release workflow is unexercised until a tag.** It runs on `push: tags`
   only, so nothing in the gate has ever executed it: a fault in the release path
   — like the three-of-fourteen list that sat in it — is discovered at the moment
   a release is being cut. Either give it a `workflow_dispatch` entry so it can be
   run against a scratch ref, or exercise it once on a throwaway tag before the
   v0.3.0 tag, and record the run. #111's platform build is what makes the client
   half of this worth re-running.

**Proves it:** M7's eight boxes checked, with the drill transcript, the proxy
proof and the audit report committed as evidence.

---

## Stage 4 — the integrity residuals

Each is small, and each is a correctness or honesty defect rather than a backlog
wish. None should be carried into a release as a "known issue" while it is an
afternoon's fix.

- **#78 — the notification record's identity fields are mutable by whoever holds
  `UPDATE`.** **Fixed in PR #115** (green on `verify`, `client` and `msrv` as of
  2026-09-27; not yet merged).
- **#72 — the receipts read routes' ownership branch is proven only by the mock
  host.** **Fixed in PR #116** (green on the same three jobs; not yet merged).
- **#71 — a correction's carry-over claim is wrong for `tax_statement` and
  `issued_on`.** **DONE**, merged as PR #117 (`75981e5`) and closed 2026-09-27.
  The documents were the wrong side, not the code: the wording is derived from the
  troop's current declaration and `issued_on` is the correction's own date; what
  carries over is the receipt's identity. The claim is now measured by a DB-backed
  probe that moves the declaration between the issue and the correction, and that
  probe fails against the carry-over variant.

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
  boxes, plus Stage 3.1's proxy-fronted proof and Stage 3.2's restore drill — the
  owner confirmed this scope on **2026-09-27** rather than re-scoping it. The
  preference is that this is a *deployment-shaped* gate — the 161st runs a tag
  they can also host — not a date and not a branch. As of 2026-09-27: both
  deployment proofs are met, Stage 1 has four of eight boxes met (offline mode
  is the fourth, PR #121), and what is left is the MCP↔Hermes proof, the UI/UX
  review, the Android artifact (#111) and the deployment itself.
- **The artifact a tag produces — decided 2026-09-27.** The tarball's plugin set
  is derived from the workspace rather than listed (it had rotted to three of
  fourteen), the client's **web** bundle is attached, and the **Android** artifact
  is attached when #111 gives it a CI build — not before, because a release that
  attaches an APK nobody built is a claim, not a build. `releasing.md` describes
  what a tag now produces.
- **Final — `v1.0.0`** when Stage 5's list is done. See
  [`releasing.md`](releasing.md) for the mechanics.

## The one action the owner owes

**The crates.io token.** Generate one and keep it where `cargo publish` can read
it (`cargo login`), and Stage 5.0's name reservation can run the same day. One
correction to how this page and `releasing.md` described it: **nothing in CI
publishes the crates** — the release workflow builds the tarball and the client
bundle, and has no publish step — so the token is not a repository secret today;
it is needed the moment someone publishes, and wiring that step is a deliberate
choice rather than an oversight to assume.

Nothing else on this page is blocked behind a decision: the owner's answers are
recorded in `plugin-roadmap.md` §6 (both the 2026-09-26 seven and the
2026-09-27 five).

## Lanes (how to run it without colliding)

Four lanes run concurrently; they touch different files, so they can go in
parallel while every PR passes the same gate. The only hard sequence is inside a
lane. State as of 2026-09-27:

- **A — client.** The harness (**done**), then `finance`'s treasurer view, then
  `archive`, then `conflicts`' case list and timeline, then offline mode
  (**done**, PR #121: the plain cache plus the queued write list the storage
  decision called for).
- **B — core and SDK.** Stage 0's #76 and #83 (**both done**), then SDK v0.3
  (**done**), then #78 and #72 (**both in green PRs**), then #71 (**done,
  merged**).
- **C — ops and deployment.** Stage 3.1's proxy-fronted proof and #54's restore
  drill (**both done**), then the host decision (**taken: troop-owned
  hardware**), then #51, #50, #52, #53 — with #53 and the dependency-scanning half
  of #51 in green PRs.
- **D — integration.** The MCP↔Hermes proof (**not started**; next on the
  path), then the notification channels (email, ntfy) with #46 and #78 folded in —
  `DELIVERY_CHANNELS` is still `[in_app]`.

Merge discipline, unchanged: one concern per PR; merge `testing` into the branch
before opening it; a red check is worked, not re-run past — and if it is a flake,
it is filed with the same-SHA contrast that proves it.
