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
| `missions` | §7.3 | Six-stage mission lifecycle, mentor matching, Lodge Commander approval | M4 | **Shipped** (v0.2.0) |
| `governance` | §7.4 | Motion lifecycle, voting, amendments, Accords versioning | M4 | **Shipped** (v0.2.0) |
| `calendar` | §7.7 | Events, RSVPs, recurrence, quorum tracking | M5 | **Shipped** (v0.2.0) |
| `mcp` | §7.10 | Permissions-aware MCP server for Hermes | M5 | **Built** (loading, tested; M5 client exit criteria pending) |
| `finance` | §7.5 | Funds, transactions, budget vs. actuals, sliding-scale dues | M6 | **Built** (routes and permissions in `docs/api-reference.md`; a member's own dues — assessment, self-report, pay — are in the client, the treasurer's ledger and budgets are not) |
| `equipment` | §7.6 | Inventory, checkout/checkin, maintenance | M6 | **Built** (routes and permissions in `docs/api-reference.md`; the pool, the item record, checkout and checkin are in the client) |
| `archive` | §7.8 | Congress proceedings, minutes, full-text search, timeline | M6 | **Built** (routes and permissions in `docs/api-reference.md`; no client surface) |
| `conflicts` | §7.9 | Conflict-resolution pathway, stage tracking, anti-dropout | M6 | **Built** (routes and permissions in `docs/api-reference.md`; no client surface — see below) |
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
> with `sdk-compatibility.md` applied; every plugin is `0.2.0` today, so the
> version move and the SDK's own v1.0 are one step (release-path Stage 5), not a
> formality to be waved through at the end.

### Tier B — per-troop integrations

Decided to exist, but a troop chooses whether to run them. They do not gate 1.0
individually; by 1.0 each must either be shipped **or** carry a dated decision
recorded in this document saying it is deferred and why.

| Integration | SPEC | Notes |
|---|---|---|
| Meshcore (LoRa) | §7.11 | Field comms; hardware-dependent; **undecided** — in M6 or post-1.0 is a Coyote Company timeline question (§6.2) |
| ATAK | §7.12 | Situational awareness; hardware-dependent; **undecided**, same question |
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
- **M5 — partially met.** `calendar` is shipped and the client MVP's named screens
  exist and go beyond them; `mcp` is built, loading and tested. Open: the MCP
  connect-and-interact proof, offline mode, the client exercised against a live
  server in CI, the UI/UX review, and the deployment to The House.
- **M6 — built, two boxes open.** The five plugins exist and are probed; `SDK v0.3`
  is not delivered and announcements' delivery is deferred by decision (#46). A
  third of M6's boxes are about the *client* surface, which release-path Stage 2.3
  carries.
- **M7 — two of eight met** (Docker Compose, CI/CD); the other six are open issues
  #49–#54.
- **M8 — not started**, and gated on M5–M7 plus one decision only the owner can
  make (the crates.io token — release-path Stage 5).

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

## 6. Open decisions this document does not settle

Listed so they are visible rather than decided by accident:

1. **Announcements: plugin or client feature?** It is in Tier A as a plugin
   because push notifications need a server-side owner. If the client owns them
   instead, it should leave the inventory deliberately.
2. **Tier B sizing.** Meshcore and ATAK are hardware integrations with real
   field use in Operation Slipperyskin. Whether they are M6 or post-1.0 is a
   decision about the Coyote Company's timeline, not about the software.
3. **What "3+ troops" means for the M8 gate** — three troops we know, or three
   independent operators? The current wording is ambiguous and should be
   tightened before it is measured.

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
