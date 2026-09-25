//! # adjutant-store — the troop's shop (SPEC §7.16)
//!
//! What a troop sells, to whom, and at what price: a catalogue (uniforms, patches,
//! insignia, camp gear, event merchandise), prices with a **sliding scale** so
//! cost never decides who belongs, **equipment rentals as a priced product**, and
//! orders with their completion — including **comp sales**, an authority to
//! complete an order at no charge.
//!
//! ## The three rules that bind it before a line of it is written
//!
//! All three are from `docs/design/plugin-to-plugin.md`:
//!
//! * **It holds no money and keeps no books.** Payment is `stripe`'s (§7.13) and
//!   the ledger is `finance`'s (§7.5). A paid order is completed by calling
//!   `stripe` as the caller, with the caller's own credential forwarded so
//!   stripe's gate re-decides; the ledger entry stays finance's (§3.3). This
//!   crate writes no `stripe.*` and no `finance.*` table and never will.
//! * **Custody belongs to `equipment`.** A rental is a priced product here; the
//!   item, its condition and the open-checkout state machine stay in `equipment`
//!   (§7.6). This plugin holds the **item id and nothing else** about it (§3.5) —
//!   never a name, never a condition — and it names equipment's own routes for
//!   availability and checkout. There is exactly one checkout state machine in
//!   this system and it is not in this crate.
//! * **A comp is not a price, it is an authority.** Completing an order at no
//!   charge needs a grant (`store:comp`), a mandatory reason, the authority
//!   recorded, and the money it forgives recorded as a **subsidy** — never as a
//!   price of zero, which would hide it.
//!
//! ## Free, reduced and comped: a draw on the scholarship fund
//!
//! **Anything free, deducted or discounted draws from the `scholarship` fund.**
//! `scholarship` is one of the six fund kinds `finance` seeds (`finance::FUND_KINDS`,
//! SPEC §7.5) — there is no second scholarship concept here, no new fund kind, and
//! no store-side ledger. An item **sells at its price**; when a member pays less
//! than that (a sliding-scale tier, or a comp), the difference is a **draw on
//! `scholarship`**, and the money moves as a **balanced transfer** from
//! `scholarship` into the fund the order's proceeds land in (`POST
//! /api/finance/transfer` writes both legs in one statement under one group id, so
//! the sum of every fund is unchanged and the subsidy stays visible and
//! reportable). Three figures are therefore recorded on every order, and the
//! database refuses an order where they disagree:
//!
//! ```text
//! price_cents    what the goods cost at the shop's price   (the sale's value)
//! charged_cents  what the member actually pays             (0 when comped)
//! funded_cents   price_cents - charged_cents               (the draw on scholarship)
//! ```
//!
//! A standard-priced order funds nothing; a supported tier funds half; a comp
//! funds the whole price. **The shop never prices above its own price**: the dues
//! scale's patron tier buys at the item's price here, because a payment split
//! across two funds is not something one Stripe payment can express
//! (`finance.transactions.external_ref` is unique per payment, so one payment is
//! one ledger entry). A member who wants to give more gives to `scholarship`
//! directly — finance records an income entry into any fund by id, and stripe's
//! checkout takes a `fund_code` per request, so `scholarship` is already a
//! donation target and an allocation target without anything new being invented.
//!
//! **Why the draw is not a zero amount.** It could not be one: `finance` refuses
//! a zero-amount transaction at two layers — `CONSTRAINT
//! transactions_amount_nonzero CHECK (amount_cents <> 0)` and `income` requiring a
//! positive amount, with the route refusing a non-positive magnitude before that.
//! The only zero amount finance can hold is a **dues waiver** (`dues_waived_is_zero`),
//! which is a membership assessment, not a sale. So a comp is *not* represented as
//! a zero ledger entry, and it does not need to be: the subsidy is a **positive**
//! transfer, which finance expresses perfectly. What the member pays is zero; what
//! the scholarship fund covers is the whole price, and the Annual Financial Report
//! (`GET /api/finance/report/annual`) shows it in the fund and transfer sections
//! because it is a real movement between two funds.
//!
//! ## The money path, and the caller it does not always have
//!
//! `plugin-to-plugin.md` §2(b) permits a plugin to reach another only **as the
//! caller**, forwarding the caller's own authorization. This crate does that in
//! three places, all with a human on the other end:
//!
//! * `POST /api/store/order/{id}/checkout` — a member's own session buys a patch:
//!   forward the caller's `authorization`/`cookie` to stripe's
//!   `POST /api/stripe/checkout`, whose gate re-decides `stripe:checkout`, and
//!   pass stripe's refusal through rather than swallowing it.
//! * `POST /api/store/order/{id}/complete` — a shopkeeper verifies the payment
//!   against stripe's own record and asks stripe to book it, both as the caller,
//!   so the order's completion and the ledger write land in one flow with
//!   answers (§3.2).
//! * `POST /api/store/order/{id}/comp` — a commander comps an order: the comp is
//!   recorded with its reason and authority, and the **draw it produces is booked
//!   in the same call** by asking finance to transfer the price out of
//!   `scholarship` as the caller, so finance's gate re-decides `finance:write`.
//!
//! **One path has no caller, and this crate says so rather than pretending
//! otherwise.** A sliding-scale reduction is applied at checkout *by the shop*,
//! and the draw it produces needs `finance:write` — which the member placing the
//! order does not hold and must not be given. Neither can a machine-originated
//! confirmation (a Stripe webhook saying an order is paid) obtain a bounded
//! Adjutant authorization: §3.2 decided the pattern that solves this — a core
//! transactional outbox with an idempotent consumer, delivery authorised by a
//! declared service principal — and **that is not built yet**. Minting a
//! credential, or calling finance with a credential that is not the caller's, is
//! the one thing §3.1 refuses, and this crate does not do it. Muting the
//! reduction into a cheaper price is the other refusal: it would hide the
//! subsidy.
//!
//! So this crate does four things about it, and stops there:
//!
//! * **It records the truth.** Every order carries `funded_cents` and a
//!   `draw_status` (`unbooked`, `attempting`, `booked`, `refused`, `failed`), and
//!   a reduction's draw is left `unbooked` with the amount visible from the
//!   moment the order is placed.
//! * **It offers a worklist.** `GET /api/store/orders/unsettled` lists the orders
//!   awaiting payment and the completed orders whose draw or ledger entry is not
//!   confirmed, and a six-hourly sweep publishes `store.orders.unsettled` (a
//!   notice; never the mechanism by which the ledger learns).
//! * **It offers the synchronous path where a caller genuinely exists.**
//!   `POST /api/store/order/{id}/draw` books an outstanding draw **as the
//!   caller** — a treasurer holding `finance:write` can close the gap by hand,
//!   today, with their own authority and nothing added.
//! * **It never fakes a completion.** An order is `paid` only after stripe's own
//!   record confirms the payment, and `comped` only with a grant and a reason.
//!
//! ## What a paid order completes
//!
//! Placement prices the order and writes it with its lines in **one statement**
//! (the core mediates one statement per call, so an order and its lines are
//! written together or not at all). Checkout then opens a Stripe Checkout session
//! for `charged_cents` as the caller and stores the result — opaque session
//! references only, never a card detail. Stripe's webhook confirms the payment to
//! *stripe*, which reaches `finance` through stripe's own documented path; this
//! plugin cannot see that webhook, so the order is marked `paid` by a shopkeeper
//! running `/complete`, which asks stripe to verify the payment and book it. The
//! order records finance's answer: `ledger_status`, the transaction id, or the
//! refusal in finance's own words.
//!
//! ## Schema (`store`)
//!
//! `store.catalogue_items` (kind `product`/`rental`, the shop's price, the fund
//! code its proceeds are addressed to, and — for a rental — the equipment item id
//! and nothing else), `store.orders` (the three money figures, the tier, the
//! completion, the comp, the draw) and `store.order_lines` (a snapshot of what
//! was sold at what price, because a sale is a record and the catalogue is
//! editable). Vocabulary that reaches a decision is a constraint, not a
//! convention: a comp with no reason is unrepresentable, a paid order with no
//! payment reference is unrepresentable, and `funded_cents = price_cents -
//! charged_cents` is enforced by the database.
//!
//! ## Configuration
//!
//! `core.plugins.config` for the `store` plugin:
//!
//! ```json
//! {
//!   "base_url": "http://127.0.0.1:8787",
//!   "fund_code": "general",
//!   "category": "store",
//!   "success_url": "https://troop.example/store/paid",
//!   "cancel_url": "https://troop.example/store",
//!   "unsettled_after_minutes": 30
//! }
//! ```
//!
//! `base_url` is *this* Adjutant instance — the address stripe's and finance's
//! routes are dialled at, as `mcp` dials it. This plugin holds **no secret of its
//! own** (it charges nothing and holds no money): it never logs, records or
//! returns a payment credential or a card detail, and the only payment reference
//! it ever writes is stripe's own opaque `pi_…`/`cs_…`.
//!
//! ## Permissions, and why there is no rank in this crate
//!
//! | Permission | What it is |
//! |---|---|
//! | `store:read` | See the catalogue and its prices, and your own orders. |
//! | `store:read_all` | See every order — the shopkeeper's record of what was sold and to whom. |
//! | `store:buy` | Place an order and pay for it. |
//! | `store:manage` | Add and edit catalogue items, order for another member, complete a paid order, book an outstanding draw. |
//! | `store:comp` | Complete an order at no charge: the authority a comp needs. |
//!
//! **"Commander and above" is a role grant, not a rank this plugin knows.** The
//! plugin defines `store:comp` and nothing else; which roles hold it is the
//! troop's business, recorded in `core.role_permissions` by an operator, exactly
//! as every other permission in Adjutant is. The software has no rank concept and
//! this crate does not invent one. A comping caller is expected to hold
//! `finance:write` as well, because the draw its comp produces is a finance
//! transfer and finance's own gate decides it.
//!
//! ## What is deliberately not here
//!
//! * **The client screens.** SPEC §7.16 buys a shop; the screens that show it are
//!   a separate parity workstream.
//! * **A cash sale.** SPEC §7.16 gives the shop one money path and it is stripe's.
//!   A cash or cheque payment recorded directly in finance is finance's own route
//!   and does **not** complete a store order: `/complete` verifies a payment
//!   against stripe's record, and there is deliberately no second way to call an
//!   order paid.
//! * **A refund route.** A refund is an *expense* in finance's vocabulary and
//!   belongs to finance's routes, as SPEC §7.13 already records for `stripe`.
//! * **Any write into `stripe.*` or `finance.*`.** Reported gaps, not patched
//!   crates: stripe's purpose vocabulary has no `purchase`, so a store sale is
//!   sent with `purpose: "donation"` (the only shape its route accepts) while
//!   `category: "store"` and the order's `fund_code` carry the truth into the
//!   ledger; and finance's write routes take a fund **id**, so addressing a fund
//!   by **code** is a read (`GET /api/finance/funds`) followed by the write.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// A thing the shop sells.
pub const KIND_PRODUCT: &str = "product";
/// A thing the shop rents: the fee is the shop's, the item stays `equipment`'s.
pub const KIND_RENTAL: &str = "rental";

/// The two kinds, and the closed set the database constrains.
pub const KINDS: [&str; 2] = [KIND_PRODUCT, KIND_RENTAL];

/// Nothing else: the category a client shows as "other".
pub const CATEGORY_OTHER: &str = "other";
/// SPEC §7.16's first listed good.
pub const CATEGORY_UNIFORM: &str = "uniform";
/// SPEC §7.16's patches.
pub const CATEGORY_PATCH: &str = "patch";
/// Insignia and badges.
pub const CATEGORY_INSIGNIA: &str = "insignia";
/// Camp gear.
pub const CATEGORY_GEAR: &str = "gear";
/// Event merchandise.
pub const CATEGORY_MERCH: &str = "merch";

/// The categories a catalogue entry may have, in the order a client shows them.
pub const CATEGORIES: [&str; 6] = [
    CATEGORY_UNIFORM,
    CATEGORY_PATCH,
    CATEGORY_INSIGNIA,
    CATEGORY_GEAR,
    CATEGORY_MERCH,
    CATEGORY_OTHER,
];

/// Placed and priced; nothing charged yet.
pub const STATUS_OPEN: &str = "open";
/// A Stripe Checkout session was opened for it, as the caller.
pub const STATUS_AWAITING_PAYMENT: &str = "awaiting_payment";
/// Completed as paid: a payment verified against stripe's own record.
pub const STATUS_PAID: &str = "paid";
/// Completed at no charge by a `store:comp` holder, with a reason.
pub const STATUS_COMPED: &str = "comped";

/// The order states, and the closed set the database constrains.
pub const ORDER_STATUSES: [&str; 4] = [
    STATUS_OPEN,
    STATUS_AWAITING_PAYMENT,
    STATUS_PAID,
    STATUS_COMPED,
];

/// The states an order can be completed from.
pub const COMPLETABLE_STATUSES: [&str; 2] = [STATUS_OPEN, STATUS_AWAITING_PAYMENT];

/// Nothing is funded: the member pays the shop's price.
pub const DRAW_NONE: &str = "none";
/// Funded, and no caller has booked it — what a reduction produces at checkout.
pub const DRAW_UNBOOKED: &str = "unbooked";
/// A cross-plugin call is in flight (or its answer was lost).
pub const DRAW_ATTEMPTING: &str = "attempting";
/// Finance wrote both legs; the draw is in the ledger.
pub const DRAW_BOOKED: &str = "booked";
/// Finance refused it, in finance's own words.
pub const DRAW_REFUSED: &str = "refused";
/// The call failed or finance answered a server error.
pub const DRAW_FAILED: &str = "failed";

/// The draw states, and the closed set the database constrains.
pub const DRAW_STATUSES: [&str; 6] = [
    DRAW_NONE,
    DRAW_UNBOOKED,
    DRAW_ATTEMPTING,
    DRAW_BOOKED,
    DRAW_REFUSED,
    DRAW_FAILED,
];

/// Nothing was booked: no caller has attempted it.
pub const LEDGER_UNBOOKED: &str = "unbooked";
/// The entry landed. Stripe answers `booked` when finance confirmed it, and this
/// plugin records that word as the order's `ledger_status`.
///
/// It is deliberately the same string as [`DRAW_BOOKED`]: one vocabulary for
/// "the money landed", whichever side of the boundary said it.
pub const LEDGER_BOOKED: &str = "booked";
/// The target refused it, in its own words.
pub const LEDGER_REFUSED: &str = "refused";
/// The call failed, or the target answered a server error.
pub const LEDGER_FAILED: &str = "failed";

/// Twice the price — but the shop charges its own price, so this tier pays it.
pub const TIER_PATRON: &str = "patron";
/// The shop's price.
pub const TIER_STANDARD: &str = "standard";
/// Half the price, funded from `scholarship`.
pub const TIER_SUPPORTED: &str = "supported";
/// No charge at all, funded from `scholarship`.
pub const TIER_HARDSHIP: &str = "hardship";

/// A sliding-scale tier and the share of a price it pays, in **basis points**
/// (1 bp = 0.01%), so a price is exact integer arithmetic all the way down.
///
/// The codes are deliberately `finance`'s dues tiers (`finance::TIERS`): the same
/// self-report a scout made for dues prices their patch, and a client can carry
/// one tier code across both plugins. The *prices* are the shop's own, and so is
/// the rule that the shop never prices above its own price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Stable code stored in `store.orders.price_tier`.
    pub code: &'static str,
    /// Display label for a client.
    pub label: &'static str,
    /// The tier's share of the price, in basis points.
    pub bps: i64,
    /// One sentence a member reads when choosing.
    pub description: &'static str,
}

/// The scale, in the order a client should show it.
///
/// Identical to the dues scale's shares on purpose — "the same principle as dues"
/// (SPEC §7.16) — with one difference that is the shop's own: a share above the
/// full price is charged as the price, never above it.
pub const TIERS: [Tier; 4] = [
    Tier {
        code: TIER_PATRON,
        label: "Patron",
        bps: 20_000,
        description: "Twice the price everywhere else; the shop charges its price and takes no more",
    },
    Tier {
        code: TIER_STANDARD,
        label: "Standard",
        bps: 10_000,
        description: "The shop's price",
    },
    Tier {
        code: TIER_SUPPORTED,
        label: "Supported",
        bps: 5_000,
        description: "Half the price — the rest is funded from the scholarship fund",
    },
    Tier {
        code: TIER_HARDSHIP,
        label: "Hardship",
        bps: 0,
        description: "No charge — funded in full from the scholarship fund, and completed with a comp",
    },
];

/// The tier codes, in the order [`TIERS`] declares them.
pub const TIER_CODES: [&str; 4] = [TIER_PATRON, TIER_STANDARD, TIER_SUPPORTED, TIER_HARDSHIP];

/// The tier an order is priced at when nobody says otherwise: the full price, so
/// the shop is never short because somebody did not answer.
pub const DEFAULT_TIER: &str = TIER_STANDARD;

/// Basis points in a whole share. A tier's share is capped at this.
pub const FULL_SHARE_BPS: i64 = 10_000;

/// **The scholarship fund**, by the code `finance` seeds it under (SPEC §7.5).
///
/// This is a contract with another plugin, not a label: every funded order's
/// subsidy is a transfer *out of this fund*, resolved by code through finance's
/// `GET /api/finance/funds` at the moment of the call — an id is never stored,
/// because an id is finance's to change (`plugin-to-plugin.md` §3.5).
pub const FUND_SCHOLARSHIP: &str = "scholarship";

/// The fund the shop's proceeds land in when a catalogue item names none.
pub const FUND_GENERAL: &str = "general";

/// The ledger category a store sale is filed under. `store` is a category of this
/// plugin's own choosing — finance accepts any non-empty string — and it is what
/// makes "what the shop took" a filter on the ledger rather than a guess.
pub const CATEGORY_STORE: &str = "store";

/// Stripe's purpose vocabulary is `dues`, `donation`, `event_fee` and its route
/// accepts nothing else, so a store sale is sent as a `donation` — money into the
/// troop's funds without a dues tag — while `category` and `fund_code` carry the
/// truth into the ledger. A `purchase` purpose in stripe (its own CHECK
/// constraint and vocabulary) is the real fix and is not this crate's to make.
pub const PURPOSE_FOR_A_STORE_SALE: &str = "donation";

/// See the catalogue and your own orders.
pub const PERM_READ: &str = "store:read";
/// See every order — the shopkeeper's record.
pub const PERM_READ_ALL: &str = "store:read_all";
/// Place an order and pay for it.
pub const PERM_BUY: &str = "store:buy";
/// Catalogue, other members' orders, paid completion, outstanding draws.
pub const PERM_MANAGE: &str = "store:manage";
/// Complete an order at no charge. A comp is an authority, not a price.
pub const PERM_COMP: &str = "store:comp";

/// An order was placed and priced.
pub const EVENT_ORDER_PLACED: &str = "store.order.placed";
/// A Stripe Checkout session was opened for it, as the caller.
pub const EVENT_ORDER_AWAITING_PAYMENT: &str = "store.order.awaiting_payment";
/// Completed as paid, with what finance answered.
pub const EVENT_ORDER_PAID: &str = "store.order.paid";
/// Completed at no charge, with its reason, authority and draw.
pub const EVENT_ORDER_COMPED: &str = "store.order.comped";
/// A draw on the scholarship fund landed in the ledger.
pub const EVENT_DRAW_BOOKED: &str = "store.order.draw_booked";
/// The sweep's notice: orders awaiting payment, or completed with a draw or a
/// ledger entry that is not confirmed.
pub const EVENT_ORDERS_UNSETTLED: &str = "store.orders.unsettled";

/// The §2(b) mechanism a call used, named in every audit entry it produces, so an
/// operator reading the log can see that a plugin reached across a boundary and
/// how.
pub const MECHANISM_STRIPE_CHECKOUT: &str = "caller-forward:POST /api/stripe/checkout";
/// A verification read against stripe's own payment record.
pub const MECHANISM_STRIPE_PAYMENT: &str = "caller-forward:GET /api/stripe/payment/{id}";
/// Asking stripe to hand the confirmed payment to finance.
pub const MECHANISM_STRIPE_BOOK: &str = "caller-forward:POST /api/stripe/payment/{id}/book";
/// The scholarship draw, as finance's balanced transfer.
pub const MECHANISM_FINANCE_TRANSFER: &str = "caller-forward:POST /api/finance/transfer";

/// Stripe's checkout route.
pub const STRIPE_CHECKOUT_PATH: &str = "/api/stripe/checkout";
/// Stripe's payment route, before the id and the sub-path are appended.
pub const STRIPE_PAYMENT_PATH: &str = "/api/stripe/payment";
/// Finance's funds route: the code → id resolution, finance owning its ids.
pub const FINANCE_FUNDS_PATH: &str = "/api/finance/funds";
/// Finance's transfer route: two legs, one statement, one group.
pub const FINANCE_TRANSFER_PATH: &str = "/api/finance/transfer";

/// The largest catalogue price accepted, in cents ($1,000,000), so a bad number
/// cannot overflow the arithmetic that follows it.
pub const MAX_PRICE_CENTS: i64 = 100_000_000;
/// The most of one line that may be ordered at once.
pub const MAX_QUANTITY: i64 = 100;
/// The most lines one order may carry.
pub const MAX_LINES: usize = 50;
/// The longest a catalogue name may be.
pub const MAX_NAME: usize = 200;
/// The longest a description may be.
pub const MAX_DESCRIPTION: usize = 1_000;
/// The longest an order note may be.
pub const MAX_NOTE: usize = 500;
/// The longest a comp reason may be. A reason is mandatory and is the whole
/// accountability of the act, so it is bounded but never optional.
pub const MAX_REASON: usize = 500;
/// The default page size.
pub const DEFAULT_LIMIT: i64 = 50;
/// The largest page a caller may ask for.
pub const MAX_LIMIT: i64 = 200;
/// How many orders the sweep names in its notice.
pub const SWEEP_LIMIT: i64 = 25;
/// An order awaiting payment for longer than this is worth a shopkeeper's look.
pub const DEFAULT_UNSETTLED_MINUTES: i64 = 30;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The plugin's parsed configuration. No secret lives here: the shop charges
/// nothing itself and holds no credential.
#[derive(Debug, Clone, Default)]
pub struct StoreConfig {
    raw: Value,
}

impl StoreConfig {
    /// Parse `core.plugins.config`, warning about anything unusable rather than
    /// silently defaulting the things that matter.
    pub fn from_value(value: &Value) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        if !value.is_null() && !value.is_object() {
            warnings.push(format!(
                "config is not an object ({value}); the defaults are in use"
            ));
        }
        let cfg = Self { raw: value.clone() };
        if cfg.base_url().is_none() {
            warnings.push(
                "no base_url configured: the shop cannot reach stripe's checkout or finance's \
                 ledger, and every money route will refuse until one is set"
                    .to_string(),
            );
        }
        if cfg.fund_code().is_empty() {
            warnings.push(
                "fund_code is blank: falling back to \"general\", the fund a sale lands in \
                 when a catalogue item names none"
                    .to_string(),
            );
        }
        (cfg, warnings)
    }

    fn text(&self, key: &str) -> Option<String> {
        self.raw
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// This Adjutant instance: where stripe's and finance's routes are dialled.
    /// Missing is refused loudly on every money route rather than defaulted to
    /// somewhere that books nothing.
    pub fn base_url(&self) -> Option<&str> {
        self.raw
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// The fund a sale lands in when the catalogue item names none.
    pub fn fund_code(&self) -> String {
        self.text("fund_code").unwrap_or_else(|| FUND_GENERAL.to_string())
    }

    /// The ledger category store sales are filed under.
    pub fn category(&self) -> String {
        self.text("category").unwrap_or_else(|| CATEGORY_STORE.to_string())
    }

    /// A default return-to-shop URL for a Stripe Checkout session.
    pub fn success_url(&self) -> Option<String> {
        self.text("success_url")
    }

    /// A default cancel URL for a Stripe Checkout session.
    pub fn cancel_url(&self) -> Option<String> {
        self.text("cancel_url")
    }

    /// How long an order may await payment before the worklist calls it unsettled.
    pub fn unsettled_after_minutes(&self) -> i64 {
        self.raw
            .get("unsettled_after_minutes")
            .and_then(Value::as_i64)
            .filter(|minutes| (0..=100_000).contains(minutes))
            .unwrap_or(DEFAULT_UNSETTLED_MINUTES)
    }
}

// ---------------------------------------------------------------------------
// The sliding scale — pure arithmetic, so the numbers are testable
// ---------------------------------------------------------------------------

/// The tier a code names, or `None` for a code this plugin does not know.
pub fn tier_of(code: &str) -> Option<&'static Tier> {
    let code = code.trim().to_ascii_lowercase();
    TIERS.iter().find(|tier| tier.code == code)
}

/// What a share of a price comes to, in cents, rounded half-up to the cent.
///
/// `price * bps / 10 000` computed in `i128` and rounded once, at the end: the
/// multiplication happens before any division, so no precision is lost to an
/// intermediate floor, and the result is clamped rather than wrapping.
pub fn share_cents(price_cents: i64, bps: i64) -> i64 {
    let price = i128::from(price_cents.max(0));
    let share = i128::from(bps.max(0));
    let rounded = (price * share + 5_000) / 10_000;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

/// What a tier pays for one unit, and the shop never takes more than its price.
///
/// Capped rather than left as a multiplier: the dues scale's patron tier pays
/// twice the *membership cost* to cover a scout who cannot, but a shop selling a
/// patch at twice its price would be inventing a second price — and a payment
/// split across two funds is not expressible through one Stripe payment. A member
/// who wants to give more gives to `scholarship` directly.
pub fn charged_unit_cents(list_price_cents: i64, tier: &str) -> Option<i64> {
    let tier = tier_of(tier)?;
    let price = list_price_cents.max(0);
    Some(share_cents(price, tier.bps).min(price))
}

/// What a line (or a whole order) prices out to: the shop's price, what the
/// member pays, and the draw on the scholarship fund. The unit is carried so a
/// caller never has to divide a total back into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pricing {
    /// What one unit is charged at this tier, capped at the shop's price.
    pub unit_cents: i64,
    /// `list_price_cents × quantity` — the sale's value.
    pub price_cents: i64,
    /// `unit_cents × quantity` — what the member pays.
    pub charged_cents: i64,
    /// The difference: the draw on the scholarship fund.
    pub funded_cents: i64,
}

/// Price one line: the shop's price, what the tier pays, and the subsidy.
pub fn pricing(list_price_cents: i64, quantity: i64, tier: &str) -> Option<Pricing> {
    let unit_cents = charged_unit_cents(list_price_cents, tier)?;
    let quantity = quantity.max(0);
    let price_cents = mul_cents(list_price_cents, quantity).ok()?;
    let charged_cents = mul_cents(unit_cents, quantity).ok()?;
    Some(Pricing {
        unit_cents,
        price_cents,
        charged_cents,
        funded_cents: price_cents - charged_cents,
    })
}

/// The whole scale for one price — what a client shows a member who is choosing.
/// Pure, so the numbers a member sees are testable without a database.
pub fn scale_table(list_price_cents: i64) -> Vec<Value> {
    TIERS
        .iter()
        .map(|tier| {
            let charged = charged_unit_cents(list_price_cents, tier.code).unwrap_or_default();
            let price = list_price_cents.max(0);
            json!({
                "tier": tier.code,
                "label": tier.label,
                "share_bps": tier.bps,
                "share_percent": format_percent(tier.bps),
                "charged_cents": charged,
                "charged_display": format_cents(charged),
                "funded_cents": price - charged,
                "funded_display": format_cents(price - charged),
                "description": tier.description,
                "self_reportable": true,
                "capped_at_price": tier.bps > FULL_SHARE_BPS,
            })
        })
        .collect()
}

/// `price × quantity` in `i128`, refused rather than wrapped when it overflows.
pub fn mul_cents(cents: i64, quantity: i64) -> Result<i64, String> {
    let product = i128::from(cents) * i128::from(quantity);
    i64::try_from(product).map_err(|_| {
        format!("{cents} cents × {quantity} is more than this ledger can hold")
    })
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// The only place money becomes a string. Every API field is `*_cents`; this
/// exists so a response can be read without a client-side formatter.
pub fn format_cents(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let digits = (abs / 100).to_string();
    let remainder = abs % 100;
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!(
        "{}$ {grouped}.{remainder:02}",
        if cents < 0 { "-" } else { "" }
    )
    .replace("$ ", "$")
}

/// Basis points as a percentage string: `1000` → `"10%"`, `250` → `"2.5%"`.
pub fn format_percent(bps: i64) -> String {
    let negative = bps < 0;
    let abs = bps.unsigned_abs();
    let whole = abs / 100;
    let fraction = abs % 100;
    let sign = if negative { "-" } else { "" };
    if fraction == 0 {
        format!("{sign}{whole}%")
    } else if fraction.is_multiple_of(10) {
        format!("{sign}{whole}.{}%", fraction / 10)
    } else {
        format!("{sign}{whole}.{fraction:02}%")
    }
}

/// A date, or a plain statement of what a date looks like.
pub fn parse_date(raw: &str) -> Result<NaiveDate, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("a date is required (YYYY-MM-DD)".to_string());
    }
    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .map_err(|_| format!("{trimmed:?} is not a date (YYYY-MM-DD)"))
}

/// A trimmed, non-blank string, or `None`.
fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A trimmed, non-blank, bounded string, or a message a client can act on.
fn bounded(raw: &str, max: usize, field: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{field} is required and must not be blank"));
    }
    if value.chars().count() > max {
        return Err(format!("{field} must be at most {max} characters"));
    }
    Ok(value.to_string())
}

/// The caller's user id, when the request is authenticated.
fn caller_of(req: &PluginRequest) -> Option<String> {
    req.identity
        .as_ref()
        .map(|identity| identity.user_id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Refuse a request that carries no credential to forward.
///
/// This is the whole point of the §2(b) mechanism: a cross-plugin call is made
/// with **the caller's own** authorization and nothing else, so a request that
/// carries none has nothing to forward — and this plugin holds no credential of
/// its own to substitute, which is what `plugin-to-plugin.md` §3.1 refuses.
fn require_forwardable(req: &PluginRequest, what: &str) -> Result<Vec<(String, String)>, SdkError> {
    let headers = forward_headers(req);
    if headers.is_empty() {
        return Err(SdkError::Forbidden(format!(
            "this request carries no authorization or session cookie, so there is no credential \
             to forward to {what}; the target's own gate decides, and this plugin holds no \
             credential of its own (plugin-to-plugin.md §2(b))"
        )));
    }
    Ok(headers)
}

/// The headers relayed to another plugin: **the caller's own credentials, and
/// nothing else.**
///
/// This is `mcp`'s discipline (`plugins/mcp/src/lib.rs`, the reference
/// implementation named in `plugin-to-plugin.md`) and the same two names `stripe`
/// forwards: no credential is minted, substituted or upgraded, and a request that
/// carries neither header has nothing to forward.
pub fn forward_headers(req: &PluginRequest) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in ["authorization", "cookie"] {
        let value = req.headers.get(name).or_else(|| {
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        });
        if let Some(value) = value {
            if !value.trim().is_empty() {
                out.push((name.to_string(), value.clone()));
            }
        }
    }
    out
}

/// The status a route answers with when another plugin refused, given that
/// plugin's own status.
///
/// Passing the target's answer through is the point: a `403` means this caller's
/// authority did not reach what they asked for, and a `409` is the target's own
/// refusal — neither is a failure *of this plugin*, and flattening both into a
/// `500` would hide which authority was missing.
fn pass_through_status(status: u16) -> u16 {
    match status {
        400..=409 => status,
        _ => 502,
    }
}

/// The `error` string out of a target's error body, or a plain statement of the
/// status.
fn message_of(body: &[u8], status: u16) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| match error {
                    Value::String(message) => Some(message.clone()),
                    Value::Object(map) => map
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
                .or_else(|| value.get("message").and_then(Value::as_str).map(str::to_string))
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| format!("the request answered {status}"))
}

/// A JSON body when one was sent, and the type's default when the caller sent
/// none — so `POST …/checkout` with no body is not an "invalid JSON" error, while
/// a body that *is* malformed stays one.
fn body_or_default<T>(req: &PluginRequest) -> Result<T, SdkError>
where
    T: for<'de> Deserialize<'de> + Default,
{
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    req.json()
}

/// The result of a call made to another plugin on the caller's behalf.
#[derive(Debug, Clone)]
struct CallOutcome {
    /// `true` when the target answered 2xx.
    ok: bool,
    /// The target's HTTP status, when there was one.
    http_status: Option<u16>,
    /// The parsed 2xx body, when there was a readable one.
    body: Value,
    /// Why it did not land, in the target's own words (or the transport's).
    error: Option<String>,
    /// The status this plugin answers with: the target's own when the *call* was
    /// refused, and this plugin's when the problem is a fact about the order.
    answer_status: u16,
}

impl CallOutcome {
    fn unreachable(error: String) -> Self {
        Self {
            ok: false,
            http_status: None,
            body: Value::Null,
            error: Some(error),
            answer_status: 502,
        }
    }

}

/// Call another plugin **as the caller**, over the core's mediated HTTP.
///
/// The headers are the caller's own; nothing here mints, upgrades or substitutes
/// a credential. A non-2xx answer is returned as an outcome rather than an error,
/// because the target's refusal is an answer this plugin must pass on.
async fn call_as_caller(
    c: &PluginContext,
    method: &str,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Value>,
    what: &str,
) -> CallOutcome {
    let payload = body.map(|value| {
        (
            "application/json".to_string(),
            serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        )
    });
    match c.http.request(method.to_string(), url, headers, payload).await {
        Ok(response) if (200..300).contains(&response.status) => {
            let parsed = response.json::<Value>().unwrap_or(Value::Null);
            CallOutcome {
                ok: true,
                http_status: Some(response.status),
                body: parsed,
                error: None,
                answer_status: response.status,
            }
        }
        Ok(response) => {
            let message = format!(
                "{what} was refused: {}",
                message_of(&response.body, response.status)
            );
            CallOutcome {
                ok: false,
                http_status: Some(response.status),
                body: Value::Null,
                error: Some(message),
                answer_status: pass_through_status(response.status),
            }
        }
        Err(e) => CallOutcome::unreachable(format!("{what} could not be reached: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Reading another plugin's answer
// ---------------------------------------------------------------------------

/// The fund whose code is `code`, out of finance's funds list — by **code**,
/// because that is the contract; finance's ids are finance's to change.
fn fund_by_code(funds: &[Value], code: &str) -> Option<Value> {
    funds
        .iter()
        .find(|fund| fund["code"].as_str() == Some(code))
        .cloned()
}

/// Resolve two fund codes into finance's ids **as the caller**.
///
/// Two codes, one read: `GET /api/finance/funds` carries the caller's own
/// credential, so finance's gate decides what they may see, and a fund finance
/// will not show the caller is a fund this plugin will not move money into.
async fn resolve_funds(
    c: &PluginContext,
    cfg: &StoreConfig,
    headers: &[(String, String)],
    codes: &[&str],
) -> Result<(Vec<(String, i64)>, CallOutcome), CallOutcome> {
    let Some(base) = cfg.base_url() else {
        return Err(CallOutcome {
            ok: false,
            http_status: None,
            body: Value::Null,
            error: Some("no base_url is configured for the store plugin".to_string()),
            answer_status: 503,
        });
    };
    let outcome = call_as_caller(
        c,
        "GET",
        format!("{base}{FINANCE_FUNDS_PATH}?include_inactive=1"),
        headers.to_vec(),
        None,
        "reading finance's funds",
    )
    .await;
    if !outcome.ok {
        return Err(outcome);
    }
    let funds = outcome
        .body
        .get("funds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut resolved = Vec::new();
    for code in codes {
        let Some(fund) = fund_by_code(&funds, code) else {
            return Err(CallOutcome {
                ok: false,
                http_status: outcome.http_status,
                body: Value::Null,
                error: Some(format!(
                    "finance has no fund with code {code:?} visible to this caller"
                )),
                answer_status: 400,
            });
        };
        if fund["active"].as_bool() == Some(false) {
            return Err(CallOutcome {
                ok: false,
                http_status: outcome.http_status,
                body: Value::Null,
                error: Some(format!("finance's fund {code:?} is inactive")),
                answer_status: 400,
            });
        }
        let Some(id) = fund["id"].as_i64() else {
            return Err(CallOutcome {
                ok: false,
                http_status: outcome.http_status,
                body: Value::Null,
                error: Some(format!("finance's fund {code:?} came back without an id")),
                answer_status: 502,
            });
        };
        resolved.push((code.to_string(), id));
    }
    Ok((resolved, outcome))
}

/// Book one draw on the scholarship fund, **as the caller**.
///
/// A draw is a **balanced transfer**: `scholarship` pays, the order's fund
/// receives, and finance writes both legs in one statement under one group id, so
/// the sum of every fund is unchanged. There is no zero-amount entry anywhere in
/// this, which is exactly why a comp can be represented at all (see the module
/// docs).
///
/// `allow_overdraft` is passed through and never assumed: whether the scholarship
/// fund may go negative is a decision for the caller to state and finance to
/// record, not one this plugin makes on their behalf.
async fn book_draw(
    c: &PluginContext,
    cfg: &StoreConfig,
    headers: &[(String, String)],
    order: &Value,
    description: &str,
    allow_overdraft: bool,
) -> CallOutcome {
    let Some(base) = cfg.base_url() else {
        return CallOutcome {
            ok: false,
            http_status: None,
            body: Value::Null,
            error: Some("no base_url is configured for the store plugin".to_string()),
            answer_status: 503,
        };
    };
    let funded = order["funded_cents"].as_i64().unwrap_or(0);
    let to_code = order["fund_code"].as_str().unwrap_or_default().to_string();
    if funded <= 0 {
        return CallOutcome {
            ok: false,
            http_status: None,
            body: Value::Null,
            error: Some("this order funds nothing, so there is no draw to book".to_string()),
            answer_status: 409,
        };
    }
    if to_code.is_empty() || to_code == FUND_SCHOLARSHIP {
        return CallOutcome {
            ok: false,
            http_status: None,
            body: Value::Null,
            error: Some(format!(
                "a draw moves money out of {FUND_SCHOLARSHIP} into the fund the order's proceeds \
                 land in; this order lands in {to_code:?}, and finance refuses a transfer between \
                 one fund and itself"
            )),
            answer_status: 409,
        };
    }
    let (funds, _) =
        match resolve_funds(c, cfg, headers, &[FUND_SCHOLARSHIP, &to_code]).await {
            Ok(resolved) => resolved,
            Err(e) => return e,
        };
    let from_id = funds
        .iter()
        .find(|(code, _)| code == FUND_SCHOLARSHIP)
        .map(|(_, id)| *id)
        .unwrap_or_default();
    let to_id = funds
        .iter()
        .find(|(code, _)| code.as_str() == to_code.as_str())
        .map(|(_, id)| *id)
        .unwrap_or_default();

    let mut body = json!({
        "from_fund_id": from_id,
        "to_fund_id": to_id,
        "amount_cents": funded,
        "description": description,
    });
    if allow_overdraft {
        body["allow_overdraft"] = json!(true);
    }
    let mut outcome = call_as_caller(
        c,
        "POST",
        format!("{base}{FINANCE_TRANSFER_PATH}"),
        headers.to_vec(),
        Some(body),
        "the scholarship draw",
    )
    .await;
    if outcome.ok {
        // Finance answers with the group id the two legs share; that is the
        // reference this plugin records, and it is opaque by construction.
        let group = outcome.body["transfer_group"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if group.is_empty() {
            outcome.ok = false;
            outcome.error = Some(
                "finance accepted the transfer but named no transfer_group, so the draw cannot \
                 be referenced"
                    .to_string(),
            );
            outcome.answer_status = 502;
        } else {
            outcome.body = json!({
                "transfer_group": group,
                "amount_cents": outcome.body["amount_cents"],
                "entries": outcome.body["entries"],
                "sum_cents": outcome.body["sum_cents"],
                "balances": outcome.body["balances"],
            });
        }
    }
    outcome
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// The Store plugin.
pub struct StorePlugin {
    ctx: OnceLock<PluginContext>,
    config: OnceLock<StoreConfig>,
}

impl StorePlugin {
    /// A plugin that has not been handed a context yet.
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
            config: OnceLock::new(),
        }
    }

    /// The core hands this in `init`; every route closure owns a clone of it.
    pub fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()/schedules()")
    }

    /// The parsed config. Panics only if `routes()` runs before `init()`, which
    /// the core's lifecycle guarantees cannot happen.
    pub fn config(&self) -> &StoreConfig {
        self.config
            .get()
            .expect("core must call init() before routes()")
    }
}

impl Default for StorePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The schema (SPEC §7.16: this plugin's own tables; stripe's and finance's are
/// theirs).
///
/// Vocabulary that reaches a decision is a constraint rather than a convention: a
/// comp with no reason, a paid order with no payment reference, and money figures
/// that do not agree are all unrepresentable.
const MIGRATION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS catalogue_items (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'product',
    sku TEXT,
    name TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT 'other',
    description TEXT NOT NULL DEFAULT '',
    base_price_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'cad',
    fund_code TEXT NOT NULL DEFAULT '',
    equipment_item_id BIGINT,
    active BOOLEAN NOT NULL DEFAULT true,
    created_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT store_items_kind_valid CHECK (kind IN ('product', 'rental')),
    CONSTRAINT store_items_name_not_blank CHECK (btrim(name) <> ''),
    CONSTRAINT store_items_category_valid CHECK (category IN (
        'uniform', 'patch', 'insignia', 'gear', 'merch', 'other')),
    CONSTRAINT store_items_price_valid CHECK (
        base_price_cents >= 0 AND base_price_cents <= 100000000),
    CONSTRAINT store_items_rental_names_its_item CHECK (
        (kind = 'rental') = (equipment_item_id IS NOT NULL)),
    CONSTRAINT store_items_equipment_id_positive CHECK (
        equipment_item_id IS NULL OR equipment_item_id > 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_store_items_sku ON catalogue_items(sku)
  WHERE sku IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_store_items_kind ON catalogue_items(kind, active);
CREATE INDEX IF NOT EXISTS idx_store_items_category ON catalogue_items(category);

CREATE TABLE IF NOT EXISTS orders (
    id BIGSERIAL PRIMARY KEY,
    member_id TEXT NOT NULL,
    placed_by TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'open',
    currency TEXT NOT NULL DEFAULT 'cad',
    price_tier TEXT NOT NULL DEFAULT 'standard',
    price_cents BIGINT NOT NULL DEFAULT 0,
    charged_cents BIGINT NOT NULL DEFAULT 0,
    funded_cents BIGINT NOT NULL DEFAULT 0,
    fund_code TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    stripe_session_row BIGINT,
    stripe_session_id TEXT NOT NULL DEFAULT '',
    checkout_url TEXT NOT NULL DEFAULT '',
    payment_ref TEXT NOT NULL DEFAULT '',
    ledger_status TEXT NOT NULL DEFAULT '',
    ledger_transaction_id TEXT,
    ledger_error TEXT,
    completed_by TEXT NOT NULL DEFAULT '',
    completed_at TIMESTAMPTZ,
    comp_reason TEXT NOT NULL DEFAULT '',
    comp_by TEXT NOT NULL DEFAULT '',
    comp_at TIMESTAMPTZ,
    draw_status TEXT NOT NULL DEFAULT 'none',
    draw_ref TEXT NOT NULL DEFAULT '',
    draw_error TEXT,
    draw_by TEXT NOT NULL DEFAULT '',
    draw_attempted_at TIMESTAMPTZ,
    draw_booked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT store_orders_member_present CHECK (btrim(member_id) <> ''),
    CONSTRAINT store_orders_status_valid CHECK (status IN (
        'open', 'awaiting_payment', 'paid', 'comped')),
    CONSTRAINT store_orders_tier_valid CHECK (price_tier IN (
        'patron', 'standard', 'supported', 'hardship')),
    CONSTRAINT store_orders_money_not_negative CHECK (
        price_cents >= 0 AND charged_cents >= 0 AND funded_cents >= 0),
    CONSTRAINT store_orders_charge_within_price CHECK (charged_cents <= price_cents),
    CONSTRAINT store_orders_funding_is_the_difference CHECK (
        funded_cents = price_cents - charged_cents),
    CONSTRAINT store_orders_draw_status_valid CHECK (draw_status IN (
        'none', 'unbooked', 'attempting', 'booked', 'refused', 'failed')),
    CONSTRAINT store_orders_draw_matches_funding CHECK (
        (funded_cents = 0) = (draw_status = 'none')),
    CONSTRAINT store_orders_draw_booked_has_reference CHECK (
        draw_status <> 'booked' OR (btrim(draw_ref) <> '' AND draw_booked_at IS NOT NULL)),
    CONSTRAINT store_orders_paid_is_charged CHECK (
        status <> 'paid' OR (charged_cents = price_cents AND charged_cents > 0
            AND btrim(payment_ref) <> '' AND completed_at IS NOT NULL)),
    CONSTRAINT store_orders_comp_is_an_authority CHECK (
        status <> 'comped' OR (charged_cents = 0 AND funded_cents = price_cents
            AND btrim(comp_reason) <> '' AND btrim(comp_by) <> '' AND comp_at IS NOT NULL)),
    CONSTRAINT store_orders_awaiting_has_a_session CHECK (
        status <> 'awaiting_payment' OR btrim(stripe_session_id) <> '')
);
CREATE INDEX IF NOT EXISTS idx_store_orders_member ON orders(member_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_status ON orders(status, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_draw ON orders(draw_status, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_comp ON orders(comp_at DESC)
  WHERE status = 'comped';

CREATE TABLE IF NOT EXISTS order_lines (
    id BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    catalogue_item_id BIGINT NOT NULL,
    item_name TEXT NOT NULL,
    item_kind TEXT NOT NULL DEFAULT 'product',
    fund_code TEXT NOT NULL DEFAULT '',
    equipment_item_id BIGINT,
    list_price_cents BIGINT NOT NULL,
    unit_price_cents BIGINT NOT NULL,
    quantity INTEGER NOT NULL,
    line_total_cents BIGINT NOT NULL,
    CONSTRAINT store_lines_kind_valid CHECK (item_kind IN ('product', 'rental')),
    CONSTRAINT store_lines_quantity_range CHECK (quantity > 0 AND quantity <= 100),
    CONSTRAINT store_lines_prices_valid CHECK (
        list_price_cents >= 0 AND unit_price_cents >= 0),
    CONSTRAINT store_lines_charged_within_price CHECK (unit_price_cents <= list_price_cents),
    CONSTRAINT store_lines_total_matches CHECK (line_total_cents = unit_price_cents * quantity),
    CONSTRAINT store_lines_rental_names_its_item CHECK (
        (item_kind = 'rental') = (equipment_item_id IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_store_lines_order ON order_lines(order_id);
CREATE INDEX IF NOT EXISTS idx_store_lines_item ON order_lines(catalogue_item_id);
"#;

#[async_trait]
impl AdjutantPlugin for StorePlugin {
    fn id(&self) -> &str {
        "store"
    }

    fn name(&self) -> &str {
        "Store"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let (cfg, warnings) = StoreConfig::from_value(&ctx.config);
        for warning in &warnings {
            eprintln!("[adjutant-store] {warning}");
        }
        let _ = self.config.set(cfg);
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new(
                PERM_READ,
                "See the catalogue and its prices, and your own orders",
            ),
            Permission::new(
                PERM_READ_ALL,
                "See every order — the troop's record of what was sold and to whom, and the \
                 orders whose draw or ledger entry is not confirmed",
            ),
            Permission::new(PERM_BUY, "Place an order and pay for it"),
            Permission::new(
                PERM_MANAGE,
                "Add and edit catalogue items, order for another member, complete a paid order, \
                 and book an outstanding scholarship draw as you (which needs your own \
                 finance:write)",
            ),
            Permission::new(
                PERM_COMP,
                "Complete an order at no charge, with a reason — a comp is an authority, and the \
                 draw it produces is booked as you (which needs your own finance:write)",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(1, "store_schema", MIGRATION_SCHEMA)]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        store_routes(self.ctx(), self.config())
    }

    // No subscriptions, on purpose. A paid order is completed by a caller
    // (plugin-to-plugin.md §2(b)) — stripe's webhook confirms the *payment* to
    // stripe, and an event could not carry an answer back here (§3.2). A
    // subscriber that marked an order paid would be exactly the fire-and-forget
    // money path that section refuses, and it could not even correlate reliably:
    // `stripe.payment.confirmed` carries the Checkout session id only for the
    // `checkout.session.completed` shape, and empty for `payment_intent.succeeded`.
    // The worklist and a caller-driven completion are the honest mechanism.

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone(); // owned: the closure must not borrow self
        let cfg = self.config().clone();
        vec![Schedule::new(
            "unsettled_sweep",
            std::time::Duration::from_secs(6 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                let cfg = cfg.clone();
                async move { unsettled_sweep(&c, &cfg).await }
            }),
        )]
    }
}

export_plugin!(StorePlugin);

/// Every route this plugin serves, in the order the API reference lists them.
fn store_routes(ctx: &PluginContext, cfg: &StoreConfig) -> Vec<RouteDefinition> {
    vec![
        route_health(ctx, cfg),
        route_list_items(ctx),
        route_create_item(ctx),
        route_get_item(ctx),
        route_edit_item(ctx),
        route_place_order(ctx, cfg),
        route_list_orders(ctx),
        route_unsettled(ctx, cfg),
        route_get_order(ctx),
        route_checkout(ctx, cfg),
        route_complete(ctx, cfg),
        route_comp(ctx, cfg),
        route_draw(ctx, cfg),
        route_comps(ctx),
    ]
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// The columns a catalogue item is read as.
const ITEM_FIELDS: &str = "i.id, i.kind, i.sku, i.name, i.category, i.description, \
     i.base_price_cents, i.currency, i.fund_code, i.equipment_item_id, i.active, \
     i.created_by, i.created_at, i.updated_at";

/// The columns an order is read as.
const ORDER_FIELDS: &str = "o.id, o.member_id, o.placed_by, o.status, o.currency, o.price_tier, \
     o.price_cents, o.charged_cents, o.funded_cents, o.fund_code, o.note, \
     o.stripe_session_row, o.stripe_session_id, o.checkout_url, \
     o.payment_ref, o.ledger_status, o.ledger_transaction_id, o.ledger_error, \
     o.completed_by, o.completed_at, o.comp_reason, o.comp_by, o.comp_at, \
     o.draw_status, o.draw_ref, o.draw_error, o.draw_by, o.draw_attempted_at, \
     o.draw_booked_at, o.created_at, o.updated_at";

/// The columns an order line is read as.
const LINE_FIELDS: &str = "l.id, l.order_id, l.catalogue_item_id, l.item_name, l.item_kind, \
     l.fund_code, l.equipment_item_id, l.list_price_cents, l.unit_price_cents, l.quantity, \
     l.line_total_cents";

/// One order and its lines, in **one statement**.
///
/// The core mediates one statement per call, so a plugin cannot open a
/// transaction across calls — which means an order and its lines are written
/// together or not at all, or a crash between them leaves an order priced for
/// goods it does not list. The data-modifying CTE is what makes that a property
/// of the statement. The lines arrive as parallel arrays (`unnest`), which is the
/// same shape `finance`'s transfer uses for its two legs.
fn sql_insert_order(c: &PluginContext) -> String {
    format!(
        "WITH new_order AS ( \
             INSERT INTO {orders} \
                 (member_id, placed_by, status, currency, price_tier, price_cents, \
                  charged_cents, funded_cents, fund_code, note, draw_status) \
             VALUES ($1, $2, 'open', $3, $4, $5, $6, $7, $8, $9, \
                     CASE WHEN $7 > 0 THEN '{unbooked}' ELSE '{none}' END) \
             RETURNING id \
         ) \
         INSERT INTO {lines} \
             (order_id, catalogue_item_id, item_name, item_kind, fund_code, \
              equipment_item_id, list_price_cents, unit_price_cents, quantity, line_total_cents) \
         SELECT o.id, l.item_id, l.item_name, l.item_kind, l.fund_code, \
                NULLIF(l.equipment_id, '')::bigint, l.list_price_cents, l.unit_price_cents, \
                l.quantity, l.line_total_cents \
         FROM new_order o, \
              unnest($10::bigint[], $11::text[], $12::text[], $13::text[], $14::text[], \
                     $15::bigint[], $16::bigint[], $17::int[], $18::bigint[]) \
                AS l(item_id, item_name, item_kind, fund_code, equipment_id, \
                     list_price_cents, unit_price_cents, quantity, line_total_cents) \
         RETURNING order_id",
        orders = c.db.table("orders"),
        lines = c.db.table("order_lines"),
        unbooked = DRAW_UNBOOKED,
        none = DRAW_NONE,
    )
}

/// One order, by id.
fn sql_order_by_id(c: &PluginContext) -> String {
    format!(
        "SELECT {ORDER_FIELDS} FROM {orders} o WHERE o.id = $1",
        orders = c.db.table("orders")
    )
}

/// An order's lines, in the order they were added.
fn sql_lines_by_order(c: &PluginContext) -> String {
    format!(
        "SELECT {LINE_FIELDS} FROM {lines} l WHERE l.order_id = $1 ORDER BY l.id",
        lines = c.db.table("order_lines")
    )
}

/// The catalogue entries a placement names, by id. `= ANY` on `int8[]` keeps it
/// one statement however many lines the order has.
fn sql_items_by_ids(c: &PluginContext) -> String {
    format!(
        "SELECT {ITEM_FIELDS} FROM {items} i WHERE i.id = ANY($1) ORDER BY i.id",
        items = c.db.table("catalogue_items")
    )
}

/// Every completion writes the same guard: the order is still completable, and the
/// row that comes back is the row the caller will see. A second completion is a
/// `409` rather than a second write.
fn sql_complete_order(c: &PluginContext, set: &str, guard: &str) -> String {
    format!(
        "UPDATE {orders} o SET {set}, o.updated_at = now() \
         WHERE o.id = $1 AND {guard} RETURNING {ORDER_FIELDS}",
        orders = c.db.table("orders")
    )
}

// ---------------------------------------------------------------------------
// The shop's catalogue
// ---------------------------------------------------------------------------

/// `GET /api/store/health` — what the shop is configured to do, and what it is
/// not.
///
/// Reports **presence**, never a value, and states the money path in the same
/// words the module docs use, so an operator reads the same truth from the
/// running system that the code holds. No queries.
fn route_health(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let _ = ctx;
    let cfg = cfg.clone();
    RouteDefinition::get_protected(
        "/api/store/health",
        PERM_READ,
        route_handler(move |_req| {
            let cfg = cfg.clone();
            async move {
                PluginResponse::json(
                    200,
                    &json!({
                        "plugin": "store",
                        "version": env!("CARGO_PKG_VERSION"),
                        "config": {
                            "base_url": if cfg.base_url().is_some() { "configured" } else { "missing" },
                            "fund_code": cfg.fund_code(),
                            "category": cfg.category(),
                            "success_url": if cfg.success_url().is_some() { "configured" } else { "missing" },
                            "cancel_url": if cfg.cancel_url().is_some() { "configured" } else { "missing" },
                            "unsettled_after_minutes": cfg.unsettled_after_minutes(),
                            "secrets": "none — this plugin charges nothing and holds no credential",
                        },
                        "ready": cfg.base_url().is_some(),
                        "scale": {
                            "tiers": scale_table(0),
                            "capped_at_price": true,
                            "note": "the shop charges its own price and never above it: a share \
                                     above the price is charged as the price, because a payment \
                                     split across two funds is not expressible through one Stripe \
                                     payment",
                        },
                        "funds": {
                            "scholarship": FUND_SCHOLARSHIP,
                            "default": cfg.fund_code(),
                            "addressed_by_code": "an item's fund_code names the fund its proceeds \
                                                  land in; every draw leaves the scholarship fund \
                                                  and enters that one, both resolved by code \
                                                  through GET /api/finance/funds at the call",
                        },
                        "money_path": {
                            "checkout": format!("caller-forward:POST {STRIPE_CHECKOUT_PATH}"),
                            "completion": format!("caller-forward:POST {STRIPE_PAYMENT_PATH}/{{id}}/book"),
                            "subsidy": format!("caller-forward:POST {FINANCE_TRANSFER_PATH}"),
                            "member_driven": "a member's own session buys a patch: the shop calls \
                                              stripe as the caller and stripe's gate re-decides",
                            "reduction_draw": "unbooked",
                            "synchronous": false,
                            "why": "a sliding-scale reduction is applied by the shop, and a Stripe \
                                    webhook confirming a payment carries no Adjutant caller, so \
                                    there is no credential to forward for either; minting one is \
                                    what plugin-to-plugin.md §3.1 refuses",
                            "unverified": "no answer comes back on a machine-originated \
                                           confirmation, so this plugin cannot tell whether the \
                                           ledger write happened; GET /api/store/orders/unsettled \
                                           is the worklist",
                            "blocked_on": "plugin-to-plugin.md §3.2 decided a core transactional \
                                           outbox with an idempotent consumer, authorised by a \
                                           declared service principal; the outbox, the relay and \
                                           the registry are not built yet",
                            "zero_amount": "a comp is not a zero-amount entry: finance refuses one \
                                            (transactions_amount_nonzero), and it does not need \
                                            one — the subsidy is a positive transfer out of the \
                                            scholarship fund into the order's fund",
                        },
                    }),
                )
            }
        }),
    )
}

/// `GET /api/store/items?kind=&category=&include_inactive=&limit=` — the
/// catalogue, with each item's whole scale.
///
/// One query. The fund code is shown as a **code**, not an id: finance's ids are
/// finance's, and the code → id step happens at the money call (`§3.5`: reference,
/// do not replicate).
fn route_list_items(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/store/items",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let kind = match req.query_param("kind") {
                    Some(raw) => match normalize_kind(raw) {
                        Ok(kind) => Some(kind),
                        Err(error) => return PluginResponse::error(400, error),
                    },
                    None => None,
                };
                let category = match req.query_param("category") {
                    Some(raw) => match normalize_category(raw) {
                        Ok(category) => Some(category),
                        Err(error) => return PluginResponse::error(400, error),
                    },
                    None => None,
                };
                let include_inactive = req.query_bool("include_inactive");
                let limit = req.query_int("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
                let rows = c
                    .db
                    .query(
                        format!(
                            "SELECT {ITEM_FIELDS} FROM {items} i \
                             WHERE ($1::bool OR i.active) \
                               AND ($2::text IS NULL OR i.kind = $2) \
                               AND ($3::text IS NULL OR i.category = $3) \
                             ORDER BY i.category, i.name, i.id LIMIT $4",
                            items = c.db.table("catalogue_items")
                        ),
                        vec![
                            SqlValue::Bool(include_inactive),
                            kind.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            category.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            SqlValue::Int(limit),
                        ],
                    )
                    .await?;
                let items: Vec<Value> = rows
                    .iter()
                    .map(|row| {
                        let mut item = item_view(row);
                        item["scale"] = json!(scale_table(
                            row["base_price_cents"].as_i64().unwrap_or(0)
                        ));
                        item
                    })
                    .collect();
                PluginResponse::json(
                    200,
                    &json!({
                        "items": items,
                        "count": items.len(),
                        "filters": {
                            "kind": kind,
                            "category": category,
                            "include_inactive": include_inactive,
                            "limit": limit,
                        },
                        "note": "prices are the shop's; a member's charge is the tier's share, \
                                 capped at the price, and anything not charged is a draw on the \
                                 scholarship fund",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/store/item` — add one thing the shop sells.
///
/// A rental names the **equipment item id** and nothing else about it: no name,
/// no condition, no custody (`plugin-to-plugin.md` §3.5). A product may not name
/// one — the database refuses it — because there is exactly one checkout state
/// machine in this system and it is equipment's.
fn route_create_item(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/store/item",
        PERM_MANAGE,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: ItemBody = body_or_default(&req)?;
                let kind = match normalize_kind(&body.kind) {
                    Ok(kind) => kind,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let name = match bounded(&body.name, MAX_NAME, "name") {
                    Ok(name) => name,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let category = match normalize_category(body.category.as_deref().unwrap_or("")) {
                    Ok(category) => category,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let description = match optional_bounded(
                    &body.description,
                    MAX_DESCRIPTION,
                    "description",
                ) {
                    Ok(description) => description,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let price = match normalize_price(body.base_price_cents) {
                    Ok(price) => price,
                    Err(error) => return PluginResponse::error(400, error),
                };
                if kind == KIND_RENTAL && body.equipment_item_id.is_none() {
                    return PluginResponse::error(
                        400,
                        "a rental must name the equipment item it rents \
                         (equipment_item_id): custody and condition stay equipment's, and this \
                         plugin holds the id and the fee",
                    );
                }
                if kind == KIND_PRODUCT && body.equipment_item_id.is_some() {
                    return PluginResponse::error(
                        400,
                        "a product may not name an equipment item: renting is `kind: \
                         \"rental\"`, and custody stays in equipment's crate",
                    );
                }
                let fund_code = trimmed(&body.fund_code)
                    .map(|code| code.to_ascii_lowercase())
                    .unwrap_or_default();
                let currency = trimmed(&body.currency)
                    .unwrap_or_else(|| "cad".to_string())
                    .to_ascii_lowercase();
                let created = c
                    .db
                    .query_one(
                        format!(
                            "INSERT INTO {items} (kind, sku, name, category, description, \
                                 base_price_cents, currency, fund_code, equipment_item_id, \
                                 created_by) \
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {ITEM_FIELDS}",
                            items = c.db.table("catalogue_items")
                        ),
                        vec![
                            SqlValue::Text(kind.clone()),
                            trimmed(&body.sku).into(),
                            SqlValue::Text(name.clone()),
                            SqlValue::Text(category.clone()),
                            SqlValue::Text(description.clone().unwrap_or_default()),
                            SqlValue::Int(price),
                            SqlValue::Text(currency.clone()),
                            SqlValue::Text(fund_code.clone()),
                            body.equipment_item_id
                                .map(SqlValue::Int)
                                .unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(caller_of(&req).unwrap_or_default()),
                        ],
                    )
                    .await?;
                let Some(item) = created else {
                    return Err(SdkError::Internal(
                        "the catalogue insert returned no row".to_string(),
                    ));
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.item.create",
                        "store_catalogue_item",
                        &item["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "kind": kind,
                            "name": name,
                            "category": category,
                            "base_price_cents": price,
                            "fund_code": fund_code,
                            "equipment_item_id": body.equipment_item_id,
                        }),
                    )
                    .await?;
                let mut view = item_view(&item);
                view["scale"] = json!(scale_table(price));
                PluginResponse::created(
                    &format!(
                        "/api/store/item/{}",
                        item["id"].as_i64().unwrap_or_default()
                    ),
                    &json!({ "item": view }),
                )
            }
        }),
    )
}

/// `GET /api/store/item/{id}` — one item, its whole scale, and — for a rental —
/// where the item actually lives.
///
/// One query. The rental's `custody` block names equipment's own routes and
/// carries the item id; the name and condition are **not** copied here, because
/// the condition is exactly what changes (`§3.5`).
fn route_get_item(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/store/item/{id}",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let Some(item) = c
                    .db
                    .query_one(
                        format!(
                            "SELECT {ITEM_FIELDS} FROM {items} i WHERE i.id = $1",
                            items = c.db.table("catalogue_items")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?
                else {
                    return PluginResponse::error(404, "no such catalogue item");
                };
                let mut view = item_view(&item);
                view["scale"] = json!(scale_table(
                    item["base_price_cents"].as_i64().unwrap_or(0)
                ));
                PluginResponse::json(
                    200,
                    &json!({
                        "item": view,
                        "custody": custody_block(&item),
                        "note": "what a member pays is the tier's share of this price, capped at \
                                 the price; the rest is a draw on the scholarship fund, never a \
                                 discount off the price",
                    }),
                )
            }
        }),
    )
}

/// `PATCH /api/store/item/{id}` — correct the catalogue record.
///
/// The `kind` is immutable: a rental and a product carry different invariants
/// (the database enforces them), and moving a rental's equipment id is a
/// different item, not an edit. An omitted field is unchanged; an **empty string
/// leaves a text field as it is** rather than blanking it, which is stated here
/// because it is the one surprising thing about this route.
fn route_edit_item(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::patch_protected(
        "/api/store/item/{id}",
        PERM_MANAGE,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let body: ItemPatch = body_or_default(&req)?;
                let name = match optional_bounded(&body.name, MAX_NAME, "name") {
                    Ok(name) => name,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let category = match body.category.as_deref() {
                    Some(raw) if !raw.trim().is_empty() => match normalize_category(raw) {
                        Ok(category) => category,
                        Err(error) => return PluginResponse::error(400, error),
                    },
                    _ => String::new(),
                };
                let description = match optional_bounded(
                    &body.description,
                    MAX_DESCRIPTION,
                    "description",
                ) {
                    Ok(description) => description,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let price = match body.base_price_cents {
                    Some(price) => match normalize_price(price) {
                        Ok(price) => Some(price),
                        Err(error) => return PluginResponse::error(400, error),
                    },
                    None => None,
                };
                let updated = c
                    .db
                    .query_one(
                        format!(
                            "UPDATE {items} i SET \
                                 name = COALESCE(NULLIF($2, ''), i.name), \
                                 sku = COALESCE(NULLIF($3, ''), i.sku), \
                                 category = COALESCE(NULLIF($4, ''), i.category), \
                                 description = COALESCE(NULLIF($5, ''), i.description), \
                                 base_price_cents = COALESCE($6::bigint, i.base_price_cents), \
                                 fund_code = COALESCE(NULLIF($7, ''), i.fund_code), \
                                 active = COALESCE($8::bool, i.active), \
                                 updated_at = now() \
                             WHERE i.id = $1 RETURNING {ITEM_FIELDS}",
                            items = c.db.table("catalogue_items")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(name.clone().unwrap_or_default()),
                            SqlValue::Text(trimmed(&body.sku).unwrap_or_default()),
                            SqlValue::Text(category.clone()),
                            SqlValue::Text(description.clone().unwrap_or_default()),
                            price.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(
                                trimmed(&body.fund_code)
                                    .map(|code| code.to_ascii_lowercase())
                                    .unwrap_or_default(),
                            ),
                            body.active
                                .map(SqlValue::Bool)
                                .unwrap_or(SqlValue::NullBool),
                        ],
                    )
                    .await?;
                let Some(item) = updated else {
                    return PluginResponse::error(404, "no such catalogue item");
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.item.edit",
                        "store_catalogue_item",
                        &id.to_string(),
                        json!({
                            "name": name,
                            "category": category,
                            "base_price_cents": price,
                            "active": body.active,
                        }),
                    )
                    .await?;
                let mut view = item_view(&item);
                view["scale"] = json!(scale_table(
                    item["base_price_cents"].as_i64().unwrap_or(0)
                ));
                PluginResponse::json(
                    200,
                    &json!({
                        "item": view,
                        "note": "an omitted field is unchanged, and an empty string leaves a text \
                                 field as it is; `kind` is immutable",
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Orders
// ---------------------------------------------------------------------------

/// `POST /api/store/order` — place an order (`store:buy`; anybody's order needs
/// `store:manage`).
///
/// The order is **priced here, from the catalogue**, never from the request: a
/// client sends item ids and quantities, and the shop computes the tier's share,
/// the price and the subsidy. Every line is priced against the item's own price,
/// so a member's discount can never become a smaller number on a slip in a
/// request body.
///
/// The order's three figures and its lines are written in **one statement** —
/// an order priced for goods it does not list is unrepresentable.
///
/// Queries: the items, the insert, the order, the lines. Then the audit write.
fn route_place_order(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/store/order",
        PERM_BUY,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let Some(caller) = caller_of(&req) else {
                    return PluginResponse::error(
                        401,
                        "placing an order needs a caller: the order records who placed it",
                    );
                };
                let body: OrderBody = body_or_default(&req)?;
                if body.lines.is_empty() {
                    return PluginResponse::error(400, "an order needs at least one line");
                }
                if body.lines.len() > MAX_LINES {
                    return PluginResponse::error(
                        400,
                        format!("an order may carry at most {MAX_LINES} lines"),
                    );
                }
                let tier = match normalize_tier(&body.tier) {
                    Ok(tier) => tier,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let member_id = match trimmed(&body.member_id) {
                    Some(member) => member,
                    None => caller.clone(),
                };
                // Ordering for somebody else is the shopkeeper's act, and it is
                // checked here because the *object* is the member.
                if member_id != caller {
                    if let Err(e) = c
                        .permissions
                        .reach(req.identity.as_ref(), PERM_MANAGE, &Scope::troop())
                        .await
                    {
                        return PluginResponse::error(e.status(), e.to_string());
                    }
                }

                let mut wanted: BTreeMap<i64, i64> = BTreeMap::new();
                for line in &body.lines {
                    if line.quantity < 1 || line.quantity > MAX_QUANTITY {
                        return PluginResponse::error(
                            400,
                            format!("quantity must be between 1 and {MAX_QUANTITY}"),
                        );
                    }
                    let entry = wanted.entry(line.item_id).or_insert(0);
                    *entry += line.quantity;
                    if *entry > MAX_QUANTITY {
                        return PluginResponse::error(
                            400,
                            format!("at most {MAX_QUANTITY} of one item may be ordered at once"),
                        );
                    }
                }
                let ids: Vec<i64> = wanted.keys().copied().collect();
                let rows = c
                    .db
                    .query(sql_items_by_ids(&c), vec![ids.clone().into()])
                    .await?;
                if rows.len() != ids.len() {
                    let found: Vec<i64> = rows
                        .iter()
                        .filter_map(|row| row["id"].as_i64())
                        .collect();
                    let missing: Vec<i64> = ids
                        .iter()
                        .copied()
                        .filter(|id| !found.contains(id))
                        .collect();
                    return PluginResponse::error(
                        404,
                        format!(
                            "no such catalogue item(s): {}",
                            missing
                                .iter()
                                .map(i64::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    );
                }
                let inactive: Vec<i64> = rows
                    .iter()
                    .filter(|row| row["active"].as_bool() != Some(true))
                    .filter_map(|row| row["id"].as_i64())
                    .collect();
                if !inactive.is_empty() {
                    return PluginResponse::error(
                        409,
                        format!(
                            "catalogue item(s) {} are not for sale (inactive)",
                            inactive
                                .iter()
                                .map(i64::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    );
                }

                let currency = trimmed(&body.currency)
                    .unwrap_or_else(|| {
                        rows.first()
                            .and_then(|row| row["currency"].as_str())
                            .unwrap_or("cad")
                            .to_string()
                    })
                    .to_ascii_lowercase();
                let default_fund = cfg.fund_code();

                // Price every line from the catalogue, and settle the order's
                // single fund: one payment lands in one fund.
                let mut price_cents: i64 = 0;
                let mut charged_cents: i64 = 0;
                let mut funds: Vec<String> = Vec::new();
                let mut item_ids: Vec<i64> = Vec::new();
                let mut names: Vec<String> = Vec::new();
                let mut kinds: Vec<String> = Vec::new();
                let mut line_funds: Vec<String> = Vec::new();
                let mut equipment_ids: Vec<String> = Vec::new();
                let mut list_prices: Vec<i64> = Vec::new();
                let mut unit_prices: Vec<i64> = Vec::new();
                let mut quantities: Vec<i64> = Vec::new();
                let mut totals: Vec<i64> = Vec::new();
                for row in &rows {
                    let Some(id) = row["id"].as_i64() else { continue };
                    let quantity = *wanted.get(&id).unwrap_or(&0);
                    if quantity <= 0 {
                        continue;
                    }
                    let list_price = row["base_price_cents"].as_i64().unwrap_or(0);
                    let Some(line) = pricing(list_price, quantity, &tier) else {
                        return PluginResponse::error(
                            409,
                            "the order's arithmetic does not fit in cents; check the price",
                        );
                    };
                    let fund_code = row["fund_code"]
                        .as_str()
                        .map(str::trim)
                        .filter(|code| !code.is_empty())
                        .unwrap_or(&default_fund)
                        .to_ascii_lowercase();
                    if !funds.contains(&fund_code) {
                        funds.push(fund_code.clone());
                    }
                    let kind = row["kind"].as_str().unwrap_or(KIND_PRODUCT).to_string();
                    price_cents = match price_cents.checked_add(line.price_cents) {
                        Some(total) => total,
                        None => {
                            return PluginResponse::error(409, "the order's price does not fit in cents")
                        }
                    };
                    charged_cents = match charged_cents.checked_add(line.charged_cents) {
                        Some(total) => total,
                        None => {
                            return PluginResponse::error(409, "the order's charge does not fit in cents")
                        }
                    };
                    item_ids.push(id);
                    names.push(
                        row["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                    kinds.push(kind.clone());
                    line_funds.push(fund_code);
                    equipment_ids.push(
                        row["equipment_item_id"]
                            .as_i64()
                            .map(|item| item.to_string())
                            .unwrap_or_default(),
                    );
                    list_prices.push(list_price);
                    unit_prices.push(line.unit_cents);
                    quantities.push(quantity);
                    totals.push(line.charged_cents);
                }
                if funds.len() != 1 {
                    return PluginResponse::error(
                        400,
                        format!(
                            "an order's proceeds land in one fund, and these lines name {}. \
                             Place separate orders, or set the items' fund_code to one fund.",
                            funds.join(", ")
                        ),
                    );
                }
                let fund_code = funds.first().cloned().unwrap_or(default_fund);
                let funded_cents = price_cents - charged_cents;
                let note = match optional_bounded(&body.note, MAX_NOTE, "note") {
                    Ok(note) => note,
                    Err(error) => return PluginResponse::error(400, error),
                };

                let inserted = c
                    .db
                    .query(
                        sql_insert_order(&c),
                        vec![
                            SqlValue::Text(member_id.clone()),
                            SqlValue::Text(caller.clone()),
                            SqlValue::Text(currency.clone()),
                            SqlValue::Text(tier.clone()),
                            SqlValue::Int(price_cents),
                            SqlValue::Int(charged_cents),
                            SqlValue::Int(funded_cents),
                            SqlValue::Text(fund_code.clone()),
                            SqlValue::Text(note.clone().unwrap_or_default()),
                            item_ids.into(),
                            SqlValue::TextArray(names),
                            SqlValue::TextArray(kinds),
                            SqlValue::TextArray(line_funds),
                            SqlValue::TextArray(equipment_ids),
                            SqlValue::IntArray(list_prices),
                            SqlValue::IntArray(unit_prices),
                            SqlValue::IntArray(quantities),
                            SqlValue::IntArray(totals),
                        ],
                    )
                    .await?;
                let Some(order_id) = inserted
                    .first()
                    .and_then(|row| row["order_id"].as_i64())
                else {
                    return Err(SdkError::Internal(
                        "the order insert returned no line".to_string(),
                    ));
                };
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(order_id)])
                    .await?
                else {
                    return Err(SdkError::Internal(
                        "the order was written but cannot be read back".to_string(),
                    ));
                };
                let lines = c
                    .db
                    .query(sql_lines_by_order(&c), vec![SqlValue::Int(order_id)])
                    .await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.order.place",
                        "store_order",
                        &order_id.to_string(),
                        json!({
                            "member_id": member_id,
                            "lines": lines.len(),
                            "price_cents": price_cents,
                            "charged_cents": charged_cents,
                            "funded_cents": funded_cents,
                            "price_tier": tier,
                            "fund_code": fund_code,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_ORDER_PLACED,
                        json!({
                            "order_id": order_id,
                            "member_id": member_id,
                            "price_cents": price_cents,
                            "charged_cents": charged_cents,
                            "funded_cents": funded_cents,
                            "price_tier": tier,
                            "fund_code": fund_code,
                            "lines": lines.len(),
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/store/order/{order_id}"),
                    &json!({
                        "order": order,
                        "lines": lines,
                        "draw": draw_block(&order),
                        "next": if charged_cents > 0 {
                            format!("open a Checkout session with POST /api/store/order/{order_id}/checkout")
                        } else {
                            format!(
                                "nothing is charged at this tier: complete the order with POST \
                                 /api/store/order/{order_id}/comp, which records the reason and \
                                 draws the whole price from the scholarship fund"
                            )
                        },
                        "note": "the shop priced this from its catalogue: what the member pays is \
                                 the tier's share of the shop's price, and the rest is a draw on \
                                 the scholarship fund — never a discount off the price",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/store/orders` — the orders, narrowed to the caller unless they hold
/// `store:read_all`.
///
/// One query, plus one for the caller's `store:read_all` when they hold no
/// obvious claim. A member sees their own orders and nobody else's; the two
/// answers are `403`-shaped the same way as their absence, so the list cannot be
/// used to count somebody else's purchases.
fn route_list_orders(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/store/orders",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req);
                let oversees = c
                    .permissions
                    .has_in_scope(req.identity.as_ref(), PERM_READ_ALL, &Scope::troop())
                    .await;
                let status = match req.query_param("status") {
                    Some(raw) => match normalize_order_status(raw) {
                        Ok(status) => Some(status),
                        Err(error) => return PluginResponse::error(400, error),
                    },
                    None => None,
                };
                let member = match req.query_param("member_id") {
                    Some(raw) => match trimmed(&Some(raw.to_string())) {
                        Some(member) => member,
                        None => return PluginResponse::error(400, "member_id must not be blank"),
                    },
                    None => String::new(),
                };
                if !oversees && !member.is_empty() && Some(member.as_str()) != caller.as_deref() {
                    return PluginResponse::error(403, "no such orders");
                }
                let subject = match (oversees, caller) {
                    (true, _) => member.clone(),
                    (false, Some(caller)) => caller,
                    (false, None) => {
                        return PluginResponse::error(403, "no such orders");
                    }
                };
                let limit = req.query_int("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
                let before_id = req.query_int("before_id");
                let rows = c
                    .db
                    .query(
                        format!(
                            "SELECT {ORDER_FIELDS} FROM {orders} o \
                             WHERE (($1::bool AND ($2::text = '' OR o.member_id = $2)) \
                                    OR (NOT $1::bool AND o.member_id = $2)) \
                               AND ($3::text IS NULL OR o.status = $3) \
                               AND ($4::bigint IS NULL OR o.id < $4) \
                             ORDER BY o.id DESC LIMIT $5",
                            orders = c.db.table("orders")
                        ),
                        vec![
                            SqlValue::Bool(oversees),
                            SqlValue::Text(subject.clone()),
                            status.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            before_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Int(limit + 1),
                        ],
                    )
                    .await?;
                let has_more = rows.len() as i64 > limit;
                let page: Vec<Value> = rows.into_iter().take(limit as usize).collect();
                let next_before_id = if has_more {
                    page.last().and_then(|row| row["id"].as_i64())
                } else {
                    None
                };
                PluginResponse::json(
                    200,
                    &json!({
                        "orders": page,
                        "count": page.len(),
                        "has_more": has_more,
                        "next_before_id": next_before_id,
                        "narrowed_to_caller": !oversees,
                        "filters": {
                            "status": status,
                            "member_id": if oversees { member } else { subject },
                            "before_id": before_id,
                            "limit": limit,
                        },
                        "note": if oversees {
                            "the troop's orders: what was sold, to whom, and what it was charged"
                        } else {
                            "your own orders; anybody else's needs store:read_all"
                        },
                    }),
                )
            }
        }),
    )
}

/// `GET /api/store/orders/unsettled?older_than_minutes=&limit=` — the worklist.
///
/// Two shapes of "not settled" appear together, because from here they cannot be
/// told apart and pretending otherwise is how a shop starts losing money quietly:
///
/// * an order **awaiting payment** older than the threshold: the member may not
///   have paid, or may have paid and this plugin cannot see it — a Stripe webhook
///   confirms the payment to *stripe*, and there is no caller for it to reach this
///   order;
/// * an order **completed** whose draw is not `booked`, or whose ledger entry
///   stripe did not confirm: money moved, the ledger has no record of it yet.
///
/// One query.
fn route_unsettled(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::get_protected(
        "/api/store/orders/unsettled",
        PERM_READ_ALL,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let older_than = req
                    .query_int("older_than_minutes")
                    .filter(|minutes| *minutes >= 0)
                    .unwrap_or_else(|| cfg.unsettled_after_minutes());
                let limit = req.query_int("limit").unwrap_or(SWEEP_LIMIT).clamp(1, MAX_LIMIT);
                let rows = c
                    .db
                    .query(
                        sql_unsettled(&c),
                        vec![
                            SqlValue::Text(older_than.to_string()),
                            SqlValue::Int(limit),
                        ],
                    )
                    .await?;
                let total = rows
                    .first()
                    .and_then(|row| row["total_unsettled"].as_i64())
                    .unwrap_or(0);
                let mut by_reason = serde_json::Map::new();
                for reason in UNSETTLED_REASONS {
                    let count = rows
                        .iter()
                        .filter(|row| row["unsettled_reason"].as_str() == Some(reason))
                        .count();
                    if count > 0 {
                        by_reason.insert(reason.to_string(), json!(count));
                    }
                }
                PluginResponse::json(
                    200,
                    &json!({
                        "orders": rows,
                        "count": rows.len(),
                        "total_unsettled": total,
                        "older_than_minutes": older_than,
                        "by_reason": Value::Object(by_reason),
                        "note": "an order awaiting payment cannot be told from one this plugin \
                                 cannot see paid, because a Stripe webhook confirms the payment to \
                                 stripe and carries no Adjutant caller (plugin-to-plugin.md §3.2, \
                                 whose outbox is not built). A shopkeeper confirms one with POST \
                                 /api/store/order/{id}/complete, verified against stripe's own \
                                 record; a treasurer books an unbooked draw with POST \
                                 /api/store/order/{id}/draw",
                    }),
                )
            }
        }),
    )
}

/// The reasons an order appears on the worklist, in the order it is reported.
pub const UNSETTLED_REASONS: [&str; 3] = [
    "awaiting_payment",
    "draw_unbooked",
    "ledger_unconfirmed",
];

/// The worklist statement. One row per unsettled order, with the count of them
/// all, so one statement answers both "which" and "how many".
fn sql_unsettled(c: &PluginContext) -> String {
    format!(
        "SELECT {ORDER_FIELDS}, \
                CASE WHEN o.status = '{awaiting}' THEN 'awaiting_payment' \
                     WHEN o.funded_cents > 0 AND o.draw_status <> '{booked}' THEN 'draw_unbooked' \
                     ELSE 'ledger_unconfirmed' END AS unsettled_reason, \
                COUNT(*) OVER ()::bigint AS total_unsettled \
         FROM {orders} o \
         WHERE (o.status = '{awaiting}' \
                AND o.created_at < now() - ($1 || ' minutes')::interval) \
            OR (o.status = '{paid}' AND o.ledger_status <> '{ledger_booked}') \
            OR (o.status IN ('{paid}', '{comped}') AND o.funded_cents > 0 \
                AND o.draw_status <> '{booked}') \
         ORDER BY o.id DESC LIMIT $2",
        orders = c.db.table("orders"),
        awaiting = STATUS_AWAITING_PAYMENT,
        paid = STATUS_PAID,
        comped = STATUS_COMPED,
        booked = DRAW_BOOKED,
        ledger_booked = LEDGER_BOOKED,
    )
}

/// `GET /api/store/order/{id}` — one order with its lines.
///
/// Your own order, or anybody's with `store:read_all`. Somebody else's order is a
/// `403` that reads `"no such order"`: a member's purchases are their own record
/// (the same rule finance applies to a member's dues).
///
/// Queries: the order, then — when it is not the caller's — the permission check,
/// then the lines.
fn route_get_order(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/store/order/{id}",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such order");
                };
                let caller = caller_of(&req);
                if !order_is_theirs(&order, caller.as_deref())
                    && c
                        .permissions
                        .reach(req.identity.as_ref(), PERM_READ_ALL, &Scope::troop())
                        .await
                        .is_err()
                {
                    // Same answer for absent and forbidden: the route cannot be
                    // used to probe which order ids exist.
                    return PluginResponse::error(403, "no such order");
                }
                let lines = c
                    .db
                    .query(sql_lines_by_order(&c), vec![SqlValue::Int(id)])
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "order": order,
                        "lines": lines,
                        "draw": draw_block(&order),
                        "ledger": ledger_block(&order),
                        "custody": custody_lines(&lines),
                    }),
                )
            }
        }),
    )
}

/// `POST /api/store/order/{id}/checkout` — the member-driven path: buy it as
/// yourself.
///
/// This is a real `§2(b)` call. The caller's own `authorization`/`cookie` are
/// forwarded to stripe's `POST /api/stripe/checkout`, whose gate re-decides
/// `stripe:checkout`; a caller without it is refused by stripe, in stripe's words,
/// and a request that carries no credential at all is refused **here, before
/// anything is called**, because there is nothing to forward and this plugin
/// holds no credential of its own.
///
/// What this plugin sends is what it must: the order's `charged_cents` (never the
/// price — the difference is the scholarship's business, not the card's), the
/// member, the order's `fund_code`, and `category: store`. It never sends a card
/// detail, because it never has one.
///
/// Queries: the order, the settle. Then the audit write.
fn route_checkout(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/store/order/{id}/checkout",
        PERM_BUY,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let Some(base) = cfg.base_url() else {
                    return PluginResponse::error(
                        503,
                        "no base_url is configured for the store plugin, so stripe's checkout \
                         cannot be reached; set it in the store plugin's config and reload",
                    );
                };
                let headers = match require_forwardable(&req, "stripe's checkout") {
                    Ok(headers) => headers,
                    Err(e) => return PluginResponse::error(e.status(), e.to_string()),
                };
                let id = req.int_param("id")?;
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such order");
                };
                let caller = caller_of(&req);
                if !order_is_theirs(&order, caller.as_deref()) {
                    if let Err(e) = c
                        .permissions
                        .reach(req.identity.as_ref(), PERM_MANAGE, &Scope::troop())
                        .await
                    {
                        return PluginResponse::error(e.status(), e.to_string());
                    }
                }
                let status = order["status"].as_str().unwrap_or_default();
                if status != STATUS_OPEN {
                    return PluginResponse::error(
                        409,
                        format!("this order is {status}: only an open order opens a Checkout session"),
                    );
                }
                let charged = order["charged_cents"].as_i64().unwrap_or(0);
                if charged <= 0 {
                    return PluginResponse::error(
                        409,
                        format!(
                            "this order charges nothing at the {} tier, and a Checkout session \
                             cannot take zero (Stripe refuses a non-positive amount): complete it \
                             with POST /api/store/order/{id}/comp, which records the reason and \
                             draws the whole price from the scholarship fund",
                            order["price_tier"].as_str().unwrap_or(DEFAULT_TIER)
                        ),
                    );
                }
                let body: CheckoutOverride = body_or_default(&req)?;
                let member_id = order["member_id"].as_str().unwrap_or_default().to_string();
                let description = match &body.description {
                    Some(text) if !text.trim().is_empty() => format!(
                        "Store order {id} — {}",
                        text.trim()
                    ),
                    _ => format!(
                        "Store order {id} — {}",
                        order["price_tier"].as_str().unwrap_or(DEFAULT_TIER)
                    ),
                };
                let mut checkout = json!({
                    "purpose": PURPOSE_FOR_A_STORE_SALE,
                    "amount_cents": charged,
                    "currency": order["currency"].as_str().unwrap_or("cad"),
                    "member_id": member_id,
                    "fund_code": order["fund_code"].as_str().unwrap_or_default(),
                    "category": cfg.category(),
                    "description": description,
                });
                if let Some(url) = trimmed(&body.success_url).or_else(|| cfg.success_url()) {
                    checkout["success_url"] = json!(url);
                }
                if let Some(url) = trimmed(&body.cancel_url).or_else(|| cfg.cancel_url()) {
                    checkout["cancel_url"] = json!(url);
                }
                let outcome = call_as_caller(
                    &c,
                    "POST",
                    format!("{base}{STRIPE_CHECKOUT_PATH}"),
                    headers,
                    Some(checkout),
                    "stripe's checkout",
                )
                .await;
                if !outcome.ok {
                    let reason = outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "stripe refused the session".to_string());
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "store.order.checkout.failed",
                            "store_order",
                            &id.to_string(),
                            json!({
                                "mechanism": MECHANISM_STRIPE_CHECKOUT,
                                "http_status": outcome.http_status,
                                "reason": reason,
                                "charged_cents": charged,
                            }),
                        )
                        .await?;
                    return PluginResponse::error(
                        outcome.answer_status,
                        format!(
                            "{reason}. The order is unchanged and nothing was charged; it can be \
                             retried."
                        ),
                    );
                }
                let session = &outcome.body["session"];
                let session_id = session["stripe_session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let checkout_url = outcome.body["checkout_url"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| session["checkout_url"].as_str().map(str::to_string))
                    .unwrap_or_default();
                let session_row = session["id"].as_i64();
                if session_id.is_empty() || checkout_url.is_empty() {
                    return PluginResponse::error(
                        502,
                        "stripe answered without a session id and url, so the order cannot be \
                         tied to a payment; nothing was charged that this plugin can see",
                    );
                }
                let Some(updated) = c
                    .db
                    .query_one(
                        sql_complete_order(
                            &c,
                            &format!(
                                "status = '{STATUS_AWAITING_PAYMENT}', stripe_session_row = $2, \
                                 stripe_session_id = $3, checkout_url = $4"
                            ),
                            &format!("o.status = '{STATUS_OPEN}'"),
                        ),
                        vec![
                            SqlValue::Int(id),
                            session_row.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(session_id.clone()),
                            SqlValue::Text(checkout_url.clone()),
                        ],
                    )
                    .await?
                else {
                    return PluginResponse::error(
                        409,
                        "this order was completed while the Checkout session was being opened; \
                         the session Stripe opened is not attached to it",
                    );
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.order.checkout",
                        "store_order",
                        &id.to_string(),
                        json!({
                            "mechanism": MECHANISM_STRIPE_CHECKOUT,
                            "http_status": outcome.http_status,
                            "charged_cents": charged,
                            "session_row": session_row,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_ORDER_AWAITING_PAYMENT,
                        json!({
                            "order_id": id,
                            "member_id": member_id,
                            "charged_cents": charged,
                            "funded_cents": updated["funded_cents"],
                            "status": STATUS_AWAITING_PAYMENT,
                            "next": format!(
                                "the payment reaches stripe, not this order: a shopkeeper marks it \
                                 paid with POST /api/store/order/{id}/complete once stripe's own \
                                 record confirms it"
                            ),
                        }),
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "order": updated,
                        "checkout_url": checkout_url,
                        "stripe": {
                            "http_status": outcome.http_status,
                            "session_row": session_row,
                            "note": "the call carried your own credential; stripe's gate decided, \
                                     and this plugin holds no credential of its own",
                        },
                        "draw": draw_block(&updated),
                        "note": "awaiting payment: Stripe confirms the payment to stripe, and this \
                                 plugin cannot see that webhook (plugin-to-plugin.md §3.2). The \
                                 order appears on GET /api/store/orders/unsettled until a \
                                 shopkeeper completes it.",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/store/order/{id}/complete` — complete a **paid** order.
///
/// The shopkeeper's act, and a `§2(b)` chain with an answer at every step:
///
/// 1. `GET /api/stripe/payment/{id}` **as the caller** — stripe says whether the
///    payment exists and what it is. This plugin verifies it against the order:
///    the amount must be what the order charged, the session must be the session
///    this order opened, and the member must be the member. A payment that does
///    not match is a `409` and nothing is written.
/// 2. `POST /api/stripe/payment/{id}/book` **as the caller** — asked so the
///    ledger write happens in this same flow, with an answer (§3.2), rather than
///    being left to an event nobody can see. Finance's gate re-decides
///    `finance:write`, and its refusal is passed through, not flattened.
/// 3. The order is marked `paid` with stripe's payment id (opaque) and finance's
///    own answer, and the *draw* is left for a treasurer if the tier funded any
///    of it — that money is a separate transfer with its own authority.
///
/// A completed order is never completed twice: the update is guarded on the
/// status, so a second call is a `409` rather than a second record.
///
/// Queries: the order, then the guarded update. Then the audit write.
fn route_complete(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected(
        "/api/store/order/{id}/complete",
        PERM_MANAGE,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let Some(base) = cfg.base_url() else {
                    return PluginResponse::error(
                        503,
                        "no base_url is configured for the store plugin, so stripe's record \
                         cannot be read; set it in the store plugin's config and reload",
                    );
                };
                let headers = match require_forwardable(&req, "stripe's payment record") {
                    Ok(headers) => headers,
                    Err(e) => return PluginResponse::error(e.status(), e.to_string()),
                };
                let id = req.int_param("id")?;
                let body: CompleteBody = body_or_default(&req)?;
                let Some(payment_row) = body.stripe_payment_id.filter(|row| *row > 0) else {
                    return PluginResponse::error(
                        400,
                        "stripe_payment_id is required: a paid order is completed against \
                         stripe's own record of the payment (its row id, from GET \
                         /api/stripe/payments)",
                    );
                };
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such order");
                };
                let status = order["status"].as_str().unwrap_or_default().to_string();
                if !COMPLETABLE_STATUSES.contains(&status.as_str()) {
                    return PluginResponse::error(
                        409,
                        format!("this order is already {status}"),
                    );
                }
                let charged = order["charged_cents"].as_i64().unwrap_or(0);
                if charged <= 0 {
                    return PluginResponse::error(
                        409,
                        "this order charges nothing: it is completed at no charge with POST \
                         /api/store/order/{id}/comp, which records the reason and the authority",
                    );
                }
                // 1. What does stripe say about this payment?
                let verification = call_as_caller(
                    &c,
                    "GET",
                    format!("{base}{STRIPE_PAYMENT_PATH}/{payment_row}"),
                    headers.clone(),
                    None,
                    "reading stripe's payment",
                )
                .await;
                if !verification.ok {
                    let reason = verification
                        .error
                        .clone()
                        .unwrap_or_else(|| "stripe refused the read".to_string());
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "store.order.complete.failed",
                            "store_order",
                            &id.to_string(),
                            json!({
                                "mechanism": MECHANISM_STRIPE_PAYMENT,
                                "http_status": verification.http_status,
                                "reason": reason,
                            }),
                        )
                        .await?;
                    return PluginResponse::error(
                        verification.answer_status,
                        format!("{reason}. The order is unchanged."),
                    );
                }
                let payment = verification.body["payment"].clone();
                if let Err(reason) = payment_matches(&order, &payment) {
                    return PluginResponse::error(409, reason);
                }
                // 2. Ask stripe to hand it to finance, in this same flow, as the
                //    caller — so the ledger write has an answer we can record.
                let booking = call_as_caller(
                    &c,
                    "POST",
                    format!("{base}{STRIPE_PAYMENT_PATH}/{payment_row}/book"),
                    headers,
                    None,
                    "stripe's booking",
                )
                .await;
                let ledger_status = if booking.ok {
                    booking.body["ledger"]["status"]
                        .as_str()
                        .map(str::trim)
                        .filter(|status| !status.is_empty())
                        .unwrap_or(LEDGER_BOOKED)
                        .to_string()
                } else {
                    // No body to read: say what the call itself was.
                    outcome_status(&booking).to_string()
                };
                let ledger_transaction_id = booking
                    .body
                    .get("finance")
                    .and_then(|finance| finance.get("transaction_id"))
                    .and_then(Value::as_i64);
                let ledger_error = booking.error.clone();
                // 3. The order is what the payment says it is.
                let payment_ref = payment["payment_id"].as_str().unwrap_or_default().to_string();
                let Some(updated) = c
                    .db
                    .query_one(
                        sql_complete_order(
                            &c,
                            &format!(
                                "status = '{STATUS_PAID}', payment_ref = $2, \
                                 ledger_status = $3, ledger_transaction_id = $4, \
                                 ledger_error = $5, completed_by = $6, completed_at = now()"
                            ),
                            &format!(
                                "o.status IN ('{open}', '{awaiting}') AND o.charged_cents > 0",
                                open = STATUS_OPEN,
                                awaiting = STATUS_AWAITING_PAYMENT
                            ),
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(payment_ref.clone()),
                            SqlValue::Text(ledger_status.clone()),
                            ledger_transaction_id
                                .map(SqlValue::Int)
                                .unwrap_or(SqlValue::NullInt),
                            ledger_error.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            SqlValue::Text(caller_of(&req).unwrap_or_default()),
                        ],
                    )
                    .await?
                else {
                    return PluginResponse::error(409, "this order is already completed");
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.order.complete",
                        "store_order",
                        &id.to_string(),
                        json!({
                            "mechanism": [MECHANISM_STRIPE_PAYMENT, MECHANISM_STRIPE_BOOK],
                            "charged_cents": charged,
                            "funded_cents": updated["funded_cents"],
                            "ledger_status": ledger_status,
                            "http_status": booking.http_status,
                            "transaction_id": ledger_transaction_id,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_ORDER_PAID,
                        json!({
                            "order_id": id,
                            "member_id": updated["member_id"],
                            "price_cents": updated["price_cents"],
                            "charged_cents": charged,
                            "funded_cents": updated["funded_cents"],
                            "fund_code": updated["fund_code"],
                            "ledger_status": ledger_status,
                            "draw_status": updated["draw_status"],
                        }),
                    )
                    .await?;
                let payload = json!({
                    "order": updated,
                    "stripe": {
                        "http_status": verification.http_status,
                        "payment_row": payment_row,
                        "session_id": payment["session_id"],
                        "note": "read as you: stripe's gate decided, and this plugin holds no \
                                 credential of its own",
                    },
                    "finance": {
                        "http_status": booking.http_status,
                        "transaction_id": ledger_transaction_id,
                        "error": booking.error,
                    },
                    "ledger": ledger_block(&updated),
                    "draw": draw_block(&updated),
                });
                if booking.ok {
                    PluginResponse::json(200, &payload)
                } else {
                    // The payment is real and the order is paid; the ledger entry
                    // is not confirmed. Say so with the target's own status rather
                    // than a 500, and leave the order on the worklist.
                    PluginResponse::json(booking.answer_status, &payload)
                }
            }
        }),
    )
}

/// `POST /api/store/order/{id}/comp` — complete an order at no charge.
///
/// **A comp is an authority, not a price.** It needs `store:comp`, it needs a
/// reason (mandatory, bounded, recorded on the order and in the audit log), and
/// it records who exercised it and when. It is not a price of zero: the order
/// still carries the shop's price, and the money it forgives is a **draw on the
/// scholarship fund**, booked in this same call by asking finance to transfer the
/// whole price out of `scholarship` into the fund the order's proceeds land in —
/// as the caller, so finance's gate re-decides `finance:write`.
///
/// **The comp lands even when the draw does not.** Telling a shopkeeper that a
/// comp failed because a ledger write did would be a lie about the goods: the
/// comp is a human act that already happened. So the comp is recorded, and the
/// draw's outcome is recorded truthfully beside it (`booked`, or `refused`/
/// `failed` with finance's own words), and finance's status is passed through.
/// A comp whose draw did not land sits on `GET /api/store/orders/unsettled` until
/// a treasurer books it with `/draw` — visible, never hidden.
///
/// Queries: the order, the attempt write, the outcome write. Then the audit write.
fn route_comp(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected(
        "/api/store/order/{id}/comp",
        PERM_COMP,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let headers = match require_forwardable(&req, "finance's ledger") {
                    Ok(headers) => headers,
                    Err(e) => return PluginResponse::error(e.status(), e.to_string()),
                };
                let id = req.int_param("id")?;
                let body: CompBody = body_or_default(&req)?;
                let reason = match bounded(&body.reason, MAX_REASON, "reason") {
                    Ok(reason) => reason,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such order");
                };
                let status = order["status"].as_str().unwrap_or_default().to_string();
                if !COMPLETABLE_STATUSES.contains(&status.as_str()) {
                    return PluginResponse::error(409, format!("this order is already {status}"));
                }
                let price = order["price_cents"].as_i64().unwrap_or(0);
                let fund_code = order["fund_code"].as_str().unwrap_or_default().to_string();
                if fund_code == FUND_SCHOLARSHIP {
                    return PluginResponse::error(
                        409,
                        format!(
                            "this order's proceeds already land in {FUND_SCHOLARSHIP}, so there is \
                             nothing to draw from it: finance refuses a transfer between a fund \
                             and itself"
                        ),
                    );
                }
                // Whether this comp has a subsidy at all. An item whose price is
                // zero funds nothing: the comp is still the authority that hands
                // it over, and there is no draw to book — which
                // `store_orders_draw_matches_funding` would refuse to see
                // pretended otherwise.
                let has_draw = price > 0;
                // The comp is recorded first, at its own figures: the whole price
                // is funded and the member is charged nothing. If the draw cannot
                // land, this is the state the worklist must be able to see.
                let attempt_set = format!(
                    "status = '{STATUS_COMPED}', charged_cents = 0, \
                     funded_cents = o.price_cents, comp_reason = $2, comp_by = $3, \
                     comp_at = now(), completed_by = $3, completed_at = now(), {draw}",
                    draw = if has_draw {
                        format!(
                            "draw_status = '{DRAW_ATTEMPTING}', draw_by = $3, \
                             draw_attempted_at = now()"
                        )
                    } else {
                        format!("draw_status = '{DRAW_NONE}'")
                    },
                );
                let Some(comped) = c
                    .db
                    .query_one(
                        sql_complete_order(
                            &c,
                            &attempt_set,
                            &format!(
                                "o.status IN ('{open}', '{awaiting}')",
                                open = STATUS_OPEN,
                                awaiting = STATUS_AWAITING_PAYMENT
                            ),
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(reason.clone()),
                            SqlValue::Text(caller_of(&req).unwrap_or_default()),
                        ],
                    )
                    .await?
                else {
                    return PluginResponse::error(409, "this order is already completed");
                };
                let outcome = if has_draw {
                    book_draw(
                        &c,
                        &cfg,
                        &headers,
                        &comped,
                        &format!("Store order {id} comp — {reason}"),
                        body.allow_overdraft.unwrap_or(false),
                    )
                    .await
                } else {
                    // Nothing to book, and nothing was called: a free item's comp
                    // has no subsidy, so it produces no transfer.
                    CallOutcome {
                        ok: true,
                        http_status: None,
                        body: Value::Null,
                        error: None,
                        answer_status: 200,
                    }
                };
                let reported = if has_draw { outcome_status(&outcome) } else { DRAW_NONE };
                let draw_ref = if outcome.ok {
                    outcome.body["transfer_group"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                } else {
                    String::new()
                };
                let settled = if has_draw {
                    c.db.query_one(
                        sql_complete_order(
                            &c,
                            &format!(
                                "draw_status = $2, draw_ref = $3, draw_error = $4, \
                                 draw_booked_at = CASE WHEN $2 = '{booked}' THEN now() ELSE NULL END",
                                booked = DRAW_BOOKED,
                            ),
                            &format!("o.status = '{STATUS_COMPED}'"),
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(reported.to_string()),
                            SqlValue::Text(draw_ref.clone()),
                            outcome.error.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                        ],
                    )
                    .await?
                    .unwrap_or(comped)
                } else {
                    comped
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.order.comp",
                        "store_order",
                        &id.to_string(),
                        json!({
                            "mechanism": MECHANISM_FINANCE_TRANSFER,
                            "price_cents": price,
                            "funded_cents": settled["funded_cents"],
                            "from_fund_code": FUND_SCHOLARSHIP,
                            "to_fund_code": fund_code,
                            "draw_status": reported,
                            "http_status": outcome.http_status,
                            "reason": reason,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_ORDER_COMPED,
                        json!({
                            "order_id": id,
                            "member_id": settled["member_id"],
                            "price_cents": price,
                            "charged_cents": 0,
                            "funded_cents": settled["funded_cents"],
                            "fund_code": fund_code,
                            "draw_status": reported,
                            "comp_by": settled["comp_by"],
                        }),
                    )
                    .await?;
                if outcome.ok {
                    c.events
                        .publish(
                            EVENT_DRAW_BOOKED,
                            json!({
                                "order_id": id,
                                "funded_cents": settled["funded_cents"],
                                "from_fund_code": FUND_SCHOLARSHIP,
                                "to_fund_code": fund_code,
                                "transfer_group": draw_ref,
                            }),
                        )
                        .await?;
                }
                let payload = json!({
                    "order": settled,
                    "draw": draw_block(&settled),
                    "finance": {
                        "http_status": outcome.http_status,
                        "transfer_group": draw_ref,
                        "error": outcome.error,
                        "note": "the draw carried your own credential; finance's gate decided, and \
                                 this plugin holds no credential of its own",
                    },
                    "note": "a comp is an authority, not a price: the order keeps the shop's \
                             price, the member is charged nothing, and the whole price is drawn \
                             from the scholarship fund",
                });
                if !has_draw || outcome.ok {
                    PluginResponse::json(200, &payload)
                } else {
                    PluginResponse::json(outcome.answer_status, &payload)
                }
            }
        }),
    )
}

/// `POST /api/store/order/{id}/draw` — book an outstanding draw **as the caller**.
///
/// This is the route a sliding-scale reduction needs and cannot have on its own:
/// the reduction is applied by the shop, so no caller holds `finance:write` at
/// that moment, and a machine-originated confirmation cannot obtain a bounded
/// authorization until `plugin-to-plugin.md` §3.2's outbox exists. A treasurer
/// holding `finance:write` closes it here, with their own authority and nothing
/// added — exactly as `stripe`'s `/book` closes the same gap for a webhook.
///
/// It is guarded on the order being **completed**: a draw for a sale that has not
/// happened would move money for goods nobody has.
///
/// Queries: the order, the attempt write, the outcome write. Then the audit write.
fn route_draw(ctx: &PluginContext, cfg: &StoreConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected(
        "/api/store/order/{id}/draw",
        PERM_MANAGE,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let headers = match require_forwardable(&req, "finance's ledger") {
                    Ok(headers) => headers,
                    Err(e) => return PluginResponse::error(e.status(), e.to_string()),
                };
                let id = req.int_param("id")?;
                let body: DrawBody = body_or_default(&req)?;
                let Some(order) = c
                    .db
                    .query_one(sql_order_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such order");
                };
                let status = order["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_PAID && status != STATUS_COMPED {
                    return PluginResponse::error(
                        409,
                        format!(
                            "this order is {status}: a draw is booked for a sale that happened, so \
                             the order must be paid or comped first"
                        ),
                    );
                }
                let funded = order["funded_cents"].as_i64().unwrap_or(0);
                if funded <= 0 {
                    return PluginResponse::error(
                        409,
                        "this order funds nothing: the member paid the shop's whole price, so \
                         there is no draw on the scholarship fund",
                    );
                }
                let draw_status = order["draw_status"].as_str().unwrap_or_default();
                if draw_status == DRAW_BOOKED {
                    return PluginResponse::error(409, "this order's draw is already booked");
                }
                // The attempt is recorded before the call: an answer that never
                // comes back must still be visible, because a lost answer may be a
                // completed transfer (the at-least-once hazard §3.2's outbox
                // exists to solve, and it is not built).
                let Some(attempting) = c
                    .db
                    .query_one(
                        sql_complete_order(
                            &c,
                            &format!(
                                "draw_status = '{attempting}', draw_by = $2, \
                                 draw_attempted_at = now()",
                                attempting = DRAW_ATTEMPTING,
                            ),
                            &format!(
                                "o.id = $1 AND o.funded_cents > 0 AND o.draw_status <> '{booked}'",
                                booked = DRAW_BOOKED
                            ),
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(caller_of(&req).unwrap_or_default())],
                    )
                    .await?
                else {
                    return PluginResponse::error(409, "this order's draw is already booked");
                };
                let outcome = book_draw(
                    &c,
                    &cfg,
                    &headers,
                    &attempting,
                    &format!("Store order {id} scholarship draw"),
                    body.allow_overdraft.unwrap_or(false),
                )
                .await;
                let draw_ref = if outcome.ok {
                    outcome.body["transfer_group"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                } else {
                    String::new()
                };
                let settled = c
                    .db
                    .query_one(
                        sql_complete_order(
                            &c,
                            &format!(
                                "draw_status = $2, draw_ref = $3, draw_error = $4, \
                                 draw_booked_at = CASE WHEN $2 = '{booked}' THEN now() ELSE NULL END",
                                booked = DRAW_BOOKED,
                            ),
                            "o.id = $1",
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(outcome_status(&outcome).to_string()),
                            SqlValue::Text(draw_ref.clone()),
                            outcome.error.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
                        ],
                    )
                    .await?
                    .unwrap_or(attempting);
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "store.order.draw",
                        "store_order",
                        &id.to_string(),
                        json!({
                            "mechanism": MECHANISM_FINANCE_TRANSFER,
                            "funded_cents": settled["funded_cents"],
                            "from_fund_code": FUND_SCHOLARSHIP,
                            "to_fund_code": settled["fund_code"],
                            "draw_status": outcome_status(&outcome),
                            "http_status": outcome.http_status,
                            "transfer_group": draw_ref,
                        }),
                    )
                    .await?;
                if outcome.ok {
                    c.events
                        .publish(
                            EVENT_DRAW_BOOKED,
                            json!({
                                "order_id": id,
                                "funded_cents": settled["funded_cents"],
                                "from_fund_code": FUND_SCHOLARSHIP,
                                "to_fund_code": settled["fund_code"],
                                "transfer_group": draw_ref,
                            }),
                        )
                        .await?;
                }
                let payload = json!({
                    "order": settled,
                    "draw": draw_block(&settled),
                    "finance": {
                        "http_status": outcome.http_status,
                        "transfer_group": draw_ref,
                        "error": outcome.error,
                        "note": "the draw carried your own credential; finance's gate decided, and \
                                 this plugin holds no credential of its own",
                    },
                    "note": "the subsidy has moved from the scholarship fund into the order's \
                             fund as one balanced transfer: both legs, one statement, and the sum \
                             of every fund is unchanged",
                });
                if outcome.ok {
                    PluginResponse::json(200, &payload)
                } else {
                    PluginResponse::json(outcome.answer_status, &payload)
                }
            }
        }),
    )
}

/// `GET /api/store/comps?from=&to=&limit=` — every comp, with its reason, its
/// authority and the draw it produced.
///
/// SPEC §7.16 asks for the zero amount to be visible in the ledger and the Annual
/// Financial Report. **The ledger shows the draw, and this route shows the comp**,
/// and the difference is worth stating plainly: finance cannot hold a zero-amount
/// transaction at all (`transactions_amount_nonzero`), so the comp's *charge* of
/// zero exists here and in `core.audit_log`; what finance holds is the positive
/// transfer out of `scholarship` that the comp produced, which
/// `GET /api/finance/report/annual` reports in its funds and transfers sections.
/// This route is where a treasurer reconciles the two by hand.
///
/// One query.
fn route_comps(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/store/comps",
        PERM_READ_ALL,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let from = match req.query_param("from") {
                    Some(raw) => match parse_date(raw) {
                        Ok(date) => Some(date),
                        Err(error) => return PluginResponse::error(400, format!("from: {error}")),
                    },
                    None => None,
                };
                let to = match req.query_param("to") {
                    Some(raw) => match parse_date(raw) {
                        Ok(date) => Some(date),
                        Err(error) => return PluginResponse::error(400, format!("to: {error}")),
                    },
                    None => None,
                };
                if let (Some(from), Some(to)) = (from, to) {
                    if to < from {
                        return PluginResponse::error(400, "to must not be before from");
                    }
                }
                let limit = req.query_int("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
                let rows = c
                    .db
                    .query(
                        format!(
                            "SELECT {ORDER_FIELDS} FROM {orders} o \
                             WHERE o.status = '{comped}' \
                               AND ($1::text IS NULL OR o.comp_at >= (($1::text)::date)::timestamptz) \
                               AND ($2::text IS NULL OR o.comp_at < (((($2::text)::date) + 1))::timestamptz) \
                             ORDER BY o.comp_at DESC, o.id DESC LIMIT $3",
                            orders = c.db.table("orders"),
                            comped = STATUS_COMPED,
                        ),
                        vec![
                            from.map(|date| SqlValue::Text(date.to_string()))
                                .unwrap_or(SqlValue::Null),
                            to.map(|date| SqlValue::Text(date.to_string()))
                                .unwrap_or(SqlValue::Null),
                            SqlValue::Int(limit),
                        ],
                    )
                    .await?;
                let funded_total: i64 = rows
                    .iter()
                    .map(|row| row["funded_cents"].as_i64().unwrap_or(0))
                    .sum();
                PluginResponse::json(
                    200,
                    &json!({
                        "comps": rows,
                        "count": rows.len(),
                        "funded_total_cents": funded_total,
                        "funded_total_display": format_cents(funded_total),
                        "from": from.map(|date| date.to_string()),
                        "to": to.map(|date| date.to_string()),
                        "note": format!(
                            "each comp charged nothing and drew its whole price from the \
                             {FUND_SCHOLARSHIP} fund — a positive transfer, because finance \
                             refuses a zero-amount transaction. The comp's own zero is here and \
                             in the audit log; the ledger shows the draw."
                        ),
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// The sweep — a notification, never a mechanism
// ---------------------------------------------------------------------------

/// Every six hours, notice the orders that are not settled.
///
/// **It notifies; it does not write.** Writing here would be this plugin reaching
/// into another plugin's domain on a timer, which is the same privileged-shortcut
/// problem as the webhook, and it would need a credential nobody has. A silent
/// sweep is a healthy one.
///
/// One query.
async fn unsettled_sweep(c: &PluginContext, cfg: &StoreConfig) -> Result<(), SdkError> {
    let minutes = cfg.unsettled_after_minutes();
    let rows = c
        .db
        .query(
            sql_unsettled(c),
            vec![
                SqlValue::Text(minutes.to_string()),
                SqlValue::Int(SWEEP_LIMIT),
            ],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let total = rows
        .first()
        .and_then(|row| row["total_unsettled"].as_i64())
        .unwrap_or(rows.len() as i64);
    let mut by_reason = serde_json::Map::new();
    for reason in UNSETTLED_REASONS {
        let count = rows
            .iter()
            .filter(|row| row["unsettled_reason"].as_str() == Some(reason))
            .count();
        if count > 0 {
            by_reason.insert(reason.to_string(), json!(count));
        }
    }
    let oldest = rows
        .iter()
        .filter_map(|row| row["created_at"].as_str())
        .min()
        .unwrap_or_default()
        .to_string();
    c.events
        .publish(
            EVENT_ORDERS_UNSETTLED,
            json!({
                "unsettled": rows.len(),
                "total_unsettled": total,
                "older_than_minutes": minutes,
                "oldest_created_at": oldest,
                "by_reason": Value::Object(by_reason),
                "orders": rows,
                "next": "a shopkeeper completes a paid order with POST \
                         /api/store/order/{id}/complete; a treasurer books an unbooked draw with \
                         POST /api/store/order/{id}/draw — the notification is not the ledger \
                         write (plugin-to-plugin.md §3.2)",
            }),
        )
        .await
}

// ---------------------------------------------------------------------------
// Shapes and small pure helpers
// ---------------------------------------------------------------------------

/// A catalogue row as a client reads it: money in cents and in a display form,
/// and a rental's `equipment_item_id` plainly marked as a reference.
fn item_view(row: &Value) -> Value {
    let price = row["base_price_cents"].as_i64().unwrap_or(0);
    json!({
        "id": row["id"],
        "kind": row["kind"],
        "sku": row["sku"],
        "name": row["name"],
        "category": row["category"],
        "description": row["description"],
        "base_price_cents": price,
        "base_price_display": format_cents(price),
        "currency": row["currency"],
        "fund_code": row["fund_code"],
        "equipment_item_id": row["equipment_item_id"],
        "active": row["active"],
        "created_by": row["created_by"],
        "created_at": row["created_at"],
        "updated_at": row["updated_at"],
    })
}

/// Where a rental's item actually lives, and where its condition is stated.
///
/// The item id and nothing else: the condition is exactly what changes, and a
/// copy of it here would be a second, drifting copy of equipment's data
/// (`plugin-to-plugin.md` §3.5).
fn custody_block(item: &Value) -> Option<Value> {
    if item["kind"].as_str() != Some(KIND_RENTAL) {
        return None;
    }
    let id = item["equipment_item_id"].as_i64()?;
    Some(json!({
        "owner": "equipment",
        "equipment_item_id": id,
        "availability": format!("GET /api/equipment/item/{id}"),
        "checkout": format!("POST /api/equipment/item/{id}/checkout"),
        "condition_source": format!("GET /api/equipment/item/{id}"),
        "note": "the fee is this shop's; the item, its condition and the open-checkout state \
                 machine are equipment's. This plugin holds the id and copies neither the name \
                 nor the condition (plugin-to-plugin.md §3.5).",
    }))
}

/// The custody references an order's rental lines carry, item ids only.
fn custody_lines(lines: &[Value]) -> Value {
    let rentals: Vec<Value> = lines
        .iter()
        .filter(|line| line["item_kind"].as_str() == Some(KIND_RENTAL))
        .map(|line| {
            json!({
                "line_id": line["id"],
                "equipment_item_id": line["equipment_item_id"],
                "checkout": format!(
                    "POST /api/equipment/item/{}/checkout",
                    line["equipment_item_id"].as_i64().unwrap_or_default()
                ),
            })
        })
        .collect();
    if rentals.is_empty() {
        Value::Null
    } else {
        json!({
            "rentals": rentals,
            "note": "a rental order charges the fee; handing the item over is equipment's act, in \
                     equipment's own checkout state machine",
        })
    }
}

/// The draw as a client reads it: what is owed to (or from) the scholarship fund,
/// where it has got to, and why the status is what it is.
fn draw_block(order: &Value) -> Value {
    let funded = order["funded_cents"].as_i64().unwrap_or(0);
    let status = order["draw_status"].as_str().unwrap_or_default();
    json!({
        "funded_cents": funded,
        "funded_display": format_cents(funded),
        "from_fund_code": FUND_SCHOLARSHIP,
        "to_fund_code": order["fund_code"],
        "status": status,
        "transfer_group": order["draw_ref"],
        "error": order["draw_error"],
        "booked_at": order["draw_booked_at"],
        "mechanism": MECHANISM_FINANCE_TRANSFER,
        "how": if funded <= 0 && order["charged_cents"].as_i64().unwrap_or(0) == 0 {
            "nothing is funded either way: the item's price is zero, so there is no subsidy to draw"
        } else if funded <= 0 {
            "nothing is funded: the member paid the shop's whole price"
        } else if status == DRAW_BOOKED {
            "finance wrote both legs in one statement: the sum of every fund is unchanged"
        } else {
            "a caller holding finance:write books the draw with POST \
             /api/store/order/{id}/draw, as themselves; a machine-originated reduction has no \
             such caller (plugin-to-plugin.md §3.2)"
        },
    })
}

/// The paid completion's ledger state, as a client reads it.
fn ledger_block(order: &Value) -> Value {
    let status = order["ledger_status"].as_str().unwrap_or_default();
    json!({
        "status": if status.is_empty() { "not_attempted" } else { status },
        "transaction_id": order["ledger_transaction_id"],
        "error": order["ledger_error"],
        "payment_ref": order["payment_ref"],
        "mechanism": MECHANISM_STRIPE_BOOK,
        "note": "finance owns every entry; this plugin asked stripe to hand the payment over and \
                 recorded what finance answered (§3.3)",
    })
}

/// The status a cross-plugin call's outcome leaves behind: `booked` when it
/// landed, `refused` when the target answered a 4xx, `failed` otherwise.
///
/// One function for two fields because the words are one vocabulary: the order's
/// `draw_status` and its `ledger_status` both say how far money got, and a
/// reader should not have to learn two spellings of "landed".
fn outcome_status(outcome: &CallOutcome) -> &'static str {
    if outcome.ok {
        return DRAW_BOOKED;
    }
    match outcome.http_status {
        Some(status) if (400..500).contains(&status) => DRAW_REFUSED,
        _ => DRAW_FAILED,
    }
}

/// Whether the order belongs to the caller: the member who bought, or whoever
/// placed it for them.
fn order_is_theirs(order: &Value, caller: Option<&str>) -> bool {
    let Some(caller) = caller else { return false };
    let member = order["member_id"].as_str().unwrap_or_default();
    let placed_by = order["placed_by"].as_str().unwrap_or_default();
    caller == member || (!placed_by.is_empty() && caller == placed_by)
}

/// Whether stripe's payment is the payment this order was waiting for.
///
/// The amount is checked always (a payment for another amount is not this sale);
/// the session is checked when stripe recorded one, and the member when the
/// payment names one. A mismatch is a `409` and nothing is written.
pub fn payment_matches(order: &Value, payment: &Value) -> Result<(), String> {
    let charged = order["charged_cents"].as_i64().unwrap_or(0);
    let paid = payment["amount_cents"].as_i64().unwrap_or(0);
    if paid != charged {
        return Err(format!(
            "that payment is for {} and this order charges {}: complete the order against the \
             payment that paid it",
            format_cents(paid),
            format_cents(charged)
        ));
    }
    let ours = order["stripe_session_id"].as_str().unwrap_or_default();
    let theirs = payment["session_id"].as_str().unwrap_or_default();
    if !ours.is_empty() && !theirs.is_empty() && ours != theirs {
        return Err(
            "that payment came from a different Checkout session than the one this order opened"
                .to_string(),
        );
    }
    let member = order["member_id"].as_str().unwrap_or_default();
    let payer = payment["member_id"].as_str().unwrap_or_default();
    if !member.is_empty() && !payer.is_empty() && member != payer {
        return Err(format!(
            "that payment names {payer:?} and this order is {member:?}"
        ));
    }
    Ok(())
}

/// A kind, or a plain statement of the two there are.
pub fn normalize_kind(raw: &str) -> Result<String, String> {
    let kind = raw.trim().to_ascii_lowercase();
    if KINDS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!("kind must be one of {}", KINDS.join(", ")))
    }
}

/// A category, or a plain statement of the set.
pub fn normalize_category(raw: &str) -> Result<String, String> {
    let category = raw.trim().to_ascii_lowercase();
    if CATEGORIES.contains(&category.as_str()) {
        Ok(category)
    } else {
        Err(format!("category must be one of {}", CATEGORIES.join(", ")))
    }
}

/// A tier, or a plain statement of the scale.
pub fn normalize_tier(raw: &Option<String>) -> Result<String, String> {
    match trimmed(raw) {
        Some(tier) => tier_of(&tier)
            .map(|tier| tier.code.to_string())
            .ok_or_else(|| format!("tier must be one of {}", TIER_CODES.join(", "))),
        None => Ok(DEFAULT_TIER.to_string()),
    }
}

/// An order status filter, or a plain statement of the set.
pub fn normalize_order_status(raw: &str) -> Result<String, String> {
    let status = raw.trim().to_ascii_lowercase();
    if ORDER_STATUSES.contains(&status.as_str()) {
        Ok(status)
    } else {
        Err(format!(
            "status must be one of {}",
            ORDER_STATUSES.join(", ")
        ))
    }
}

/// A price the database will accept, refusing rather than wrapping.
pub fn normalize_price(price_cents: i64) -> Result<i64, String> {
    if price_cents < 0 {
        return Err("base_price_cents must not be negative".to_string());
    }
    if price_cents > MAX_PRICE_CENTS {
        return Err(format!(
            "base_price_cents must be at most {MAX_PRICE_CENTS} ({}): the shop is a troop's shop",
            format_cents(MAX_PRICE_CENTS)
        ));
    }
    Ok(price_cents)
}

/// A trimmed, non-blank, bounded string, or `None` when nothing usable was given.
fn optional_bounded(raw: &Option<String>, max: usize, field: &str) -> Result<Option<String>, String> {
    match trimmed(raw) {
        Some(value) => bounded(&value, max, field).map(Some),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ItemBody {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    sku: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    base_price_cents: i64,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    fund_code: Option<String>,
    #[serde(default)]
    equipment_item_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct ItemPatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    sku: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    base_price_cents: Option<i64>,
    #[serde(default)]
    fund_code: Option<String>,
    #[serde(default)]
    active: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct OrderBody {
    #[serde(default)]
    member_id: Option<String>,
    #[serde(default)]
    lines: Vec<OrderLineBody>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OrderLineBody {
    #[serde(default)]
    item_id: i64,
    #[serde(default)]
    quantity: i64,
}

#[derive(Debug, Default, Deserialize)]
struct CheckoutOverride {
    #[serde(default)]
    success_url: Option<String>,
    #[serde(default)]
    cancel_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CompleteBody {
    #[serde(default)]
    stripe_payment_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct CompBody {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    allow_overdraft: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct DrawBody {
    #[serde(default)]
    allow_overdraft: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use adjutant_sdk::testing::{response_json, TestHost, TestRequest};

    // --- harness ---------------------------------------------------------

    async fn store_plugin(config: Value) -> (TestHost, StorePlugin, Vec<RouteDefinition>) {
        let host = TestHost::new().with_config(config);
        let mut plugin = StorePlugin::new();
        plugin.init(host.context("store")).await.expect("init");
        let routes = plugin.routes();
        (host, plugin, routes)
    }

    fn configured_host() -> Value {
        json!({ "base_url": "http://127.0.0.1:8787", "fund_code": "general" })
    }

    fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
        routes
            .iter()
            .find(|r| r.method.as_str() == method && r.path == path)
            .unwrap_or_else(|| panic!("route {method} {path}"))
    }

    async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, Value) {
        match handler(req).await {
            Ok(resp) => (resp.status, response_json(&resp)),
            Err(e) => (e.status(), json!({ "error": e.to_string() })),
        }
    }

    /// The rendered bind parameters of the last call whose SQL contains
    /// `needle` — the house helper (`plugins/conflicts/tests/conflicts.rs`),
    /// because `SqlValue` deliberately implements no `PartialEq`, so a bind
    /// parameter can only be asserted on its rendered form.
    fn param_text(host: &TestHost, needle: &str) -> String {
        let params = host
            .db
            .last_query_params(needle)
            .or_else(|| host.db.last_execute_params(needle))
            .unwrap_or_else(|| panic!("no query or execute whose SQL contains {needle:?}"));
        params
            .iter()
            .map(|p| format!("{p:?}"))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// The audit log's action is a bind parameter, so a test has to read it back
    /// rather than look for it in the statement.
    fn assert_audited(host: &TestHost, action: &str) {
        let rendered = param_text(host, "audit_log");
        assert!(rendered.contains(action), "no {action} audit: {rendered}");
    }

    /// A caller's credential, as the core delivers it.
    fn bearer(req: TestRequest) -> TestRequest {
        req.header("authorization", "Bearer test-session")
    }

    fn item_row(id: i64, kind: &str, price: i64, fund: &str, equipment: Value) -> Value {
        json!({
            "id": id,
            "kind": kind,
            "sku": Value::Null,
            "name": if kind == KIND_RENTAL { "Tent, 4-person" } else { "Camp patch" },
            "category": if kind == KIND_RENTAL { CATEGORY_GEAR } else { CATEGORY_PATCH },
            "description": "",
            "base_price_cents": price,
            "currency": "cad",
            "fund_code": fund,
            "equipment_item_id": equipment,
            "active": true,
            "created_by": "bea",
            "created_at": "2026-09-25 12:00:00+00",
            "updated_at": "2026-09-25 12:00:00+00"
        })
    }

    fn order_row(id: i64, status: &str, price: i64, charged: i64, funded: i64, draw: &str) -> Value {
        json!({
            "id": id,
            "member_id": "bea",
            "placed_by": "bea",
            "status": status,
            "currency": "cad",
            "price_tier": if charged == price { TIER_STANDARD } else { TIER_SUPPORTED },
            "price_cents": price,
            "charged_cents": charged,
            "funded_cents": funded,
            "fund_code": FUND_GENERAL,
            "note": "",
            "stripe_session_row": 4,
            "stripe_session_id": "cs_test_1",
            "checkout_url": "https://checkout.stripe.test/1",
            "payment_ref": if status == STATUS_PAID { "pi_test_1" } else { "" },
            "ledger_status": if status == STATUS_PAID { DRAW_BOOKED } else { "" },
            "ledger_transaction_id": Value::Null,
            "ledger_error": Value::Null,
            "completed_by": if status == STATUS_PAID { "casey" } else { "" },
            "completed_at": Value::Null,
            "comp_reason": if status == STATUS_COMPED { "the tent was needed that weekend" } else { "" },
            "comp_by": if status == STATUS_COMPED { "casey" } else { "" },
            "comp_at": Value::Null,
            "draw_status": draw,
            "draw_ref": "",
            "draw_error": Value::Null,
            "draw_by": "",
            "draw_attempted_at": Value::Null,
            "draw_booked_at": Value::Null,
            "created_at": "2026-09-25 12:00:00+00",
            "updated_at": "2026-09-25 12:00:00+00"
        })
    }

    // --- the sliding scale (pure) ---------------------------------------

    #[test]
    fn the_scale_is_the_dues_scale_and_never_prices_above_the_price() {
        assert_eq!(TIER_CODES, [TIER_PATRON, TIER_STANDARD, TIER_SUPPORTED, TIER_HARDSHIP]);
        // Standard pays the shop's price; supported pays half of it.
        assert_eq!(charged_unit_cents(2_000, TIER_STANDARD), Some(2_000));
        assert_eq!(charged_unit_cents(2_000, TIER_SUPPORTED), Some(1_000));
        assert_eq!(charged_unit_cents(2_000, TIER_HARDSHIP), Some(0));
        // Patron pays twice everywhere *else*: here the shop charges its own
        // price and takes no more, because a payment split across two funds is
        // not expressible through one Stripe payment.
        assert_eq!(charged_unit_cents(2_000, TIER_PATRON), Some(2_000));
        // A code this plugin does not know is refused rather than guessed at.
        assert_eq!(charged_unit_cents(2_000, "wizard"), None);
        assert_eq!(normalize_tier(&Some(" SUPPORTED ".to_string())).unwrap(), TIER_SUPPORTED);
        assert!(normalize_tier(&Some("wizard".to_string())).is_err());
        assert_eq!(normalize_tier(&None).unwrap(), DEFAULT_TIER);
    }

    #[test]
    fn a_line_splits_into_price_charge_and_draw() {
        // 3 × $20 at the supported tier: the sale is $60, the member pays $30,
        // and $30 is drawn from the scholarship fund.
        let supported = pricing(2_000, 3, TIER_SUPPORTED).expect("a price");
        assert_eq!(supported.unit_cents, 1_000);
        assert_eq!(supported.price_cents, 6_000);
        assert_eq!(supported.charged_cents, 3_000);
        assert_eq!(supported.funded_cents, 3_000);
        // The draw is always the difference, and never negative.
        assert_eq!(pricing(2_000, 1, TIER_STANDARD).unwrap().funded_cents, 0);
        assert_eq!(pricing(2_000, 1, TIER_PATRON).unwrap().charged_cents, 2_000);
        assert_eq!(pricing(2_000, 2, TIER_HARDSHIP).unwrap().funded_cents, 4_000);
        // Rounding is half-up, once, at the cent.
        assert_eq!(charged_unit_cents(999, TIER_SUPPORTED), Some(500));
        assert_eq!(charged_unit_cents(1, TIER_SUPPORTED), Some(1));
        assert!(mul_cents(i64::MAX, 2).is_err());
    }

    #[test]
    fn the_published_scale_shows_the_draw_every_tier_produces() {
        let scale = scale_table(2_000);
        assert_eq!(scale.len(), 4);
        assert_eq!(scale[1]["tier"], TIER_STANDARD);
        assert_eq!(scale[1]["charged_cents"], 2_000);
        assert_eq!(scale[1]["funded_cents"], 0);
        assert_eq!(scale[2]["tier"], TIER_SUPPORTED);
        assert_eq!(scale[2]["charged_display"], "$10.00");
        assert_eq!(scale[2]["funded_cents"], 1_000);
        assert_eq!(scale[3]["funded_cents"], 2_000);
        assert_eq!(scale[1]["capped_at_price"], false);
        assert_eq!(scale[0]["capped_at_price"], true);
        assert_eq!(format_percent(5_000), "50%");
        assert_eq!(format_percent(250), "2.5%");
        assert_eq!(format_cents(123_456), "$1,234.56");
        assert_eq!(format_cents(0), "$0.00");
    }

    #[test]
    fn the_vocabulary_is_the_one_the_sql_spells() {
        // The codes the handlers accept have to be the codes the CHECK
        // constraints spell, or a 400 becomes a 500.
        assert_eq!(KINDS, [KIND_PRODUCT, KIND_RENTAL]);
        assert_eq!(
            CATEGORIES,
            [
                CATEGORY_UNIFORM,
                CATEGORY_PATCH,
                CATEGORY_INSIGNIA,
                CATEGORY_GEAR,
                CATEGORY_MERCH,
                CATEGORY_OTHER
            ]
        );
        assert_eq!(
            ORDER_STATUSES,
            [STATUS_OPEN, STATUS_AWAITING_PAYMENT, STATUS_PAID, STATUS_COMPED]
        );
        assert_eq!(
            DRAW_STATUSES,
            [
                DRAW_NONE,
                DRAW_UNBOOKED,
                DRAW_ATTEMPTING,
                DRAW_BOOKED,
                DRAW_REFUSED,
                DRAW_FAILED
            ]
        );
        for code in [KIND_PRODUCT, KIND_RENTAL] {
            assert!(MIGRATION_SCHEMA.contains(&format!("'{code}'")));
        }
        for tier in TIER_CODES {
            assert!(MIGRATION_SCHEMA.contains(&format!("'{tier}'")));
        }
        for draw in DRAW_STATUSES {
            assert!(MIGRATION_SCHEMA.contains(&format!("'{draw}'")));
        }
        // The two rules that make a comp and a payment real are constraints.
        assert!(MIGRATION_SCHEMA.contains("store_orders_comp_is_an_authority"));
        assert!(MIGRATION_SCHEMA.contains("store_orders_paid_is_charged"));
        assert!(MIGRATION_SCHEMA.contains("store_orders_funding_is_the_difference"));
        assert!(MIGRATION_SCHEMA.contains("store_orders_draw_matches_funding"));
        // Custody is a foreign id, never a copy of equipment's facts.
        assert!(MIGRATION_SCHEMA.contains("equipment_item_id"));
        assert!(
            !MIGRATION_SCHEMA.contains("\"condition\""),
            "the item's condition is equipment's and must not be copied here"
        );
    }

    #[test]
    fn the_ledger_words_are_the_draw_words_for_landed_refused_and_failed() {
        // One vocabulary for "how far did the money get", whichever side of the
        // boundary said it: the order's draw_status and the ledger_status copied
        // from finance's answer are read by the same eyes.
        assert_eq!(LEDGER_BOOKED, DRAW_BOOKED);
        assert_eq!(LEDGER_REFUSED, DRAW_REFUSED);
        assert_eq!(LEDGER_FAILED, DRAW_FAILED);
        assert_ne!(LEDGER_UNBOOKED, DRAW_NONE);
        for word in [LEDGER_UNBOOKED, LEDGER_BOOKED, LEDGER_REFUSED, LEDGER_FAILED] {
            assert!(DRAW_STATUSES.contains(&word), "{word} must be a draw status too");
        }
    }

    #[test]
    fn a_zero_amount_is_not_expressible_in_finance_which_is_why_the_draw_is_a_transfer() {
        // The finding this crate is built on, pinned so it is not rediscovered:
        // finance refuses a zero-amount ledger entry at two layers, so a comp
        // cannot be a zero transaction and is a positive transfer instead.
        let finance = include_str!("../../finance/src/lib.rs");
        assert!(finance.contains("CONSTRAINT transactions_amount_nonzero CHECK (amount_cents <> 0)"));
        assert!(finance.contains("CONSTRAINT transactions_income_positive"));
        assert!(finance.contains("\"amount must be a positive magnitude in cents"));
        // The one zero finance *can* hold is a dues waiver, which is an
        // assessment, not a sale.
        assert!(finance.contains("CONSTRAINT dues_waived_is_zero"));
        // And the scholarship fund is one of the six kinds, by code.
        assert!(finance.contains("pub const FUND_SCHOLARSHIP: &str = \"scholarship\";"));
        assert!(finance.contains("FUND_KINDS"));
    }

    // --- the plugin's shape ----------------------------------------------

    #[tokio::test]
    async fn every_route_is_under_its_own_namespace_and_gated() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        let _ = host;
        assert_eq!(routes.len(), 14, "every route is listed");
        let mut seen: Vec<(String, String)> = Vec::new();
        for route in &routes {
            assert!(
                route.path.starts_with("/api/store/"),
                "{} escapes the namespace",
                route.path
            );
            assert!(route.required_permission.is_some(), "{} is ungated", route.path);
            let key = (route.method.as_str().to_string(), route.path.clone());
            assert!(!seen.contains(&key), "duplicate route {key:?}");
            seen.push(key);
        }
        let gated: Vec<&str> = routes
            .iter()
            .filter(|r| r.required_permission.as_deref() == Some(PERM_COMP))
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(
            gated,
            vec!["/api/store/order/{id}/comp"],
            "a comp is the only act that needs store:comp"
        );
        // The catalogue and the worklist are the shopkeeper's; a comp is not.
        let read_all: Vec<&str> = routes
            .iter()
            .filter(|r| r.required_permission.as_deref() == Some(PERM_READ_ALL))
            .map(|r| r.path.as_str())
            .collect();
        assert!(read_all.contains(&"/api/store/comps"));
        assert!(read_all.contains(&"/api/store/orders/unsettled"));
    }

    #[test]
    fn entry_symbol_is_exported() {
        let raw = adjutant_plugin_create();
        assert!(!raw.is_null());
        let boxed = unsafe { Box::from_raw(raw) };
        assert_eq!(boxed.id(), "store");
        assert_eq!(adjutant_sdk_abi(), adjutant_sdk::SDK_ABI_VERSION);
    }

    // --- placing an order -------------------------------------------------

    #[tokio::test]
    async fn an_order_is_priced_from_the_catalogue_never_from_the_request() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        // The catalogue: a patch at $20, and a tent rental at $50 addressed to
        // the equipment fund.
        host.db.push_rows(vec![item_row(1, KIND_PRODUCT, 2_000, FUND_GENERAL, Value::Null)]);
        host.db.push_rows(vec![json!({ "order_id": 9 })]);
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 1_000, 1_000, DRAW_UNBOOKED)]);
        host.db.push_rows(vec![]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order").handler,
            bearer(TestRequest::post("/api/store/order"))
                .identity("bea", &["scout"])
                .json(&json!({
                    "lines": [{ "item_id": 1, "quantity": 1 }],
                    "tier": TIER_SUPPORTED,
                    "note": "for camp"
                }))
                .build(),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        // The insert carried the price the shop computed: $20 sold, $10 charged,
        // $10 drawn from the scholarship fund.
        let bound = param_text(&host, "INSERT INTO");
        // The handler computed the price, the charge and the draw from the
        // catalogue; none of the three came from the request.
        assert!(bound.contains("Int(2000)"), "the price is the shop's: {bound}");
        assert!(bound.contains("Int(1000)"), "the charge is the tier's share: {bound}");
        assert!(bound.contains("Int(1000)"), "the draw is the difference: {bound}");
        assert!(param_text(&host, "WHERE o.id = $1").contains("Int(9)"));
        assert_audited(&host, "store.order.place");
        assert!(host.events.published_types().contains(&EVENT_ORDER_PLACED.to_string()));
    }

    #[tokio::test]
    async fn an_order_whose_lines_name_two_funds_is_refused() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![
            item_row(1, KIND_PRODUCT, 2_000, FUND_GENERAL, Value::Null),
            item_row(2, KIND_PRODUCT, 2_000, FUND_SCHOLARSHIP, Value::Null),
        ]);
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order").handler,
            bearer(TestRequest::post("/api/store/order"))
                .identity("bea", &["scout"])
                .json(&json!({
                    "lines": [
                        { "item_id": 1, "quantity": 1 },
                        { "item_id": 2, "quantity": 1 }
                    ]
                }))
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("one fund"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn ordering_for_somebody_else_needs_the_shopkeepers_authority() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        // The permission check answers 0 rows: the caller does not hold it.
        host.db.push_rows(vec![json!({ "n": 0 })]);
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order").handler,
            bearer(TestRequest::post("/api/store/order"))
                .identity("bea", &["scout"])
                .json(&json!({
                    "member_id": "someone-else",
                    "lines": [{ "item_id": 1, "quantity": 1 }]
                }))
                .build(),
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains(PERM_MANAGE));
    }

    // --- the member-driven path (a real §2(b) call) ----------------------

    #[tokio::test]
    async fn checkout_refuses_before_calling_anything_when_there_is_no_credential_to_forward() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/checkout").handler,
            TestRequest::post("/api/store/order/9/checkout")
                .param("id", "9")
                .identity("bea", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("nothing to forward")
            || body["error"].as_str().unwrap_or_default().contains("no credential to forward"));
        // Nothing was called, and nothing was read: the refusal is first.
        assert!(host.http.request_urls().is_empty());
        assert_eq!(host.db.query_count(), 0);
    }

    #[tokio::test]
    async fn checkout_calls_stripe_as_the_caller_for_the_charge_and_never_the_price() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 1_200, 800, DRAW_UNBOOKED)]);
        host.http.push_json(
            201,
            &json!({
                "session": { "id": 4, "stripe_session_id": "cs_test_9", "checkout_url": "https://checkout.stripe.test/9" },
                "checkout_url": "https://checkout.stripe.test/9"
            }),
        );
        host.db.push_rows(vec![order_row(
            9,
            STATUS_AWAITING_PAYMENT,
            2_000,
            1_200,
            800,
            DRAW_UNBOOKED,
        )]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/checkout").handler,
            bearer(TestRequest::post("/api/store/order/9/checkout"))
                .param("id", "9")
                .identity("bea", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let urls = host.http.request_urls();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].0, "POST");
        assert!(urls[0].1.ends_with(STRIPE_CHECKOUT_PATH), "{urls:?}");
        // The session was settled against the order, and the status moved.
        assert_eq!(body["order"]["status"], STATUS_AWAITING_PAYMENT);
        assert_eq!(body["checkout_url"], "https://checkout.stripe.test/9");
        // The draw is still exactly what the shop priced: unbooked, and visible.
        assert_eq!(body["draw"]["status"], DRAW_UNBOOKED);
        assert_eq!(body["draw"]["funded_cents"], 800);
        assert!(host.events.published_types().contains(&EVENT_ORDER_AWAITING_PAYMENT.to_string()));
    }

    #[tokio::test]
    async fn a_refused_checkout_passes_stripes_own_status_through_and_leaves_the_order_alone() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE)]);
        host.http.push_json(403, &json!({ "error": "requires stripe:checkout at scope troop" }));

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/checkout").handler,
            bearer(TestRequest::post("/api/store/order/9/checkout"))
                .param("id", "9")
                .identity("bea", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("requires stripe:checkout"),
            "stripe's own words, not a flattened 500: {body}"
        );
        assert!(
            body["error"].as_str().unwrap_or_default().contains("nothing was charged"),
            "{body}"
        );
        // No settle statement ran: the order is untouched.
        assert!(!host
            .db
            .executed_sql()
            .iter()
            .chain(host.db.queried_sql().iter())
            .any(|sql| sql.contains("awaiting_payment")));
    }

    #[tokio::test]
    async fn a_zero_charge_is_never_sent_to_stripe() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 0, 2_000, DRAW_UNBOOKED)]);
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/checkout").handler,
            bearer(TestRequest::post("/api/store/order/9/checkout"))
                .param("id", "9")
                .identity("bea", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("/comp"), "{body}");
        assert!(host.http.request_urls().is_empty(), "stripe was never called");
    }

    // --- completing a paid order -----------------------------------------

    #[tokio::test]
    async fn a_paid_order_is_completed_against_stripes_own_record_and_the_ledger_asked_to_land() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(
            9,
            STATUS_AWAITING_PAYMENT,
            2_000,
            2_000,
            0,
            DRAW_NONE,
        )]);
        host.http.push_json(
            200,
            &json!({ "payment": {
                "payment_id": "pi_test_9",
                "amount_cents": 2_000,
                "session_id": "cs_test_1",
                "member_id": "bea",
                "ledger_status": "delegated_event"
            }}),
        );
        host.http.push_json(
            200,
            &json!({
                "ledger": { "status": DRAW_BOOKED },
                "finance": { "http_status": 201, "transaction_id": 77 }
            }),
        );
        // The row the write returns: what the handler put there, read back.
        let mut paid = order_row(9, STATUS_PAID, 2_000, 2_000, 0, DRAW_NONE);
        paid["ledger_transaction_id"] = json!(77);
        host.db.push_rows(vec![paid]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/complete").handler,
            bearer(TestRequest::post("/api/store/order/9/complete"))
                .param("id", "9")
                .identity("casey", &["storekeeper"])
                .json(&json!({ "stripe_payment_id": 5 }))
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let urls = host.http.request_urls();
        assert_eq!(urls.len(), 2, "verified, then booked: {urls:?}");
        assert_eq!(
            urls[0].1,
            format!("http://127.0.0.1:8787{STRIPE_PAYMENT_PATH}/5")
        );
        assert_eq!(
            urls[1].1,
            format!("http://127.0.0.1:8787{STRIPE_PAYMENT_PATH}/5/book")
        );
        assert_eq!(body["order"]["status"], STATUS_PAID);
        // What the handler wrote: stripe's own payment id, opaque, and finance's
        // transaction id from the answer above.
        let written = param_text(&host, "status = 'paid'");
        assert!(written.contains("pi_test_9"), "{written}");
        assert!(written.contains("Int(77)"), "{written}");
        assert_audited(&host, "store.order.complete");
        assert_eq!(body["ledger"]["status"], DRAW_BOOKED);
        assert_eq!(body["ledger"]["transaction_id"], 77);
        // The event carries opaque ids and money, never a credential.
        let published = host.events.payloads(EVENT_ORDER_PAID);
        assert_eq!(published.len(), 1);
        assert!(published[0].get("payment_ref").is_none());
        assert!(published[0].get("stripe_session_id").is_none());
    }

    #[tokio::test]
    async fn a_payment_that_is_not_this_orders_is_a_conflict_and_writes_nothing() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(
            9,
            STATUS_AWAITING_PAYMENT,
            2_000,
            2_000,
            0,
            DRAW_NONE,
        )]);
        host.http.push_json(
            200,
            &json!({ "payment": {
                "payment_id": "pi_other",
                "amount_cents": 1_500,
                "session_id": "cs_other",
                "member_id": "bea"
            }}),
        );
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/complete").handler,
            bearer(TestRequest::post("/api/store/order/9/complete"))
                .param("id", "9")
                .identity("casey", &["storekeeper"])
                .json(&json!({ "stripe_payment_id": 5 }))
                .build(),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("$15.00"), "{body}");
        assert_eq!(host.http.request_urls().len(), 1, "the ledger was never asked");
    }

    // --- the comp and the draw -------------------------------------------

    #[tokio::test]
    async fn a_comp_needs_a_reason_the_authority_and_books_the_draw_as_the_caller() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE)]);
        host.db.push_rows(vec![order_row(9, STATUS_COMPED, 2_000, 0, 2_000, DRAW_ATTEMPTING)]);
        host.http.push_json(
            200,
            &json!({ "funds": [
                { "id": 2, "code": FUND_SCHOLARSHIP, "active": true },
                { "id": 1, "code": FUND_GENERAL, "active": true }
            ]}),
        );
        host.http.push_json(
            201,
            &json!({
                "transfer_group": "9c1e2f00-0000-0000-0000-000000000001",
                "amount_cents": 2_000,
                "sum_cents": 0,
                "entries": [{ "amount_cents": -2_000 }, { "amount_cents": 2_000 }]
            }),
        );
        let mut booked = order_row(9, STATUS_COMPED, 2_000, 0, 2_000, DRAW_BOOKED);
        booked["draw_ref"] = json!("9c1e2f00-0000-0000-0000-000000000001");
        host.db.push_rows(vec![booked]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/comp").handler,
            bearer(TestRequest::post("/api/store/order/9/comp"))
                .param("id", "9")
                .identity("casey", &["commander"])
                .json(&json!({ "reason": "the tent was needed for the weekend expedition" }))
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let urls = host.http.request_urls();
        assert_eq!(urls.len(), 2, "the funds were resolved, then the transfer asked: {urls:?}");
        assert!(urls[0].1.contains(FINANCE_FUNDS_PATH));
        assert!(urls[1].1.ends_with(FINANCE_TRANSFER_PATH));
        assert_eq!(body["order"]["status"], STATUS_COMPED);
        assert_eq!(body["order"]["charged_cents"], 0);
        assert_eq!(body["draw"]["funded_cents"], 2_000);
        assert_eq!(body["draw"]["status"], DRAW_BOOKED);
        assert_eq!(body["draw"]["from_fund_code"], FUND_SCHOLARSHIP);
        assert!(host.events.published_types().contains(&EVENT_ORDER_COMPED.to_string()));
        assert!(host.events.published_types().contains(&EVENT_DRAW_BOOKED.to_string()));
        assert_audited(&host, "store.order.comp");
    }

    #[tokio::test]
    async fn a_comp_without_a_reason_is_refused_before_anything_is_read() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/comp").handler,
            bearer(TestRequest::post("/api/store/order/9/comp"))
                .param("id", "9")
                .identity("casey", &["commander"])
                .json(&json!({ "reason": "   " }))
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("reason"));
        assert_eq!(host.db.query_count(), 0, "no order was read");
        assert!(host.http.request_urls().is_empty());
    }

    #[tokio::test]
    async fn a_comp_on_an_order_whose_proceeds_are_the_scholarship_fund_is_refused() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        let mut order = order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE);
        order["fund_code"] = json!(FUND_SCHOLARSHIP);
        host.db.push_rows(vec![order]);
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/comp").handler,
            bearer(TestRequest::post("/api/store/order/9/comp"))
                .param("id", "9")
                .identity("casey", &["commander"])
                .json(&json!({ "reason": "a donation, comped" }))
                .build(),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains(FUND_SCHOLARSHIP));
    }

    #[tokio::test]
    async fn a_refused_draw_leaves_the_comp_standing_and_the_money_visible() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE)]);
        host.db.push_rows(vec![order_row(9, STATUS_COMPED, 2_000, 0, 2_000, DRAW_ATTEMPTING)]);
        host.http.push_json(
            200,
            &json!({ "funds": [
                { "id": 2, "code": FUND_SCHOLARSHIP, "active": true },
                { "id": 1, "code": FUND_GENERAL, "active": true }
            ]}),
        );
        host.http.push_json(
            409,
            &json!({ "error": "would take fund scholarship from $0.00 to -$20.00" }),
        );
        let mut refused = order_row(9, STATUS_COMPED, 2_000, 0, 2_000, DRAW_REFUSED);
        refused["draw_error"] = json!("would take fund scholarship from $0.00 to -$20.00");
        host.db.push_rows(vec![refused]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/comp").handler,
            bearer(TestRequest::post("/api/store/order/9/comp"))
                .param("id", "9")
                .identity("casey", &["commander"])
                .json(&json!({ "reason": "hardship" }))
                .build(),
        )
        .await;
        // Finance's own status, not a flattened 500: the caller can see it was
        // the scholarship fund's balance that refused, and can pass
        // allow_overdraft if that is their decision.
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["order"]["status"], STATUS_COMPED);
        assert_eq!(body["draw"]["status"], DRAW_REFUSED);
        assert!(
            body["draw"]["error"]
                .as_str()
                .unwrap_or_default()
                .contains("scholarship"),
            "{body}"
        );
        assert!(
            body["note"].as_str().unwrap_or_default().contains("authority"),
            "the comp still stands, and the response says what it was"
        );
    }

    #[tokio::test]
    async fn a_comp_of_a_free_item_has_no_subsidy_and_calls_nobody() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 0, 0, 0, DRAW_NONE)]);
        let mut comped = order_row(9, STATUS_COMPED, 0, 0, 0, DRAW_NONE);
        comped["comp_reason"] = json!("a free neckerchief for a new scout");
        host.db.push_rows(vec![comped]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/comp").handler,
            bearer(TestRequest::post("/api/store/order/9/comp"))
                .param("id", "9")
                .identity("casey", &["commander"])
                .json(&json!({ "reason": "a free neckerchief for a new scout" }))
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        // Nothing to draw, so nothing was called: a free item's comp is an
        // authority with no money in it, not a transfer of $0.00.
        assert!(
            host.http.request_urls().is_empty(),
            "a free item's comp calls nobody: {:?}",
            host.http.request_urls()
        );
        assert_eq!(body["draw"]["funded_cents"], 0);
        assert_eq!(body["draw"]["status"], DRAW_NONE);
        assert!(param_text(&host, "status = 'comped'").contains("neckerchief"));
        assert_audited(&host, "store.order.comp");
    }

    #[tokio::test]
    async fn a_reduction_is_left_unbooked_and_a_treasurer_books_it_with_their_own_authority() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_PAID, 2_000, 1_200, 800, DRAW_UNBOOKED)]);
        host.db.push_rows(vec![order_row(9, STATUS_PAID, 2_000, 1_200, 800, DRAW_ATTEMPTING)]);
        host.http.push_json(
            200,
            &json!({ "funds": [
                { "id": 2, "code": FUND_SCHOLARSHIP, "active": true },
                { "id": 1, "code": FUND_GENERAL, "active": true }
            ]}),
        );
        host.http.push_json(
            201,
            &json!({ "transfer_group": "9c1e2f00-0000-0000-0000-000000000002", "amount_cents": 800 }),
        );
        let mut booked = order_row(9, STATUS_PAID, 2_000, 1_200, 800, DRAW_BOOKED);
        booked["draw_ref"] = json!("9c1e2f00-0000-0000-0000-000000000002");
        host.db.push_rows(vec![booked]);

        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/draw").handler,
            bearer(TestRequest::post("/api/store/order/9/draw"))
                .param("id", "9")
                .identity("treasurer", &["treasurer"])
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["draw"]["status"], DRAW_BOOKED);
        assert_eq!(
            body["draw"]["transfer_group"],
            "9c1e2f00-0000-0000-0000-000000000002"
        );
        assert!(host.events.published_types().contains(&EVENT_DRAW_BOOKED.to_string()));
        assert_audited(&host, "store.order.draw");
    }

    #[tokio::test]
    async fn a_draw_is_not_booked_for_a_sale_that_has_not_happened() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 1_200, 800, DRAW_UNBOOKED)]);
        let (status, body) = call(
            &route(&routes, "POST", "/api/store/order/{id}/draw").handler,
            bearer(TestRequest::post("/api/store/order/9/draw"))
                .param("id", "9")
                .identity("treasurer", &["treasurer"])
                .build(),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("paid or comped"));
        assert!(host.http.request_urls().is_empty());
    }

    // --- reads ------------------------------------------------------------

    #[tokio::test]
    async fn a_members_orders_are_narrowed_to_them_and_somebody_elses_needs_read_all() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        // Not a store:read_all holder, so the list narrows to the caller.
        host.db.push_rows(vec![json!({ "n": 0 })]);
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE)]);
        let (status, body) = call(
            &route(&routes, "GET", "/api/store/orders").handler,
            TestRequest::get("/api/store/orders")
                .identity("bea", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["narrowed_to_caller"], true);
        let listed = param_text(&host, "ORDER BY o.id DESC");
        assert!(
            listed.contains("Bool(false)"),
            "the oversight flag is false: {listed}"
        );
    }

    #[tokio::test]
    async fn somebody_elses_order_is_a_403_that_reads_like_its_absence() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![order_row(9, STATUS_OPEN, 2_000, 2_000, 0, DRAW_NONE)]);
        host.db.push_rows(vec![json!({ "n": 0 })]); // not a store:read_all holder
        let (status, body) = call(
            &route(&routes, "GET", "/api/store/order/{id}").handler,
            TestRequest::get("/api/store/order/9")
                .param("id", "9")
                .identity("mallory", &["scout"])
                .build(),
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"], "no such order");
    }

    #[tokio::test]
    async fn a_rental_names_equipment_and_copies_none_of_its_facts() {
        let (host, _plugin, routes) = store_plugin(configured_host()).await;
        host.db.push_rows(vec![item_row(
            3,
            KIND_RENTAL,
            5_000,
            FUND_GENERAL,
            json!(42),
        )]);
        let (status, body) = call(
            &route(&routes, "GET", "/api/store/item/{id}").handler,
            TestRequest::get("/api/store/item/3").param("id", "3").build(),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["item"]["equipment_item_id"], 42);
        assert_eq!(body["custody"]["equipment_item_id"], 42);
        assert_eq!(body["custody"]["checkout"], "POST /api/equipment/item/42/checkout");
        // The scale is the same arithmetic a member sees for a patch.
        assert_eq!(body["item"]["scale"][2]["charged_cents"], 2_500);
        assert_eq!(body["item"]["scale"][2]["funded_cents"], 2_500);
        // No condition and no name from equipment is copied into the answer: the
        // custody block points at equipment's own routes instead.
        let text = body.to_string();
        assert!(!text.contains("\"condition\""), "{text}");
        assert!(text.contains("condition_source"), "{text}");
        assert!(!text.contains("\"unserviceable\""), "{text}");
    }

    #[test]
    fn a_price_is_bounded_and_a_date_is_a_date() {
        assert_eq!(normalize_price(0).unwrap(), 0);
        assert!(normalize_price(-1).is_err());
        assert!(normalize_price(MAX_PRICE_CENTS + 1).is_err());
        assert_eq!(parse_date("2026-09-25").unwrap().to_string(), "2026-09-25");
        for bad in ["whenever", "25/09/2026", "2026-13-45", ""] {
            assert!(parse_date(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
