# Plugin Roadmap & the v1.0 Gate

**Status:** adopted inventory; sequencing proposals marked as such.
**Owner:** the roadmap is maintained in the same PR that changes a plugin's state.

[`SPEC.md`](../SPEC.md) §7 defines what each plugin *does*. This document owns the
answer to three questions SPEC deliberately does not answer: **which plugins
ship, in what order, and what "1.0" means.**

---

## 1. The v1.0 rule

> **No 1.0 until every plugin we have already decided should exist does exist,
> is stable, and is usable.**

"Already decided" means this repository's own inventory — SPEC §7 and the
`docs/milestones/` record — not a wish list added along the way. The inventory
below is therefore closed: a plugin enters it by editing this document in a PR,
which is a visible decision, not by appearing in a client.

### What "shipped, stable, and usable" means (three tests)

Every Tier A plugin must pass all three before 1.0. They are written to be
checkable, not aspirational.

**1. Exists**
- Loads through the blessed path from a pristine database: `adjutant validate-plugin`
  is green in CI for native plugins; the WASM probe covers any sandboxed plugin.
- Migrations apply cleanly from empty, and the plugin is exercised by
  `adjutant test-plugin` in CI.
- Its routes and permissions appear in `docs/api-reference.md`.

**2. Stable**
- Plugin version is `>= 1.0.0`; the SDK compatibility policy
  (`sdk-compatibility.md`) has been applied to it.
- No open High-severity issue against it.
- Covered by tests written against the public `adjutant_sdk::testing` module.
- Its DB-gated tests fail loudly when the database is unavailable — a test that
  reports `ok` while skipping does not count as coverage (issue #25).

**3. Usable**
- A scout or leader can complete the plugin's primary task end to end **without
  SQL, curl, or an admin's help** — through the Flutter client or the CLI.
- That path is exercised by the fresh-machine harness in CI, not demonstrated
  once by hand.

The bar for "usable" is deliberately about a person, not an API: a plugin that is
only reachable by a developer is not shipped, it is deployed.

---

## 2. Milestone numbering

Two different milestones are both called "M4" in this repository, and that
collision is worth fixing once, here.

- `docs/milestones/M4-core-sdk-stabilization.md` landed first and is an
  **inserted stabilization gate** — core hardening, SDK contract lock, the WASM
  prototype — that sits between SPEC's M3 and SPEC's M4.
- SPEC §15's **M4 is Missions + Governance** and is **complete** (2026-09-25;
  record: [`milestones/M4-missions-governance.md`](milestones/M4-missions-governance.md)).

Records exist for M1, M2, M3, the inserted M3-S, M4 and M6. **M5, M7 and M8 have
no record file yet** — their exit criteria are SPEC §15's, and
[`release-path.md`](release-path.md) is where the work remaining against them is
ordered and tracked.

**Resolution: SPEC numbering is canonical.** In prose this document calls the
inserted one **M3-S (core & SDK stabilization)**. The file keeps its existing
name: renaming it now would break the v0.2.0 evidence trail for no gain, and
SPEC §15's numbering note carries the cross-reference. If the name is ever
changed, it is a mechanical follow-up across the records, not a decision.

---

## 3. The inventory

### Tier A — required for 1.0

| Plugin | SPEC | Purpose | Milestone | Status |
|---|---|---|---|---|
| `auth` | §7.1 | Authentication, sessions, roles, OIDC | M3 | **Shipped** (v0.2.0) |
| `membership` | §7.2 | Roster, lodges, patrols, proficiency, OSG import | M3 | **Shipped** (v0.2.0) |
| `missions` | §7.3 | Six-stage mission lifecycle, mentor matching, Lodge Commander approval | M4 | **Shipped** (v0.3.0) |
| `governance` | §7.4 | Motion lifecycle, voting, amendments, Accords versioning | M4 | **Shipped** (v0.3.0) |
| `calendar` | §7.7 | Events, RSVPs, recurrence, quorum tracking | M5 | **Shipped** (v0.3.0) |
| `mcp` | §7.10 | Permissions-aware MCP server for Hermes | M5 | **Built** (loading, tested; M5 client exit criteria pending) |
| `finance` | §7.5 | Funds, transactions, budget vs. actuals, sliding-scale dues | M6 | **Built** (routes and permissions in `docs/api-reference.md`; a member's own dues — assessment, self-report, pay — are in the client, the treasurer's ledger and budgets are not) |
| `equipment` | §7.6 | Inventory, checkout/checkin, maintenance | M6 | **Built** (routes and permissions in `docs/api-reference.md`; the pool, the item record, checkout and checkin are in the client) |
| `archive` | §7.8 | Congress proceedings, minutes, full-text search, timeline | M6 | **Built** (routes and permissions in `docs/api-reference.md`; no client surface) |
| `conflicts` | §7.9 | Conflict-resolution pathway, stage tracking, anti-dropout | M6 | **Built** (routes and permissions in `docs/api-reference.md`; party-facing client surface — the case list and the stage timeline — decided 2026-09-26, release-path Stage 2.3) |
| `announcements` | §7.14 | Troop communication, read receipts, categories, push | M6 | **Built** (routes and permissions in `docs/api-reference.md`; the inbox, unread badge and mark-read are in the client; delivery deferred — the record is `core.notifications` (#46 slice 1) and the plugin is not wired to it) |

> **"Shipped" vs "Built" vs the v1.0 gate.** `auth`, `membership`, `missions`,
> `governance` and `calendar` are **Shipped**: they load through the blessed path,
> are covered by tests against `adjutant_sdk::testing`, and their routes and
> permissions are in `api-reference.md`. The M6 batch — `finance`, `equipment`,
> `archive`, `conflicts`, `announcements` — meets those same first two tests and
> is marked **Built**. The M6 batch's record is
> `docs/milestones/M6-operational-plugins.md`.
>
> **No plugin passes §1's test 3 yet, and the client screens are not the reason.**
> Test 3 asks for the path to be *exercised by the fresh-machine harness in CI*.
> Four flows now have client surfaces — `governance` (the motions list, one
> motion's record, casting a vote), `equipment` (the pool, the item record,
> checkout/checkin), `announcements` (the inbox, unread badge, mark-read) and the
> member half of `finance` (dues: assessment, self-report, paying) — but the
> client's tests run against a **mock** HTTP client and the end-to-end harnesses
> drive the API, not the app, so **no client path is exercised against a live
> server in CI at all**. Until that harness exists, "usable by a scout" is
> demonstrated by hand rather than gated, and the screens landing is a
> prerequisite, not the proof. That harness is the first item of
> [`release-path.md`](release-path.md) Stage 1; the remaining flows are tracked in
> #56.
>
> `archive` has no client surface, and `conflicts` may never take test 3 in its
> current form — a private case between two people may belong in a conversation
> rather than a screen. Either way the outcome belongs *here*, as a dated
> exception, rather than as a silent omission.
>
> **The version bar is unmet too.** Test 2 requires a plugin version `>= 1.0.0`
> with `sdk-compatibility.md` applied; every plugin is `0.3.0` today, so the
> version move and the SDK's own v1.0 are one step (release-path Stage 5), not a
> formality to be waved through at the end.

### Tier B — per-troop integrations

Decided to exist, but a troop chooses whether to run them. They do not gate 1.0
individually; by 1.0 each must either be shipped **or** carry a dated decision
recorded in this document saying it is deferred and why.

| Integration | SPEC | Notes |
|---|---|---|
| Meshcore (LoRa) | §7.11 | Field comms; hardware-dependent. **Deferred to post-1.0 — dated decision, 2026-09-26** (§6.2) |
| ATAK | §7.12 | Situational awareness; hardware-dependent. **Deferred to post-1.0 — dated decision, 2026-09-26** (§6.2) |
| Stripe | §7.13 | Payments; only for troops that take money online. **In use** — the store's checkout and the dues payment both go through it, and its ledger booking is an outbox intent |
| Store | §7.16 | The troop's shop — catalogue, sliding scale, rentals, comp sales; needs `stripe`. **Built and client-covered**: the catalogue (after #79), orders, the scholarship draw and the admin screens are in |
| Community suites | §7.15 | Google, Apple, Microsoft, Nextcloud, Proton — explicitly optional per troop; **not started**, and each needs its own dated decision or a shipped integration before 1.0 |

---

## 4. Sequencing

### Decided by this document

- **M4 — Missions + Governance + SDK v0.2.** Unchanged from SPEC §15, with the
  scope prerequisite in §5 below.
- **M5 — MCP + Flutter client MVP + Calendar (moved forward from M6).**
  M5's stated deliverable is "a working MVP that scouts can actually use… the
  first release to real users". A troop's daily life is its calendar: without
  events and RSVPs the troop keeps planning in a group chat, and the software
  stops being the place people go. Calendar is therefore part of the first
  release, not of the operational batch behind it.
- **M6 — Finance + Equipment + Archive + Conflicts + Announcements.**
  Archive, conflicts and announcements had no milestone assigned anywhere;
  they are assigned here rather than left to drift.
- **M7 — Production hardening.** Unchanged.
- **M8 — v1.0.** The gate becomes the inventory test in §1, replacing "all 7
  core plugins stable and tested".

### Where each milestone stands (2026-09-26)

The exit criteria in SPEC §15 and in `docs/milestones/*` remain the authority for
*whether* a milestone is done; this is the one-line state of each, and
[`release-path.md`](release-path.md) owns the ordered work remaining.

- **M1, M2, M3, M3-S, M4 — done**, each with a record and live evidence.
- **M5 — partially met: three of eight boxes, one partial, four open**, and it now
  has a record ([`milestones/M5-mcp-and-flutter-mvp.md`](milestones/M5-mcp-and-flutter-mvp.md)).
  `calendar` is shipped with recurrence, RSVPs and quorum; the client's named
  screens exist and go well beyond them; `mcp` is built, loading and tested; and
  the two things this line used to name as missing — the client exercised against
  a live server in CI, and the plugin set chosen rather than assumed — both landed
  on 2026-09-27 (the harness is a `verify` step; #89/#90 closed with #110). Open:
  the MCP connect-and-interact proof, offline mode (storage decided: a plain cache
  plus a queued write list), the UI/UX review, the Android artifact (#111), and
  the deployment to the 161st (host decided: troop-owned hardware).
- **M6 — built, one box open.** **SDK v0.3 landed after this line was written**
  (`permissions!` and `migrations!`, `SDK_ABI_VERSION` still 4, `conflicts`
  converted as the proof; the record carries the dated correction), so what is
  open is announcements' delivery, deferred by decision (#46 — `DELIVERY_CHANNELS`
  is still `[in_app]`). A third of M6's boxes are about the *client* surface,
  which release-path Stage 2.3 carries.
- **M7 — three of eight met** (CI/CD; the proxy-fronted deployment proven; the
  restore drill from a destroyed volume). The compose stack's "met" was withdrawn
  on 2026-09-26 when it had never actually started, and the other boxes are the
  open issues #49–#54, ordered in [`release-path.md`](release-path.md) Stage 3.
- **M8 — not started**, and gated on M5–M7 plus one decision only the owner can
  make (the crates.io token — release-path Stage 5). Nothing in CI publishes the
  crates today; the release workflow builds a tarball and the client's web bundle.

### The day-one release (M5)

The first release to real users contains: `auth`, `membership`, `missions`,
`governance`, `calendar`, `mcp`, and the Flutter client (login, mission list,
roster, calendar). Everything else waits; nothing else is required for a troop
to run on it. The client has since grown well past that list — announcements,
dues, equipment, governance, store and the admin screens are all in it — so the
constraint on the day-one release is no longer the screens: it is the harness that
proves them in CI, and the deployment that puts them in front of the 161st.

### Reasoning worth recording

Archive and conflicts are the two plugins that express the Accords in software —
searchable Congress history, and the conflict pathway the Accords define.
They are requirements, not extras: a tool that models governance but forgets its
own records is a form generator. Archive in particular is the strongest thing to
show an outside body, because it is the only part no vendor can sell back to us.

---

## 5. Prerequisites inside M4 (not plugins)

Both of these are work that must land **before** missions and governance are
built on top of it, because both plugins are full of scope decisions:

1. **Scope enforcement (SPEC §9.2). — met.** The route gate consults scope:
   ordinary routes require a troop-covering grant, object routes
   (`*_protected_any_scope`) check the object in the handler, and `delete` is
   troop-only. A lodge-scoped grant no longer behaves troop-wide. See issues
   #19–#22 and [`docs/design/scoped-permissions.md`](design/scoped-permissions.md).
   Missions and governance can now build Lodge Commander approval / voting on it.
2. **The v0.2.0 boundary findings. — met.** The three demonstrated escapes are
   closed: the plugin connection is now the restricted principal (its own
   `LOGIN` role and pool, migrations on that pool), and the #17–#25 authorization
   defects are fixed with committed regression probes. Isolation is a
   confinement, not a convention:
   [`docs/design/plugin-isolation.md`](design/plugin-isolation.md).

---

## 6. Decisions taken (2026-09-26 and 2026-09-27)

This section used to list what the document did not settle. Those questions were
put to the owner on **2026-09-26** and answered; the answers are recorded here so
the reasoning survives the commits that implement them. Five more were put and
answered on **2026-09-27** (below), against the release-path question "are we
ready to release 0.3?".

1. **Announcements: plugin or client, and which channel? — the plugin owns
   delivery, and delivery is not one channel.** The vocabulary is the notification
   record's existing `delivery_channel`: in-app, **email**, **ntfy**, and
   FCM/APNs only if a platform ever forces it. Email and ntfy first. ntfy is the
   self-hosted push service — the House already runs one, so no vendor enters the
   stack, and it is the UnifiedPush path a GrapheneOS user expects, which is what
   makes "native app notifications" and "no Google" stop being opposites. The
   plugin stays in Tier A; the channel model is the `core.notifications` row, not
   a new table.
2. **Tier B sizing — Meshcore and ATAK are both post-1.0, each with a dated
   decision** (see the Tier B table above and
   [`release-path.md`](release-path.md) Stage 5). Hardware integration is not what
   makes 1.0 real, and neither is refused — they are sequenced.
3. **What "3+ troops" meant — nothing: the criterion is removed.** SPEC §15's M8
   list no longer counts adopters (see its note). Adoption is an outcome we hope
   for, not a gate we hold ourselves behind.
4. **Conflicts in the client — build the party-facing surface.** The roadmap's
   earlier argument (a private case belongs in a conversation, not a screen) is
   overruled by the owner: *a pathway nobody can see is a pathway nobody uses*.
   The case list and the stage timeline are Stage 2 work; the stage machinery is
   already built and probed.
5. **iOS — deferred entirely** (no runner, no distribution path) and revisitable
   after 1.0. SPEC §15's M5 box now reads Android + Web.
6. **TLS — the application terminates nothing.** It is deployed alongside Caddy,
   nginx or Traefik; the M7 box is the proven proxy-fronted deployment, not a
   certificate inside the binary.
7. **The deployment host stays undecided** until the proxy-fronted deployment is
   finished and exercised, then it is chosen against something real. **Answered
   2026-09-27 (below): hardware the 161st owns or is given.**

**One action the owner owes, and nothing is blocked behind anything else:** the
crates.io token. Both crate names are free today, and reserving them at the
current `0.2.x` — with the API promise starting at 1.0 — is a five-minute errand
that unblocks SPEC M3's publication box and M8's. *(Corrected 2026-09-27: nothing
in CI publishes the crates — the release workflow builds a tarball and the
client's web bundle — so this token is read by a human running `cargo publish`,
not by a workflow step.)*

### Decisions taken (2026-09-27)

Put to the owner against the question "are we ready to release 0.3?", and answered
in one sitting. Each gates work rather than describing it.

1. **The v0.3.0 gate stays as written.** Stage 1's boxes plus Stage 3.1's
   proxy-fronted proof and Stage 3.2's restore drill — no re-scoping, and no
   earlier tag on the deployment path alone. Two of the eight Stage 1 boxes were
   already met by the time the question was asked, and the rest are the work.
2. **Offline mode uses a plain cache, not drift.** A durable read cache plus a
   queued write list, with no schema on the device. drift/SQLite stays the answer
   *if* offline queries turn out to need one; it is not the opening move, because
   a client-side schema is a second migration story to keep in step with the
   server's.
3. **The 161st's instance runs on hardware the troop owns or is given.** Familiar
   and theirs, which is the point — the troop's data and the troop's box. One
   question follows it and belongs with their network rather than this repository:
   how that box is reached from outside (a Cloudflare Tunnel, which opens no ports
   and is the shape the proxy proof exercised, or a port-forward with a
   certificate of their own).
4. **A release attaches both the server and the client.** The tarball's plugin set
   is derived from the workspace — it had rotted to three of fourteen libraries,
   the same defect #88 found in the Dockerfile — and the client's **web** bundle
   is attached now. The **Android** artifact is attached when #111 gives it a CI
   build, not before: a release that attaches an APK nobody built is a claim
   rather than a build.
5. **The next lane is the honesty pass**, ahead of #111 and the MCP proof: the
   release workflow's plugin set, this document's stage states, `release-path.md`
   and the missing M5 record. The plan of record had drifted from the branch
   (three landed items still described as "do first"), which is the failure the
   document's own update rule exists to prevent.

---

## 7. How to update this document

- Changing a plugin's status, milestone, or tier happens in the same PR as the
  change it describes.
- Adding a plugin is an edit to §3 with a milestone; a plugin with no milestone
  is a note, not a commitment.
- The milestone's own exit criteria (`SPEC.md` §15, `docs/milestones/*`) remain
  the authority for *when* a milestone is done. This document decides what is in
  each one.
- **The status lines and tables here are read as of the date on them.** A number
  in a milestone record is a dated run, not a current claim; when a record's
  figures move on, the record gains the current figure beside the old one
  (`docs/milestones/M2-core-server.md` does this: "28 passed … 62 today") rather
  than being rewritten.
- **Where the remaining work is ordered is [`release-path.md`](release-path.md),
  not here.** This document decides membership (§3), sequencing (§4) and the gate
  (§1); that one decides order, lanes and what blocks what. A change to either
  belongs in the same PR as the state it describes.
