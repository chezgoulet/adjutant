# Funding a waiver: the Scholarship rule applied to dues

**Status:** design note; nothing implemented. The rule is the owner's (2026-09-25) and is
recorded in SPEC §7.5; this note is how it reaches `finance`'s dues.
**Decides:** what a waived dues row records, which route books the money that funds it, and
who must be allowed to do each half.
**Relates to:** [`plugin-to-plugin.md`](plugin-to-plugin.md) §3.2 (the money path is
synchronous and compensatable) and §3.3 (only `finance` writes the ledger),
[`plugin-isolation.md`](plugin-isolation.md), SPEC §7.5 and §7.16,
[`../api-reference.md`](../api-reference.md) (`POST /api/finance/dues/assess`,
`POST /api/finance/transfer`).
**Implements:** issue #61 (this note is its acceptance criteria), which carries the dues half
of issue #58 (the scholarship treatment). Touches #59 (receipts) where a waiver's funding is
receipted. Not #57 (§3.2's outbox and service principal); see §7.

---

## 1. The rule

> Anything free, deducted or discounted draws from the Scholarship fund.

Recorded in SPEC §7.5 (line 547): a comp, a sliding-scale reduction, a scholarship award or
a waived due is *drawn from* `scholarship` — a balanced transfer into the fund that would
otherwise have received the money — "so the subsidy is visible in the ledger and the Annual
Financial Report instead of being expressed as a price of zero. A zero price hides who paid;
a draw names them." The shop already works this way (SPEC §7.16 lines 691, 694).

The account already exists: `FUND_SCHOLARSHIP = "scholarship"` is one of the six seeded
kinds (`plugins/finance/src/lib.rs:107`, `FUND_KINDS` at 118–125, the seed row at 1157).
What is missing is only the *treatment* — and for dues it is the last place a price of zero
survives.

## 2. Why a zero assessment is the wrong shape

A waiver today is expressed as a price of zero, in three places at once:

* `STATUS_WAIVED = "waived"`, documented as "always with an assessment of zero"
  (`lib.rs:218–219`);
* the database refuses anything else: `CONSTRAINT dues_waived_is_zero CHECK (status <>
  'waived' OR assessed_cents = 0)` (`lib.rs:1149`), named in the schema comment as what makes
  "waived but owing" unrepresentable (`lib.rs:1056`);
* the route enforces it again in code — "A waiver assesses nothing, whoever asked for it"
  (`lib.rs:2226–2231`), restated in its doc comment (`lib.rs:2188–2193`) and in the API
  reference (`docs/api-reference.md:218`).

The constraint is an honest defence of one invariant. It also makes a second fact
unrepresentable: **"waived, and here is who paid for it."** A waived row contributes a zero
to the report and no identifiable expense anywhere. `GET /api/finance/report/annual`
(`lib.rs:2975–2988`) can count waivers and count members at no cost
(`sql_annual_dues`, `lib.rs:1000–1011`, the `at_no_cost` and `waived` counts at 1004 and
1006) and can report what the ledger collected in dues (`sql_annual_collected`,
`lib.rs:1014–1019`) — but it cannot say the troop *spent* money on access, because no figure
for that spend exists in the system. Free access is invisible, which is exactly the outcome
the Annual Financial Report exists to prevent.

The shop's plugin states this argument for comps in its own words — a comp is not a cheaper
price, and "muting the reduction into a cheaper price... would hide the subsidy"
(`plugins/store/src/lib.rs:99–101`, and the whole "Why the draw is not a zero amount"
passage at 59–68). Dues is the same case with a different subject.

## 3. The model: the comp's three figures, applied to a dues row

The store records `price_cents` / `charged_cents` / `funded_cents` and refuses an order where
they disagree (`plugins/store/src/lib.rs:44–46`). A dues row already has the first two of the
same three:

| figure | today | after |
| --- | --- | --- |
| `base_cents` — the troop's membership cost (`lib.rs:1130`) | the treasurer's number | unchanged |
| `assessed_cents` — what the tier assesses (`lib.rs:1131`, computed by `tier_assessment` / `assessed_cents`, `lib.rs:3831–3841`) | forced to 0 for a waiver (2227–2231) | the tier's share, named as the constraint that only `assessed_cents >= 0` (1146) |
| `funded_cents` — what `scholarship` covers (new) | does not exist | the subsidy, and the figure the report needs |

The member's own share is `assessed_cents - funded_cents`. A **waiver** is the case where that
share is 0 and `funded_cents = assessed_cents`: the member is measured at a tier (say
standard, `TIER_STANDARD` at `lib.rs:154`, 10 000 bps at 186), the row assesses it honestly,
and the whole assessment is funded from `scholarship` into the fund the dues would have landed
in. The draw amount is `base_cents - assessed_cents` for a reduction and `assessed_cents` for a
waiver, clamped at zero — a patron (20 000 bps, `lib.rs:180`) assesses *above* the base cost and
funds nothing.

This also separates two things the current report merges. `TIER_HARDSHIP` is a real tier at
0 bps (`lib.rs:196–200`) that assesses nothing; that is not a failure to collect, it is the
Accords' mandatory minimum (`MINIMUM_DUES_CENTS`, `lib.rs:206–208`). A waiver is a full
assessment funded by somebody. Today both are zero rows counted by `at_no_cost`
(`lib.rs:1004`); after this change one funds nothing and the other funds its assessment.

## 4. What changes in finance

**4.1 The constraint.** `dues_waived_is_zero` (`lib.rs:1149`) is *replaced*, not simply
dropped — the invariant it defends is kept by re-expressing it in terms of the member's own
share, and a funded figure is added:

```sql
CONSTRAINT dues_funded_valid CHECK (funded_cents >= 0),
CONSTRAINT dues_funded_within_assessment CHECK (funded_cents <= assessed_cents),
CONSTRAINT dues_waived_is_funded CHECK (status <> 'waived' OR funded_cents = assessed_cents)
```

What survives: **"waived but owing" is still unrepresentable**, because `assessed_cents -
funded_cents` (what the member owes) is 0 for a waiver and can never go negative. What is
gained: a waiver names what was funded. Note the constraint deliberately does *not* re-encode
the tier→basis-point scale in SQL: the scale is Rust's (`TIERS`, `lib.rs:176–201`;
`tier_assessment`, `lib.rs:3839`) and a SQL copy would be the second scale this whole rule
exists to avoid.

The alternative issue #58 offers — drop `dues_waived_is_zero` and make a waiver "an ordinary
assessment plus a draw" — is simpler but loses the invariant and leaves `status = 'waived'`
meaning nothing: a waived member could again be recorded as owing money, which is what
`lib.rs:1149` was written to prevent. Not recommended.

**4.2 The waiver route's behaviour.** There is no `POST /api/finance/dues/waive` and none is
needed: a waiver is `status: "waived"` on `POST /api/finance/dues/assess` (`lib.rs:2196`,
permission `finance:manage_dues` at 2200; routes list 1251–1272). After the change that route:

1. writes the row with the tier's assessment, computed by `tier_assessment` (`lib.rs:3839`) —
   the zero override at `lib.rs:2227–2231` is deleted;
2. sets `funded_cents = assessed_cents` for a waiver, and `base_cents - assessed_cents` for a
   self-reported reduction;
3. **books the draw in the same call when the caller also holds `finance:write`**, as the
   store's comp does (`plugins/store/src/lib.rs:85–88`);
4. otherwise records the row with the funded amount and an **outstanding** draw, in the draw
   vocabulary the store already uses (`DRAW_NONE`/`DRAW_UNBOOKED`/`DRAW_ATTEMPTING`/
   `DRAW_BOOKED`/`DRAW_REFUSED`/`DRAW_FAILED`, `plugins/store/src/lib.rs:266–286`), so nothing
   is silent;
5. carries the funded figure into the audit entry (today `lib.rs:2255–2270`) and the
   `finance.dues.assessed` event (today `lib.rs:2271–2284`), so a subscriber sees the subsidy
   and not just a zero;
6. answers with the assessment and the funding line (`lib.rs:2285–2291`). A client that today
   prints "Waived" against a `$0` assessment (`client/lib/screens/dues_screen.dart:490`) will
   need the new fields rendered; that is implementation, not this note.

`finance:read_all` callers see it immediately: `GET /api/finance/dues` (`lib.rs:2517`) lists
rows with their `funded_cents` and draw state.

**4.3 The row already waived at zero, in a deployed database.** `finance` has exactly one
migration — `Migration::new(1, "finance_schema", MIGRATION_SCHEMA)` (`lib.rs:1210–1211`) — and
the core records applied versions per schema in `core.schema_migrations` and **skips a version
already applied** (`server/src/db.rs:504–509`, `server/src/plugin_runtime.rs:585–588`).
Editing version 1 therefore changes nothing on a database that has run it. This ships as
**version 2**:

* **What it does to existing rows.** `ALTER TABLE dues ADD COLUMN IF NOT EXISTS funded_cents
  BIGINT NOT NULL DEFAULT 0;` (and, for 4.2(4), the draw-state column and a `draw_ref UUID`
  carrying the transfer group). Then the constraint swap: every waiver in the database is
  currently `assessed_cents = 0` (that is what `lib.rs:1149` enforced), so `funded_cents = 0`
  satisfies all three new constraints and the swap applies cleanly. Then the backfill: the
  tier's share is a Rust computation, so SQL must not guess it — the recompute is a **guarded,
  idempotent repair in the plugin** (a startup step, or a `finance:manage` route) touching only
  rows where `status = 'waived' AND assessed_cents = 0`, computing `tier_assessment(base_cents,
  tier)` from the row's own `base_cents` and `tier` (`lib.rs:3839`). Where `base_cents` is 0 —
  no membership cost was configured when the row was opened (`configured_membership_cost` may
  be `None`, and the route defaults the base to 0, `lib.rs:2214–2217`) — there is nothing to
  fund and the row is left as it is.
* **What the operator sees.** The migration and the repair run at plugin startup with the rest
  of `finance`'s migrations and leave a line in the log; the row then reads `waived, funded
  $X, draw unbooked` (or `waived, nothing to fund`) in `GET /api/finance/dues`, and the Annual
  Financial Report shows the `scholarship` → dues-fund transfer in its fund and transfer
  sections with `funded_cents` shown beside the tier counts in the dues section. Money that is
  funded but unbooked is visible as exactly that, never as a zero.
* **A decision this note cannot make for the owner.** Recomputing already-recorded rows changes
  a *past* year's report. The alternative is to apply the funding rule only from the change
  forward, leaving historical waived rows at zero and letting the outstanding-draw worklist
  name them. Both are defensible — see §8.

**4.4 One report change is required.** `sql_annual_dues` (`lib.rs:1000–1011`) has no funding
column: it needs `SUM(funded_cents)` and a count of unbooked draws, or the report still cannot
say what the troop spent on access. No change is needed to `sql_annual_collected`
(`lib.rs:1014–1019`): dues *collected* is read from the ledger by category
(`CATEGORY_DUES`, `lib.rs:146`) and a draw is a *transfer* (`CATEGORY_TRANSFER`, 148), so the
draw does not inflate collected dues — it appears where it belongs, in the fund and transfer
sections. The model already fits the report as written.

## 5. How a draw is recorded

**The route.** `POST /api/finance/transfer` (`lib.rs:1853–1857`), whose single statement
(`TRANSFER_SQL`, `lib.rs:1587–1600`) writes both legs under one `gen_random_uuid()` group id
(1588), so a failure writes neither and the sum of all funds is unchanged (1844–1845; the route
re-checks that the legs sum to zero itself, 1917–1925).

**Direction and amount.** From `scholarship` into the fund the dues would have landed in —
the configured dues fund, `finance.dues_fund_code`, defaulting to `general`
(`CONFIG_DUES_FUND_CODE`, `lib.rs:242`; `configured_dues_fund`, `lib.rs:737`), which is the
same fund the dues payment route resolves (`lib.rs:2859–2872`). The amount is the positive
magnitude from §3, and it is never zero: the route refuses a non-positive magnitude
(`lib.rs:1867–1873`) and the database refuses it too (`transactions_amount_nonzero`,
`lib.rs:1092`; `income` positive / `expense` negative, 1093–1094). **An amount that computes to
zero is no transfer at all, not a zero transfer** — a waiver of a zero assessment funds nothing
and books nothing, and its draw state says so.

**Edge case.** If the configured dues fund *is* `scholarship`, the draw has the same fund on
both sides and the route refuses it (`lib.rs:1874–1880`) — correctly: the subsidy is already
inside the fund that would fund it. The waiver is still valid; nothing is booked.

**The hazard to name — a separate finding.** `TransferBody` (`lib.rs:487–502`) carries no key
of any kind, and `TRANSFER_SQL` (`lib.rs:1587–1600`) writes neither leg's `external_ref`,
although the column exists with a unique index (`lib.rs:1089`, `lib.rs:1101`). Every other
ledger write path *does* have a key: `/api/finance/transaction` accepts one and writes
`ON CONFLICT (external_ref) DO NOTHING` (`lib.rs:3256–3280`), the dues payment route accepts
one (`lib.rs:2872`, `lib.rs:2898`), and the `payment.received` subscriber's entire idempotency
rests on it (`lib.rs:3687–3690`, `3714`, `3776`). So a transfer whose *answer* is lost — a
timeout after the write — cannot be retried safely: the retry is a new group id with two new
legs and nothing recognises the second as a duplicate. For a draw that means the same waiver
can be funded twice, and the only evidence is a transfer group nobody asked about twice.
Recommended with the implementation: give the transfer route a reference and make the draw
carry a deterministic one (`dues:{fiscal_year}:{member_id}`, the shape the store uses for its
`draw_ref` field, `plugins/store/src/lib.rs:3609`). This is not a prerequisite for the
constraint change, but it *is* why the draw states in 4.2(4) must exist rather than a
fire-and-forget call.

## 6. Who authorizes it

Two acts, two permissions, declared together but separate (`lib.rs:1184–1208`):

* **the waiver** is an act by a holder of `finance:manage_dues` (`lib.rs:2200`; the permission
  reads "Open dues assessments, set tiers, waive dues", `lib.rs:1200–1202`);
* **the draw** is a ledger write needing `finance:write` (`lib.rs:1857`; "Record income,
  expenses and transfers", 1192–1194).

**They are not the same caller by default.** The happy path is one treasurer holding both: the
assess call books the draw as that caller, forwarding nothing but their own credential — the
discipline `require_forwardable` states plainly (`plugins/store/src/lib.rs:735–745`) and §3.1
requires (`plugin-to-plugin.md:68–73`). When the person making the waiver does **not** hold
`finance:write`, the waiver must be neither refused nor silently unfunded: it records the row
with its funded amount and an outstanding draw, and a `finance:write` holder books it later —
precisely the store's shape for a reduction it cannot book itself
(`plugins/store/src/lib.rs:3192–3199`, and its own statement at 3620–3622). The correction
route is `POST /api/finance/transfer` called by that holder, with their own authority and
nothing added.

A draw with **no caller at all** — a webhook, a schedule, a machine-originated confirmation —
is not this note's problem and must not be solved by minting a credential or substituting one:
that is what §3.1 refuses (`plugin-to-plugin.md:111–121`), and it is §3.2's problem
(`plugin-to-plugin.md:88–109`, tracked as #57).

## 7. The sliding-scale case — different, and machine-originated by construction

`POST /api/finance/dues/self-report` (`lib.rs:2679–2684`) is the member's own call under
`finance:self_report`, protected for any scope; the row is written `self_reported = true` with
`status = self_reported` and `assessed_cents = tier_assessment(base, tier)` (`lib.rs:2741`,
`2758–2760`). **No human decides anything** — the software has no income field and does not
want one (the sliding-scale route's own note, `lib.rs:2171–2174`).

Under the rule, the reduction (`base_cents - assessed_cents`) is a scholarship draw. But the
caller is the member, holding `finance:self_report` and not `finance:write` — and must not be
given it, or the sliding scale becomes a spending authority. So **this draw is
machine-originated by construction**: §2(b) of `plugin-to-plugin.md` (call the target as the
caller) cannot produce it, because there is no caller whose authority covers a ledger write.

Therefore this case waits for §3.2's transactional outbox and service principal (#57), or is
left outstanding with its amount visible for a `finance:write` holder to book — which is
exactly what the store does with a reduction today (`plugins/store/src/lib.rs:90–108`). The
mechanism is decided in §3.2; this note does not re-decide it and does not build it.

Two consequences worth stating now:

* a member self-reporting `hardship` (0 bps, `lib.rs:196–200`) assesses nothing and therefore
  funds nothing — a legitimate zero; a member self-reporting `supported` (5 000 bps,
  `lib.rs:190–194`) funds half the membership cost from `scholarship`, and that is a real
  subsidy the report should show;
* so `at_no_cost` (`lib.rs:1004`) stops being the interesting count for a treasurer and
  `funded_cents` becomes it — which is the whole point of SPEC §7.5's inclusion of the
  sliding-scale reduction in the rule.

## 8. What NOT to do

1. **No second scholarship concept** — no `dues_scholarship`, no `waiver_fund` config, no
   reason enum standing in for a fund. `scholarship` is one of the six kinds (`lib.rs:107`,
   `118–125`; the closed set `funds_kind_valid`, `lib.rs:1070`) and the store already refuses a
   second one in its own words (`plugins/store/src/lib.rs:31–34`).
2. **No new fund kind.** The kind check is a closed set of six (`lib.rs:1070`) and
   `FUND_KINDS` is the seed order (`lib.rs:118–125`).
3. **No write into `finance.*` from another plugin** (§3.3, `plugin-to-plugin.md:129–136`).
   Dues is finance's own table and this change stays inside finance; the store's draw is a call,
   not a cross-schema write.
4. **No zero-amount transaction** (`lib.rs:1092`, `1093–1094`; the route's refusal at
   `1867–1873`). A subsidy of zero value books nothing and records that state; it is not a zero
   row.
5. **Don't lower the recorded assessment to hide the subsidy** (SPEC §7.16 line 691 makes this
   explicit for the shop; the same argument holds for dues) and don't leave `status = 'waived'`
   meaning two different things at once.

## 9. Open questions for the owner

* **Backfill or not** (§4.3): do already-waived rows get recomputed to their tier's assessment,
  changing a past year's figures, or does the funding rule apply only from the change forward,
  with the outstanding-draw worklist naming the historical rows? This one needs the owner,
  because both change what a historic Annual Financial Report says.
* **Shape of the funded figure** — one `funded_cents` column on the assessment row (recommended:
  the row is the assessment and a waiver has one draw) versus a separate funding table.
* **Once #57 lands**, does the waiver-when-the-caller-lacks-`finance:write` case book through a
  service principal automatically, or does the human worklist stay as the visible state (and
  the reconciliation control)? Recommended: keep the worklist, let the relay remove the manual
  step.

---

### The current code this note rests on

| claim | where |
| --- | --- |
| `scholarship` is one of six seeded kinds | `plugins/finance/src/lib.rs:107`, `118–125`, `1157` |
| a waiver is a zero assessment | `lib.rs:218–219`, `1149`, `2226–2231`, `1056` |
| the waiver's route and permission | `lib.rs:2196`, `2200`, `1251–1272` |
| the transfer writes both legs under one group | `lib.rs:1587–1600`, `1842–1845`, `1917–1925` |
| the transfer has no idempotency key | `lib.rs:487–502`, `1587–1600`, `1089`, `1101` |
| a zero amount is unrepresentable | `lib.rs:1092`, `1093–1094`, `1867–1873` |
| one migration, versions skipped once applied | `lib.rs:1210–1211`, `server/src/db.rs:504–509`, `server/src/plugin_runtime.rs:585–588` |
| the report counts waivers at no cost but never the spend | `lib.rs:1000–1011`, `1014–1019` |
| the tier scale | `lib.rs:176–201`, `208`, `3831–3841` |
| the store's comp-as-draw precedent | `plugins/store/src/lib.rs:44–46`, `266–286`, `3192–3199`, `3600–3625` |
| SPEC §7.5 rule / §7.16 comps | `SPEC.md:547`, `691`, `694` |
