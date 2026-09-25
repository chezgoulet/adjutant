# Plugin-to-plugin interaction: how one plugin gets what it needs from another

**Status:** decision taken (owner, 2026-09-25). Design note; nothing implemented.
**Decides:** how a plugin obtains data or an action that another plugin owns — the question
payments, store, equipment rentals and the ledger force.
**Blocks:** Stripe/payments, `store`, equipment rentals. Not M6.
**Relates to:** [`plugin-isolation.md`](plugin-isolation.md) (a plugin cannot read another's
schema), [`boundary.md`](boundary.md) (what must be core), [`plugin-capabilities.md`](plugin-capabilities.md)
(`ctx.http` as a declared, allowlisted capability).

---

## 1. The decision

**A plugin may obtain another plugin's data only by (a) subscribing to an event it publishes, or
(b) calling its API as the caller — never as itself.** There is no privileged in-process call
between plugins, and there never will be.

The reason is not tidiness. Plugins are independently loaded `cdylib`s with their own database
role and their own schema; a direct call would be a hole in the isolation model that
`plugin-isolation.md` exists to close. What makes (b) safe is that the call carries **the
caller's own authorization and nothing else**, so the target plugin's route gate decides — the
same gate that would decide if a human had made the request. A plugin therefore cannot exceed
the authority of the person on whose behalf it is acting.

`mcp` already does exactly this and is the reference implementation: it invokes another plugin's
route over `ctx.http` with the caller's credentials forwarded, and states plainly that the core's
gate decides again, so the plugin cannot exceed the caller's authority.

---

## 2. The two mechanisms, and when each applies

### Events — for facts that already happened

Use when the sender has nothing to learn from the receiver, and the fact is complete on its own.
`mission.completed`, `motion.passed`, `announcement.published`.

An event is a **broadcast of a past fact, not a request**. It has no response, so it cannot
carry an answer, and it must not be used where the sender needs to know whether the receiver
succeeded. Subscribers must be idempotent — a replayed event must not duplicate its effect
(Archive's ingestion is the worked example).

### Acting as the caller — for everything that needs an answer

Use when plugin A needs a value or an action from plugin B *on behalf of a user*: "what does
this cost this person?", "is this order comped?", "check this item out to me."

The call is made over `ctx.http` with the caller's authorization forwarded. Three consequences
worth stating:

1. **Authority is never amplified.** A plugin can do only what the caller could do. A scout
   cannot spend the troop's money through a bug in the store.
2. **Egress is declared.** Per `plugin-capabilities.md`, `net.http.egress` carries a per-plugin
   allowlist, so a plugin can reach only the hosts it declared — not the whole API surface.
3. **It is a real HTTP call.** Latency, failure and partial success are real. See §3.2.

### Permission questions — for authorization, not data

To ask "may this identity act on this scope?", a plugin uses the permission service. It does not
read another plugin's tables and it does not call another plugin to find out. `boundary.md` is
explicit that authorization must not depend on a plugin call.

---

## 3. The hazards, and the rules that answer them

### 3.1 Authority amplification — the hole to keep closed

The failure mode: a plugin holds a privileged token and uses it to do something the user could
not have done. Every cross-plugin call must forward the caller's credential, never a
service credential, and the target must re-check. A "trusted internal call" shortcut is the
one thing that would break the model, and it is refused here.

### 3.2 The money path cannot be event-only — the hard one

A purchase touches three plugins: **store** records the order, **payments** takes the money,
**finance** writes the ledger. If that is wired with events, then a successful card charge
followed by a failed ledger write produces money that moved with no record — in a troop whose
Accords mandate open books.

**Rule: the money path is synchronous and compensatable, never fire-and-forget.** An event may
notify (*"an order was placed"*). It may not be the mechanism by which the ledger learns it must
write. Concretely, one plugin owns the completion of a paid order and is responsible for the
ledger entry within the same flow, so a failure is either rolled back or compensated — not
silently orphaned.

The specific pattern (single-transaction ownership, or an outbox with retry) is left open here
deliberately; it should be decided with the payments plugin, against the real shape of Stripe's
webhooks. What is decided now is the constraint: **no money event without a matching ledger
outcome**.

### 3.3 The ledger stays single-source

Only `finance` writes transactions. `store` and `payments` request ledger entries; they never
keep their own books and never write into `finance.*`. Two writers is two ledgers, and the
Annual Financial Report would be a reconciliation problem instead of a report.

`payments` writing to `finance.transactions`, as SPEC §7.13 describes, must therefore be
implemented as an API call on finance as the caller — not a cross-schema write.

### 3.4 Circular events

A publishes `x`, B reacts by publishing `y`, A subscribes to `y`. Nothing in the bus prevents
this. **Rule: a subscriber must not publish into the cycle it consumes.** Where a genuine
two-way flow is needed, it belongs on the caller path (§2), not the bus.

### 3.5 Reference, don't replicate

When `payments` holds a rental listing that points at an equipment item, it stores the **item id
and nothing else** about it. Copying the item's name and condition would create a second, drifting
copy of another plugin's data — and the item's condition is exactly what changes. A display needs
the name; it gets it by asking, as the caller, or the client fetches both. Stale copies of another
plugin's facts are how a system starts lying.

---

## 4. What is not built

Stated so nobody assumes otherwise:

- **`net.http.egress` allowlisting is designed, not implemented.** `plugin-capabilities.md` is a
  design note. Until it lands, a plugin's `ctx.http` is not yet fenced to declared hosts, and
  the safety of §2(b) rests on forwarding the caller's credential rather than on the allowlist.
- **No plugin currently calls another plugin as the caller except `mcp`.**
- **The payments plugin does not exist**, so §3.2's pattern is a constraint rather than an
  implementation.

---

## 5. Why this is written before the plugins it constrains

The same reason `client-and-plugin-ui.md` was written before the first domain plugin: a contract
that exists before its dependents is a contract; one written afterwards is a description of what
happened. `store`, `payments` and equipment rentals all sit on this, and the money path in §3.2
is the kind of thing that is cheap to decide now and expensive to retrofit.
