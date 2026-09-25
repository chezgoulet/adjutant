# API Reference

Adjutant's HTTP API is JSON over HTTP. Plugin routes live under
`/api/{plugin}/…`; the core owns a small admin surface. See
[`architecture.md`](architecture.md) for the request path and
[`plugin-development.md`](plugin-development.md) for the SDK.

## Conventions

- **Content type:** request and response bodies are JSON unless a plugin says
  otherwise.
- **Errors:** every error response has the shape `{"error": "message"}`. Statuses
  follow `SdkError::status()`: `400` bad request, `401` unauthenticated, `403`
  forbidden, `404` not found, `409` conflict, `500` internal. **5xx responses
  never include internal detail** (it is logged server-side).
- **Auth:** send the session cookie `adjutant_session=…` (set by login), or
  `Authorization: Bearer <token>`. The spoofable `x-dev-user`/`x-dev-role`
  headers are available only when dev headers are explicitly enabled, and only
  when no identity provider claimed the request.
- **Request id:** every response carries `x-request-id`; the same id appears in
  the access log.
- **Rate limiting:** fixed window per client IP (config `ADJUTANT_RATE_MAX`,
  `ADJUTANT_RATE_WINDOW`; `0` disables). A `429` includes `retry-after`.
- **CORS:** controlled by `ADJUTANT_CORS` (empty = same-origin only).

## Core

| Method | Path | Permission | Purpose |
|---|---|---|---|
| GET | `/` | — | Health: `{"service":"adjutant","status":"ok"}` |
| GET | `/api/plugins` | `core:admin` | Plugin registry, route/permission inventory, `kind` (`native`/`wasm`) |
| POST | `/api/plugins/{name}/enable` | `core:admin` | Enable a plugin |
| POST | `/api/plugins/{name}/disable` | `core:admin` | Disable (routes 404, subscriptions stop) |
| DELETE | `/api/plugins/{name}` | `core:admin` | Uninstall (data archived, not dropped) |
| POST | `/api/plugins/reload` | `core:admin` | Rescan the plugin dir and hot-swap |
| GET | `/api/events/recent?since=<id>&limit=<1..500>` | `core:admin` | Event replay with cursor |
| GET | `/api/audit/verify` | `core:admin` | Verify the audit hash chain |

## Auth plugin

Config lives in the `auth` row's `core.plugins.config` (`{"oidc": {…},
"session_ttl_hours": N}`); reload to apply. OIDC routes answer `501` when no
`oidc` block is configured.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/auth/login` | — | `{username, password}` → session token + `adjutant_session` cookie |
| POST | `/api/auth/logout` | — | Ends the session |
| GET | `/api/auth/me` | session | Current user and scoped role grants |
| POST | `/api/auth/register` | — | `{username, email?, password}`; **open only while no users exist**; first user becomes `chief` (min 8-char password) |
| POST | `/api/auth/users` | `auth:manage_users` | `{username, email?, password, roles?}` |
| POST | `/api/auth/roles` | `auth:manage_users` | `{username, roles: string[]}` (replaces grants) |
| GET | `/api/auth/users` | `auth:manage_users` | List users |
| GET | `/api/auth/oidc/login` | — | Redirects to the IdP authorize endpoint (state stored server-side) |
| GET | `/api/auth/oidc/callback` | — | Code exchange, id_token verification, session creation |

## Membership plugin

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| GET | `/api/membership/members` | `membership:read_all` | Roster |
| GET | `/api/membership/member?id=<id>` | `membership:read` | One member (query param); `read_all` reaches any, `read_lodge` reaches its lodge, `read` only your own |
| POST | `/api/membership/member` | `membership:manage` | `{username, email?, display_name, trail_name?, osg_id?, bg_check?, patrol?, is_active?}` |
| POST | `/api/membership/import` | `membership:manage` | OSG CSV (see plugin docs); upsert by username |
| GET | `/api/membership/lodges` | `membership:read_lodge` | Lodges |
| POST | `/api/membership/lodge` | `membership:manage` | `{name}` |
| POST | `/api/membership/patrol` | `membership:manage` | `{name, lodge?}` |
| GET | `/api/membership/proficiencies` | `membership:read_lodge` | Proficiencies |
| POST | `/api/membership/proficiency` | `membership:manage` | `{code, title, domain?}` |
| POST | `/api/membership/proficiency/complete` | `membership:manage` | `{member_id, proficiency_id, signed_off_by?}` |
| GET | `/api/membership/stewards` | `membership:read_lodge` | Stewards |
| POST | `/api/membership/steward` | `membership:manage` | `{member_id, position, lodge?}` |

### Missions (SPEC §7.3, Accords Art 8)

Config lives in the `missions` row's `core.plugins.config` (unused today).

The six stages are `request → review → approval → execution → debrief → report`;
`state` is `open`, `rejected`, or `completed`. A rejected mission can be appealed
to the Troop Council, which decides it once one other Council member seconds it.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| GET | `/api/missions/missions?stage=&state=&category=&lodge=&limit=` | `missions:read` | Narrowed to the caller's lodge scopes and their own proposals unless they read troop-wide |
| GET | `/api/missions/mission/{id}` | `missions:read` | The mission + milestones, mentorships, progress, stage trail, appeals |
| POST | `/api/missions/mission` | `missions:create` | `{title, purpose, objectives, expected_impact, category?, lodge_id?, lodge_name?, tags?, location?, starts_on?, ends_on?, resources_needed?, risk_notes?, youth_safety_notes?, participant_count?}` — the structured proposal form |
| PATCH | `/api/missions/mission/{id}` | `missions:update` | Any subset of the proposal fields, while the mission is in `request` or `review` |
| POST | `/api/missions/mission/{id}/submit` | `missions:update` | `request → review` |
| POST | `/api/missions/mission/{id}/review` | `missions:approve` | `{success_criteria, scope_notes?, mentor?, recommendation?(advance\|return), guidance?}` → `review → approval` |
| POST | `/api/missions/mission/{id}/decision` | `missions:approve` | `{decision: approved\|rejected, guidance?}` (guidance required on rejection) → `approval → execution` |
| POST | `/api/missions/mission/{id}/appeal` | `missions:appeal` | `{reason}` — the mission must be `rejected` |
| POST | `/api/missions/appeal/{id}/decide` | `missions:approve` (troop) | `{seconded_by, outcome: overturned\|upheld, votes_for?, votes_against?, note?}` — overturning puts the mission into `execution` |
| POST | `/api/missions/mission/{id}/milestone` | `missions:update` | `{title, detail?, due_on?, position?}` |
| PATCH | `/api/missions/milestone/{id}` | `missions:update` | `{title?, detail?, status?, progress_pct?, due_on?}` (`done` implies 100%) |
| DELETE | `/api/missions/milestone/{id}` | `missions:approve` (troop) | Destructive: troop-covering grant only |
| POST | `/api/missions/mission/{id}/progress` | `missions:update` | `{note, progress_pct?, service_hours?, participant_count?}` — execution only |
| POST | `/api/missions/mission/{id}/debrief` | `missions:update` | `{notes, goals_met?, lessons?, service_hours?, participant_count?}` → `execution → debrief` |
| POST | `/api/missions/mission/{id}/report` | `missions:update` | `{summary, impact_metrics?, service_hours?, participant_count?}` → `debrief → report` |
| POST | `/api/missions/mission/{id}/complete` | `missions:approve` | `report → completed`; publishes `mission.completed` |
| POST | `/api/missions/mentor/profile` | `missions:mentor` | `{member_id, display_name?, affiliation?, expertise?, capacity?, is_active?, notes?}` |
| GET | `/api/missions/mission/{id}/mentor/suggestions` | `missions:approve` | Ranked mentors: expertise overlap, then spare capacity |
| POST | `/api/missions/mission/{id}/mentor` | `missions:approve` | `{mentor_member, mentee_member?, role?, notes?}` |
| POST | `/api/missions/mentorship/{id}/close` | `missions:update` | `{status?: completed\|ended, notes?}` |
| GET | `/api/missions/mentorships?member=&mission=` | `missions:read` | Scoped to the caller unless they read troop-wide |
| GET | `/api/missions/impact?lodge=` | `missions:read` (troop) | Cumulative Impact Report: totals, by category, by year, by lodge |

**Events:** `mission.created` (propose), `mission.approved` (approve),
`mission.completed` (complete, payload `MissionCompleted`).

### Governance (SPEC §7.4, Accords Art 5/9/12/17)

Motion lifecycle: `proposed → seconded → debate → voting → decided →
implemented` (or `withdrawn`). Thresholds: `simple_majority` (default),
`two_thirds`, `unanimous`. Vote methods: `voice`, `show_of_hands`, `ballot`,
`roll_call`. Quorum bases: `one_third_registered` (Congress), `majority_members`
(Troop Council, default), `fixed`.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/governance/meeting` | `governance:manage` | `{body: congress\|tc\|lodge\|committee, title, scheduled_for?, lodge_id?, location?, quorum_basis?, expected_voters?, quorum_required?}` |
| GET | `/api/governance/meetings?body=&status=&lodge=&limit=` | `governance:read` | Meetings with present count and motion count |
| GET | `/api/governance/meeting/{id}` | `governance:read` | Meeting + live quorum + motions |
| PATCH | `/api/governance/meeting/{id}` | `governance:manage` | `{title?, scheduled_for?, location?, quorum_basis?, expected_voters?, quorum_required?}` |
| POST | `/api/governance/meeting/{id}/open` | `governance:manage` | Opens the meeting and reports quorum |
| POST | `/api/governance/meeting/{id}/close` | `governance:manage` | Closes it; reports any undecided motions |
| POST | `/api/governance/meeting/{id}/attendance` | `governance:manage` | `{member_id, present?, method?: present\|remote\|proxy\|absent}` (upsert) |
| GET | `/api/governance/meeting/{id}/quorum` | `governance:read` | Real-time display: required, present, met, short by |
| POST | `/api/governance/meeting/{id}/minutes/draft` | `governance:manage` | Auto-drafts minutes from the motion record |
| POST | `/api/governance/meeting/{id}/minutes/adopt` | `governance:manage` | `{minutes?}` — adopts (and stores) the minutes of record |
| GET | `/api/governance/meeting/{id}/minutes` | `governance:read` | The stored minutes and their status |
| POST | `/api/governance/motion` | `governance:propose` | `{title, text, body, meeting_id?, lodge_id?, category?, threshold?, amends_accords?}` |
| GET | `/api/governance/motions?meeting=&body=&stage=&result=&category=&limit=` | `governance:read` | Motion list |
| GET | `/api/governance/motion/{id}` | `governance:read` | Motion + votes + amendments + the tally a close would produce now |
| POST | `/api/governance/motion/{id}/second` | `governance:vote` | A second member seconds it; the mover cannot second their own |
| POST | `/api/governance/motion/{id}/debate` | `governance:manage` | `{open: bool, note?}` — opens debate (`seconded → debate`) or closes it (`debate → voting`) |
| POST | `/api/governance/motion/{id}/vote` | `governance:vote` | `{choice: yes\|no\|abstain, method?, note?}` — one vote per member; a re-vote is `409` |
| POST | `/api/governance/motion/{id}/close` | `governance:manage` | Tallies; requires quorum when the motion is in a meeting; publishes `motion.passed` / `motion.failed` |
| POST | `/api/governance/motion/{id}/implement` | `governance:manage` | `{note?}` — a passed motion only |
| POST | `/api/governance/motion/{id}/withdraw` | `governance:propose` | `{note?}` — the mover, or a chair |
| POST | `/api/governance/motion/{id}/amendment` | `governance:amend` | `{kind: friendly\|formal, text, rationale?}` |
| POST | `/api/governance/amendment/{id}/accept` | `governance:amend` | Friendly only; the mover (or a chair) accepts, and the text is appended to the motion |
| POST | `/api/governance/amendment/{id}/reject` | `governance:manage` | `{note?}` |
| POST | `/api/governance/amendment/{id}/vote` | `governance:vote` | `{choice, method?, note?}` — formal only |
| POST | `/api/governance/amendment/{id}/close` | `governance:manage` | Tallies a formal amendment; a passing one is applied to the motion text |
| POST | `/api/governance/accords/adopt` | `governance:manage` | `{motion_id, title, summary?, body_md?, adopted_on?, congress?}` — the motion must have **passed in a Congress**; creates the next version and supersedes the previous adopted one |
| GET | `/api/governance/accords?status=` | `governance:read` | Version list |
| GET | `/api/governance/accords/{version}` | `governance:read` | One version, including its text |

**Events:** `motion.proposed`, `motion.passed`, `motion.failed` (the SPEC §5.4
trio), plus `accords.adopted` for the archive.

**Role grants are the operator's, not the plugin's.** The core seeds `chief`
with every permission it finds after load; other roles are mapped through
`core.role_permissions`. For the Accords' own pathway, `lodge_commander` needs
`missions:approve` (scoped to their lodge) and `tc`-equivalent roles need
`missions:approve` at troop scope plus `governance:manage` for the chair:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'missions:approve'),
  ('lodge_commander', 'missions:read'),
  ('scout',           'missions:read'),
  ('scout',           'missions:create'),
  ('scout',           'governance:read'),
  ('scout',           'governance:propose'),
  ('scout',           'governance:vote')
ON CONFLICT DO NOTHING;
```

### Finance (SPEC §7.5)

A troop's books: the six funds, every transaction in the ledger, the budget
envelope each fund spends against, the sliding-scale dues the Accords set so that
cost never decides who belongs, and the Annual Financial Report. Fund balances,
budget actuals and whether a member's dues are settled are derived from the
ledger on read, so none of them can be stale.

Config lives in the `finance` row's `core.plugins.config`, under `finance.<key>`
or the top level: `membership_cost_cents` (the base the sliding scale is a
fraction of), `dues_fund_code` (the fund dues and donations land in, General by
default) and `fiscal_year_start_month` (January by default).

**Money is an integer number of cents** (`BIGINT`), and there is no `f64` in
this crate — a JSON body that sends a float is refused by the deserializer
before a handler sees it. An amount on the wire is `amount_cents` (an integer)
or `amount` (a dollar string like `"12.50"`, parsed exactly; anything finer than
a cent is refused rather than rounded). **A balance is derived, never stored**:
`finance.funds` holds identity and no balance column, a fund's figure is
`SUM(amount_cents)` over its rows, a budget's actual is the same sum filtered,
and a member's `paid_cents` is the sum of the payments tagged to them.

**A transfer is two ledger entries written by one statement.** Each host
database call is its own statement, so `POST /api/finance/transfer` is a single
`INSERT … SELECT` over `unnest`ed arrays that writes both legs (`-a` and `+a`)
under one group id, guarded so a missing fund or an unauthorised overdraft
inserts both legs or neither. `ledger_integrity` re-checks on demand that every
group still has exactly two entries summing to zero, and `/api/finance/health`
reports that verdict.

**The sliding scale is honor-system.** A scout self-reports their tier
(`patron`, `standard`, `supported`, `hardship`); nobody verifies income, and
this crate has no field, route or code path for doing so. The mandatory minimum
is `$0`, so hardship never removes a scout, and a self-report never sets the
base cost it is a fraction of — that is the treasurer's number.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| GET | `/api/finance/funds?include_inactive=` | `finance:read` (troop) | The funds and their derived figures, plus `total_cents`; `include_inactive=true` adds retired funds |
| POST | `/api/finance/fund` | `finance:manage` (troop) | `{kind, code?, name?, purpose?, restricted?, target_cents?}` — `kind` is one of the six, `code` a lowercase slug (defaults to the kind). The six SPEC funds are seeded; this adds a seventh envelope without inventing a seventh kind. `409` when the code is taken. Publishes `finance.fund.created` |
| GET | `/api/finance/fund/{id}?fiscal_year=` | `finance:read` (troop) | The fund, its figures, its budget lines with variance, and its most recent 20 entries |
| PATCH | `/api/finance/fund/{id}` | `finance:manage` (troop) | Any subset of `{kind?, name?, purpose?, restricted?, active?, target_cents?}`; `code` is immutable, because a config names the fund by it. `400` when no editable field is supplied |
| GET | `/api/finance/transactions?fund_id=&fiscal_year=&kind=&category=&member_id=&transfer_group=&from=&to=&before_id=&limit=` | `finance:read_all` (troop) | The ledger page, newest first; `limit` defaults to 50 and is capped at 200, `has_more` + `next_before_id` page it, and the unfiltered-by-page total comes back as `filtered_total_cents` |
| POST | `/api/finance/transaction` | `finance:write` (troop) | `{fund_id, kind: income\|expense, amount_cents\|amount, category?, description?, member_id?, occurred_on?, fiscal_year?, allow_overdraft?, external_ref?}` — the amount is a **magnitude** and the sign comes from `kind`. `409` on an unauthorised overdraft, `200 {duplicate: true}` for a replayed `external_ref`. Publishes `finance.transaction.recorded` |
| POST | `/api/finance/transfer` | `finance:write` (troop) | `{from_fund_id, to_fund_id, amount_cents\|amount, description?, occurred_on?, fiscal_year?, allow_overdraft?}` — both legs under one `transfer_group`, or neither. Publishes `finance.transfer.recorded` |
| GET | `/api/finance/budgets?fiscal_year=` | `finance:read` (troop) | Budget lines against their ledger actuals, with favourable-positive variances; omitting `fiscal_year` returns every year |
| POST | `/api/finance/budget` | `finance:manage` (troop) | `{fund_id, direction: income\|expense, amount_cents\|amount, category?, fiscal_year?, note?}` — one fund, one year, one direction, one category, so posting the same line again revises it rather than adding a second. Publishes `finance.budget.set` |
| GET | `/api/finance/sliding-scale?base_cents=` | `finance:read` (troop) | The whole scale for a base cost (the configured membership cost by default): each tier's share, assessment and description, and the `$0` minimum |
| POST | `/api/finance/dues/assess` | `finance:manage_dues` (troop) | `{member_id, tier, fiscal_year?, base_cents?, lodge_id?, status?: assessed\|self_reported\|waived, note?}` — the treasurer's route; a waiver always assesses zero. Publishes `finance.dues.assessed` |
| POST | `/api/finance/dues/lodge` | `finance:manage_dues` **covering that Lodge** | `{lodge_id, share_bps\|share_percent, fiscal_year?, base_cents?, note?}` — a Lodge's levy as a fraction of the membership cost (at most 100 000 bp = 1000%). Publishes `finance.dues.assessed` |
| GET | `/api/finance/dues/lodge/{lodge}?fiscal_year=` | `finance:read` **covering that Lodge** | The Lodge's levy, its members' standings from the ledger, and the per-member levy |
| GET | `/api/finance/dues?fiscal_year=&lodge_id=&tier=&status=` | `finance:read_all` (troop) | Every member assessment for the year with `paid_cents`/`outstanding_cents` derived from the ledger, totalled and grouped by tier, capped at 500 rows |
| GET | `/api/finance/dues/member/{member}?fiscal_year=` | `finance:read` at any scope for **your own** record; `finance:read_all` (troop) for anybody else's | One scout's assessment, derived standing, and dues payments |
| POST | `/api/finance/dues/self-report` | `finance:self_report` at any scope — yourself only; a `member_id` naming somebody else needs `finance:manage_dues` | `{tier, fiscal_year?, note?, member_id?}` — the honor system's one write. Never sets the base cost, and `409` when there is no assessment and no configured membership cost. Publishes `finance.dues.self_reported` |
| POST | `/api/finance/dues/payment` | `finance:write` (troop) | `{member_id, amount_cents\|amount, fiscal_year?, fund_id?, occurred_on?, description?, external_ref?}` — an income entry tagged `dues` in the configured dues fund, which is what moves a member's standing. Publishes `finance.dues.payment` |
| GET | `/api/finance/report/annual?fiscal_year=` | `finance:read_all` (troop) | The Annual Financial Report: per-fund opening/income/expense/transfers/closing, the budget lines with variances, the dues section, and the ledger's integrity verdict |
| GET | `/api/finance/health` | `finance:read` (troop) | Re-derives the books: every transfer group two entries summing to zero, and the ledger total equal to the sum of every fund's balance |

**Events:** `finance.fund.created`, `finance.transaction.recorded`,
`finance.transfer.recorded`, `finance.budget.set`, `finance.dues.assessed` (a
member assessment and a Lodge levy both), `finance.dues.self_reported`,
`finance.dues.payment`, `finance.payment.recorded` (a booked
`payment.received`), and `finance.ledger.imbalanced` — which the schedule below
publishes only when the books do not add up.

**Subscribes to:** `payment.received` (SPEC §5.4, from a payments plugin) → one
income entry in the fund the payment names (or `dues_fund_code`, General by
default), keyed on the provider's `payment_id` in the unique `external_ref`, so
a replayed event is a no-op rather than a second deposit. `external_ref` is also
how the ledger route and the dues-payment route detect a replay.

**Schedules:** `ledger_audit`, daily — re-derives the totals and the transfer
groups and stays silent unless something is wrong.

**The six funds.** Migration 1 seeds General, Scholarship, Equipment,
Expedition, Impact and Commencement; `kind` is one of those six and `code` is
the stable slug the API addresses, so a troop adds a fund without inventing a
seventh kind. Categories are otherwise free text, but `dues` and `transfer` are
reserved: the first is what a member's payment standing derives from, the second
is what both legs of a transfer are filed under.

**Reads are scoped.** Money is sensitive, so balances and the sliding scale are
`finance:read`, while the ledger, the dues list and the Annual Financial Report
are `finance:read_all`. A member's own dues are readable with `finance:read` at
any scope — their own record is an ownership check, not a grant — and anybody
else's needs `finance:read_all` covering the troop. The two Lodge routes and the
self-report route declare the permission at *some* scope and check the object in
the handler; every other route here demands a **troop-covering** grant, and none
of them is destructive.

**The fiscal year.** `fiscal_year` is accepted between 2000 and 2200 and derived
from the entry's date against `fiscal_year_start_month` when it is not stated,
so a July start puts a March entry in the year before. The Annual Financial
Report's period is the first and last day of that year inclusive.

**Role grants.** `chief` is seeded with every permission the core finds; the
rest is the operator's, as for the other plugins. A `scout` needs `finance:read`
and `finance:self_report` to see the scale and choose their own tier, and a
`lodge_commander` a Lodge-scoped `finance:read` to read their Lodge's levy and
members' dues; `finance:write`, `finance:manage`, `finance:manage_dues` and
`finance:read_all` are the treasurer's and the Finance Subcouncil's to hold:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'finance:read'),
  ('scout',           'finance:read'),
  ('scout',           'finance:self_report')
ON CONFLICT DO NOTHING;
```

### Equipment (SPEC §7.6)

The gear pool. SPEC §7.6 gives the plugin five responsibilities — an inventory
catalogue (item, condition, location), checkout/checkin tracking, maintenance
schedules, replacement flagging, and the availability view a mission planner
asks for — and this is those five over the schema `equipment.items` and
`equipment.checkouts`.

**An item is one physical thing, not a quantity.** Three identical tents are
three rows, which is what lets a checkout name the actual item: "who has the
good tent" has an answer, and damage has a period of use to attach to. No
count is ever stored where it could drift — `service_count` is the one
accumulator, and it is incremented by a checkin. Everything else the API
reports (`in_pool`, replacement candidates, availability) is derived from the
rows at read time.

**Every permission here is troop-scope**, so the table does not repeat it per
row: SPEC §7.6 gives equipment no Lodge-scoped authority, and `location` is
where gear physically lives rather than who owns the decision about it. Every
route is declared with a `*_protected` constructor; all four permissions come
from `permissions_granted()`.

Config lives in the `equipment` row's `core.plugins.config`
(`{"replacement_service_count": 40, "replacement_age_years": 10,
"replacement_conditions": ["poor", "unserviceable"], "maintenance_lead_days": 14,
"overdue_grace_days": 0}`); every value is clamped to a sane range, so a typo
cannot flag the whole catalogue.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/equipment/item` | `equipment:write` (troop) | `{name, asset_tag?, category?, description?, condition?, location?, acquired_on?, source?, next_service_on?, replacement_note?}` — one row per physical thing; `condition` defaults to `good`, `category` to `other`. A duplicate `asset_tag` is a `409` |
| GET | `/api/equipment/items?status=&category=&location=&q=&include_retired=&limit=` | `equipment:read` (troop) | The catalogue. Retired items are excluded unless `include_retired=true`; `q` matches name or asset tag; categories are a fixed vocabulary |
| GET | `/api/equipment/availability?from=&to=&days=&category=&location=&include_retired=&limit=` | `equipment:read` (troop) | "What can I take on these dates": the pool partitioned into `available` and `unavailable`, each refusal with its `reasons` and, when the item is out, who holds it and when it is due back. Defaults to the next 7 days; the window is inclusive and capped at 365 days |
| GET | `/api/equipment/replacements?limit=` | `equipment:read` (troop) | Replacement candidates, worst first, each with the reasons that fired |
| GET | `/api/equipment/maintenance?limit=` | `equipment:read` (troop) | The maintenance picture in buckets: `in_service`, `overdue`, `due`, `scheduled`, `needs_schedule`, with counts and the thresholds it was computed with |
| GET | `/api/equipment/checkouts?state=&item_id=&member=&mission_id=&overdue=&limit=` | `equipment:read` (troop) | The checkout log — who has what. `state` is `all` (default), `open` or `closed`; `overdue=true` narrows to open checkouts past their due date by the grace period |
| GET | `/api/equipment/item/{id}` | `equipment:read` (troop) | The item, its open checkout, the last 20 checkouts, and the derived `flags` (pool membership, replacement, maintenance) |
| GET | `/api/equipment/item/{id}/history?limit=` | `equipment:read` (troop) | The full checkout log for one item, newest first |
| PATCH | `/api/equipment/item/{id}` | `equipment:write` (troop) | Any subset of the create body plus `replacement_flagged` (the human decision). Deliberately cannot change `status`: leaving the pool, coming back and being retired are transitions with their own routes. An empty patch is a `400` |
| POST | `/api/equipment/item/{id}/checkout` | `equipment:checkout` (troop) | `{checked_out_by?, due_on?, purpose?, mission_id?, destination?, condition?, note?}` — the holder defaults to the caller; `condition` (the grade it leaves in) defaults to the item's current grade. `409` when the item is retired, in maintenance, unserviceable, or already out to somebody else; `400` when `due_on` is in the past |
| POST | `/api/equipment/item/{id}/checkin` | `equipment:checkout` (troop) | `{condition, note?, damaged?}` — `condition` is required, and closes the open checkout. `damaged` defaults to "the grade got worse than `condition_out`"; `409` when there is no open checkout |
| POST | `/api/equipment/item/{id}/maintenance` | `equipment:manage` (troop) | `{reason, until?, next_service_on?}` — out of the pool, not lost. `409` when the item is already in maintenance, is retired, or is physically out |
| POST | `/api/equipment/item/{id}/return-to-service` | `equipment:manage` (troop) | `{condition?, note?, next_service_on?}` — back into the pool, and the way back from a mistaken retirement |
| POST | `/api/equipment/item/{id}/schedule-service` | `equipment:manage` (troop) | `{on, note?}` — a service date on an item that stays in the pool; `on` in the past is a `400` |
| POST | `/api/equipment/item/{id}/retire` | `equipment:manage` (troop) | `{reason}` — out of the pool for good. `409` when the item is already retired or is checked out |
| DELETE | `/api/equipment/item/{id}` | `equipment:manage` (troop), **troop-covering** | Only ever an entry with no checkout history — deleting one that was used would delete the record of who held what. `409` tells the caller to retire it instead |

**Events:** `equipment.item.created`, `equipment.item.updated`,
`equipment.item.retired`, `equipment.item.deleted`, `equipment.checked_out`,
`equipment.checked_in`, `equipment.maintenance.flagged`,
`equipment.maintenance.cleared`, `equipment.maintenance.scheduled`,
`equipment.replacement.flagged`, `equipment.maintenance.due` (the daily
`maintenance_due` schedule, which stays silent when nothing is due — an empty
reminder every morning is how a troop learns to ignore reminders).

**Subscribes to:** nothing. The plugin reacts to no other plugin's events; the
only thing that runs without a caller is its own daily maintenance schedule.

**Checkout and checkin is a state machine, and the database enforces it.** An
item is *out* exactly while it has an open checkout (`checked_in_at IS NULL`).
Two rules follow, and each is enforced twice: the handler checks first so the
caller gets a `409` naming the holder, and the constraint is the backstop for a
race or a direct SQL session. An item cannot be checked out twice
(`idx_checkouts_open_item` is a partial unique index on `item_id WHERE
checked_in_at IS NULL`), and a checkin must reference an open checkout
(`checkouts_returned_consistent` makes `checked_in_at` and `condition_in`
arrive together), so a checkin with nothing open is a `409`, never a silent
second row. The log is append-only in practice: rows are only ever closed, and
only deleted with their item.

**Condition is recorded at both ends, so damage is attributable.** Every
checkout records `condition_out` and every checkin records `condition_in`,
which is required — a checkin that does not state a condition cannot be
attributed, which is the point of recording it. The pair brackets a period of
use, a downgrade between them happened while that member had it, and the checkin
carries the grade forward onto the item and increments `service_count`.

**Maintenance is not an automatic expiry.** `maintenance_until` is the date the
troop expects the item back, not a date on which it silently becomes available
again: a month in a repair queue is not a month of availability. Separately,
`next_service_on` is a schedule on an item that is still in the pool (oil the
lantern), surfaced here and by the daily event.

**Replacement flagging is derived, with a human override.** An item is a
candidate when a rule fires — condition `poor`/`unserviceable`, or
`service_count` / age crossing a threshold — and that set is computed, never
stored, so fixing the condition removes the candidate. `replacement_flagged` is
the separate human decision and shows up as its own reason. A checkin that
*creates* a candidate publishes `equipment.replacement.flagged` only on the
transition, so the event means "this just became a problem".

**An overdue open checkout blocks indefinitely.** A closed checkout occupies its
item up to its return date; an open one ends on its `due_on` only while that
date is still in the future, and otherwise blocks every window that starts on or
after it left. A due date is a promise, and one already broken is not a promise
a planner should plan against.

**Role grants.** `chief` is seeded with every permission the core finds; the
rest is the operator's, as for the other plugins. Taking gear out is a scout
action and correcting the catalogue is not, so a troop might start here:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('scout',           'equipment:read'),
  ('scout',           'equipment:checkout')
ON CONFLICT DO NOTHING;
```

`equipment:write` and `equipment:manage` are what the quarterly gear check
wants, and are usually held by the `chief`-equivalent role alone — note that
`equipment:manage` is also what `DELETE` requires, and that route will only
answer for a troop-covering grant.

### Calendar (SPEC §7.7)

Events at troop or Lodge scope, with iCal `RRULE` recurrence, RSVPs (per
occurrence for a recurring event, `EXDATE` for a cancelled meeting), and the
Congress quorum projection.

**Every timestamp on this API is a wall clock** — `YYYY-MM-DD`,
`YYYY-MM-DDTHH:MM` or `YYYY-MM-DDTHH:MM:SS`, never a UTC designator — and the
event's `timezone` (an IANA name PostgreSQL knows, e.g. `America/New_York`)
goes with it. That is what "Tuesdays at 7pm" means to a troop: the local time is
fixed and its UTC offset moves with DST. Occurrence times come back as wall
clock too, beside the base occurrence's absolute `starts_at`.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/calendar/event` | `calendar:create` at the event's scope | `{title, starts_at, scope_type?: troop\|lodge, scope_id?, category?, body?, meeting_id?, description?, location?, timezone?, all_day?, ends_at?, rrule?, exdates?, quorum_basis?, expected_voters?, quorum_required?}` |
| GET | `/api/calendar/events?status=&scope_type=&scope_id=&body=&category=&from=&to=&limit=` | `calendar:read` | Narrowed to the scopes the caller covers. `status` defaults to `scheduled`; `all` includes cancelled |
| GET | `/api/calendar/upcoming?days=&category=&scope_type=&scope_id=&limit=&total=&from=` | `calendar:read` | The home screen: the next occurrences across every visible event, **plus the seasonal prompts** due in the same window |
| GET | `/api/calendar/season?on=&days=` | `calendar:read` (troop) | `on=2026-10` for a month view, `on=2026-10-01&days=90` for a window |
| GET | `/api/calendar/event/{id}` | `calendar:read` | Event + RSVPs + counts + quorum |
| GET | `/api/calendar/event/{id}/occurrences?from=&limit=` | `calendar:read` | The expanded series, `EXDATE`s removed |
| GET | `/api/calendar/event/{id}/quorum?occurrence=` | `calendar:read` | The quorum projection for one occurrence |
| POST | `/api/calendar/event/{id}/rsvp` | `calendar:rsvp` at the event's scope | `{response: going\|not_going\|maybe\|pending, occurrence?, note?}` — omitted `occurrence` answers for the whole series; a re-answer updates |
| POST | `/api/calendar/event/{id}/rsvp/{member}` | `calendar:manage` | The same body, recorded for another member (the phoned-in RSVP) |
| GET | `/api/calendar/event/{id}/rsvps` | `calendar:read` | Every answer, plus the counts for the next occurrence |
| PATCH | `/api/calendar/event/{id}` | `calendar:manage` at the event's scope | Any subset of the create body; validated against the whole event |
| POST | `/api/calendar/event/{id}/cancel` | `calendar:manage` at the event's scope | Cancels the series, keeping its RSVPs and the record |
| POST | `/api/calendar/event/{id}/occurrence/cancel` | `calendar:manage` at the event's scope | `{occurrence, reason?}` — an `EXDATE`; the rest of the series stands |
| DELETE | `/api/calendar/event/{id}` | `calendar:manage`, **troop-covering** | Destructive routes are never available from a Lodge grant (§8 #3) — cancel instead |

**Events:** `event.created` (the SPEC §5.4 type), plus `event.updated`,
`event.cancelled`, `event.occurrence.cancelled`, `event.rsvp`, `event.deleted`,
`event.season.upcoming` (the weekly seasonal reminder).

**Subscribes to:** `mission.completed` → a provisional debrief event one week
later at 6pm, in the mission's Lodge. `source_mission_id` is unique, so a
replayed event does not create a second debrief.

**Quorum.** `quorum_basis` reuses governance's vocabulary (`one_third_registered`
— the Congress rule the 3rd Congress locked, `ceil(expected/3)`;
`majority_members`; `fixed`) and adds `none` for an ordinary event. The arithmetic
is governance's, so the two plugins cannot disagree about what "one-third of
registered scouts" is; what differs is the input. Calendar counts **intent**
(RSVPs marked `going` for one occurrence, with series-level answers counting for
every occurrence and an occurrence-specific answer overriding them) and names it
a projection; governance counts **attendance**, which is the number that decides
a motion. A Congress event carries governance's `meeting_id`, and the quorum
response returns `governance_quorum`
(`/api/governance/meeting/{id}/quorum`) so a client shows both. Registered-scout
counts are recorded on the event (`expected_voters`) because a plugin role can
only read its own schema; an unconfigured rule fails closed.

**Role grants.** `chief` is seeded with every permission the core finds; the rest
is the operator's, as for the other plugins:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'calendar:read'),
  ('lodge_commander', 'calendar:create'),
  ('lodge_commander', 'calendar:manage'),
  ('scout',           'calendar:read'),
  ('scout',           'calendar:rsvp')
ON CONFLICT DO NOTHING;
```

### Archive (SPEC §7.8)

The troop's memory: Congress proceedings, Troop Council and Lodge minutes, the
motions proposed and the decisions taken about them, policies (including each
adopted version of the Accords), mission reports and the impacts they reported,
plus the correspondence and notes the troop chose to keep. It owns full-text
search over those records, one chronological timeline across every kind, and the
typed edges of the SPEC's chain `decisions → policies → missions → impact`. It
also accrues records by itself — governance's motions and missions' completions
are filed without anyone re-typing a minute.

**Records are immutable, and it is enforced, not promised.** There is no route
that modifies or deletes a record: the whole surface is `POST` (file), `POST
…/supersede` (correct) and `DELETE …/link/{id}` (unlink a mis-typed edge, which
is not a record). Migration 2 installs a `BEFORE UPDATE OR DELETE` trigger on
`records` that raises unconditionally, so a future route, a native plugin or a
hand-typed `psql` UPDATE fails loudly with a message that names the correction
route instead. A correction is a new record carrying `supersedes_id` and a
required `reason`; the original survives with its own fingerprint, and its
`superseded_by` is derived from the correction rather than stored, so there is no
bookkeeping pointer that can fall out of step and no UPDATE anywhere in the
plugin.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/archive/record` | `archive:write` at the record's scope | `{kind, title, summary?, body?, body_code?, outcome?, scope_type?: troop\|lodge, scope_id?, occurred_at?, source?, source_ref?, source_url?}` — `kind` is one of the nine the database accepts; `occurred_at` defaults to now; a `body_code` (`congress\|tc\|lodge\|committee`) is troop-wide and cannot be scoped to a Lodge; a repeated `source_ref` is `409` |
| POST | `/api/archive/record/{id}/supersede` | `archive:write` at the record's scope | `{reason, title?, summary?, body?, body_code?, outcome?, kind?, occurred_at?}` — files the correction; `reason` is required, omitted fields inherit the original (including its `occurred_at` and scope); correcting an already-corrected record is `409` and names the current version |
| GET | `/api/archive/records?kind=&body=&outcome=&scope_type=&scope_id=&from=&to=&source=&source_ref=&cursor=&limit=&include_superseded=` | `archive:read` | The catalogue, newest first; `kind` takes a comma-separated list; live versions only unless `include_superseded=true`; `cursor` pages it and the next one comes back as `next_cursor`; `limit` defaults 50, caps at 200 |
| GET | `/api/archive/record/{id}` | `archive:read` | The record, its direct edges with one sentence each (`steps`, `why_it_exists`, `what_it_produced`), and `current` / `superseded_by` |
| GET | `/api/archive/record/{id}/chain` | `archive:read` | The correction history walked both ways: `original_id`, `current_id`, `corrected`, `corrections`, `ancestors`, `descendants`, capped at 50 steps per leg |
| GET | `/api/archive/timeline?kind=&body=&outcome=&scope_type=&scope_id=&from=&to=&cursor=&limit=&include_superseded=` | `archive:read` | Every kind in one order (`occurred_at DESC, id DESC`), each item carrying its `kind_label`, with `counts_by_kind` for the page, `order`, and the `scope` the caller was read at |
| GET | `/api/archive/stats?scope_type=&scope_id=` | `archive:read` | Totals (records, corrections, superseded, without_links_out, sources, earliest, latest), `by_kind`, `by_body`, `by_year`, and `kinds_without_records` — the archive keeper's gaps |
| GET | `/api/archive/vocabulary` | `archive:read` | The codes and what they mean: kinds and labels, body codes, outcomes, scope types, sources, every relation with its phrase, meaning and expected endpoint kinds, the chain, the search modes, the timeline order and the window rule. No database, still gated — an open vocabulary is a free map of the troop |
| GET | `/api/archive/search?q=&mode=&kind=&body=&outcome=&scope_type=&scope_id=&from=&to=&limit=&include_superseded=` | `archive:search` | `q` is required; `mode=websearch` (the default — quoted phrases, `OR`, `-exclude`) or `all_words`; every result carries `rank` and a `ts_headline` `snippet` with the match marked; the same visibility predicate as the reads |
| POST | `/api/archive/record/{id}/link` | `archive:link` covering **both** records' scopes | `{to_id, relation, note?}` — `relation` is one of the seven; linking a record to itself is `400`; re-linking a pair updates the `note` and keeps the edge (`200`, idempotent); an endpoint pair the relation does not usually join is stored as given and returned with `advice` |
| GET | `/api/archive/record/{id}/links` | `archive:read` | Every edge touching the record, both directions, with the far record's kind, title and occurrence already joined on |
| GET | `/api/archive/record/{id}/lineage?direction=both\|upstream\|downstream&depth=` | `archive:read` | The chain around the record: `nodes`, `upstream`, `downstream`, and one plain-language `explanation` sentence per step; `depth` defaults 3, caps at 6, and a cycle guard bounds the walk |
| DELETE | `/api/archive/link/{id}` | `archive:manage`, **troop-covering** | Removes a relationship drawn in error and audits what it said; the two records are untouched (`404` if there is no such link) |

**Events:** `archive.record.filed` (every filing, by hand or by ingestion),
`archive.record.superseded`, `archive.link.created`, `archive.link.deleted`.

**Subscribes to:** `motion.` — `motion.proposed` files a `motion` record, and
`motion.passed` / `motion.failed` file a `decision` record linked `outcome` from
the motion, because a decision is not an edit of the motion it decides.
`accords.` — `accords.adopted` files a `policy` record linked `amends` to the
previous version. `mission.` — `mission.completed` files a `mission_report`
record, plus an `impact` record linked `produces`. The prefix filters are
deliberate: a future `motion.amended` lands in a handler that already knows how
to file a motion. Every ingested record carries a `source_ref`
`<plugin>:<entity>:<id>` under a unique index, and every write is `ON CONFLICT
(source_ref) DO NOTHING`, so a redelivered event finds the row, returns none and
files nothing; a manual record's `source_ref` is NULL, which no unique index
constrains, so hand-filed records never collide.

**Search.** The `search` column is a generated, weighted `tsvector` (title `A`,
summary `B`, body `D`) under a GIN index, and the predicate is `search @@
websearch_to_tsquery('english', q)` — or `plainto_tsquery` in `all_words` mode —
ordered by `ts_rank_cd`, never a `LIKE` scan. Each result carries its `rank` and
a `ts_headline` snippet, so the caller sees why a record matched. A query that
parses to no terms (stopwords only) returns zero matches beside the parsed query
and a note, rather than looking like an empty archive.

**Timeline and windows.** One order for every kind, `occurred_at DESC, id DESC`,
with a keyset cursor `<rfc3339>|<id>` returned as `next_cursor` — both halves,
so two records filed in the same second still page deterministically and a decade
of history pages stably while new records are filed. `from`/`to` are
**half-open** (`from <= occurred_at < to`), so `to=2026-11-01` means "up to *the
start of* that day"; every response that applied a window says so with
`"to_exclusive": true`. Lists default to the live version of each record;
`include_superseded=true` brings the corrected ones back.

**Relationships.** Edges read `from <relation> to` and always point cause →
effect: `outcome` (motion → decision), `decides` (decision → policy),
`authorizes` (policy → mission_report), `produces` (mission_report → impact),
plus `amends` (an earlier version), `documents` (minutes → the record they
record) and `relates_to` (an edge the troop drew by hand). `GET …/lineage` walks
the graph in both directions with a recursive CTE and returns one sentence per
step, so *"why does this policy exist?"* is the `upstream` half and *"what did
this decision actually produce?"* is the `downstream` half.

**Scope.** Every read is scoped exactly as the rest of the system is (SPEC
§9.2): a troop-covering grant sees the whole archive, a Lodge-covering grant sees
troop-wide records plus its own Lodge's, and the author always sees what they
filed — including after a grant is narrowed. The same predicate is applied inside
`/search`, because a search that leaked a Lodge's minutes to the troop would undo
the point of scoping them. Absent and forbidden are the same `403` (`no record
{id}, or you cannot see it`), so a status code never confirms that a record the
caller cannot read exists.

**Role grants.** `chief` is seeded with every permission the core finds; the rest
is the operator's, as for the other plugins. The write permissions are meant for
an archive keeper and the chairs rather than every scout, and a Lodge-covering
grant files Lodge records only — the handler checks the record's own scope, which
is why `POST /api/archive/record` is declared `any_scope` and takes no
`required_scope`. `archive:manage` is turned troop-covering by the SDK's delete
constructor, so unlinking is never available from a Lodge grant (SPEC §8 #3):

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'archive:read'),
  ('lodge_commander', 'archive:write'),
  ('lodge_commander', 'archive:link'),
  ('scout',           'archive:read'),
  ('scout',           'archive:search')
ON CONFLICT DO NOTHING;
```

### Conflicts (SPEC §7.9)

A dispute is resolved as close to the parties as it can be, and escalates only
when the stage below it genuinely fails: `direct_conversation → facilitation →
arbitration → troop_council`. The pathway is forward-only — a stage may be
skipped (the reason recorded with the move accounts for it) but never reversed,
so a stage that failed stays in the case's history rather than being replaced by
the escalation that followed it.

**A case is private to its parties and to the facilitators assigned to it.**
Nothing here grants read access by role, by troop or by Lodge: "can read this
case" is an object-level check against the case's own `party_ids` and
`facilitator_ids`, and a case is deliberately not modelled as a troop or Lodge
scope at all. `conflicts:manage` staffs a case but does not read it, and a manage
holder may not appoint themselves as facilitator — so there is no route from
"administers the pathway" to "reads every dispute". Absent and forbidden answer
the same `403`, so case ids cannot be probed. Every event is an opaque reference:
a case id and a stage, never a party.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/conflicts/case` | `conflicts:file` | `{title, summary, parties?, reason?}` — the filer becomes a party by construction, and the case starts **unstaffed** |
| GET | `/api/conflicts/cases?status=&role=&stage=&limit=` | `conflicts:read_own` | The cases the caller has standing in — as a party or as an assigned facilitator. Defaults to `open`; `status=all` for everything |
| GET | `/api/conflicts/case/{id}` | `conflicts:read_own` | The case, the caller's standing, and what they may do with it (`may`: `advance`, `record_resolution`, `record_agreement`, `add_party`, `withdraw`) |
| GET | `/api/conflicts/case/{id}/log` | `conflicts:read_own` | The append-only history: who moved it, from where to where, when, and why |
| POST | `/api/conflicts/case/{id}/stage` | `conflicts:facilitate` | `{stage, reason}` — forward only; a skip is allowed and the reason is what accounts for it. Restarts the stage's dropout clock |
| POST | `/api/conflicts/case/{id}/resolution` | `conflicts:facilitate` | `{outcome, agreement?, reason?}` — the facilitator closes the case at whatever stage it reached. The outcome is what was agreed, not a verdict against a person |
| POST | `/api/conflicts/case/{id}/agreement` | `conflicts:file` | `{agreement, outcome?, reason?}` — a **party** records the agreement the parties reached themselves; allowed at the entry stage only, because from any later stage a third party is in the room and the record is theirs to keep |
| POST | `/api/conflicts/case/{id}/withdraw` | `conflicts:file` | `{reason}` — a party stops the case. Withdrawn, never deleted: the record stays, and either party may file again |
| POST | `/api/conflicts/case/{id}/facilitator` | `conflicts:manage` | `{user_id, action?: assign\|release, reason?}` — staffing. Self-appointment is refused, and the appointment is recorded in the case's log naming both the appointer and the appointee |
| POST | `/api/conflicts/case/{id}/party` | `conflicts:file` | `{user_id, reason}` — somebody already on the case widens its visibility list. The reason is mandatory and lands in the log, because widening it hands over the record |
| GET | `/api/conflicts/stalled?hours=&limit=` | `conflicts:facilitate` | The cases the caller carries that have stopped moving: opaque id, stage, age, nudge count — metadata only |

**Events:** `conflict.filed`, `conflict.escalated` (the SPEC §5.4 type),
`conflict.resolved`, `conflict.withdrawn`, `conflict.stage.stalled`,
`conflict.staffing.changed`, `conflict.party.added`. None carries a party id, a
name or a case's text; a consumer that needs detail goes through the case, with
standing.

**Subscribes to:** nothing, on purpose. The pathway is moved by people, and an
event that could advance a case or widen its visibility would be an unaudited
actor on a private record. The anti-dropout mechanism is a schedule, not a
reactor.

**The ledger is append-only, and the database enforces it.** Every move writes a
row in `conflicts.stage_log` carrying the actor, the from/to stage and a
mandatory reason, and migration 2 installs a `BEFORE UPDATE OR DELETE` trigger
that raises unconditionally — so the history cannot be rewritten by a route, by
a native plugin, or by hand in `psql`. `stage_log` is the one place a party id
lives outside the case itself, which is why it is reachable only through the
case.

**Anti-dropout.** The `stage_nudge` schedule runs every six hours: an open case
whose stage has sat beyond `stage_stall_hours` (default 168) and whose
`nudge_cooldown_hours` (default 72) has expired gets a `nudge` entry in its
ledger (actor `conflicts:auto`) and a `conflict.stage.stalled` event, and appears
in `GET /api/conflicts/stalled` for the facilitator who carries it. A case with
no facilitator yet is still nudged: the ledger entry is the record that nobody
has picked it up.

**Role grants.** `chief` is seeded with every permission the core finds; the rest
is the operator's, as for the other plugins. A facilitator is a member too, so
the role carries the member-level permissions alongside the one that lets them
carry a case:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('conflict_facilitator', 'conflicts:facilitate'),
  ('conflict_facilitator', 'conflicts:file'),
  ('conflict_facilitator', 'conflicts:read_own'),
  ('troop_council',        'conflicts:manage'),
  ('scout',                'conflicts:file'),
  ('scout',                'conflicts:read_own')
ON CONFLICT DO NOTHING;
```

## Hermes MCP plugin (SPEC §7.10)

The permissions-aware MCP surface Hermes connects to. Every tool **is** an
Adjutant API call: the plugin checks the permission the tool's route requires,
then calls the route over the core's mediated HTTP client with the caller's own
`authorization`/`cookie` headers forwarded — so the core's route gate checks the
same permission a second time, and the agent path and the UI path meet at one
authorization decision. The plugin holds no credentials of its own, so an
invocation can never exceed the calling user's authority.

`GET /api/mcp/tools` lists only the tools the caller may invoke (a tool whose
plugin is not installed is invisible: no role holds its permission). Every
invocation — including every refusal — is recorded in `mcp.invocations` and in
`core.audit_log`.

Config lives in the `mcp` row's `core.plugins.config`:
`{"base_url": "http://127.0.0.1:8787", "connection_ttl_hours": 24, "max_result_bytes": 65536,
"tools": {"disable": [], "override": {}, "add": []}}`.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/mcp/connect` | `mcp:connect` | `{client?, client_version?, protocolVersion?, capabilities?}` → `{connection_id, token, expires_at, protocolVersion, server, capabilities}`. Only the token's SHA-256 is stored; the token is returned once |
| GET | `/api/mcp/tools` | `mcp:connect` | The filtered catalogue: `{name, description, inputSchema, requiredPermission, scope}` per tool, plus `warnings` |
| POST | `/api/mcp/invoke` | `mcp:invoke` | `{tool, arguments, connection_id?}` or a connection token in `connection_token` / the `mcp-session-id` header → `{tool, status: ok\|denied\|error, http_status?, result?, error?, duration_ms, invocation_id}`. The tool's own permission is re-checked, arguments are strictly validated against the tool's schema, and the API's status is passed through |
| GET | `/api/mcp/invocations?tool=&status=&limit=` | `mcp:connect` (own rows) / `mcp:audit` (troop) | The audit trail: user, connection, tool, arguments, the downstream request, status, HTTP status, duration |

Tools (path and permission mirror the owning plugin's route exactly):

| Tool | Route | Permission |
|---|---|---|
| `membership_list_members` | `GET /api/membership/members` | `membership:read_all` (troop) |
| `membership_get_member` | `GET /api/membership/member` | `membership:read` (object scope) |
| `missions_list_missions` | `GET /api/missions/missions` | `missions:read` (object scope) |
| `missions_get_mission` | `GET /api/missions/mission/{id}` | `missions:read` (object scope) |
| `missions_create_mission` | `POST /api/missions/mission` | `missions:create` (object scope) |
| `governance_list_motions` | `GET /api/governance/motions` | `governance:read` (troop) |
| `governance_get_motion` | `GET /api/governance/motion/{id}` | `governance:read` (troop) |
| `governance_create_motion` | `POST /api/governance/motion` | `governance:propose` (troop) |
| `calendar_list_events` | `GET /api/calendar/events` | `calendar:read` (object scope) |
| `calendar_get_event` | `GET /api/calendar/event/{id}` | `calendar:read` (object scope) |
| `calendar_create_event` | `POST /api/calendar/event` | `calendar:create` (object scope) |

Permissions this plugin defines: `mcp:connect`, `mcp:invoke`, `mcp:audit`.

### Announcements (SPEC §7.14)

Troop and Lodge notices in the troop's three categories — `urgent`,
`informational`, `event` — with an inbox, a per-member unread badge and read
receipts. SPEC §7.14 also lists push notifications; no push provider is wired
anywhere in Adjutant and the core has no delivery channel, so this plugin does
not pretend. It records the announcement, its category, its scope and every
receipt, publishes `announcement.published` with everything a sender would need,
and says `"delivery": "deferred…"` in the responses it returns — recorded, never
"sent". A future notification-delivery plugin subscribes to that event; until one
exists the event is the seam.

Authority and audience are two different rules, and the difference is visible on
every read route. Writing, editing, retracting and reading the receipt list use
the scoped model's **coverage** — a troop-wide grant covers every Lodge — but
visibility does not, because an announcement is addressed to a scope and must not
leak past it: the caller has to be addressed by the announcement's own scope, so
a troop-wide notice reaches troop-addressed readers and a Lodge 3 notice reaches
Lodge 3 readers, and neither sees the other.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/announcements/announcement` | `announcements:write` at the announcement's scope | `{title, body?, category?: urgent\|informational\|event, scope_type?: troop\|lodge, scope_id?, publish?: bool, expires_at?, related_event_id?}` — `201` + `Location`. `publish: true` sends it at once (stamping `published_at`) and needs `announcements:publish_urgent` when the category is `urgent`; otherwise it is left a draft, which reaches nobody. An expiry already in the past is refused |
| GET | `/api/announcements/announcements?status=&category=&scope_type=&scope_id=&unread=&include_expired=&limit=&offset=` | `announcements:read` | The inbox, narrowed to the scopes the caller is addressed by. `status` defaults to `published` (`all` includes drafts and retracted); expired are hidden unless `include_expired=1`. Urgent sorts first, then `published_at DESC`. The badge beside it is the caller's whole count, not the filtered page's |
| GET | `/api/announcements/unread` | `announcements:read` | The badge on its own: `{member_id, visible, unread, read, urgent_unread, has_urgent, unread_by_category, addressed}` |
| GET | `/api/announcements/announcement/{id}` | `announcements:read` | The announcement + `is_read` + `my_receipt` (null when unread) + `read_count`. An announcement not sent to a scope you hold is a `403`, like one that does not exist |
| GET | `/api/announcements/categories` | `announcements:read` (**troop-covering**) | Reference data, no database: `{categories: [{code, label, description, requires_permission}], statuses, scopes}`. `urgent` names two required permissions, the other two name one |
| POST | `/api/announcements/announcement/{id}/publish` | `announcements:write` for your own draft, `announcements:manage` for somebody else's draft (at the object's scope) | Sends a draft and publishes `announcement.published` exactly once — the `UPDATE … WHERE status = 'draft'` makes a second send a `409`, so the seam cannot fire twice. An urgent draft needs `announcements:publish_urgent` again; a draft whose expiry has passed is a `409` |
| PATCH | `/api/announcements/announcement/{id}` | `announcements:write` for your own draft, `announcements:manage` otherwise (at the object's scope) | Any subset of `{title, body, category, scope_type, scope_id, expires_at, related_event_id}`, validated against the whole announcement; an empty patch is named rather than sent. Re-addressing needs `announcements:write` at the **new** scope |
| POST | `/api/announcements/announcement/{id}/retract` | `announcements:manage` at the object's scope | Withdraws a published announcement: out of the inbox and the badge, still visible to its audience marked `retracted`. Manage, not the sharp permission — even for an urgent one |
| DELETE | `/api/announcements/announcement/{id}` | `announcements:manage`, **troop-covering** | Erases the record and its receipts. Destructive routes are never available from a Lodge grant (§8 #3) — retract instead |
| POST | `/api/announcements/announcement/{id}/read` | `announcements:read`, addressed by the object's scope | `{via?}` (`api` by default, a short tag naming the surface) → `{receipt, is_read: true, already_read, unread}`. Idempotent; a draft has no readers to record (`409`) |
| POST | `/api/announcements/announcement/{id}/unread` | `announcements:read`, addressed by the object's scope | Clears the caller's own receipt → `{receipt: null, is_read: false, forgotten, unread}`. Forgetting twice is not an error and publishes nothing |
| GET | `/api/announcements/announcement/{id}/receipts` | `announcements:manage` at the object's scope | Who has read it: `{receipts, count, read_count, scope, status}`. Who read, never who has not — that answer needs the roster |

**Events:** `announcement.created` (drafted or sent), `announcement.published`
(the delivery seam, emitted once, when the announcement becomes visible to its
audience), `announcement.updated`, `announcement.retracted`,
`announcement.receipt` (only for a receipt actually created),
`announcement.unread`, `announcement.deleted`.

**Subscribes to:** nothing, and nothing is scheduled — the delivery seam is this
plugin's output, not its input. A digest that sweeps unread announcements belongs
to the delivery plugin that does not exist yet.

**Delivery is deferred.** Every write response carries `"delivery"` as
`deferred: no push provider is wired — announcement.published carries what a
delivery plugin needs`. The event's payload is the whole contract a sender needs
and nothing it does not: `announcement_id`, `title`, `preview` (up to 160
characters of the body, whitespace collapsed), `category`, `urgent`,
`scope_type`, `scope_id`, `published_at`, `published_by`, `expires_at`,
`related_event_id`.

**Urgent is hard to abuse.** Publishing an urgent announcement needs two
permissions at the announcement's scope — `announcements:write` *and*
`announcements:publish_urgent` — so the emergency authority is an addition to the
ordinary one, never a substitute. The gate is re-checked when a **published**
announcement is edited *into* the urgent category, which is the other way to cry
wolf; an announcement already published as urgent can still be corrected by a
manager. Drafting one without the sharper permission is allowed (a draft reaches
nobody), and retracting one needs only `announcements:manage` — putting a fire
out is not the act that needs the sharp permission.

**Two scope rules, on purpose.** Authority checks go through
`ctx.permissions.reach(…, &scope)` and follow coverage. The audience rule does
not: the caller's own `announcements:read` and `announcements:manage` grants are
asked for in one batched lookup, and the announcement's scope must be one of
them, so a Lodge reader does not see a troop-wide announcement unless they
actually hold a troop-scope grant. Oversight is not lost — a `manage` grant at a
covering scope sees every scope below it, drafts and retracted notices included —
and a caller always sees their own drafts, so a scribe is not blind to what they
wrote.

**Receipts are idempotent and the badge is a count.** `POST …/{id}/read` is one
statement with `ON CONFLICT (announcement_id, member_id) DO NOTHING`, and the
unique key is the invariant: a second mark-read writes no row, publishes no
second event and reports the receipt that already existed with
`already_read: true`. Unread counts never load a member's receipts — one query
counts from the indexed side, probing the unique key per candidate announcement
and producing the per-category breakdown with `FILTER`, so the cost is how many
announcements this member could read, not how many receipts the troop has.
Retracted and expired announcements never hold a badge open.

**What is deliberately not here.** A roster — "who has *not* read this" needs
membership's roster, and a plugin role reads its own schema only, which is why
`/receipts` returns who read rather than a completion percentage. Patrol
audiences — SPEC §7.14 says troop or Lodge, and a patrol notice is a Lodge notice
with the patrol in the body. Recording somebody else's receipt — a receipt means
this member opened it, and a receipt on their behalf would make the number a
fiction. Deleting a member's receipts when they leave — the record of who was
told is kept (SPEC §2's audit posture). Write vocabulary that reaches a reader
(`category`, `scope_type`, `status`, the Lodge/`scope_id` pairing) is constrained
in the database, so a category the troop does not know cannot reach it.

**Role grants.** `chief` is seeded with every permission the core finds; the rest
is the operator's, as for the other plugins. Lodges read their own notices and
their commanders write and retract them in scope; `announcements:publish_urgent`
is left out of the starting set, because it is the permission to interrupt the
whole troop and is granted at troop scope deliberately:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'announcements:read'),
  ('lodge_commander', 'announcements:write'),
  ('lodge_commander', 'announcements:manage'),
  ('scout',           'announcements:read')
ON CONFLICT DO NOTHING;
```

## Example plugins

`hello` (native) and `hello_wasm` (sandboxed) both expose:

| Method | Path | Permission | Purpose |
|---|---|---|---|
| GET | `/api/hello` | — | Open route |
| GET | `/api/hello/greetings` | `hello:read` | List greetings |
| POST | `/api/hello/greet` | `hello:write` | Insert + publish `hello.greeted` + audit |
| GET | `/api/hello/greetings/{id}` | `hello:read` | Path capture |

`hello_wasm` is the same shape under `/api/hello_wasm` with the
`hello_wasm:*` permissions, running in the WASM sandbox.

## SDK surface (`adjutant-sdk`)

| Item | Purpose |
|---|---|
| `AdjutantPlugin` | The trait every plugin implements (`id`, `name`, `version`, `init`, `routes`, `migrations`, `permissions_granted`, `subscriptions`, `shutdown`) |
| `PluginContext` | Runtime services handed to `init`: `db`, `config`, `events`, `permissions`, `audit`, `identity`, `http` |
| `RouteDefinition`, `Method` | Route registration. `get`/`post`/`put`/`patch`/`delete`/`head` (+ `_protected`, which requires a troop-covering grant) and `_protected_any_scope` (the handler checks the object's scope; `delete` is always troop-only) |
| `PluginRequest` / `PluginResponse` | Framework-neutral request/response (`param`, `query_param`, `json`, `redirect`, `created`, …) |
| `HostDb` / `HostEvents` / `HostHttp` | Host-mediated I/O traits (implemented by the core) |
| `Migration`, `Permission`, `Scope`, `ScopeType`, `RoleGrant` | Declarations and scoped permissions; `Identity.grants` is the source of truth (`roles()` is derived) |
| `SdkError` | Error type with a single HTTP mapping (`status()`) |
| `SqlValue` | Typed bind parameters (uuid, typed nulls, arrays, JSON) |
| `prelude` | One import for plugin authors |
| `testing` | `TestHost`, `MockDb`/`MockEvents`/`MockHttp`/`MockIdentity`, `TestRequest` |

Generated API docs: `cargo doc --workspace --no-deps --open`.
