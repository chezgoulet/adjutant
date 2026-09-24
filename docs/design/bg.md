# Background-check tracking: status, expiry, and the two capabilities it needs

**Status:** decided (owner, 2026-09-24). Not yet implemented.
**Decides:** what the software records about a background check, when it warns, and what the core
has to grow to warn at all.
**Related:** [`localization.md`](localization.md) (notifications are localized),
[`osg-interop.md`](osg-interop.md) (OSG never discloses a check's contents — a status is the
permanent ceiling).

---

## 1. The decision

> "We should have a timer on the background check with notifications to get it renewed when it
> expires. The user will have to put in the data initially."

Three parts, and the third matters as much as the first: the troop enters the data by hand. There is
no OSG feed (deferred), so nothing is synchronised and nothing is inferred — a leader records what
they have and when it was verified.

## 2. What the record has to hold

`membership.members.bg_check TEXT` is not enough — a bare string cannot say *when* a check was
verified, *when* it lapses, or *who* attested it, and OSG requires a **yearly** check.

Replace it with an explicit, dated record (per member, or per adult member):

- `status` — a stable code, never display text: `not_started` · `submitted` · `current` ·
  `expired` · `declined` (OSG can privately decline membership; the software must be able to hold
  that without implying why)
- `verified_on` — the date the operator says the check cleared
- `expires_on` — computed as `verified_on + 12 months` by default, **overridable**, because OSG's
  cycle is yearly but a late entry should not silently shorten the next one
- `source` / `attested_by` — who entered it, so an audit can distinguish "OSG confirmed" from "a
  leader typed it in"
- `notes` — free text for the operator, never for status

**What it must never hold: the contents of a check.** OSG states it will never disclose them
(legally required), so the software's ceiling is a status plus dates. Say so in the field docs so a
future contributor does not "improve" it by storing a report.

## 3. The rule that makes the dashboard honest

A compliance view must never show a stale check as though it were current. Concretely:

- A check past `expires_on` is `expired` **because the date passed**, regardless of what a human
  last set the status to — the date is the authority, and the software recomputes rather than
  trusting the stored label.
- The view shows the date it is relying on, not just a colour. "Current — verified 2026-03-04" is
  honest; a green tick is not.
- A member with no record at all is `not_started`, which is visibly different from `current`. The
  absence of data must not read as compliance.

That last point is the whole reason this is a design note: the failure mode of a compliance
dashboard is not a wrong colour, it is a leader taking a scout camping believing a check was done.

## 4. What the core does not have yet — the real cost of this decision

A timer with notifications needs two things the core has never done:

1. **Periodic work.** The event bus is reactive: a plugin can respond to an event, but nothing
   schedules work. There is no scheduler, no due-date sweep, no "run this daily". Either the core
   grows a scheduled-task capability (a plugin declares a schedule; the core runs it with the same
   capability/audit discipline as anything else), or every plugin invents a background thread —
   which is exactly the mess the host-mediated I/O rule exists to prevent.
2. **A delivery channel.** There is nowhere to send a notification. No email sender, no push
   registration, no in-app inbox. `announcements` (M6) was flagged as needing a server-side owner
   for exactly this reason.

Both are core capabilities, in the same class as the declared-capability model for field plugins:
the schema change is an afternoon, the plumbing is the work. Which means this requirement, small as
it reads, is a **core** workstream, not a membership-plugin detail — and it is the first real
constraint on how the client and the server talk about notifications.

## 5. What to decide before implementing

1. **How does a leader hear about an expiring check?** Email (needs a provider and a sender
   identity), push via the client (needs registration and the client in real use), or in-app only
   (honest but useless until people open the app). My recommendation: in-app first, email next,
   push when the client exists — and never claim a notification was delivered if it was only
   recorded.
2. **Who is warned?** The member, their patrol leader, the GSM/Chief, or all three with different
   messages? Youth-safety policy may already answer this; the software should follow it, not invent
   it.
3. **How early?** 30/60/90 days is a policy question. Two warnings and a final one is a reasonable
   default; the number belongs to the troop.
4. **Does an expired check block anything in the software?** Recommended: it blocks being recorded
   as eligible to lead, and surfaces loudly — but the software should not *silently* disqualify
   anyone, because the underlying record is operator-entered.
5. **Data-entry cadence.** If leaders re-enter dates yearly by hand, the record goes stale by
   omission. Decide whether the software nudges for the *entry* too (e.g. "we have no check on file
   for these four adults").
