# Notifications: one record, three surfaces, and the sentence we must never say

**Status:** decided (owner, 2026-09-25). Slice 1 (the recorded in-app notification) **implemented**;
the transports are not.
**Decides:** how the core records that a thing happened *for a person*, what it records beside it,
and which of the owner's three surfaces this is.
**Related:** [`bg.md`](bg.md) §4 (names the missing delivery capability; the background-check timer
is the first consumer), [`localization.md`](localization.md) §4 (a notification carries a locale),
[`scoped-permissions.md`](scoped-permissions.md) and SPEC §9.2 (a caller reading their own record is
an ownership check, not a grant).

---

## 1. The decision this implements

> **Decided 2026-09-25 (owner).** The channel is **Web Push** to the browser, **push notification on
> device** when the app is installed, and an **in-app announcement board**. Three surfaces, one
> message.

And the part that governs everything below:

> **The announcement board in the app is not a fallback: it is the *record*. Everything else is a
> notification that something is on the board.**

Three surfaces carry one message:

| Surface | Transport | Slice |
|---|---|---|
| The board, in the app | none — the record *is* the surface | **slice 1** |
| Web Push, browser (VAPID, RFC 8030/8291) | `web_push` | later |
| Device push, installed app (FCM/APNs where the platform requires it) | `device_push` | later |
| Email | `email` | later |

**Slice 1 is the record and only the record.** It is the surface the owner called *not a fallback* —
the one that has no external dependency, no vendor account and no token lifecycle, and the one that
is useful the moment a client exists. Nothing in slice 1 sends anything anywhere.

**Email is named in the issue's cheapest-first list but is not built here.** The owner's decision
note supersedes the ordering in the issue body: the record comes first because everything else is a
notification *that something is on the record*. A transcript of the ordering decision lives in the
issue; this note records what was built.

## 2. The record is not the delivery — and the schema says so in its own terms

**The hard rule: never report a notification as delivered when it was only recorded.**

Two facts, two columns, two states, and neither is derivable from the other:

- **The record.** `core.notifications` has a row. `read_at` is the *read* fact: `NULL` until the
  recipient opens it. This is a fact about the person, not about a transport.
- **The delivery.** `delivery_channel` names the transport the record is *for*; `delivery_state` is
  `recorded` (nothing has been sent), `delivered` (a transport confirmed it) or `failed` (a
  transport tried and did not). `delivered_at` carries the *evidence* of a delivery.

The schema refuses the lie rather than trusting a code path to:

```sql
delivery_state  TEXT NOT NULL DEFAULT 'recorded'
                CHECK (delivery_state IN ('recorded', 'delivered', 'failed')),
delivered_at    TIMESTAMPTZ,
-- 'delivered' is a fact and it is stored with its evidence …
CHECK (delivery_state <> 'delivered' OR delivered_at IS NOT NULL),
-- … and no delivery time may exist without the delivered state.
CHECK (delivered_at IS NULL OR delivery_state = 'delivered')
```

Consequences a reader should be able to hold at once:

- **Marking read does not deliver anything.** `POST /api/notifications/{id}/read` writes `read_at`
  and leaves `delivery_state` alone. A recipient reading the record in the app is proof that the
  *record* was read; it is not proof that a push arrived, and it must never be reported as one.
- **Slice 1 writes `recorded` and only `recorded`.** There is no transport, so there is nothing that
  could move a row to `delivered`. The state is on the row from the first version so that adding the
  transport later is a code path, not a schema change — and so that the *current* absence of any
  delivery is legible rather than implied.
- **The API keeps the two facts in separate objects** (`read` and `delivery`, §5), so a client cannot
  collapse them into one dot by accident.

This is the same posture as `core.outbox` (`state <> 'delivered' OR delivered_at IS NOT NULL`),
`announcements`' `read_count` versus a send, and `bg.md` §3's "show the date you are relying on".
Adjutant's recurring failure mode is a *claim* outrunning its *evidence*; the schema is where that is
cheapest to prevent.

## 3. Locating the record: core, not a plugin

The issue asks for a **core** record. The code agrees, and the reasons are structural rather than
stylistic:

- **The recipient is a `core.users` row.** A plugin reaches the database only through `ctx.db`, and
  a plugin role reads its own schema only (`docs/design/plugin-isolation.md`). A table whose
  recipient is a core user and whose reader is *any* user (whichever plugin produced the notice)
  cannot live in a plugin schema: the second producer would need a grant on the first plugin's
  schema, which is exactly the cross-plugin read the isolation model forbids.
- **The inbox is one list.** A member's notices arrive from membership (an expiring check), calendar
  (a changed event), store (an order), conflicts (a case) — the reader is one screen and the reader
  is the person. N records in N schemas cannot be listed in one query without N cross-schema grants.
- **`bg.md` §4 already names it as a core capability**, in the same class as the scheduler: "either
  the core grows a scheduled-task capability … or every plugin invents a background thread".

So the record is a `core.*` table, its routes are the core's own (mounted in `server.rs`, beside the
outbox operator surface and `/api/audit/verify`), and it is migrated by the core's own migration
runner. **This is not a plugin** — and `announcements`' current shape confirms it: that plugin
addresses an announcement to a *scope* (troop or Lodge) and records one receipt per member. It has
no single-member audience and no machine-authored path by construction, which is why it could record
the notice but could not deliver it. See §7 for the relationship.

## 4. The policy seam: in-app today must not block email tomorrow

Two rules keep the record from hard-coding a transport:

**4.1 The channel is data on the row, chosen by a policy the core owns.** `delivery_channel` is a
column, not a constant read at the call site. The core declares which channels exist and which one a
record is created for:

```rust
pub const CHANNEL_IN_APP: &str = "in_app";
/// The transports this core can be responsible for. Widening this list and the
/// table's CHECK is what adding a transport *is*.
pub const DELIVERY_CHANNELS: &[&str] = &[CHANNEL_IN_APP];
```

The table's `CHECK (delivery_channel IN ('in_app'))` is deliberate friction, not an oversight: **a
row may not claim a channel the core has no transport for**, or a notification would record a
delivery promise no code can keep. Adding `email` or `web_push` is a migration that widens this CHECK
plus a relay that drains `delivery_state = 'recorded'` rows for that channel — a code path and one
line of DDL, not a rework of the record, the read path or the route.

**4.2 Nothing in the read path knows the channel.** `list_notifications` and
`mark_notification_read` filter on `recipient` and write `read_at`. Neither branches on
`delivery_channel` or `delivery_state`; the delivery facts are *reported*, never consulted. A
transport can therefore be added, and a row can move from `recorded` to `delivered`, without a single
change to how a recipient reads their inbox. This is the seam: in-app is one value in a list and one
default, not an assumption baked into a query.

A producer does **not** choose the channel. Choosing is delivery policy, and the core owns it — the
producer states *what happened*, not *how it should be sent*. A producer that could name a channel
could name one that does not exist, which is the same lie by another route.

## 5. The message and its vocabulary

A notification carries **no display text**. It carries what a translator would need:

| Field | Meaning |
|---|---|
| `source` | which plugin (or `core`) recorded it — the namespace of the code |
| `message_code` | a stable, lower-case, dot-separated identifier: `bg_check.expiring` |
| `message_params` | a JSON object of **data**: ISO-8601 dates, ids, counts — never a sentence |
| `locale` | the recipient's language **at creation**, BCP-47 (`en`, `fr-CA`, or `und`) |

`message_code` is validated by shape (`^[a-z][a-z0-9_.]*$`) and namespaced by convention
(`<source>.<event>`); the core cannot and does not police a plugin's own codes. `locale` is validated
as a language tag. Neither is a `CHECK` against a fixed list — the vocabulary of codes belongs to the
producers, and a closed set in the core would make every producer's new code a core migration.

**Localization is honoured exactly as `localization.md` §4 requires** ("Notifications carry a locale
so the delivery channel can render the message in the recipient's language"):

- The **client** renders `(message_code, message_params, locale)` from its own catalogue, or the
  owning plugin's localised catalogue, falling back per `localization.md` §3 — the client never
  renders a missing translation as an empty label, and it never translates the code itself.
- The core renders **nothing**. There is no server-side message table, because
  `localization.md` §2 puts plugin-supplied strings with the plugin and client chrome with the
  client; a core-side renderer would be a third catalogue that nobody owns.
- Because the row holds *data* and not a sentence, a French recipient's notice is a French render of
  the same row — no duplicate records, no second push, no "which one is authoritative".

`locale` is a **snapshot taken when the record is created**: it is what the producer knew about the
recipient at that moment. It is not resolved at read time, because nothing yet stores a per-user
language preference for the core to read (see §8).

## 6. What slice 1 is, exactly

**Declared in the core:**

- `core.notifications` (core migration 10) — the record, with the delivery state from §2 and the
  message shape from §5.
- `core.notify(recipient, message_code, message_params, locale)` — a `SECURITY DEFINER` function a
  **plugin role** may `EXECUTE`, deriving `source` from `session_user` (never a parameter) exactly as
  `core.outbox_enqueue` derives its producer. This is the seam a **scheduled run** uses: a plugin's
  [`Schedule`](../../plugins/sdk/src/lib.rs) handler runs on the plugin's own pool, as its own role,
  so a sweep over a roster writes notifications through this one function and needs no grant on
  `core.notifications` (which is `REVOKE`d from `PUBLIC`).
- Core routes, mounted in `server.rs` beside the core's other surfaces:

| Method | Path | Gate |
|---|---|---|
| `GET` | `/api/notifications` | authenticated; returns **the caller's own** records |
| `POST` | `/api/notifications/{id}/read` | authenticated; marks **the caller's own** record read |

**No permission is declared, and that is the decision,** not an omission. SPEC §9.2: *"a caller
reading their own record is an ownership check, not a grant."* An inbox is the canonical ownership
check — there is no scope at which "may read somebody else's notifications" is a sensible grant, and
inventing one would be inventing an oversight power nobody asked for. What the routes do enforce is
**authentication** (401 without an identity) and **ownership in the query itself**: the list is
`WHERE recipient = $caller`, and mark-read is `UPDATE … WHERE id = $1 AND recipient = $2` — so a
notification that is not the caller's updates no row and is reported as **404**, not 403, because
whether somebody else has a notification is not the caller's business.

**Deliberately not in slice 1** (each named so a reader does not go looking):

- The transports — Web Push, device push, email (§1).
- The client: no service worker, no subscription record, no board screen. The record exists to be
  rendered; the renderer is a later slice (and the issue asks for none of it here).
- A manager/oversight route listing other people's notifications (§6 above).
- A `related` link from a notification to a board entry. The owner's framing allows it ("a
  notification *that something is on the board*"); slice 1 records the message, not the pointer.
- Wiring `announcements` to this record. That plugin keeps publishing `announcement.published`
  (`plugins/announcements/src/lib.rs`); a delivery/notification plugin subscribing to it is a later
  slice, and nothing here changes that plugin's behaviour.
- A per-user language preference store (§8).

## 7. Notifications are not announcements

Both record "a thing the troop should know", and they are not the same object:

| | `announcements` (plugin, M6) | `core.notifications` (slice 1) |
|---|---|---|
| Addressed to | a scope — troop or one Lodge | **one user** |
| Authored by | a person (a scribe, a leader) | a **machine** — a scheduled sweep, a server fact |
| Read state | one receipt per member per announcement | `read_at` on the record, one row per recipient |
| Audience rule | "the caller must be addressed by the announcement's own scope" | "the row is the caller's, or it does not exist" |

The board holds the troop's notices; the inbox holds *your* notices. A scheduled run producing "your
background check expires in 30 days" is not an announcement — it is addressed to a person and nobody
wrote it. Keeping them separate is why `announcements` was honest when it said it could not deliver:
it was never the object that delivery addresses.

## 8. Open questions for the owner

**Unanswered on purpose.** These are youth-safety and troop policy, and the software must follow
policy rather than invent it — `bg.md` §5 asks them and the answers are not a developer's to guess.
Nothing below is implemented, and no default has been chosen:

1. **Who is warned when a background check is expiring?** The member, their patrol leader, the
   GSM/Chief, or all three with different messages? (`bg.md` §5 Q2.) The record supports any answer —
   a notification has one recipient, so "all three" is three records — but *which* three is policy.
2. **How early?** 30/60/90 days before `expires_on`; how many warnings; a final one when it has
   expired? (`bg.md` §5 Q3.) This decides the schedule's cadence and the sweep's predicate, so it
   must be answered *before* the timer is written, not after.
3. **Does an expired check block anything?** (`bg.md` §5 Q4.) Recommended there: blocks being
   recorded as eligible to lead and surfaces loudly, but never a silent disqualification.
4. **Data-entry cadence** — does the software nudge for the *entry* too? (`bg.md` §5 Q5.)
5. **Which language is the fallback** when a recipient's language is unknown? (`localization.md` §6
   Q1.) Slice 1 accepts `und` rather than picking a language on the owner's behalf.
6. **Is there a per-user language preference, and who sets it?** The record carries a `locale`
   because `localization.md` §4 requires one, but nothing in the core today stores a member's
   language — so a scheduled sweep must be *told* it. The honest next step is a user-preference
   store (a client setting, or `Accept-Language` captured at login); until then a producer supplies
   the locale and `und` is the honest value when it does not know.
7. **Retention.** Does a read notification age out, or is the record permanent like the audit log and
   a receipt? Not decided; the record is permanent until the owner says otherwise.
