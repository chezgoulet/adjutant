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
- SPEC §15's **M4 is Missions + Governance** and has not started.

**Resolution: SPEC numbering is canonical.** In prose this document calls the
inserted one **M3-S (core & SDK stabilization)**. Renaming the milestone file to
`M3-S-core-sdk-stabilization.md` is a mechanical follow-up; it is not done here
so that the v0.2.0 evidence trail keeps its existing name.

---

## 3. The inventory

### Tier A — required for 1.0

| Plugin | SPEC | Purpose | Milestone | Status |
|---|---|---|---|---|
| `auth` | §7.1 | Authentication, sessions, roles, OIDC | M3 | **Shipped** (v0.2.0) |
| `membership` | §7.2 | Roster, lodges, patrols, proficiency, OSG import | M3 | **Shipped** (v0.2.0) |
| `missions` | §7.3 | Six-stage mission lifecycle, mentor matching, Lodge Commander approval | M4 | **Shipped** (v0.2.0) |
| `governance` | §7.4 | Motion lifecycle, voting, amendments, Accords versioning | M4 | **Shipped** (v0.2.0) |
| `calendar` | §7.7 | Events, RSVPs, recurrence, quorum tracking | M5 | Planned |
| `mcp` | §7.10 | Permissions-aware MCP server for Hermes | M5 | Planned |
| `finance` | §7.5 | Funds, transactions, budget vs. actuals, sliding-scale dues | M6 | Planned |
| `equipment` | §7.6 | Inventory, checkout/checkin, maintenance | M6 | Planned |
| `archive` | §7.8 | Congress proceedings, minutes, full-text search, timeline | M6 | Planned |
| `conflicts` | §7.9 | Conflict-resolution pathway, stage tracking, anti-dropout | M6 | Planned |
| `announcements` | §7.14 | Troop communication, read receipts, categories, push | M6 | Planned |

> **"Shipped" vs the v1.0 gate.** `missions` and `governance` load through the
> blessed path, are covered by tests against `adjutant_sdk::testing`, and their
> routes and permissions are in `api-reference.md`. The §1 gate's third test
> ("usable by a scout without SQL or curl") needs the Flutter client and the M5
> fresh-machine harness, and is **not** met by this milestone.

### Tier B — per-troop integrations

Decided to exist, but a troop chooses whether to run them. They do not gate 1.0
individually; by 1.0 each must either be shipped **or** carry a dated decision
recorded in this document saying it is deferred and why.

| Integration | SPEC | Notes |
|---|---|---|
| Meshcore (LoRa) | §7.11 | Field comms; hardware-dependent |
| ATAK | §7.12 | Situational awareness; hardware-dependent |
| Stripe | §7.13 | Payments; only for troops that take money online |
| Community suites | §7.15 | Google, Apple, Microsoft, Nextcloud, Proton — explicitly optional per troop |

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

### The day-one release (M5)

The first release to real users contains: `auth`, `membership`, `missions`,
`governance`, `calendar`, `mcp`, and the Flutter client (login, mission list,
roster, calendar). Everything else waits; nothing else is required for a troop
to run on it.

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
