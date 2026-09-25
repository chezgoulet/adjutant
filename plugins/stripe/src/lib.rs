//! # adjutant-stripe — payments for a troop's own things (SPEC §7.13)
//!
//! SPEC §7.13 gives this plugin the payment path: dues collection, fundraising
//! donations, event fees, and the webhook that confirms a payment happened. It is
//! a per-troop integration — a troop chooses whether to run it — and it is the
//! only place a card is ever charged.
//!
//! Two rules bind it before a line of it is written, both in
//! `docs/design/boundary.md`:
//!
//! * **Finance owns the ledger.** A confirmed payment reaches
//!   `finance.transactions` by calling finance's API **as the caller**, with the
//!   caller's credential forwarded so finance's own gate re-decides — never by a
//!   privileged internal call, and never by writing another plugin's schema.
//! * **The money path cannot be event-only.** A charged card with a failed
//!   ledger write is money moved with no record, in a troop whose Accords
//!   mandate open books. Events may notify; they may not be how the ledger
//!   learns to write.
//!
//! ## What this crate does with those rules, honestly
//!
//! The second mechanism is **not available on the webhook path, and this crate
//! says so rather than pretending otherwise.**
//!
//! `docs/design/plugin-to-plugin.md` §2(b) permits exactly two ways for one
//! plugin to reach another: subscribe to an event it publishes, or call its API
//! **as the caller**, forwarding the caller's own authorization. A Stripe
//! webhook is a server-to-server POST from Stripe to this plugin: it carries no
//! session cookie and no bearer token, because nobody is making the request.
//! There is therefore no credential to forward, and finance's
//! `POST /api/finance/transaction` requires `finance:write` — so the callerless
//! path is a `401` by construction. Minting a credential for this plugin to use
//! (a service token, a dev header, a "trusted internal call") is the one thing
//! §3.1 refuses outright, and this crate does not do it.
//!
//! So a webhook-confirmed payment reaches `finance.transactions` by **enqueuing its
//! ledger booking as a durable intent** — the mechanism `plugin-to-plugin.md` §3.2
//! decided, built in the core as the `core.outbox` table, a draining relay, and a
//! registry of declared service principals. The intent is written by
//! `core.outbox_enqueue(…)` **as an expression inside this plugin's own `INSERT`**
//! into `stripe.payments`, so the confirmed payment and its ledger booking commit
//! together or neither does: there is no window in which the charge is recorded and
//! its booking is not. The relay then delivers it to finance's
//! `POST /api/finance/transaction` as the declared service principal
//! `svc.stripe.ledger` — grant `finance:write`, declared in the core's
//! `SERVICE_PRINCIPALS` for the `stripe` producer only — retries a retryable
//! failure with exponential backoff, and writes the target's own answer onto the
//! intent row. The failure the event path could not see is therefore a failure the
//! relay retries, and its exhaustion lands **in the data** as `exhausted`.
//!
//! Three limits of that are load-bearing, so they are stated here rather than
//! discovered later.
//!
//! 1. **The payload is complete before the statement runs, and it never needs a
//!    read to be.** The fund is named by finance's **id** when the read above
//!    answered (`GET /api/finance/funds`, resolving the code exactly as
//!    `POST /api/stripe/payment/{id}/book` resolves it), and by the fund's
//!    **code** when it did not — and that read is a §2(b) call carrying the
//!    caller's credential, which a webhook does not have. finance accepts either
//!    and resolves whichever it is sent *inside the statement that writes the
//!    entry*, so **issue #60 — finance's write path taking only a fund id — is
//!    closed for this path**: a producer with no read credential still composes a
//!    complete instruction. Only a fund this plugin has **seen** to be missing or
//!    inactive yields no intent: then the payment is still recorded, `unbooked`,
//!    and handed to finance through `payment.received`, with the reason in the
//!    response and the audit.
//! 2. **`ledger_status` says which of those happened.** `intent_enqueued` means an
//!    intent is enqueued and the relay will deliver it — neither booked nor
//!    unbooked. The relay's own outcome events settle it to `booked` (finance
//!    answered 2xx and its transaction id is recorded), `refused` (the target or its
//!    gate said no) or `failed` (the attempts ran out). `booked` keeps exactly its
//!    old meaning: finance confirmed the entry.
//! 3. **`GET /api/stripe/unbooked` keeps the in-flight case visible.** A payment
//!    whose intent is in flight is neither booked nor unbooked, so it is listed
//!    explicitly with its `ledger_intent_id` and the intent's own state (read
//!    through `core.outbox_producer_view()`, which is the producer's only way to
//!    read its intents). It leaves the worklist when the intent's durable state is
//!    `delivered` — the intent, not the notification, is what decides that, so a
//!    dropped event cannot hide a booking or invent one.
//!
//! `payment.received` (SPEC §5.4) is still published — but only when no intent could
//! be composed, and as the fallback it always was: finance subscribes to it and
//! books idempotently, keyed on the provider's payment id
//! (`finance.transactions.external_ref`, unique). It is deliberately *not* claimed
//! to be the synchronous, compensatable flow §3.2 requires, because **no answer
//! comes back**. Three properties of the core make that concrete, and they are why
//! the outbox is the mechanism and this is not:
//!
//! 1. A failed subscriber is a log line. `server/src/events.rs` calls the
//!    handler and, on `Err`, only `tracing::warn!("event handler failed")` — no
//!    retry, no dead-letter queue.
//! 2. Delivery itself is lossy: subscribers read from a bounded broadcast
//!    channel, and a lagged subscriber's events are **dropped**, not queued
//!    (`BUS_CAPACITY`, `RecvError::Lagged`).
//! 3. `payment.received` is a broadcast: the publisher gets `Ok(())` when the
//!    event was persisted to `core.events`, never when it was handled.
//!
//! A missing income entry also does not *imbalance* the ledger, so finance's
//! daily `ledger_audit` would not catch it: books that never recorded a payment
//! still add up.
//!
//! This crate therefore does three things about the money path, and stops there:
//!
//! * **It records the truth.** Every confirmed payment carries a
//!   `ledger_status`, the mechanism that carried it, its intent when it has one, and
//!   finance's own answer when one came back. `GET /api/stripe/unbooked` is the
//!   worklist of payments with no *confirmed* ledger entry, and a
//!   `stripe.ledger.unbooked` event notices them (a notification; never the
//!   mechanism by which the ledger learns).
//! * **It offers the synchronous path where a caller genuinely exists.**
//!   `POST /api/stripe/payment/{id}/book` is a real §2(b) call: it forwards the
//!   caller's `authorization`/`cookie` to finance over `ctx.http` and finance's
//!   own gate re-decides. A treasurer who holds `finance:write` can therefore
//!   close the gap by hand, today, with their own authority and nothing added.
//! * **It never invents an authority it does not have.** The plugin holds two
//!   secrets, and neither is ever used as an Adjutant credential. The only identity
//!   a delivery as this plugin carries is `svc.stripe.ledger`, which the core
//!   attests from the intent row and which this plugin cannot name, mint or widen.
//!
//! ## Idempotency, twice over
//!
//! A redelivered webhook must not double-count, and it must not *lose* a payment
//! either. Three keys do that, at three layers:
//!
//! * `stripe.webhook_events.event_id` (Stripe's `evt_…`, unique) is the receipt
//!   ledger: one row per distinct delivery, with a `redeliveries` counter.
//! * `stripe.payments.payment_id` (Stripe's `pi_…`, unique) is the payment
//!   record, and it is the **same key finance holds** — `external_ref`. A
//!   payment that reaches the ledger by both paths is still one entry, because
//!   finance's unique index refuses the second.
//! * The **intent's idempotency key is that same `payment_id`**, so a
//!   redelivered webhook calls `core.outbox_enqueue` with the key it already
//!   used and gets **the intent it already has** rather than a second one
//!   (`core.outbox` is unique on `(producer_plugin, idempotency_key)`, and the
//!   function returns the existing id). One payment, one intent — the statement
//!   that records the payment carries `ledger_intent_id`, and a unique index
//!   refuses a second payment claiming one intent.
//!
//! A redelivery is *self-healing*, not merely tolerated: the handler checks the
//! payment's `ledger_status` rather than the event receipt, so a delivery whose
//! `payment.received` publish failed retries the publish on the next delivery
//! while a genuinely-recorded payment is a no-op.
//!
//! ## Schema (`stripe`)
//!
//! `stripe.checkout_sessions` — one row per Checkout session this plugin opens,
//! inserted `pending` *before* Stripe is called and settled afterwards, so a
//! call that never answered is a visible `failed` attempt rather than a
//! mystery. `stripe.webhook_events` — the raw receipt ledger.
//! `stripe.payments` — the confirmed payments, with the ledger outcome and
//! `ledger_intent_id`, the intent its booking rides on when it has one.
//!
//! ## Configuration
//!
//! `core.plugins.config` for the `stripe` plugin:
//!
//! ```json
//! {
//!   "secret_key": "sk_live_…",
//!   "webhook_secret": "whsec_…",
//!   "base_url": "http://127.0.0.1:8787",
//!   "api_base": "https://api.stripe.com",
//!   "currency": "cad",
//!   "success_url": "https://troop.example/paid",
//!   "cancel_url": "https://troop.example/dues",
//!   "webhook_tolerance_seconds": 300,
//!   "dues_fund_code": "general",
//!   "donation_fund_code": "general",
//!   "event_fund_code": "general",
//!   "unbooked_after_minutes": 30
//! }
//! ```
//!
//! `secret_key` and `webhook_secret` are the plugin's own secrets: never
//! logged, never returned, never placed in an event payload or an audit detail
//! (see [`StripeConfig`]). `base_url` is *this* Adjutant instance — the address
//! the ledger call dials, as `mcp` dials it. Both are refused loudly when
//! missing rather than defaulted to something that charges nobody.
//!
//! ## What is deliberately not here
//!
//! The storefront (uniforms, patches), equipment rentals with a troop-set fee,
//! sliding-scale pricing across the storefront, and free/comp sales for
//! commanders and above were specified by the owner and have no SPEC section
//! yet. Where they live — an extension of this plugin or a new `store` plugin —
//! is not decided, so nothing here anticipates it. Refunds are also absent
//! (SPEC §7.13 does not ask for them): a refund is an *expense* in finance's
//! vocabulary and belongs to finance's routes.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// Dues collection (SPEC §7.13). The payment is filed in finance's reserved
/// `dues` category so it counts toward the member's standing.
pub const PURPOSE_DUES: &str = "dues";
/// A fundraising donation.
pub const PURPOSE_DONATION: &str = "donation";
/// An event fee.
pub const PURPOSE_EVENT_FEE: &str = "event_fee";

/// The three purposes a Checkout session may have (SPEC §7.13), and the closed
/// set the database constrains.
pub const PURPOSES: [&str; 3] = [PURPOSE_DUES, PURPOSE_DONATION, PURPOSE_EVENT_FEE];

/// The ledger category a dues payment is filed under. `dues` is *finance's*
/// reserved category: its dues-standing view sums transactions with
/// `category = 'dues'` by member and fiscal year, so this string is a contract
/// with another plugin, not a label.
pub const CATEGORY_DUES: &str = "dues";
/// The ledger category a donation is filed under.
pub const CATEGORY_DONATION: &str = "donation";
/// The ledger category an event fee is filed under.
pub const CATEGORY_EVENT_FEE: &str = "event_fee";

/// A Checkout session this plugin has recorded but not yet sent to Stripe.
pub const SESSION_PENDING: &str = "pending";
/// Stripe answered and the session exists.
pub const SESSION_CREATED: &str = "created";
/// Stripe confirmed the session was paid.
pub const SESSION_COMPLETED: &str = "completed";
/// The session was abandoned (recorded when a confirmation names it as such).
pub const SESSION_EXPIRED: &str = "expired";
/// Stripe refused the creation, or could not be reached.
pub const SESSION_FAILED: &str = "failed";

/// Every session status, constrained in the database.
pub const SESSION_STATUSES: [&str; 5] = [
    SESSION_PENDING,
    SESSION_CREATED,
    SESSION_COMPLETED,
    SESSION_EXPIRED,
    SESSION_FAILED,
];

/// A confirmed payment this plugin has not seen a ledger entry for, and whose
/// booking no intent carries: the pre-existing case (a row written before the
/// outbox existed, or a payment whose fund could not be resolved at enqueue
/// time). This is the worklist's remaining job — `POST
/// /api/stripe/payment/{id}/book` as a caller holding `finance:write`.
pub const LEDGER_UNBOOKED: &str = "unbooked";
/// An outbox intent is enqueued and the core's relay will deliver it to finance.
/// **Neither booked nor unbooked**: the fact and the intent committed together,
/// the ledger entry does not exist yet, and the relay's answer — read from the
/// intent's own durable state — decides what happens next.
pub const LEDGER_INTENT_ENQUEUED: &str = "intent_enqueued";
/// Confirmed, and handed to finance through `payment.received`. **Finance's
/// answer is unknown** — see the module docs. The usual outcome is that
/// finance's subscriber books it within the same second; this plugin cannot see
/// that, and does not claim it. Published only when no intent could be composed.
pub const LEDGER_DELEGATED_EVENT: &str = "delegated_event";
/// Finance answered `2xx` — to a booking **made as the caller**, or to a
/// delivery of this payment's outbox intent. Either way the ledger entry is
/// confirmed to exist.
pub const LEDGER_BOOKED: &str = "booked";
/// Finance answered a non-2xx to a booking made as the caller, or to a delivery
/// of the payment's intent. Its own message is recorded verbatim in
/// `ledger_error`. A repeat of its `external_ref` uniqueness check lands here
/// too, and that is informative, not a bug.
pub const LEDGER_REFUSED: &str = "refused";
/// The booking could not be attempted or did not complete as a call (no
/// credential to forward, or the host HTTP call failed) — or the relay spent
/// every attempt on the payment's intent without it landing. The money path's
/// visible failure: the fact is real and the ledger entry is not there.
pub const LEDGER_FAILED: &str = "failed";

/// Every ledger status, constrained in the database (migration 2 replaced the
/// check when `intent_enqueued` was added).
pub const LEDGER_STATUSES: [&str; 6] = [
    LEDGER_UNBOOKED,
    LEDGER_INTENT_ENQUEUED,
    LEDGER_DELEGATED_EVENT,
    LEDGER_BOOKED,
    LEDGER_REFUSED,
    LEDGER_FAILED,
];

/// The mechanism recorded when a payment was handed to finance as an event.
pub const MECHANISM_EVENT: &str = "event:payment.received";
/// The mechanism recorded when a payment was booked as the caller.
pub const MECHANISM_CALLER_FORWARD: &str = "caller-forward:POST /api/finance/transaction";
/// The mechanism recorded when the payment's ledger booking is an outbox intent.
pub const MECHANISM_OUTBOX: &str = "outbox:POST /api/finance/transaction";

/// The service principal the core declares for this producer, and the only
/// identity a delivery as this plugin can carry. **Not a credential this plugin
/// holds**: the core builds it from the intent row
/// (`core::outbox::identity_for`), and this plugin cannot name, mint or widen
/// it. Named here so the intent it enqueues and the core's declaration can be
/// checked against each other in a test.
pub const LEDGER_PRINCIPAL: &str = "svc.stripe.ledger";

/// The prefix of the core's outbox outcome events this plugin subscribes to, to
/// settle its own `ledger_status` from the relay's terminal answer.
pub const OUTBOX_EVENT_PREFIX: &str = "core.outbox.";

// The relay's intent states, as this plugin reads them (mirrors
// `adjutant_server::outbox`, which is core code this plugin cannot import).
// Read through `core.outbox_producer_view()`: its answer is the durable record,
// where the `core.outbox.*` events are only a notification.
/// The target answered 2xx: the ledger entry is confirmed. One intent, one
/// delivery.
pub const OUTBOX_DELIVERED: &str = "delivered";
/// The target — or its gate — said no. Terminal.
pub const OUTBOX_REFUSED: &str = "refused";
/// Every attempt was spent and the intent still did not land: the money path's
/// visible failure.
pub const OUTBOX_EXHAUSTED: &str = "exhausted";

/// `stripe:read` — see the troop's checkout sessions. A caller without
/// [`PERM_READ_ALL`] sees only the sessions they opened or that name them.
pub const PERM_READ: &str = "stripe:read";
/// `stripe:read_all` — see every session and every payment, with its ledger
/// status. The treasurer's window.
pub const PERM_READ_ALL: &str = "stripe:read_all";
/// `stripe:checkout` — open a Checkout session.
pub const PERM_CHECKOUT: &str = "stripe:checkout";
/// `stripe:manage` — open a session on somebody else's behalf, and ask finance
/// to book a confirmed payment **as the caller** (which additionally needs the
/// caller's own `finance:write`; this plugin cannot supply it).
pub const PERM_MANAGE: &str = "stripe:manage";

/// Stripe's API root.
pub const DEFAULT_API_BASE: &str = "https://api.stripe.com";
/// This Adjutant instance, for the plugin-to-plugin call. The same default
/// `mcp` uses (the core's own bind).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8787";
/// ISO-4217, lowercase as Stripe wants it.
pub const DEFAULT_CURRENCY: &str = "cad";
/// How old a signed webhook may be. Stripe's own recommendation, and the
/// replay window this plugin enforces.
pub const DEFAULT_TOLERANCE_SECONDS: i64 = 300;
/// A ceiling on the tolerance: a year-long window is not a signature check.
pub const MAX_TOLERANCE_SECONDS: i64 = 3600;
/// How long a confirmed payment may sit unbooked before the sweep notices.
pub const DEFAULT_UNBOOKED_AFTER_MINUTES: i64 = 30;
/// The most rows one page returns.
pub const MAX_PAGE: i64 = 200;
/// The default page size.
pub const DEFAULT_PAGE: i64 = 50;
/// The most payments one sweep notification names.
pub const SWEEP_LIMIT: i64 = 50;

/// Finance's funds route — read (as the caller) to turn a fund *code* into the
/// fund *id* finance's transaction route wants. A configured id would be a
/// stale copy of another plugin's primary key (`plugin-to-plugin.md` §3.5).
pub const FINANCE_FUNDS_PATH: &str = "/api/finance/funds";
/// Finance's route that writes one ledger entry.
pub const FINANCE_TRANSACTION_PATH: &str = "/api/finance/transaction";

/// The prefix of the `client_reference_id` this plugin sends to Stripe, from
/// which a webhook finds the session row it came from.
pub const CLIENT_REFERENCE_PREFIX: &str = "stripe-session-";

// Config keys, named as constants so a typo is a compile error rather than a
// silently ignored setting.
/// `secret_key` — Stripe's API key.
pub const CONFIG_SECRET_KEY: &str = "secret_key";
/// `webhook_secret` — the endpoint's signing secret (`whsec_…`).
pub const CONFIG_WEBHOOK_SECRET: &str = "webhook_secret";
/// `api_base` — Stripe's API root.
pub const CONFIG_API_BASE: &str = "api_base";
/// `base_url` — this Adjutant instance.
pub const CONFIG_BASE_URL: &str = "base_url";
/// `currency` — ISO-4217 code for the sessions this plugin opens.
pub const CONFIG_CURRENCY: &str = "currency";
/// `success_url` — where Stripe returns a payer by default.
pub const CONFIG_SUCCESS_URL: &str = "success_url";
/// `cancel_url` — where Stripe returns a payer who backs out.
pub const CONFIG_CANCEL_URL: &str = "cancel_url";
/// `webhook_tolerance_seconds` — the replay window.
pub const CONFIG_TOLERANCE: &str = "webhook_tolerance_seconds";
/// `dues_fund_code` — finance's fund for dues.
pub const CONFIG_DUES_FUND: &str = "dues_fund_code";
/// `donation_fund_code` — finance's fund for donations.
pub const CONFIG_DONATION_FUND: &str = "donation_fund_code";
/// `event_fund_code` — finance's fund for event fees.
pub const CONFIG_EVENT_FUND: &str = "event_fund_code";
/// `unbooked_after_minutes` — the sweep's threshold.
pub const CONFIG_UNBOOKED_AFTER: &str = "unbooked_after_minutes";

/// The header Stripe signs with.
pub const SIGNATURE_HEADER: &str = "stripe-signature";
/// The signature scheme this plugin verifies. `v0` is Stripe's retired scheme
/// and is not accepted: accepting an unversioned MAC would be a downgrade.
pub const SIGNATURE_VERSION: &str = "v1";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The plugin's `core.plugins.config` block.
///
/// The two secrets live here because this *is* the sanctioned place for a
/// plugin's own credentials — `core.plugins.config`, handed to the plugin as
/// `ctx.config` and never returned by any API route (unlike
/// `core.plugins.db_secret`, which exists precisely because the isolation
/// password must not reach the plugin it confines).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StripeConfig {
    /// Stripe's API key. Never logged, never returned.
    #[serde(default)]
    pub secret_key: Option<String>,
    /// The webhook endpoint's signing secret. Never logged, never returned.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Stripe's API root (default [`DEFAULT_API_BASE`]).
    #[serde(default)]
    pub api_base: Option<String>,
    /// This Adjutant instance (default [`DEFAULT_BASE_URL`]).
    #[serde(default)]
    pub base_url: Option<String>,
    /// ISO-4217 code for the sessions this plugin opens.
    #[serde(default)]
    pub currency: Option<String>,
    /// Default `success_url` for a Checkout session.
    #[serde(default)]
    pub success_url: Option<String>,
    /// Default `cancel_url` for a Checkout session.
    #[serde(default)]
    pub cancel_url: Option<String>,
    /// The replay window a signed webhook must fall inside.
    #[serde(default)]
    pub webhook_tolerance_seconds: Option<i64>,
    /// Finance's fund for dues.
    #[serde(default)]
    pub dues_fund_code: Option<String>,
    /// Finance's fund for donations.
    #[serde(default)]
    pub donation_fund_code: Option<String>,
    /// Finance's fund for event fees.
    #[serde(default)]
    pub event_fund_code: Option<String>,
    /// How long a confirmed payment may sit unbooked before the sweep notices.
    #[serde(default)]
    pub unbooked_after_minutes: Option<i64>,
}

/// A trimmed, non-blank setting, or `None`.
fn setting(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

impl StripeConfig {
    /// Parse the plugin config.
    ///
    /// A malformed block is reported through `warnings` and the defaults are
    /// used — a typo must not make the plugin refuse to load. The warning names
    /// the line and column, **never serde's own message**: a deserialization
    /// error can quote the offending value (`invalid type: string "sk_…"`), and
    /// one of these fields is a live secret key. Line and column are enough to
    /// find the field.
    pub fn from_value(value: &Value) -> (Self, Vec<String>) {
        if value.is_null() {
            return (Self::default(), Vec::new());
        }
        match serde_json::from_value::<StripeConfig>(value.clone()) {
            Ok(cfg) => (cfg, Vec::new()),
            Err(e) => {
                let hint = format!(
                    "ignoring unusable stripe config (a field has the wrong type, at line {} \
                     column {}); using defaults — every setting is a string except \
                     webhook_tolerance_seconds and unbooked_after_minutes, which are numbers",
                    e.line(),
                    e.column()
                );
                (Self::default(), vec![hint])
            }
        }
    }

    /// Stripe's API key, if configured.
    pub fn secret_key(&self) -> Option<String> {
        setting(&self.secret_key)
    }

    /// The endpoint's signing secret, if configured.
    pub fn webhook_secret(&self) -> Option<String> {
        setting(&self.webhook_secret)
    }

    /// Stripe's API root, without a trailing slash.
    pub fn api_base(&self) -> String {
        setting(&self.api_base)
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string()
    }

    /// This Adjutant instance, without a trailing slash.
    pub fn base_url(&self) -> String {
        setting(&self.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string()
    }

    /// The currency sessions are opened in, lowercased.
    pub fn currency(&self) -> String {
        setting(&self.currency)
            .unwrap_or_else(|| DEFAULT_CURRENCY.to_string())
            .to_ascii_lowercase()
    }

    /// The replay window, clamped to a sane range.
    pub fn tolerance_seconds(&self) -> i64 {
        self.webhook_tolerance_seconds
            .filter(|s| *s > 0 && *s <= MAX_TOLERANCE_SECONDS)
            .unwrap_or(DEFAULT_TOLERANCE_SECONDS)
    }

    /// How long an unbooked payment may sit before the sweep notices.
    pub fn unbooked_after_minutes(&self) -> i64 {
        self.unbooked_after_minutes
            .filter(|m| *m > 0 && *m <= 24 * 30)
            .unwrap_or(DEFAULT_UNBOOKED_AFTER_MINUTES)
    }

    /// The default `success_url`, if configured.
    pub fn success_url(&self) -> Option<String> {
        setting(&self.success_url)
    }

    /// The default `cancel_url`, if configured.
    pub fn cancel_url(&self) -> Option<String> {
        setting(&self.cancel_url)
    }

    /// Which of Stripe's key *kinds* is configured. One bit, and it says
    /// whether this deployment is pointed at real money — worth knowing, and it
    /// reveals nothing that attempting a payment would not.
    pub fn key_mode(&self) -> &'static str {
        match self.secret_key().as_deref() {
            Some(key) if key.starts_with("sk_live_") || key.starts_with("rk_live_") => "live",
            Some(key) if key.starts_with("sk_test_") || key.starts_with("rk_test_") => "test",
            Some(_) => "unknown",
            None => "unconfigured",
        }
    }

    /// Finance's fund code for a purpose. Donations and event fees default to
    /// the dues fund's code rather than a hard-coded `general`, so a troop that
    /// points dues at its own fund does not have donations land elsewhere by
    /// surprise.
    pub fn fund_code_for(&self, purpose: &str) -> String {
        let dues = setting(&self.dues_fund_code).unwrap_or_else(|| "general".to_string());
        match purpose {
            PURPOSE_DUES => dues,
            PURPOSE_DONATION => setting(&self.donation_fund_code).unwrap_or_else(|| dues.clone()),
            PURPOSE_EVENT_FEE => setting(&self.event_fund_code).unwrap_or(dues),
            _ => dues,
        }
    }
}

/// The ledger category a purpose is filed under: finance's vocabulary, not a
/// label. `dues` is load-bearing (it drives a member's dues standing), which is
/// why the mapping is code rather than config.
pub fn category_for(purpose: &str) -> &'static str {
    match purpose {
        PURPOSE_DUES => CATEGORY_DUES,
        PURPOSE_EVENT_FEE => CATEGORY_EVENT_FEE,
        _ => CATEGORY_DONATION,
    }
}

/// A purpose from a caller or from Stripe metadata, or the conservative
/// default. An unrecognized purpose is a **donation**: money into the general
/// fund, never mis-filed as dues against somebody's standing.
pub fn normalize_purpose(raw: &str) -> String {
    let purpose = raw.trim().to_ascii_lowercase();
    if PURPOSES.contains(&purpose.as_str()) {
        purpose
    } else {
        PURPOSE_DONATION.to_string()
    }
}

// ---------------------------------------------------------------------------
// Webhook signature verification (pure CPU — HMAC over bytes)
// ---------------------------------------------------------------------------

/// Type alias so the HMAC is not repeated at every call site.
type HmacSha256 = Hmac<Sha256>;

/// A signature that verified: the timestamp it was signed at, and the `v1`
/// signature that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStamp {
    /// The `t=` timestamp (Unix seconds).
    pub timestamp: i64,
    /// The `v1=` value that matched. A MAC over this payload — not a secret.
    pub matched: String,
}

/// Compare two byte strings without leaking where they differ.
///
/// A plain `==` on a MAC is a timing oracle. The comparison is over the hex
/// digits, so it is constant-time in the length that matches, which is the
/// property that matters here.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

/// The hex HMAC-SHA256 of `message` under `secret`.
pub fn sign(secret: &str, message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(message);
    hex::encode(mac.finalize().into_bytes())
}

/// The hex SHA-256 of a body — the digest recorded in the receipt ledger so an
/// audit can point at *this* delivery without storing a payment payload.
pub fn body_digest(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// Verify a `Stripe-Signature` header against the raw body.
///
/// Stripe signs `"{t}.{raw body}"` with the endpoint's signing secret and sends
/// `Stripe-Signature: t=…,v1=…`. This checks the scheme, the timestamp window
/// and the MAC, and accepts when **any** `v1` matches — Stripe sends several
/// while a secret is being rotated, and refusing the valid one because an old
/// one did not match would break a rotation.
///
/// Errors are the reason a delivery was refused, and never include the secret
/// or the body.
pub fn verify_signature(
    header: Option<&str>,
    body: &[u8],
    secret: &str,
    tolerance_seconds: i64,
    now: i64,
) -> Result<SignatureStamp, String> {
    if secret.trim().is_empty() {
        return Err("no webhook signing secret is configured".to_string());
    }
    let Some(header) = header.map(str::trim).filter(|h| !h.is_empty()) else {
        return Err(format!("no {SIGNATURE_HEADER} header"));
    };

    let mut timestamp: Option<i64> = None;
    let mut candidates: Vec<String> = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => {
                timestamp = Some(value.trim().parse::<i64>().map_err(|_| {
                    "the signature header's timestamp is not a number".to_string()
                })?);
            }
            version if version == SIGNATURE_VERSION => {
                let value = value.trim();
                if !value.is_empty() {
                    candidates.push(value.to_string());
                }
            }
            _ => {}
        }
    }

    let Some(timestamp) = timestamp else {
        return Err("the signature header has no timestamp".to_string());
    };
    if candidates.is_empty() {
        return Err(format!(
            "the signature header has no {SIGNATURE_VERSION} signature"
        ));
    }
    // Both directions: a delivery from the future is as suspect as an old one,
    // and a clock skew of hours is a misconfiguration worth refusing.
    let age = now.saturating_sub(timestamp).abs();
    if age > tolerance_seconds {
        return Err(format!(
            "the signature is {age}s outside the {tolerance_seconds}s tolerance"
        ));
    }

    let mut signed = format!("{timestamp}.").into_bytes();
    signed.extend_from_slice(body);
    let expected = sign(secret, &signed);

    for candidate in &candidates {
        // Case-insensitively: the value is hex, and a producer's casing is not
        // a signature property.
        if constant_time_eq(
            expected.as_bytes(),
            candidate.to_ascii_lowercase().as_bytes(),
        ) {
            return Ok(SignatureStamp {
                timestamp,
                matched: candidate.clone(),
            });
        }
    }
    Err(format!(
        "no {SIGNATURE_VERSION} signature matches the payload"
    ))
}

// ---------------------------------------------------------------------------
// What Stripe sends (pure — unit-tested below)
// ---------------------------------------------------------------------------

/// The envelope of a webhook delivery: what it is and when.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEnvelope {
    /// Stripe's `evt_…` id — the receipt key.
    pub event_id: String,
    /// `checkout.session.completed`, `payment_intent.succeeded`, …
    pub event_type: String,
    /// Whether this event happened in live mode.
    pub livemode: bool,
    /// The API version Stripe rendered the payload at.
    pub api_version: String,
    /// The event's `created` (Unix seconds), when present.
    pub created: Option<i64>,
}

/// A payment read out of an event — the facts this plugin records and hands to
/// finance.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentFact {
    /// Stripe's `pi_…`: this plugin's payment key and finance's `external_ref`.
    pub payment_id: String,
    /// The amount in cents, as Stripe reports it. An integer, never a float.
    pub amount_cents: i64,
    /// Lowercase ISO-4217.
    pub currency: String,
    /// The Checkout session this came from (`cs_…`), or empty.
    pub session_id: String,
    /// This plugin's `stripe.checkout_sessions.id`, when the reference parses.
    pub session_ref: Option<i64>,
    /// `dues`, `donation`, or `event_fee`.
    pub purpose: String,
    /// The member the payment is for, when the metadata names one.
    pub member_id: String,
    /// The fund code the metadata named, if any.
    pub fund_code: Option<String>,
    /// The category the metadata named, if any.
    pub category: Option<String>,
    /// A human description.
    pub description: String,
    /// The fiscal year the metadata named, if any.
    pub dues_year: Option<i32>,
    /// The calendar event the fee was for, if any.
    pub related_event_id: String,
}

/// What a delivery means to this plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookOutcome {
    /// A payment was confirmed and can be recorded.
    Payment(Box<PaymentFact>),
    /// A delivery that carries no payment this plugin books.
    Nothing,
    /// A payment-shaped delivery this plugin could not read. Recorded and
    /// audited, never guessed at.
    Unusable(String),
}

/// Read the envelope. An event with no `id` is an error: without it the
/// delivery cannot be made idempotent, which is the one property that must hold.
pub fn webhook_envelope(payload: &Value) -> Result<WebhookEnvelope, String> {
    let event_id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "the payload has no event id".to_string())?
        .to_string();
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown")
        .to_string();
    Ok(WebhookEnvelope {
        event_id,
        event_type,
        livemode: payload
            .get("livemode")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        api_version: payload
            .get("api_version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created: payload.get("created").and_then(Value::as_i64),
    })
}

/// A metadata value, trimmed.
fn meta(object: &Value, key: &str) -> Option<String> {
    object
        .get("metadata")
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The `stripe.checkout_sessions.id` from a `client_reference_id`.
pub fn session_ref_from_reference(reference: &str) -> Option<i64> {
    reference
        .trim()
        .strip_prefix(CLIENT_REFERENCE_PREFIX)?
        .parse::<i64>()
        .ok()
}

/// Classify a delivery.
///
/// Two shapes confirm a payment, and both are accepted because Stripe sends
/// both to one endpoint: `checkout.session.completed` (the session, whose
/// `payment_intent` is the payment) and `payment_intent.succeeded` (the payment
/// itself). Both are keyed on the `pi_…` at the end, so whichever arrives first
/// records the payment and the other is a no-op.
pub fn classify_webhook(payload: &Value, event_type: &str) -> WebhookOutcome {
    let object = payload.get("data").and_then(|d| d.get("object"));
    match event_type {
        "payment_intent.succeeded" => {
            let Some(object) = object else {
                return WebhookOutcome::Unusable("the event has no data.object".to_string());
            };
            let payment_id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if payment_id.is_empty() {
                return WebhookOutcome::Unusable("the payment intent has no id".to_string());
            }
            // `amount_received` is what actually settled; `amount` is what was
            // asked for. Prefer the settled number.
            let amount_cents = object
                .get("amount_received")
                .and_then(Value::as_i64)
                .or_else(|| object.get("amount").and_then(Value::as_i64))
                .unwrap_or(0);
            if amount_cents <= 0 {
                return WebhookOutcome::Unusable(format!(
                    "payment intent {payment_id} reports an amount of {amount_cents} cents"
                ));
            }
            let reference = meta(object, "session_ref").unwrap_or_default();
            WebhookOutcome::Payment(Box::new(PaymentFact {
                payment_id,
                amount_cents,
                currency: object
                    .get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_CURRENCY)
                    .to_ascii_lowercase(),
                session_id: meta(object, "stripe_session_id").unwrap_or_default(),
                session_ref: session_ref_from_reference(&reference),
                purpose: normalize_purpose(&meta(object, "purpose").unwrap_or_default()),
                member_id: meta(object, "member_id").unwrap_or_default(),
                fund_code: meta(object, "fund_code"),
                category: meta(object, "category"),
                description: meta(object, "description").unwrap_or_default(),
                dues_year: meta(object, "dues_year").and_then(|y| y.parse::<i32>().ok()),
                related_event_id: meta(object, "related_event_id").unwrap_or_default(),
            }))
        }
        "checkout.session.completed" => {
            let Some(object) = object else {
                return WebhookOutcome::Unusable("the event has no data.object".to_string());
            };
            // A session that was not paid (an async method still pending, or a
            // zero-total session) is not a payment. Refusing to guess is the
            // point: Stripe sends `completed` for both.
            let paid = object
                .get("payment_status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if paid != "paid" {
                return WebhookOutcome::Nothing;
            }
            let payment_id = object
                .get("payment_intent")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if payment_id.is_empty() {
                return WebhookOutcome::Unusable(
                    "the completed session names no payment intent".to_string(),
                );
            }
            let amount_cents = object
                .get("amount_total")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if amount_cents <= 0 {
                return WebhookOutcome::Unusable(format!(
                    "session {} reports a total of {amount_cents} cents",
                    object.get("id").and_then(Value::as_str).unwrap_or("?")
                ));
            }
            let reference = object
                .get("client_reference_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            WebhookOutcome::Payment(Box::new(PaymentFact {
                payment_id,
                amount_cents,
                currency: object
                    .get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_CURRENCY)
                    .to_ascii_lowercase(),
                session_id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                session_ref: session_ref_from_reference(&reference),
                purpose: normalize_purpose(&meta(object, "purpose").unwrap_or_default()),
                member_id: meta(object, "member_id").unwrap_or_default(),
                fund_code: meta(object, "fund_code"),
                category: meta(object, "category"),
                description: meta(object, "description").unwrap_or_default(),
                dues_year: meta(object, "dues_year").and_then(|y| y.parse::<i32>().ok()),
                related_event_id: meta(object, "related_event_id").unwrap_or_default(),
            }))
        }
        _ => WebhookOutcome::Nothing,
    }
}

// ---------------------------------------------------------------------------
// Stripe Checkout: request building (pure)
// ---------------------------------------------------------------------------

/// Percent-encode for an `application/x-www-form-urlencoded` body.
///
/// `[` and `]` are left literal: Stripe's own examples write bracket paths
/// (`line_items[0][price_data][currency]`) unescaped, and the form stays
/// readable in a log. Everything else outside the unreserved set is escaped, so
/// a caller-supplied value can never inject a form field.
pub fn form_encode_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'[' | b']' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A form body from ordered pairs.
pub fn form_encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode_value(key), form_encode_value(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The facts a Checkout session is opened with.
#[derive(Debug, Clone)]
pub struct CheckoutRequest {
    /// This plugin's row id — the `client_reference_id`.
    pub row_id: i64,
    /// `dues`, `donation`, or `event_fee`.
    pub purpose: String,
    /// The amount, in cents. Always positive.
    pub amount_cents: i64,
    /// Lowercase ISO-4217.
    pub currency: String,
    /// What the payer sees on the line item.
    pub label: String,
    /// The member the payment is for, when known.
    pub member_id: String,
    /// The fund the ledger entry belongs in (a finance fund **code**).
    pub fund_code: String,
    /// The ledger category.
    pub category: String,
    /// The related calendar event, when the fee is for one.
    pub related_event_id: String,
    /// The fiscal year the payment belongs to, when stated.
    pub dues_year: Option<i32>,
    /// Where Stripe returns a payer.
    pub success_url: String,
    /// Where Stripe returns a payer who backs out.
    pub cancel_url: String,
}

/// The `metadata` pairs a session carries, so the facts survive onto the
/// PaymentIntent and back through the webhook.
pub fn checkout_metadata(request: &CheckoutRequest) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("purpose".to_string(), request.purpose.clone()),
        ("fund_code".to_string(), request.fund_code.clone()),
        ("category".to_string(), request.category.clone()),
        (
            "session_ref".to_string(),
            format!("{CLIENT_REFERENCE_PREFIX}{}", request.row_id),
        ),
    ];
    for (key, value) in [
        ("member_id", &request.member_id),
        ("description", &request.label),
        ("related_event_id", &request.related_event_id),
    ] {
        if !value.trim().is_empty() {
            pairs.push((key.to_string(), value.clone()));
        }
    }
    if let Some(year) = request.dues_year {
        pairs.push(("dues_year".to_string(), year.to_string()));
    }
    pairs
}

/// The form body for `POST /v1/checkout/sessions`.
///
/// The metadata is emitted twice — on the session and on its PaymentIntent —
/// because either event shape can be the one Stripe delivers first, and a
/// payment whose metadata only exists on the session would arrive at the ledger
/// as an anonymous donation.
pub fn checkout_form(request: &CheckoutRequest) -> Vec<(String, String)> {
    let mut form: Vec<(String, String)> = vec![
        ("mode".to_string(), "payment".to_string()),
        ("success_url".to_string(), request.success_url.clone()),
        ("cancel_url".to_string(), request.cancel_url.clone()),
        (
            "client_reference_id".to_string(),
            format!("{CLIENT_REFERENCE_PREFIX}{}", request.row_id),
        ),
        ("line_items[0][quantity]".to_string(), "1".to_string()),
        (
            "line_items[0][price_data][currency]".to_string(),
            request.currency.clone(),
        ),
        (
            "line_items[0][price_data][unit_amount]".to_string(),
            request.amount_cents.to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]".to_string(),
            request.label.clone(),
        ),
    ];
    for (key, value) in checkout_metadata(request) {
        form.push((format!("metadata[{key}]"), value.clone()));
        form.push((format!("payment_intent_data[metadata][{key}]"), value));
    }
    form
}

/// The label a payer sees: what they are paying for, and for whom.
pub fn checkout_label(purpose: &str, dues_year: Option<i32>, member_id: &str) -> String {
    let what = match purpose {
        PURPOSE_DUES => match dues_year {
            Some(year) => format!("Troop dues {year}"),
            None => "Troop dues".to_string(),
        },
        PURPOSE_EVENT_FEE => "Event fee".to_string(),
        _ => "Donation to the troop".to_string(),
    };
    if member_id.trim().is_empty() {
        what
    } else {
        format!("{what} (member {member_id})")
    }
}

/// Parse a human dollar amount into cents, **exactly**.
///
/// Accepts `"25"`, `"25.5"`, `"25.00"`, `"$1,234.56"`. Refuses a value finer
/// than a cent (`"25.005"`), a trailing decimal point, a negative amount (a
/// Checkout session charges; it does not refund) and anything not a number. A
/// JSON *number* never reaches here — the field is a string, so `serde` refuses
/// a float before this is called, and no amount in this plugin is ever an
/// `f64`.
pub fn parse_amount_to_cents(raw: &str) -> Result<i64, String> {
    let cleaned = raw.trim();
    let digits = cleaned.strip_prefix('$').unwrap_or(cleaned).replace(',', "");
    let digits = digits.trim();
    if digits.is_empty() {
        return Err(format!("{raw:?} is not an amount"));
    }
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    if whole.is_empty() && fraction.is_none() {
        return Err(format!("{raw:?} is not an amount"));
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{raw:?} is not an amount (give a positive number of dollars, like \"25.00\")"
        ));
    }
    let whole_cents = whole
        .parse::<i64>()
        .map_err(|_| format!("{raw:?} is too large to be money"))?
        .checked_mul(100)
        .ok_or_else(|| format!("{raw:?} is too large to be money"))?;
    let fraction_cents = match fraction {
        None => 0,
        Some("") => return Err(format!("{raw:?} ends in a decimal point")),
        Some(fraction) if fraction.len() > 2 => {
            return Err(format!(
                "{raw:?}: a cent is the smallest unit (use amount_cents for an exact integer)"
            ));
        }
        Some(fraction) if !fraction.bytes().all(|b| b.is_ascii_digit()) => {
            return Err(format!("{raw:?} is not an amount"));
        }
        Some(fraction) if fraction.len() == 1 => fraction
            .parse::<i64>()
            .map_err(|_| format!("{raw:?} is not an amount"))?
            * 10,
        Some(fraction) => fraction
            .parse::<i64>()
            .map_err(|_| format!("{raw:?} is not an amount"))?,
    };
    whole_cents
        .checked_add(fraction_cents)
        .ok_or_else(|| format!("{raw:?} is too large to be money"))
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CheckoutBody {
    purpose: String,
    #[serde(default)]
    amount_cents: Option<i64>,
    /// A dollars string (`"25.00"`). A JSON number here is refused by `serde`.
    #[serde(default)]
    amount: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    member_id: Option<String>,
    #[serde(default)]
    fund_code: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    related_event_id: Option<String>,
    #[serde(default)]
    dues_year: Option<i32>,
    #[serde(default)]
    success_url: Option<String>,
    #[serde(default)]
    cancel_url: Option<String>,
}

/// A trimmed, non-blank string, or `None`.
fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Refuse two spellings of the same amount, and resolve one to cents.
fn resolve_amount(cents: Option<i64>, dollars: &Option<String>) -> Result<i64, String> {
    match (cents, trimmed(dollars)) {
        (Some(_), Some(_)) => Err(
            "give amount_cents or amount, not both (the amount would be ambiguous)".to_string(),
        ),
        (Some(cents), None) => Ok(cents),
        (None, Some(dollars)) => parse_amount_to_cents(&dollars),
        (None, None) => Err("an amount is required (amount_cents, or amount as a dollar string)"
            .to_string()),
    }
}

// ---------------------------------------------------------------------------
// The caller's credential, forwarded (the §2(b) mechanism)
// ---------------------------------------------------------------------------

/// The headers relayed to another plugin: **the caller's own credentials, and
/// nothing else.**
///
/// This is `mcp`'s [`forward_headers`] discipline (`plugins/mcp/src/lib.rs`,
/// the reference implementation named in `plugin-to-plugin.md`), restated here
/// because this plugin must not be a second, weaker copy of it: the two names
/// are the only ones this plugin ever puts on a cross-plugin request, no
/// credential is minted, substituted or upgraded, and a request that carries
/// neither header has nothing to forward.
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

/// What finance answered to a booking attempt.
#[derive(Debug, Clone)]
struct LedgerOutcome {
    /// One of [`LEDGER_BOOKED`], [`LEDGER_REFUSED`], [`LEDGER_FAILED`].
    status: &'static str,
    /// [`MECHANISM_CALLER_FORWARD`] whenever the call was made.
    mechanism: &'static str,
    /// Finance's HTTP status, when there was one.
    http_status: Option<u16>,
    /// Finance's transaction id, on success.
    transaction_id: Option<String>,
    /// Why it was refused, in finance's own words (or the transport's).
    error: Option<String>,
    /// The status the booking route answers with: finance's own when the *call*
    /// was refused or failed, and this plugin's when the problem is a fact
    /// (a fund finance does not have) rather than a failed request.
    answer_status: u16,
}

/// The status a booking route answers with, given finance's.
///
/// Passing finance's own answer through is the point: a `403` means this
/// caller's `finance:write` did not reach what they asked for, and a `409` is
/// finance's own refusal — neither is a failure *of this plugin*, and flattening
/// both into a `500` would hide which authority was missing.
fn pass_through_status(status: u16) -> u16 {
    match status {
        400..=409 => status,
        _ => 502,
    }
}

/// The `error` string out of finance's (or Stripe's) error body, or a plain
/// statement of the status.
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

/// What `GET /api/finance/funds` answered.
///
/// Shared by the two places this plugin needs finance's fund list: the caller's
/// booking (`POST /api/stripe/payment/{id}/book`) and the **enqueue-time**
/// resolution the webhook does so its intent payload can be complete. Neither
/// call carries a credential of this plugin's own — neither invents one.
enum FundsRead {
    /// The list, and the status it came back with.
    Listed { status: u16, funds: Vec<Value> },
    /// finance refused the read, in its own words.
    Refused { status: u16, message: String },
    /// The call did not complete.
    Unreachable(String),
}

/// Read finance's funds, forwarding exactly the headers given: the caller's
/// credential where there is a caller, and nothing at all where there is not —
/// finance's own gate decides either way.
async fn read_funds(
    c: &PluginContext,
    cfg: &StripeConfig,
    headers: &[(String, String)],
) -> FundsRead {
    let url = format!("{}{FINANCE_FUNDS_PATH}?include_inactive=1", cfg.base_url());
    match c
        .http
        .request("GET".to_string(), url, headers.to_vec(), None)
        .await
    {
        Ok(response) if (200..300).contains(&response.status) => FundsRead::Listed {
            status: response.status,
            funds: response
                .json::<Value>()
                .ok()
                .and_then(|value| value.get("funds").and_then(Value::as_array).cloned())
                .unwrap_or_default(),
        },
        Ok(response) => FundsRead::Refused {
            status: response.status,
            message: message_of(&response.body, response.status),
        },
        Err(e) => FundsRead::Unreachable(format!("could not reach finance's funds route: {e}")),
    }
}

/// The fund an intent's payload names.
///
/// finance accepts either, and the two mean the same thing to it: an id is its
/// own key, a code is the **reference** this plugin actually holds (§3.5). The id
/// is used when the read below answered; the code is what makes a complete payload
/// possible when it could not — the callerless webhook path, where there is no
/// credential to read funds with (issue #60). Both are resolved by finance inside
/// the statement that writes the entry, so neither is a guess.
#[derive(Debug, Clone)]
enum IntentFund {
    /// finance's primary key, resolved at enqueue time.
    Id(i64),
    /// The fund's code, resolved by finance at delivery.
    Code(String),
}

/// Why a fund code is not finance's fund id.
enum FundResolution {
    /// finance's primary key for the fund.
    Id(i64),
    /// No fund with that code is visible.
    Missing,
    /// The fund exists and is inactive.
    Inactive,
    /// A fund row without an id, which cannot be used as one.
    NoId,
}

/// The code → id step, and nothing else: pure, so both callers resolve a fund
/// identically. Resolved rather than configured because finance owns its ids,
/// and a configured primary key is exactly the stale copy
/// `plugin-to-plugin.md` §3.5 warns about.
fn resolve_fund(funds: &[Value], code: &str) -> FundResolution {
    let Some(fund) = funds
        .iter()
        .find(|fund| fund["code"].as_str() == Some(code))
    else {
        return FundResolution::Missing;
    };
    if fund["active"].as_bool() == Some(false) {
        return FundResolution::Inactive;
    }
    match fund["id"].as_i64() {
        Some(id) => FundResolution::Id(id),
        None => FundResolution::NoId,
    }
}

/// Resolve the fund **at enqueue time**, for a complete intent payload.
///
/// The relay cannot read-then-write at delivery, so the payload has to be
/// complete before the statement runs: finance's write route takes a fund
/// **id**, not the code the payment names. Where it cannot be resolved — no
/// credential to forward (a webhook has none), no such fund, an inactive fund,
/// finance unreachable — the caller records the payment *without* an intent and
/// says why, rather than enqueuing a payload finance would refuse. Issue #60 is
/// why a read is needed at all: finance's write paths accept only an id.
async fn fund_for_intent(
    c: &PluginContext,
    cfg: &StripeConfig,
    headers: &[(String, String)],
    fund_code: &str,
) -> Result<(IntentFund, Option<String>), String> {
    if fund_code.trim().is_empty() {
        return Err("the payment names no fund code".to_string());
    }
    match read_funds(c, cfg, headers).await {
        // The read answered, so this plugin KNOWS something about the fund and
        // acts on it: a fund finance does not have, or one that is closed, is not
        // worth an intent — a doomed instruction shows up in reconciliation as a
        // refusal, where an `unbooked` payment with a stated reason is information.
        FundsRead::Listed { funds, .. } => match resolve_fund(&funds, fund_code) {
            FundResolution::Id(id) => Ok((IntentFund::Id(id), None)),
            FundResolution::Missing => Err(format!(
                "finance has no fund with code {fund_code:?} visible to this caller, so no intent \
                 was enqueued: the booking would be refused at delivery"
            )),
            FundResolution::Inactive => {
                Err(format!("finance's fund {fund_code:?} is inactive"))
            }
            // finance answered with the fund but no usable id: the code is still
            // the reference, and finance resolves it itself.
            FundResolution::NoId => Ok((
                IntentFund::Code(fund_code.to_string()),
                Some(
                    "finance answered with the fund but no usable id, so the payload names the \
                     fund by its code and finance resolves it"
                        .to_string(),
                ),
            )),
        },
        // Nothing could be read — no credential to forward (every real webhook),
        // or finance was unreachable — so this plugin knows nothing against the
        // fund and does not pretend otherwise: the payload names the fund by its
        // **code**, which is complete, because finance resolves it inside the
        // statement that writes the entry. This is the path that makes the intent
        // real on a callerless webhook (issue #60).
        FundsRead::Refused { status, message } => Ok((
            IntentFund::Code(fund_code.to_string()),
            Some(format!(
                "finance refused the funds read (HTTP {status}): {message}; with nothing read and \
                 no credential to forward there is no id to name, so the payload names the fund by \
                 its code (issue #60) and finance resolves it where the entry is written"
            )),
        )),
        FundsRead::Unreachable(message) => Ok((
            IntentFund::Code(fund_code.to_string()),
            Some(format!(
                "{message}; the payload names the fund by its code rather than wait for an id, and \
                 finance resolves it at delivery"
            )),
        )),
    }
}

/// The intent payload: finance's `POST /api/finance/transaction` body, complete
/// and self-contained, in finance's own field names.
///
/// * `kind: "income"` and a **positive** `amount_cents` — finance takes a
///   magnitude and lets the kind carry the sign, so income is positive.
/// * `category` is finance's own vocabulary for the purpose (`dues`,
///   `donation`, `event_fee`), which is what its dues-standing view sums.
/// * `external_ref` is Stripe's payment id, so finance's unique index makes a
///   second delivery a no-op instead of a second entry.
/// * The fund is named `fund_id` when finance's key was resolved, else
///   `fund_code` — the reference this plugin holds. Never a configured copy of
///   finance's key, and never a guess: finance resolves whichever is sent inside
///   the statement that writes the entry.
fn ledger_intent_payload(fact: &PaymentFact, category: &str, fund: &IntentFund) -> Value {
    let description = match fact.description.trim() {
        "" => format!("Stripe {} {}", fact.purpose, fact.payment_id),
        text => format!("Stripe {} {} — {text}", fact.purpose, fact.payment_id),
    };
    let mut payload = json!({
        "kind": "income",
        "amount_cents": fact.amount_cents,
        "category": category,
        "member_id": fact.member_id,
        "description": description,
        "external_ref": fact.payment_id,
        // The booking is dated when the payment was confirmed: this statement
        // writes the row that sets `confirmed_at` to now().
        "occurred_on": Utc::now().date_naive().to_string(),
    });
    match fund {
        IntentFund::Id(id) => payload["fund_id"] = json!(id),
        IntentFund::Code(code) => payload["fund_code"] = json!(code),
    }
    if let Some(year) = fact.dues_year {
        payload["fiscal_year"] = json!(year);
    }
    payload
}

/// Ask finance to book a payment, **as the caller**.
///
/// Two calls, both with the caller's credential forwarded and neither with a
/// credential of this plugin's own:
///
/// 1. `GET /api/finance/funds` — resolve the fund *code* the payment names into
///    the fund *id* finance's transaction route takes. Resolved rather than
///    configured because finance owns its ids, and a configured primary key is
///    exactly the stale copy `plugin-to-plugin.md` §3.5 warns about.
/// 2. `POST /api/finance/transaction` — one income entry, `external_ref` set to
///    Stripe's payment id, so finance's unique index makes a double-booking
///    impossible even if the `payment.received` subscriber already wrote it.
///
/// Finance's gate decides both times. This plugin cannot widen what the caller
/// may do, and does not try.
async fn ask_finance_to_book(
    c: &PluginContext,
    cfg: &StripeConfig,
    headers: Vec<(String, String)>,
    payment: &Value,
) -> LedgerOutcome {
    let mechanism = MECHANISM_CALLER_FORWARD;
    let unbookable = |error: String, answer_status: u16| LedgerOutcome {
        status: LEDGER_FAILED,
        mechanism,
        http_status: None,
        transaction_id: None,
        error: Some(error),
        answer_status,
    };
    let refused = |error: String, http_status: Option<u16>, answer_status: u16| LedgerOutcome {
        status: LEDGER_REFUSED,
        mechanism,
        http_status,
        transaction_id: None,
        error: Some(error),
        answer_status,
    };

    let payment_id = payment["payment_id"].as_str().unwrap_or_default().to_string();
    let amount_cents = payment["amount_cents"].as_i64().unwrap_or(0);
    if payment_id.is_empty() || amount_cents <= 0 {
        return unbookable(
            "the payment record has no id or no positive amount, so there is nothing to book"
                .to_string(),
            409,
        );
    }
    let fund_code = payment["fund_code"].as_str().unwrap_or_default().to_string();
    if fund_code.is_empty() {
        return unbookable("the payment names no fund code".to_string(), 409);
    }
    let base = cfg.base_url();

    // 1. The fund code → id, as the caller.
    let (funds_status, funds) = match read_funds(c, cfg, &headers).await {
        FundsRead::Listed { status, funds } => (status, funds),
        FundsRead::Refused { status, message } => {
            return refused(
                format!("reading finance's funds was refused: {message}"),
                Some(status),
                pass_through_status(status),
            )
        }
        FundsRead::Unreachable(message) => return unbookable(message, 502),
    };
    let fund_id = match resolve_fund(&funds, &fund_code) {
        FundResolution::Id(id) => id,
        FundResolution::Missing => {
            return refused(
                format!("finance has no fund with code {fund_code:?} visible to this caller"),
                Some(funds_status),
                400,
            )
        }
        FundResolution::Inactive => {
            return refused(
                format!("finance's fund {fund_code:?} is inactive"),
                Some(funds_status),
                400,
            )
        }
        FundResolution::NoId => {
            return unbookable(
                format!("finance's fund {fund_code:?} came back without an id"),
                502,
            )
        }
    };

    // 2. The entry, as the caller.
    let occurred_on = payment["confirmed_at"]
        .as_str()
        .and_then(|stamp| stamp.get(0..10))
        .unwrap_or_default()
        .to_string();
    let description = match payment["description"].as_str() {
        Some(text) if !text.trim().is_empty() => {
            format!("Stripe {provider_purpose} {payment_id} — {text}", provider_purpose = payment["purpose"].as_str().unwrap_or("payment"), text = text.trim())
        }
        _ => format!(
            "Stripe {} {payment_id}",
            payment["purpose"].as_str().unwrap_or("payment")
        ),
    };
    let mut body = json!({
        "fund_id": fund_id,
        "kind": "income",
        "amount_cents": amount_cents,
        "category": payment["category"].as_str().unwrap_or_default(),
        "member_id": payment["member_id"].as_str().unwrap_or_default(),
        "description": description,
        "external_ref": payment_id,
    });
    if !occurred_on.is_empty() {
        body["occurred_on"] = json!(occurred_on);
    }
    if let Some(year) = payment["dues_year"].as_i64() {
        body["fiscal_year"] = json!(year);
    }

    let url = format!("{base}{FINANCE_TRANSACTION_PATH}");
    let payload = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    match c
        .http
        .request(
            "POST".to_string(),
            url,
            headers,
            Some(("application/json".to_string(), payload)),
        )
        .await
    {
        Ok(response) if (200..300).contains(&response.status) => {
            let transaction_id = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("transaction").and_then(|t| t.get("id")).cloned())
                .map(|id| match id {
                    Value::Number(number) => number.to_string(),
                    other => other.as_str().unwrap_or_default().to_string(),
                })
                .filter(|id| !id.is_empty());
            LedgerOutcome {
                status: LEDGER_BOOKED,
                mechanism,
                http_status: Some(response.status),
                transaction_id,
                error: None,
                answer_status: 200,
            }
        }
        Ok(response) => refused(
            message_of(&response.body, response.status),
            Some(response.status),
            pass_through_status(response.status),
        ),
        Err(e) => unbookable(format!("the booking call did not complete: {e}"), 502),
    }
}

// ---------------------------------------------------------------------------
// SQL
//
// One statement per runtime call (the host prepares each one), so every write
// below is a single statement and any multi-statement work is a migration.
// ---------------------------------------------------------------------------

/// A Checkout session as the API states it (for `RETURNING`/selection, alias `s`).
const SESSION_FIELDS: &str = r#"
    s.id, s.purpose, s.amount_cents, s.currency, s.member_id, s.fund_code, s.category,
    s.description, s.related_event_id, s.dues_year, s.status, s.stripe_session_id,
    s.checkout_url, s.created_by, s.created_at::text AS created_at,
    s.updated_at::text AS updated_at
"#;

/// A confirmed payment as the API states it (alias `p`).
const PAYMENT_FIELDS: &str = r#"
    p.id, p.payment_id, p.event_id, p.session_id, p.purpose, p.amount_cents, p.currency,
    p.member_id, p.fund_code, p.category, p.description, p.dues_year, p.livemode,
    p.ledger_status, p.ledger_mechanism, p.ledger_intent_id, p.ledger_transaction_id,
    p.ledger_error, p.confirmed_at::text AS confirmed_at,
    p.ledger_attempted_at::text AS ledger_attempted_at
"#;

/// The payment columns the sweep and the worklist need — narrower than
/// [`PAYMENT_FIELDS`] because a notification should not carry a whole record.
///
/// The `v.*` columns are the payment's intent, read through
/// `core.outbox_producer_view()`: the producer's own intents, without a grant on
/// `core.outbox`, scoped to the plugin role by the function itself. They are
/// `NULL` for a payment with no intent. Both it and `core.outbox_enqueue` are
/// core migration 9; a core older than that cannot serve this worklist (or accept
/// this plugin's enqueue), and the mechanism depends on that migration.
const SWEEP_FIELDS: &str = r#"
    p.id, p.payment_id, p.purpose, p.amount_cents, p.currency, p.member_id,
    p.ledger_status, p.ledger_mechanism, p.ledger_error, p.confirmed_at::text AS confirmed_at,
    p.ledger_intent_id, v.state AS intent_state, v.attempts AS intent_attempts,
    v.max_attempts AS intent_max_attempts, v.answer_status AS intent_answer_status,
    v.last_error AS intent_last_error, v.delivered_at::text AS intent_delivered_at,
    COUNT(*) OVER () AS total_unbooked
"#;

fn sql_insert_webhook_event(c: &PluginContext) -> String {
    format!(
        "INSERT INTO {events} AS w \
           (event_id, event_type, livemode, api_version, signature_timestamp, payload_digest) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (event_id) DO NOTHING \
         RETURNING w.id, w.redeliveries",
        events = c.db.table("webhook_events")
    )
}

fn sql_bump_redeliveries(c: &PluginContext) -> String {
    format!(
        "UPDATE {events} SET redeliveries = redeliveries + 1 WHERE event_id = $1",
        events = c.db.table("webhook_events")
    )
}

/// Record a confirmed payment, **and enqueue its ledger booking as an intent in
/// the same statement**.
///
/// That is the whole design of the money path: `core.outbox_enqueue` returns a
/// scalar, so it fits as the last expression in `VALUES`, and PostgreSQL runs
/// the statement in one implicit transaction — the payment row and its intent
/// commit together or neither does. There is no window in which a charge is
/// recorded and its booking is not, and no transaction API is added to the SDK
/// to get it (the plugin holds no `BEGIN`).
///
/// Two shapes of the *same* statement, because they bind a different number of
/// parameters:
///
/// * `intent`: `ledger_intent_id` is `core.outbox_enqueue($15, 'POST',
///   '/api/finance/transaction', $16::jsonb, $1)` — the **idempotency key is the
///   payment's own `payment_id`**, so a redelivered webhook calls it with the
///   key it already used and is handed back the intent it already has. The
///   function derives the producer from `session_user` (this plugin's own role)
///   and refuses any principal not declared for it; it takes no identity
///   parameter, so this plugin cannot enqueue as anything but itself.
/// * no intent: `ledger_intent_id` is `NULL`, for a payment whose fund could not
///   be resolved at enqueue time (the residual named in the module docs) or for
///   the pre-existing rows this plugin still books as the caller.
///
/// `ON CONFLICT (payment_id) DO NOTHING` keeps the *payment* unique. Note what
/// the `VALUES` expression means for a redelivery: PostgreSQL evaluates it
/// before the conflict is detected, so `core.outbox_enqueue` **is** called again
/// — and returns the same intent id, because the key is the same. One payment,
/// one intent, on every delivery.
fn sql_insert_payment(c: &PluginContext, intent: bool) -> String {
    let ledger_intent_id = if intent {
        // $15 the principal, $16 the payload. The route is a literal: it is a
        // contract with finance, not a request value.
        format!(
            "core.outbox_enqueue($15, 'POST', '{FINANCE_TRANSACTION_PATH}', $16::jsonb, $1)"
        )
    } else {
        "NULL".to_string()
    };
    format!(
        "INSERT INTO {payments} AS p \
           (payment_id, event_id, session_id, purpose, amount_cents, currency, member_id, \
            fund_code, category, description, dues_year, livemode, ledger_status, \
            ledger_mechanism, ledger_intent_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                 {ledger_intent_id}) \
         ON CONFLICT (payment_id) DO NOTHING \
         RETURNING {PAYMENT_FIELDS}",
        payments = c.db.table("payments")
    )
}

/// Either variant takes the same first fourteen parameters, in order:
/// `$1` payment_id, `$2` event_id, `$3` session_id, `$4` purpose, `$5`
/// amount_cents, `$6` currency, `$7` member_id, `$8` fund_code, `$9` category,
/// `$10` description, `$11` dues_year, `$12` livemode, `$13` ledger_status,
/// `$14` ledger_mechanism. The intent variant adds `$15` the principal and `$16`
/// the payload (JSONB).
///
/// Split out so the DB-backed probes can drive the statement the handler drives,
/// with the same binds, including the count that decides which variant fits.
fn payment_insert_params(
    fact: &PaymentFact,
    envelope: &WebhookEnvelope,
    fund_code: &str,
    category: &str,
    ledger_status: &str,
    ledger_mechanism: &str,
) -> Vec<SqlValue> {
    vec![
        SqlValue::Text(fact.payment_id.clone()),
        SqlValue::Text(envelope.event_id.clone()),
        SqlValue::Text(fact.session_id.clone()),
        SqlValue::Text(fact.purpose.clone()),
        SqlValue::Int(fact.amount_cents),
        SqlValue::Text(fact.currency.clone()),
        SqlValue::Text(fact.member_id.clone()),
        SqlValue::Text(fund_code.to_string()),
        SqlValue::Text(category.to_string()),
        SqlValue::Text(fact.description.clone()),
        fact.dues_year
            .map(|year| SqlValue::Int(i64::from(year)))
            .unwrap_or(SqlValue::NullInt),
        SqlValue::Bool(envelope.livemode),
        SqlValue::Text(ledger_status.to_string()),
        SqlValue::Text(ledger_mechanism.to_string()),
    ]
}

/// Settle a payment from its intent's terminal outcome — what the relay
/// publishes, read by the producer.
///
/// Guarded on `ledger_status = 'intent_enqueued'` and on the intent id, so a
/// replayed event is a no-op (one intent, one answer) and only the payment that
/// enqueued *this* intent can be settled by it.
fn sql_settle_from_intent(c: &PluginContext) -> String {
    format!(
        "UPDATE {payments} AS p \
         SET ledger_status = $2, ledger_mechanism = '{MECHANISM_OUTBOX}', \
             ledger_transaction_id = $3, ledger_error = $4, ledger_attempted_at = now() \
         WHERE p.ledger_intent_id = $1 AND p.ledger_status = '{LEDGER_INTENT_ENQUEUED}' \
         RETURNING {PAYMENT_FIELDS}",
        payments = c.db.table("payments")
    )
}

fn sql_payment_by_id(c: &PluginContext) -> String {
    format!(
        "SELECT {PAYMENT_FIELDS} FROM {payments} p WHERE p.id = $1",
        payments = c.db.table("payments")
    )
}

/// The payment a Checkout session produced, if any.
fn sql_payment_by_session_id(c: &PluginContext) -> String {
    format!(
        "SELECT {PAYMENT_FIELDS} FROM {payments} p WHERE p.session_id = $1 \
         ORDER BY p.id DESC LIMIT 1",
        payments = c.db.table("payments")
    )
}

fn sql_payment_by_payment_id(c: &PluginContext) -> String {
    format!(
        "SELECT {PAYMENT_FIELDS} FROM {payments} p WHERE p.payment_id = $1",
        payments = c.db.table("payments")
    )
}

fn sql_set_ledger_outcome(c: &PluginContext) -> String {
    format!(
        "UPDATE {payments} AS p \
         SET ledger_status = $2, ledger_mechanism = $3, ledger_transaction_id = $4, \
             ledger_error = $5, ledger_attempted_at = now() \
         WHERE p.id = $1 RETURNING {PAYMENT_FIELDS}",
        payments = c.db.table("payments")
    )
}

fn sql_insert_session(c: &PluginContext) -> String {
    format!(
        "INSERT INTO {sessions} AS s \
           (purpose, amount_cents, currency, member_id, fund_code, category, description, \
            related_event_id, dues_year, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         RETURNING {SESSION_FIELDS}",
        sessions = c.db.table("checkout_sessions")
    )
}

fn sql_set_session_created(c: &PluginContext) -> String {
    format!(
        "UPDATE {sessions} AS s \
         SET stripe_session_id = $2, checkout_url = $3, status = '{SESSION_CREATED}', \
             updated_at = now() \
         WHERE s.id = $1 RETURNING {SESSION_FIELDS}",
        sessions = c.db.table("checkout_sessions")
    )
}

/// Settle a session this plugin opened, by **its own row id** — the reference
/// the `client_reference_id` it sends Stripe names.
fn sql_set_session_status_by_row(c: &PluginContext) -> String {
    format!(
        "UPDATE {sessions} SET status = $2, updated_at = now() WHERE id = $1 AND status <> $2",
        sessions = c.db.table("checkout_sessions")
    )
}

/// Settle a session by Stripe's own id, for one this plugin did not open (a
/// session created in Stripe's dashboard still settles here by its id).
fn sql_set_session_status(c: &PluginContext) -> String {
    format!(
        "UPDATE {sessions} SET status = $2, updated_at = now() \
         WHERE stripe_session_id = $1 AND status <> $2",
        sessions = c.db.table("checkout_sessions")
    )
}

fn sql_session_by_id(c: &PluginContext) -> String {
    format!(
        "SELECT {SESSION_FIELDS} FROM {sessions} s WHERE s.id = $1",
        sessions = c.db.table("checkout_sessions")
    )
}

/// Sessions, newest first. `$1` session id, `$2` purpose, `$3` status,
/// `$4` member id, `$5` caller (narrowing; a TEXT null means "no narrowing"),
/// `$6` before_id, `$7` limit.
fn sql_list_sessions(c: &PluginContext) -> String {
    format!(
        "SELECT {SESSION_FIELDS} FROM {sessions} s \
         WHERE ($1::bigint IS NULL OR s.id = $1) \
           AND ($2::text IS NULL OR s.purpose = $2) \
           AND ($3::text IS NULL OR s.status = $3) \
           AND ($4::text IS NULL OR s.member_id = $4) \
           AND ($5::text IS NULL OR s.created_by = $5 OR s.member_id = $5) \
           AND ($6::bigint IS NULL OR s.id < $6) \
         ORDER BY s.id DESC LIMIT $7",
        sessions = c.db.table("checkout_sessions")
    )
}

/// Payments, newest first. `$1` purpose, `$2` ledger status, `$3` payment id,
/// `$4` member id, `$5` before_id, `$6` limit.
fn sql_list_payments(c: &PluginContext) -> String {
    format!(
        "SELECT {PAYMENT_FIELDS} FROM {payments} p \
         WHERE ($1::text IS NULL OR p.purpose = $1) \
           AND ($2::text IS NULL OR p.ledger_status = $2) \
           AND ($3::text IS NULL OR p.payment_id = $3) \
           AND ($4::text IS NULL OR p.member_id = $4) \
           AND ($5::bigint IS NULL OR p.id < $5) \
         ORDER BY p.id DESC LIMIT $6",
        payments = c.db.table("payments")
    )
}

/// Payments with no *confirmed* ledger entry, older than the threshold — the
/// worklist, and what each case in it means.
///
/// `$1` minutes (text, an interval), `$2` limit. The window count comes back
/// with the page so one statement answers both "which" and "how many".
///
/// The cases, spelled out because the worklist is only honest if a reader can
/// tell them apart:
///
/// * `intent_enqueued` — an intent is enqueued and the relay will deliver it.
///   **Neither booked nor unbooked**, so it is listed (not silently counted as
///   either) carrying its `ledger_intent_id` and the intent's own state, read
///   through `core.outbox_producer_view()`. The durable intent is what decides
///   the row's fate: once its state is `delivered` the ledger entry is confirmed
///   and the row drops out of the worklist, whatever a lost notification did or
///   did not say.
/// * `unbooked` with no intent — the pre-existing case, and **the remaining
///   job**: a row written before the outbox existed, or a payment whose fund
///   could not be resolved at enqueue time (`ledger_intent_id IS NULL`, with the
///   reason in the response and the audit of its delivery). A caller holding
///   `finance:write` books it at `POST /api/stripe/payment/{id}/book`.
/// * `delegated_event` — handed to finance through `payment.received`, whose
///   subscriber books it idempotently but returns no answer.
/// * `refused` / `failed` — a booking as the caller did not land, or the relay
///   spent its attempts on this payment's intent. The fact is real and the
///   ledger entry is not there.
fn sql_unbooked(c: &PluginContext) -> String {
    format!(
        "SELECT {SWEEP_FIELDS} FROM {payments} p \
         LEFT JOIN core.outbox_producer_view() v ON v.id = p.ledger_intent_id \
         WHERE p.ledger_status <> '{LEDGER_BOOKED}' \
           AND v.state IS DISTINCT FROM '{OUTBOX_DELIVERED}' \
           AND p.confirmed_at < now() - ($1 || ' minutes')::interval \
         ORDER BY p.id DESC LIMIT $2",
        payments = c.db.table("payments")
    )
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct StripePlugin {
    ctx: OnceLock<PluginContext>,
    config: OnceLock<StripeConfig>,
}

impl StripePlugin {
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
            config: OnceLock::new(),
        }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()")
    }

    /// The parsed config. Panics only if `routes()` is called before `init()`,
    /// which the core's lifecycle guarantees cannot happen.
    pub fn config(&self) -> &StripeConfig {
        self.config
            .get()
            .expect("core must call init() before routes()")
    }
}

impl Default for StripePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The schema (SPEC §7.13: this plugin's own tables; finance's are finance's).
///
/// Vocabulary that a later reader would have to *trust* is a constraint
/// instead: a purpose, a session status, a ledger status, a positive amount, a
/// redelivery counter that cannot go backwards. The SQL lives in
/// `migrations/*.sql`, one file per version:
///
/// * `001_stripe_schema` — the three tables, byte-identical to the SQL that
///   shipped as `MIGRATION_SCHEMA` (the version and name in
///   `core.schema_migrations` are unchanged, so no deployed database re-runs
///   anything).
/// * `002_payment_ledger_intent` — `payments.ledger_intent_id` and the replaced
///   `ledger_status` check that admits `intent_enqueued`. A **new version** on
///   purpose: the runner skips an applied version *without comparing its SQL*,
///   so amending version 1 would be invisible on every deployed database while
///   looking correct on a fresh one.
pub mod migrations {
    adjutant_sdk::migrations! {
        1 => "stripe_schema" => "../migrations/001_stripe_schema.sql";
        2 => "payment_ledger_intent" => "../migrations/002_payment_ledger_intent.sql";
    }
}

#[async_trait]
impl AdjutantPlugin for StripePlugin {
    fn id(&self) -> &str {
        "stripe"
    }

    fn name(&self) -> &str {
        "Stripe"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let (cfg, warnings) = StripeConfig::from_value(&ctx.config);
        for warning in &warnings {
            eprintln!("[adjutant-stripe] {warning}");
        }
        if cfg.secret_key().is_none() {
            eprintln!(
                "[adjutant-stripe] no secret_key configured: checkout will refuse until one is set"
            );
        }
        if cfg.webhook_secret().is_none() {
            eprintln!(
                "[adjutant-stripe] no webhook_secret configured: the webhook endpoint will refuse \
                 every delivery rather than accept an unverified one"
            );
        }
        let _ = self.config.set(cfg);
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new(
                PERM_READ,
                "See the checkout sessions you opened or that name you",
            ),
            Permission::new(
                PERM_READ_ALL,
                "See every checkout session and every confirmed payment, with whether it \
                 reached the ledger",
            ),
            Permission::new(
                PERM_CHECKOUT,
                "Open a Stripe Checkout session for dues, a donation or an event fee",
            ),
            Permission::new(
                PERM_MANAGE,
                "Open a Checkout session for another member, and ask finance to book a \
                 confirmed payment as you (which needs your own finance:write)",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        migrations::all()
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        stripe_routes(self.ctx(), self.config())
    }

    /// The core's own **notification** that an intent reached a terminal state.
    ///
    /// Not how the ledger learns anything — the intent is. This is how this
    /// plugin settles its own `ledger_status` from finance's answer, which the
    /// core's docs say a producer should do. The worklist does not depend on it
    /// (`sql_unbooked` reads the intent's durable state), so a dropped event
    /// cannot make a booking invisible or invent one.
    fn subscriptions(&self) -> Vec<EventSubscription> {
        let ctx = self.ctx().clone();
        vec![EventSubscription::new(
            OUTBOX_EVENT_PREFIX,
            event_handler(move |ev| {
                let c = ctx.clone();
                async move {
                    let intent_id = ev.payload["intent_id"].as_i64().unwrap_or_default();
                    // The bus is a broadcast: every subscriber sees every
                    // producer's outcome, so one of them has to be somebody
                    // else's business and is passed over.
                    if intent_id == 0 || ev.payload["producer"].as_str() != Some("stripe") {
                        return Ok(());
                    }
                    let (status, transaction_id) = match ev.payload["state"].as_str() {
                        Some(OUTBOX_DELIVERED) => (
                            LEDGER_BOOKED,
                            ev.payload["answer"]["transaction"]["id"]
                                .as_i64()
                                .map(|id| id.to_string()),
                        ),
                        Some(OUTBOX_REFUSED) => (LEDGER_REFUSED, None),
                        Some(OUTBOX_EXHAUSTED) => (LEDGER_FAILED, None),
                        // `pending`/`attempting` are not terminal: the relay is
                        // still working, and a transient failure is not an
                        // outcome to write down.
                        _ => return Ok(()),
                    };
                    let error = ev.payload["error"].as_str().map(str::to_string);
                    c.db.execute(
                        sql_settle_from_intent(&c),
                        vec![
                            SqlValue::Int(intent_id),
                            SqlValue::Text(status.to_string()),
                            transaction_id
                                .map(SqlValue::Text)
                                .unwrap_or(SqlValue::Null),
                            error.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                        ],
                    )
                    .await?;
                    eprintln!(
                        "[adjutant-stripe] intent {intent_id} {status}: a payment's ledger_status \
                         settled from its intent's outcome"
                    );
                    Ok(())
                }
            }),
        )]
    }

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone();
        let cfg = self.config().clone();
        vec![Schedule::new(
            "unbooked_sweep",
            std::time::Duration::from_secs(6 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                let cfg = cfg.clone();
                async move { unbooked_sweep(&c, &cfg).await }
            }),
        )]
    }
}

export_plugin!(StripePlugin);

/// Every route this plugin serves, in the order the API reference lists them.
fn stripe_routes(ctx: &PluginContext, cfg: &StripeConfig) -> Vec<RouteDefinition> {
    vec![
        route_health(cfg),
        route_checkout(ctx, cfg),
        route_list_sessions(ctx),
        route_get_session(ctx),
        route_webhook(ctx, cfg),
        route_list_payments(ctx),
        route_unbooked(ctx, cfg),
        route_get_payment(ctx),
        route_book(ctx, cfg),
    ]
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// `GET /api/stripe/health` — what this plugin is configured to do, and what it
/// is not.
///
/// Reports **presence**, never a value: `"secret_key": "configured"`. The
/// `ledger` block is the money-path statement from the module docs, so an
/// operator reads the same truth from the running system that the code holds.
///
/// No queries.
fn route_health(cfg: &StripeConfig) -> RouteDefinition {
    let cfg = cfg.clone();
    RouteDefinition::get_protected(
        "/api/stripe/health",
        PERM_READ,
        route_handler(move |_req| {
            let cfg = cfg.clone();
            async move {
                PluginResponse::json(
                    200,
                    &json!({
                        "plugin": "stripe",
                        "version": env!("CARGO_PKG_VERSION"),
                        "config": {
                            "secret_key": if cfg.secret_key().is_some() { "configured" } else { "missing" },
                            "webhook_secret": if cfg.webhook_secret().is_some() { "configured" } else { "missing" },
                            "key_mode": cfg.key_mode(),
                            "api_base": cfg.api_base(),
                            "base_url": cfg.base_url(),
                            "currency": cfg.currency(),
                            "success_url": cfg.success_url(),
                            "cancel_url": cfg.cancel_url(),
                            "webhook_tolerance_seconds": cfg.tolerance_seconds(),
                            "unbooked_after_minutes": cfg.unbooked_after_minutes(),
                            "funds": {
                                PURPOSE_DUES: cfg.fund_code_for(PURPOSE_DUES),
                                PURPOSE_DONATION: cfg.fund_code_for(PURPOSE_DONATION),
                                PURPOSE_EVENT_FEE: cfg.fund_code_for(PURPOSE_EVENT_FEE),
                            },
                        },
                        "checkout_ready": cfg.secret_key().is_some(),
                        "webhook_ready": cfg.webhook_secret().is_some(),
                        "default_urls_configured": cfg.success_url().is_some()
                            && cfg.cancel_url().is_some(),
                        "ledger": {
                            "path": "outbox",
                            "principal": LEDGER_PRINCIPAL,
                            "target_route": format!("POST {FINANCE_TRANSACTION_PATH}"),
                            "synchronous": false,
                            "how_the_fund_is_named": "by finance's fund id when the funds read \
                                                      answers, else by the fund's **code** — finance \
                                                      accepts either and resolves it inside the \
                                                      statement that writes the entry, which is what \
                                                      makes the intent real on a callerless webhook \
                                                      (issue #60)",
                            "intent": "a confirmed payment and its ledger intent are written in \
                                       one statement (core.outbox_enqueue as an expression in this \
                                       plugin's own INSERT), so neither can exist without the \
                                       other",
                            "booked_as_the_caller": format!("POST /api/stripe/payment/{{id}}/book"),
                            "why": "a Stripe webhook carries no Adjutant caller, so there is no \
                                    credential to forward and finance's own gate would answer 401 \
                                    (plugin-to-plugin.md §2(b)). The core's relay delivers the \
                                    intent as a declared service principal and records finance's \
                                    answer on the intent row",
                            "fallback": "the funds read answered and said finance has no such fund (or that it is inactive), so no \
                                         intent is enqueued: the payment is recorded, then handed \
                                         to finance through payment.received — status \
                                         'delegated_event', no answer. When the read could not be \
                                         made at all, the fund is named by its **code** in the \
                                         intent instead — finance accepts either (issue #60)",
                            "unverified": "the relay's delivery is asynchronous: 'intent_enqueued' \
                                           is neither booked nor unbooked, and GET \
                                           /api/stripe/unbooked is the worklist of everything \
                                           without a confirmed ledger entry, with each payment's \
                                           intent and its state",
                            "blocked_on": "nothing on this path: a fund code is a complete answer. What a \
                                           machine-originated producer still cannot do is READ \
                                           finance (funds, balances, the ledger) — it can instruct, \
                                           not ask, because the read is a caller's and it holds no \
                                           credential",
                        },
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------------

/// `POST /api/stripe/checkout` — open a Checkout session for dues, a donation or
/// an event fee (SPEC §7.13).
///
/// A session row is written **pending** first, then Stripe is called, then the
/// row is settled — so a call that never answered leaves a visible `failed`
/// attempt rather than a mystery, and the `client_reference_id` Stripe echoes
/// back names a row that exists before Stripe ever sees it.
///
/// Queries: the pending row, the settle; an `execute` marks the failure. Then
/// the audit write.
fn route_checkout(ctx: &PluginContext, cfg: &StripeConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/stripe/checkout",
        PERM_CHECKOUT,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let Some(secret) = cfg.secret_key() else {
                    return PluginResponse::error(
                        503,
                        "no Stripe secret_key is configured for this plugin, so no session can be \
                         opened; set it in the stripe plugin's config and reload",
                    );
                };
                let body: CheckoutBody = req.json()?;
                let purpose = normalize_purpose(&body.purpose);
                if !PURPOSES.contains(&purpose.as_str()) {
                    return PluginResponse::error(
                        400,
                        format!("purpose must be one of {}", PURPOSES.join(", ")),
                    );
                }
                let amount_cents = match resolve_amount(body.amount_cents, &body.amount) {
                    Ok(amount) => amount,
                    Err(reason) => return PluginResponse::error(400, reason),
                };
                if amount_cents <= 0 {
                    return PluginResponse::error(
                        400,
                        "amount must be positive — a Checkout session charges, it does not \
                         refund (a refund is an expense in finance's ledger)",
                    );
                }
                let caller = caller_of(&req);
                let member_id = trimmed(&body.member_id).unwrap_or_default();
                // Opening a session on somebody else's behalf is the treasurer's
                // act, and it is checked here because the *object* is the member.
                if !member_id.is_empty() && Some(member_id.as_str()) != caller.as_deref() {
                    if let Err(e) = c
                        .permissions
                        .reach(req.identity.as_ref(), PERM_MANAGE, &Scope::troop())
                        .await
                    {
                        return PluginResponse::error(e.status(), e.to_string());
                    }
                }
                let success_url = match trimmed(&body.success_url)
                    .or_else(|| cfg.success_url())
                {
                    Some(url) => url,
                    None => {
                        return PluginResponse::error(
                            400,
                            "success_url is required (or set a default with the plugin's \
                             success_url config)",
                        )
                    }
                };
                let cancel_url = match trimmed(&body.cancel_url).or_else(|| cfg.cancel_url()) {
                    Some(url) => url,
                    None => {
                        return PluginResponse::error(
                            400,
                            "cancel_url is required (or set a default with the plugin's \
                             cancel_url config)",
                        )
                    }
                };
                let fund_code = trimmed(&body.fund_code)
                    .unwrap_or_else(|| cfg.fund_code_for(&purpose));
                let category = trimmed(&body.category)
                    .unwrap_or_else(|| category_for(&purpose).to_string());
                let currency = trimmed(&body.currency)
                    .unwrap_or_else(|| cfg.currency())
                    .to_ascii_lowercase();
                let related_event_id = trimmed(&body.related_event_id).unwrap_or_default();
                let description = trimmed(&body.description).unwrap_or_default();
                let label = if description.is_empty() {
                    checkout_label(&purpose, body.dues_year, &member_id)
                } else {
                    description.clone()
                };

                let created = c
                    .db
                    .query_one(
                        sql_insert_session(&c),
                        vec![
                            SqlValue::Text(purpose.clone()),
                            SqlValue::Int(amount_cents),
                            SqlValue::Text(currency.clone()),
                            SqlValue::Text(member_id.clone()),
                            SqlValue::Text(fund_code.clone()),
                            SqlValue::Text(category.clone()),
                            SqlValue::Text(label.clone()),
                            SqlValue::Text(related_event_id.clone()),
                            body.dues_year
                                .map(|year| SqlValue::Int(i64::from(year)))
                                .unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(caller.clone().unwrap_or_default()),
                        ],
                    )
                    .await?;
                let Some(session) = created else {
                    return Err(SdkError::Internal(
                        "the checkout session insert returned no row".to_string(),
                    ));
                };
                let row_id = session["id"].as_i64().unwrap_or_default();

                let request = CheckoutRequest {
                    row_id,
                    purpose: purpose.clone(),
                    amount_cents,
                    currency: currency.clone(),
                    label: label.clone(),
                    member_id: member_id.clone(),
                    fund_code: fund_code.clone(),
                    category: category.clone(),
                    related_event_id: related_event_id.clone(),
                    dues_year: body.dues_year,
                    success_url,
                    cancel_url,
                };
                let form = form_encode(&checkout_form(&request));
                let response = c
                    .http
                    .request(
                        "POST".to_string(),
                        format!("{}/v1/checkout/sessions", cfg.api_base()),
                        vec![
                            ("authorization".to_string(), format!("Bearer {secret}")),
                            (
                                "content-type".to_string(),
                                "application/x-www-form-urlencoded".to_string(),
                            ),
                        ],
                        Some((
                            "application/x-www-form-urlencoded".to_string(),
                            form.into_bytes(),
                        )),
                    )
                    .await;

                let (stripe_id, checkout_url, failure) = match response {
                    Ok(response) if (200..300).contains(&response.status) => {
                        let parsed = serde_json::from_slice::<Value>(&response.body).ok();
                        let id = parsed
                            .as_ref()
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let url = parsed
                            .as_ref()
                            .and_then(|value| value.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if id.is_empty() || url.is_empty() {
                            (String::new(), String::new(), Some((
                                response.status,
                                "Stripe answered without a session id and url".to_string(),
                            )))
                        } else {
                            (id, url, None)
                        }
                    }
                    Ok(response) => (
                        String::new(),
                        String::new(),
                        Some((
                            response.status,
                            message_of(&response.body, response.status),
                        )),
                    ),
                    Err(e) => (
                        String::new(),
                        String::new(),
                        Some((
                            502,
                            format!("Stripe could not be reached: {e}"),
                        )),
                    ),
                };

                if let Some((status, reason)) = failure {
                    // The row stays, marked: an attempt that charged nobody is
                    // still a fact, and erasing it would hide a misconfiguration.
                    c.db.execute(
                        sql_set_session_status_by_row(&c),
                        vec![
                            SqlValue::Int(row_id),
                            SqlValue::Text(SESSION_FAILED.to_string()),
                        ],
                    )
                    .await
                    .unwrap_or(0);
                    let _ = c
                        .audit
                        .log(
                            req.identity.as_ref(),
                            "stripe.checkout.failed",
                            "stripe_checkout_session",
                            &row_id.to_string(),
                            json!({
                                "purpose": purpose,
                                "amount_cents": amount_cents,
                                "member_id": member_id,
                                "http_status": status,
                                "reason": reason,
                            }),
                        )
                        .await;
                    return PluginResponse::error(
                        pass_through_status(status),
                        format!(
                            "Stripe did not open the session: {reason}. The attempt is recorded as \
                             session {row_id} in status '{SESSION_FAILED}'; no card was charged."
                        ),
                    );
                }

                let settled = c
                    .db
                    .query_one(
                        sql_set_session_created(&c),
                        vec![
                            SqlValue::Int(row_id),
                            SqlValue::Text(stripe_id.clone()),
                            SqlValue::Text(checkout_url.clone()),
                        ],
                    )
                    .await?;
                let url = settled
                    .as_ref()
                    .and_then(|row| row["checkout_url"].as_str())
                    .unwrap_or_default()
                    .to_string();
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "stripe.checkout.create",
                        "stripe_checkout_session",
                        &row_id.to_string(),
                        json!({
                            "stripe_session_id": stripe_id,
                            "purpose": purpose,
                            "amount_cents": amount_cents,
                            "currency": currency,
                            "member_id": member_id,
                            "fund_code": fund_code,
                            "category": category,
                            "related_event_id": related_event_id,
                            "dues_year": body.dues_year,
                            "created_by": caller,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "stripe.checkout.created",
                        json!({
                            "session_id": row_id,
                            "stripe_session_id": settled
                                .as_ref()
                                .map(|row| row["stripe_session_id"].clone())
                                .unwrap_or(Value::Null),
                            "purpose": purpose,
                            "amount_cents": amount_cents,
                            "member_id": member_id,
                            "status": SESSION_CREATED,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/stripe/session/{row_id}"),
                    &json!({
                        "session": settled,
                        "checkout_url": url,
                        "checkout": {
                            "provider": "stripe",
                            "mode": "payment",
                            "line_item": label,
                            "amount_cents": amount_cents,
                            "currency": currency,
                        },
                        "ledger": {
                            "fund_code": fund_code,
                            "category": category,
                            "not_yet": "nothing is booked until Stripe confirms the payment",
                        },
                    }),
                )
            }
        }),
    )
}

/// `GET /api/stripe/sessions` — the troop's Checkout sessions.
///
/// `stripe:read` at some scope; a caller without `stripe:read_all` is narrowed
/// to the sessions they opened or that name them, so a scout sees their own
/// dues session and not the troop's.
///
/// One query.
fn route_list_sessions(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/stripe/sessions",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let some = |value: Option<&str>| -> SqlValue {
                    match value {
                        Some(value) => SqlValue::Text(value.to_string()),
                        None => SqlValue::Null,
                    }
                };
                let purpose = req
                    .query_param("purpose")
                    .map(normalize_purpose);
                let status = req.query_param("status").map(|raw| {
                    let status = raw.trim().to_ascii_lowercase();
                    if SESSION_STATUSES.contains(&status.as_str()) {
                        status
                    } else {
                        String::new()
                    }
                });
                if let Some(status) = &status {
                    if status.is_empty() {
                        return PluginResponse::error(
                            400,
                            format!("status must be one of {}", SESSION_STATUSES.join(", ")),
                        );
                    }
                }
                let limit = req.query_int("limit").unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
                let before_id = req.query_int("before_id");
                let session_id = req.query_int("id");
                let member_id = req
                    .query_param("member_id")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);

                let everything = c
                    .permissions
                    .has_in_scope(req.identity.as_ref(), PERM_READ_ALL, &Scope::troop())
                    .await;
                let caller = caller_of(&req);
                let narrow = if everything { None } else { caller };

                let mut rows = c
                    .db
                    .query(
                        sql_list_sessions(&c),
                        vec![
                            session_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            some(purpose.as_deref()),
                            some(status.as_deref()),
                            some(member_id.as_deref()),
                            some(narrow.as_deref()),
                            before_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Int(limit + 1),
                        ],
                    )
                    .await?;
                let has_more = rows.len() as i64 > limit;
                rows.truncate(limit as usize);
                let next_before_id = if has_more {
                    rows.last().and_then(|row| row["id"].as_i64())
                } else {
                    None
                };
                PluginResponse::json(
                    200,
                    &json!({
                        "sessions": rows,
                        "count": rows.len(),
                        "has_more": has_more,
                        "next_before_id": next_before_id,
                        "narrowed_to_caller": narrow.is_some(),
                        "note": if narrow.is_some() {
                            "you are seeing the sessions you opened or that name you; \
                             stripe:read_all sees the troop's"
                        } else {
                            "you hold stripe:read_all, so this is every session"
                        },
                    }),
                )
            }
        }),
    )
}

/// `GET /api/stripe/session/{id}` — one session, with the payment it produced
/// (if any) and that payment's ledger status.
///
/// Two queries: the session, then its payment.
fn route_get_session(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/stripe/session/{id}",
        PERM_READ,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let Some(session) = c
                    .db
                    .query_one(sql_session_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such checkout session");
                };
                let owner = session["created_by"].as_str().unwrap_or_default();
                let named = session["member_id"].as_str().unwrap_or_default();
                let caller = caller_of(&req);
                let everything = c
                    .permissions
                    .has_in_scope(req.identity.as_ref(), PERM_READ_ALL, &Scope::troop())
                    .await;
                let mine = caller.as_deref().is_some_and(|caller| caller == owner || caller == named);
                if !everything && !mine {
                    // Absent and forbidden are the same 403, so existence is not
                    // leaked to somebody who may not see it.
                    return PluginResponse::error(403, "no such checkout session");
                }
                let payment = match session["stripe_session_id"].as_str() {
                    Some(stripe_id) if !stripe_id.is_empty() => {
                        c.db.query_one(
                            sql_payment_by_session_id(&c),
                            vec![SqlValue::Text(stripe_id.to_string())],
                        )
                        .await?
                    }
                    _ => None,
                };
                PluginResponse::json(
                    200,
                    &json!({
                        "session": session,
                        "payment": payment,
                        "ledger": {
                            "booked": payment
                                .as_ref()
                                .map(|payment| payment["ledger_status"] == LEDGER_BOOKED)
                                .unwrap_or(false),
                            "status": payment
                                .as_ref()
                                .map(|payment| payment["ledger_status"].clone())
                                .unwrap_or(Value::Null),
                        },
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// The webhook
// ---------------------------------------------------------------------------

/// `POST /api/stripe/webhook` — Stripe's confirmation (SPEC §7.13).
///
/// **Open, and deliberately so:** the request comes from Stripe's servers, not
/// from a user, so there is no Adjutant session to require. The credential is
/// the HMAC signature over the raw body, verified against the endpoint's
/// signing secret before a byte of the payload is trusted; an unverified
/// delivery is a `400` and touches no table.
///
/// Queries: the receipt, the resolution of the fund (over `ctx.http`, at enqueue
/// time, as a §2(b) call), and the one statement that records the payment and
/// enqueues its ledger intent together. A redelivery is self-healing — see the
/// module docs.
fn route_webhook(ctx: &PluginContext, cfg: &StripeConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post(
        "/api/stripe/webhook",
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move { handle_webhook(&c, &cfg, &req).await }
        }),
    )
}

/// The webhook handler. Split out so tests can drive it directly.
pub async fn handle_webhook(
    c: &PluginContext,
    cfg: &StripeConfig,
    req: &PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let Some(secret) = cfg.webhook_secret() else {
        return PluginResponse::error(
            503,
            "no webhook_secret is configured for this plugin, so the endpoint refuses every \
             delivery rather than accepting an unverified one; set it in the stripe plugin's \
             config and reload",
        );
    };
    let header = req
        .headers
        .get(SIGNATURE_HEADER)
        .or_else(|| {
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(SIGNATURE_HEADER))
                .map(|(_, v)| v)
        })
        .map(String::as_str);
    let now = Utc::now().timestamp();
    let stamp = match verify_signature(header, &req.body, &secret, cfg.tolerance_seconds(), now) {
        Ok(stamp) => stamp,
        Err(reason) => {
            // The refused delivery is audited, without the body: an audit entry
            // must not become a second copy of a payment payload.
            let _ = c
                .audit
                .log(
                    None,
                    "stripe.webhook.refused",
                    "stripe_webhook",
                    "",
                    json!({ "reason": reason, "digest": body_digest(&req.body) }),
                )
                .await;
            return PluginResponse::error(
                400,
                format!("the delivery is not a verified Stripe webhook: {reason}"),
            );
        }
    };

    let payload: Value = match serde_json::from_slice(&req.body) {
        Ok(payload) => payload,
        Err(e) => {
            return PluginResponse::error(400, format!("the delivery body is not JSON: {e}"));
        }
    };
    let envelope = match webhook_envelope(&payload) {
        Ok(envelope) => envelope,
        Err(reason) => return PluginResponse::error(400, reason),
    };

    // The receipt ledger. A repeat delivery is counted, not refused: the
    // *payment* is what must not be recorded twice, and that is gated below.
    let fresh = c
        .db
        .query_one(
            sql_insert_webhook_event(c),
            vec![
                SqlValue::Text(envelope.event_id.clone()),
                SqlValue::Text(envelope.event_type.clone()),
                SqlValue::Bool(envelope.livemode),
                SqlValue::Text(envelope.api_version.clone()),
                SqlValue::Int(stamp.timestamp),
                SqlValue::Text(body_digest(&req.body)),
            ],
        )
        .await?;
    let mut redelivered = 0i64;
    if let Some(row) = &fresh {
        redelivered = row["redeliveries"].as_i64().unwrap_or(0);
    } else {
        c.db.execute(
            sql_bump_redeliveries(c),
            vec![SqlValue::Text(envelope.event_id.clone())],
        )
        .await?;
    }

    let outcome = classify_webhook(&payload, &envelope.event_type);
    let fact = match outcome {
        WebhookOutcome::Payment(fact) => *fact,
        WebhookOutcome::Nothing => {
            c.audit
                .log(
                    None,
                    "stripe.webhook.ignored",
                    "stripe_webhook",
                    &envelope.event_id,
                    json!({
                        "event_type": envelope.event_type,
                        "livemode": envelope.livemode,
                        "redelivery": fresh.is_none(),
                    }),
                )
                .await?;
            return PluginResponse::json(
                200,
                &json!({
                    "received": true,
                    "duplicate": false,
                    "event_id": envelope.event_id,
                    "event_type": envelope.event_type,
                    "payment": Value::Null,
                    "note": "this delivery carries no payment this plugin books",
                }),
            );
        }
        WebhookOutcome::Unusable(reason) => {
            c.audit
                .log(
                    None,
                    "stripe.webhook.unusable",
                    "stripe_webhook",
                    &envelope.event_id,
                    json!({
                        "event_type": envelope.event_type,
                        "reason": reason,
                        "livemode": envelope.livemode,
                    }),
                )
                .await?;
            c.events
                .publish(
                    "stripe.webhook.unusable",
                    json!({
                        "event_id": envelope.event_id,
                        "event_type": envelope.event_type,
                        "reason": reason,
                        "livemode": envelope.livemode,
                    }),
                )
                .await?;
            // 200: Stripe retrying a payload this plugin cannot read would not
            // make it readable. The refusal is recorded and announced instead.
            return PluginResponse::json(
                200,
                &json!({
                    "received": true,
                    "duplicate": false,
                    "event_id": envelope.event_id,
                    "event_type": envelope.event_type,
                    "payment": Value::Null,
                    "unusable": reason,
                    "note": "recorded and audited; Stripe is not asked to retry",
                }),
            );
        }
    };

    let fund_code = fact
        .fund_code
        .clone()
        .unwrap_or_else(|| cfg.fund_code_for(&fact.purpose));
    let category = fact
        .category
        .clone()
        .unwrap_or_else(|| category_for(&fact.purpose).to_string());

    // The ledger booking, composed **at enqueue time** so the payload can be
    // complete: the relay cannot read-then-write at delivery, so finance's fund
    // *id* is resolved here, exactly as `POST /api/stripe/payment/{id}/book`
    // resolves it — with the credential this request carried, which for a Stripe
    // webhook is none, and that is the limit the module docs state (issue #60).
    //
    // The payload is complete either way: the fund is named by finance's id when
    // the read answered, and by the fund's **code** when it did not — finance
    // resolves a code inside the statement that writes the entry, so no read at
    // delivery is needed and none is attempted. Only a fund this plugin has
    // *seen* to be missing or closed yields no intent at all: then the payment is
    // still recorded, truthfully `unbooked`, and handed to finance through
    // `payment.received`, with the reason in the response and the audit.
    let (intent, intent_refusal, fund_note) =
        match fund_for_intent(c, cfg, &forward_headers(req), &fund_code).await {
            Ok((fund, note)) => (
                Some(ledger_intent_payload(&fact, &category, &fund)),
                None::<String>,
                note,
            ),
            Err(reason) => (None, Some(reason), None),
        };

    // One statement, two shapes: with the intent, `ledger_intent_id` is the
    // scalar `core.outbox_enqueue(...)` returns — so the payment row and its
    // intent commit together or neither does. See `sql_insert_payment`.
    let ledger_status = if intent.is_some() {
        LEDGER_INTENT_ENQUEUED
    } else {
        LEDGER_UNBOOKED
    };
    let ledger_mechanism = if intent.is_some() { MECHANISM_OUTBOX } else { "" };
    let mut params = payment_insert_params(
        &fact,
        &envelope,
        &fund_code,
        &category,
        ledger_status,
        ledger_mechanism,
    );
    if let Some(payload) = &intent {
        // $15 the principal, $16 the payload. The principal is not a credential
        // this plugin holds: the core checks it against its own declaration for
        // this producer, and a refusal is an error in this transaction — the
        // payment is not written either, which is the point of one statement.
        params.push(SqlValue::Text(LEDGER_PRINCIPAL.to_string()));
        params.push(SqlValue::Json(payload.to_string()));
    }
    let inserted = c
        .db
        .query_one(sql_insert_payment(c, intent.is_some()), params)
        .await?;

    let (payment, newly_recorded) = match inserted {
        Some(row) => (row, true),
        None => {
            // Another delivery already recorded this payment. Re-read it: its
            // ledger status decides whether this one has work left to do.
            let existing = c
                .db
                .query_one(
                    sql_payment_by_payment_id(c),
                    vec![SqlValue::Text(fact.payment_id.clone())],
                )
                .await?;
            match existing {
                Some(row) => (row, false),
                None => {
                    return Err(SdkError::Internal(format!(
                        "payment {} is neither insertable nor readable, which cannot happen",
                        fact.payment_id
                    )))
                }
            }
        }
    };
    let payment_id = payment["id"].as_i64().unwrap_or_default();
    let ledger_status = payment["ledger_status"]
        .as_str()
        .unwrap_or(LEDGER_UNBOOKED);
    let already_delegated = matches!(
        ledger_status,
        LEDGER_DELEGATED_EVENT | LEDGER_INTENT_ENQUEUED
    );
    // The payment as returned: the row as inserted (or as already recorded)
    // until the hand-off settles it, then the settled row.
    let mut delegated = payment.clone();

    if newly_recorded {
        // Settle the session this payment came from. The `client_reference_id`
        // this plugin sent names our own row, so that is the direct key; a
        // session created outside this plugin (in Stripe's dashboard) has no
        // reference and is settled by Stripe's id instead. Either way it is one
        // statement, and a session already completed stays put.
        match fact.session_ref {
            Some(row_id) => {
                c.db.execute(
                    sql_set_session_status_by_row(c),
                    vec![
                        SqlValue::Int(row_id),
                        SqlValue::Text(SESSION_COMPLETED.to_string()),
                    ],
                )
                .await?;
            }
            None if !fact.session_id.is_empty() => {
                c.db.execute(
                    sql_set_session_status(c),
                    vec![
                        SqlValue::Text(fact.session_id.clone()),
                        SqlValue::Text(SESSION_COMPLETED.to_string()),
                    ],
                )
                .await?;
            }
            None => {}
        }
    }

    // The ledger hand-off, for a payment whose booking is **not** an intent.
    // `payment.received` is finance's documented contract (SPEC §5.4), and its
    // subscriber keys on `payment_id`, so re-publishing after a failed attempt
    // cannot double-count. A payment whose booking *is* an intent is handed off
    // by the relay instead: publishing both would be two mechanisms for one
    // fact, and the second one to arrive would be refused for a duplicate
    // `external_ref`.
    if !already_delegated {
        c.events
            .publish(
                event_type::PAYMENT_RECEIVED,
                payment_received_payload(&fact, &fund_code, &category, &payment),
            )
            .await?;
        delegated = c
            .db
            .query_one(
                sql_set_ledger_outcome(c),
                vec![
                    SqlValue::Int(payment_id),
                    SqlValue::Text(LEDGER_DELEGATED_EVENT.to_string()),
                    SqlValue::Text(MECHANISM_EVENT.to_string()),
                    SqlValue::Null,
                    SqlValue::Null,
                ],
            )
            .await?
            .unwrap_or_else(|| payment.clone());
        if newly_recorded {
            c.events
                .publish(
                    "stripe.payment.confirmed",
                    json!({
                        "payment_id": fact.payment_id,
                        "amount_cents": fact.amount_cents,
                        "currency": fact.currency,
                        "purpose": fact.purpose,
                        "member_id": fact.member_id,
                        "fund_code": fund_code,
                        "category": category,
                        "session_id": fact.session_id,
                        "livemode": envelope.livemode,
                        "ledger_status": LEDGER_DELEGATED_EVENT,
                    }),
                )
                .await?;
        }
    } else if newly_recorded {
        // The intent path: the payment and its intent are already committed, and
        // finance will answer through the relay. The notification says which
        // mechanism carried it and which intent to watch.
        c.events
            .publish(
                "stripe.payment.confirmed",
                json!({
                    "payment_id": fact.payment_id,
                    "amount_cents": fact.amount_cents,
                    "currency": fact.currency,
                    "purpose": fact.purpose,
                    "member_id": fact.member_id,
                    "fund_code": fund_code,
                    "category": category,
                    "session_id": fact.session_id,
                    "livemode": envelope.livemode,
                    "ledger_status": ledger_status,
                    "ledger_intent_id": payment["ledger_intent_id"],
                }),
            )
            .await?;
    }

    if newly_recorded {
        c.audit
            .log(
                None,
                "stripe.payment.recorded",
                "stripe_payment",
                &fact.payment_id,
                json!({
                    "event_id": envelope.event_id,
                    "event_type": envelope.event_type,
                    "amount_cents": fact.amount_cents,
                    "currency": fact.currency,
                    "purpose": fact.purpose,
                    "member_id": fact.member_id,
                    "fund_code": fund_code,
                    "category": category,
                    "livemode": envelope.livemode,
                    "ledger_status": ledger_status,
                    "ledger_intent_id": payment["ledger_intent_id"],
                    // Why no intent was enqueued, when that is what happened: the
                    // fund could not be resolved at enqueue time, so the payload
                    // could not be completed (issue #60). The payment is recorded
                    // either way — this is the reason on the record.
                    "intent_refused": intent_refusal,
                    "fund_note": fund_note,
                    "signature_timestamp": stamp.timestamp,
                }),
            )
            .await?;
    }

    let duplicate = !newly_recorded && already_delegated;
    let ledger = webhook_ledger_block(&delegated, intent_refusal.as_deref(), fund_note.as_deref());
    PluginResponse::json(
        200,
        &json!({
            "received": true,
            "duplicate": duplicate,
            "redelivered": fresh.is_none(),
            "redeliveries": redelivered,
            "event_id": envelope.event_id,
            "event_type": envelope.event_type,
            "payment": delegated,
            "ledger": ledger,
        }),
    )
}

/// The ledger block a webhook's response carries: which mechanism this delivery
/// used, and what is still not known about it.
///
/// Two shapes, because the two are not the same thing and a caller must be able
/// to tell them apart: an intent is enqueued (the relay will deliver it, and its
/// state is readable), or no intent could be composed and the event carried the
/// payment with the reason it was not.
fn webhook_ledger_block(
    payment: &Value,
    intent_refusal: Option<&str>,
    fund_note: Option<&str>,
) -> Value {
    let status = payment["ledger_status"]
        .as_str()
        .unwrap_or(LEDGER_UNBOOKED);
    if status == LEDGER_INTENT_ENQUEUED {
        return json!({
            "path": "outbox",
            "principal": LEDGER_PRINCIPAL,
            "target_route": format!("POST {FINANCE_TRANSACTION_PATH}"),
            "intent_id": payment["ledger_intent_id"],
            // Why the payload names the fund by code, when it did.
            "fund_note": fund_note,
            "synchronous": false,
            "status": status,
            "why": "a Stripe webhook carries no Adjutant caller, so there is no credential to \
                    forward (plugin-to-plugin.md §2(b)). The booking is an outbox intent written \
                    in the same statement as the payment, and the core's relay delivers it as the \
                    declared service principal svc.stripe.ledger, retrying with backoff and \
                    recording finance's own answer on the intent row",
            "verify": "GET /api/stripe/unbooked lists it while the intent is in flight, with the \
                       intent's own state; the relay's outcome settles ledger_status",
        });
    }
    json!({
        "path": "event",
        "event": event_type::PAYMENT_RECEIVED,
        "synchronous": false,
        "status": status,
        "why": "no intent was enqueued for this payment, so the fallback mechanism carried it: a \
                Stripe webhook carries no Adjutant caller, so there is no credential to forward \
                and finance's own gate would answer 401 (plugin-to-plugin.md §2(b)); the payment \
                is handed to finance through payment.received, whose subscriber is idempotent on \
                the payment id",
        // Null unless the intent could not be composed — in which case this is
        // the reason, and the payment is on the worklist as `unbooked`.
        "intent_refused": intent_refusal,
        "verify": "POST /api/stripe/payment/{id}/book, as a caller holding finance:write, makes \
                   the ledger write synchronous",
    })
}

/// The `payment.received` payload (SPEC §5.4) — the shape finance's subscriber
/// deserializes, field for field. Finance's keys are not this plugin's to
/// rename.
fn payment_received_payload(
    fact: &PaymentFact,
    fund_code: &str,
    category: &str,
    payment: &Value,
) -> Value {
    let mut payload = json!({
        "payment_id": fact.payment_id,
        "amount_cents": fact.amount_cents,
        "fund_code": fund_code,
        "category": category,
        "member_id": fact.member_id,
        "description": format!(
            "Stripe {} {}",
            fact.purpose, fact.payment_id
        ),
    });
    if let Some(occurred_on) = payment["confirmed_at"]
        .as_str()
        .and_then(|stamp| stamp.get(0..10))
    {
        payload["occurred_on"] = json!(occurred_on);
    }
    if let Some(year) = fact.dues_year {
        payload["fiscal_year"] = json!(year);
    }
    payload
}

// ---------------------------------------------------------------------------
// Payments, the worklist, and the booking
// ---------------------------------------------------------------------------

/// `GET /api/stripe/payments` — every confirmed payment and whether it reached
/// the ledger. `stripe:read_all` (troop): the troop's money record.
///
/// One query.
fn route_list_payments(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/stripe/payments",
        PERM_READ_ALL,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let some = |value: Option<&str>| -> SqlValue {
                    match value {
                        Some(value) => SqlValue::Text(value.to_string()),
                        None => SqlValue::Null,
                    }
                };
                let ledger_status = match req.query_param("ledger_status") {
                    Some(raw) => {
                        let status = raw.trim().to_ascii_lowercase();
                        if !LEDGER_STATUSES.contains(&status.as_str()) {
                            return PluginResponse::error(
                                400,
                                format!("ledger_status must be one of {}", LEDGER_STATUSES.join(", ")),
                            );
                        }
                        Some(status)
                    }
                    None => None,
                };
                let purpose = req.query_param("purpose").map(normalize_purpose);
                let limit = req.query_int("limit").unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
                let before_id = req.query_int("before_id");
                let member_id = req
                    .query_param("member_id")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let payment_id = req
                    .query_param("payment_id")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);

                let mut rows = c
                    .db
                    .query(
                        sql_list_payments(&c),
                        vec![
                            some(purpose.as_deref()),
                            some(ledger_status.as_deref()),
                            some(payment_id.as_deref()),
                            some(member_id.as_deref()),
                            before_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Int(limit + 1),
                        ],
                    )
                    .await?;
                let has_more = rows.len() as i64 > limit;
                rows.truncate(limit as usize);
                let next_before_id = if has_more {
                    rows.last().and_then(|row| row["id"].as_i64())
                } else {
                    None
                };
                let unbooked = rows
                    .iter()
                    .filter(|row| row["ledger_status"] != LEDGER_BOOKED)
                    .count();
                PluginResponse::json(
                    200,
                    &json!({
                        "payments": rows,
                        "count": rows.len(),
                        "has_more": has_more,
                        "next_before_id": next_before_id,
                        "unbooked_in_page": unbooked,
                        "note": "ledger_status is what this plugin knows: 'booked' means finance \
                                 confirmed the entry (to a booking made as the caller, or to the \
                                 delivery of the payment's intent); 'intent_enqueued' means the \
                                 booking is an outbox intent the relay is delivering, with \
                                 `ledger_intent_id` naming it; 'delegated_event' means the \
                                 payment was handed to finance through payment.received and no \
                                 answer came back",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/stripe/payment/{id}` — one payment.
///
/// One query.
fn route_get_payment(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/stripe/payment/{id}",
        PERM_READ_ALL,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                match c
                    .db
                    .query_one(sql_payment_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                {
                    Some(payment) => PluginResponse::json(
                        200,
                        &json!({
                            "payment": payment,
                            "ledger": ledger_block(&payment),
                        }),
                    ),
                    None => PluginResponse::error(404, "no such payment"),
                }
            }
        }),
    )
}

/// The truthful account of one payment's ledger state, and what to do next.
fn ledger_block(payment: &Value) -> Value {
    let status = payment["ledger_status"].as_str().unwrap_or(LEDGER_UNBOOKED);
    json!({
        "status": status,
        "booked": status == LEDGER_BOOKED,
        "mechanism": payment["ledger_mechanism"],
        "intent_id": payment["ledger_intent_id"],
        "transaction_id": payment["ledger_transaction_id"],
        "error": payment["ledger_error"],
        "attempted_at": payment["ledger_attempted_at"],
        "next": match status {
            LEDGER_BOOKED => "nothing: finance confirmed the entry",
            LEDGER_INTENT_ENQUEUED => "nothing, yet: the payment and its ledger intent committed \
                                       together, and the core's relay will deliver the intent to \
                                       finance as svc.stripe.ledger, retrying with backoff. \
                                       `intent_id` names it; GET /api/stripe/unbooked shows the \
                                       intent's state while it is in flight, and an operator can \
                                       see it in GET /api/outbox/intents (core:admin). The \
                                       relay's answer sets this to booked, refused or failed",
            LEDGER_DELEGATED_EVENT => "the payment was handed to finance through payment.received \
                                       and finance's subscriber books it idempotently; this plugin \
                                       cannot see the answer. POST /api/stripe/payment/{id}/book \
                                       as a caller holding finance:write makes it synchronous \
                                       (finance will refuse a duplicate external_ref)",
            LEDGER_UNBOOKED => "no intent was enqueued for this payment (its fund could not be \
                                resolved at enqueue time, or it predates the outbox); POST \
                                /api/stripe/payment/{id}/book as a caller holding finance:write",
            LEDGER_REFUSED => "finance or its gate said no — to the booking as the caller, or to \
                               the intent's delivery. Its message is in `error`. If it names \
                               external_ref, the entry is already there: check finance's own \
                               transactions with finance:read_all",
            _ => "the booking did not land: the call did not complete, or the relay spent every \
                  attempt on the intent. The next unbooked_sweep will notice it again",
        },
    })
}

/// `GET /api/stripe/unbooked` — the payments with no *confirmed* ledger entry,
/// and **the in-flight case stated rather than hidden**.
///
/// This is the honest answer to "is every charged card on the books?". A payment
/// whose ledger booking is an outbox intent is neither booked nor unbooked, so it
/// is listed with its `ledger_intent_id` and the intent's own state
/// (`intent_state`, `intent_attempts`, `intent_last_error`, read through
/// `core.outbox_producer_view()`), while a payment with no intent at all is the
/// older job — the one `POST /api/stripe/payment/{id}/book` closes. The two are
/// kept visible together and separated by status rather than assumed to be fine,
/// and a payment whose intent is `delivered` is gone from here: the durable
/// intent says the ledger entry exists, whatever a lost notification said.
///
/// One query.
fn route_unbooked(ctx: &PluginContext, cfg: &StripeConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::get_protected(
        "/api/stripe/unbooked",
        PERM_READ_ALL,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let older_than = req
                    .query_int("older_than_minutes")
                    .filter(|minutes| *minutes >= 0)
                    .unwrap_or_else(|| cfg.unbooked_after_minutes());
                let limit = req.query_int("limit").unwrap_or(SWEEP_LIMIT).clamp(1, MAX_PAGE);
                let rows = c
                    .db
                    .query(
                        sql_unbooked(&c),
                        vec![SqlValue::Text(older_than.to_string()), SqlValue::Int(limit)],
                    )
                    .await?;
                let total = rows
                    .first()
                    .and_then(|row| row["total_unbooked"].as_i64())
                    .unwrap_or(0);
                let mut by_status = serde_json::Map::new();
                for status in LEDGER_STATUSES {
                    let count = rows
                        .iter()
                        .filter(|row| row["ledger_status"].as_str() == Some(status))
                        .count();
                    if count > 0 {
                        by_status.insert(status.to_string(), json!(count));
                    }
                }
                // The in-flight case is counted apart from the rest, because it
                // is the one an operator should *not* act on by hand: the relay
                // is already delivering it, and `intent_state` says what it is
                // doing.
                let in_flight = rows
                    .iter()
                    .filter(|row| row["ledger_status"].as_str() == Some(LEDGER_INTENT_ENQUEUED))
                    .count();
                PluginResponse::json(
                    200,
                    &json!({
                        "payments": rows,
                        "count": rows.len(),
                        "total_unbooked": total,
                        "older_than_minutes": older_than,
                        "by_status": Value::Object(by_status),
                        "in_flight": in_flight,
                        "note": "each row says which case it is. 'intent_enqueued' means the \
                                 booking is an outbox intent the relay is delivering — neither \
                                 booked nor unbooked: `ledger_intent_id` names it and the intent_* \
                                 columns carry its own state, and the row leaves this list once \
                                 that state is 'delivered'. 'unbooked' with no intent is the real \
                                 job (the fund could not be resolved at enqueue time, or the row \
                                 predates the outbox): book it as a caller holding finance:write \
                                 from POST /api/stripe/payment/{id}/book. 'delegated_event' means \
                                 finance was told through payment.received and did not answer; \
                                 'refused'/'failed' mean the booking did not land",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/stripe/payment/{id}/book` — ask finance to book a confirmed
/// payment **as the caller** (the §2(b) mechanism).
///
/// The credential forwarded is the caller's own; finance's route gate re-decides
/// `finance:write`. A caller without it is refused by finance, in finance's
/// words. A request that carries no credential at all is refused here, before
/// anything is called — because there is nothing to forward, and inventing one
/// is the shortcut `plugin-to-plugin.md` §3.1 refuses.
///
/// Queries: the payment, then the outcome write. Then the audit write.
fn route_book(ctx: &PluginContext, cfg: &StripeConfig) -> RouteDefinition {
    let c = ctx.clone();
    let cfg = cfg.clone();
    RouteDefinition::post_protected(
        "/api/stripe/payment/{id}/book",
        PERM_MANAGE,
        route_handler(move |req| {
            let c = c.clone();
            let cfg = cfg.clone();
            async move {
                let id = req.int_param("id")?;
                let Some(payment) = c
                    .db
                    .query_one(sql_payment_by_id(&c), vec![SqlValue::Int(id)])
                    .await?
                else {
                    return PluginResponse::error(404, "no such payment");
                };
                let headers = forward_headers(&req);
                if headers.is_empty() {
                    let _ = c
                        .audit
                        .log(
                            req.identity.as_ref(),
                            "stripe.payment.book.failed",
                            "stripe_payment",
                            &id.to_string(),
                            json!({
                                "reason": "no credential to forward",
                                "mechanism": MECHANISM_CALLER_FORWARD,
                            }),
                        )
                        .await;
                    return PluginResponse::error(
                        403,
                        "this request carries no authorization or session cookie, so there is no \
                         credential to forward; finance's gate decides the booking, and this \
                         plugin holds no credential of its own (plugin-to-plugin.md §2(b))",
                    );
                }
                let outcome = ask_finance_to_book(&c, &cfg, headers, &payment).await;
                let settled = c
                    .db
                    .query_one(
                        sql_set_ledger_outcome(&c),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(outcome.status.to_string()),
                            SqlValue::Text(outcome.mechanism.to_string()),
                            outcome
                                .transaction_id
                                .clone()
                                .map(SqlValue::Text)
                                .unwrap_or(SqlValue::Null),
                            outcome
                                .error
                                .clone()
                                .map(SqlValue::Text)
                                .unwrap_or(SqlValue::Null),
                        ],
                    )
                    .await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "stripe.payment.book",
                        "stripe_payment",
                        &id.to_string(),
                        json!({
                            "payment_id": payment["payment_id"],
                            "amount_cents": payment["amount_cents"],
                            "fund_code": payment["fund_code"],
                            "mechanism": outcome.mechanism,
                            "ledger_status": outcome.status,
                            "http_status": outcome.http_status,
                            "transaction_id": outcome.transaction_id,
                            "error": outcome.error,
                        }),
                    )
                    .await?;
                let payload = json!({
                    "payment": settled,
                    "ledger": ledger_block(settled.as_ref().unwrap_or(&payment)),
                    "finance": {
                        "url": format!("{}{FINANCE_TRANSACTION_PATH}", cfg.base_url()),
                        "http_status": outcome.http_status,
                        "transaction_id": outcome.transaction_id,
                        "error": outcome.error,
                        "note": "the call carried your own credential; finance's gate decided, and \
                                 this plugin holds no credential of its own",
                    },
                });
                if outcome.status == LEDGER_BOOKED {
                    c.events
                        .publish(
                            "stripe.payment.booked",
                            json!({
                                "payment_id": payment["payment_id"],
                                "amount_cents": payment["amount_cents"],
                                "transaction_id": outcome.transaction_id,
                                "http_status": outcome.http_status,
                            }),
                        )
                        .await?;
                    PluginResponse::json(200, &payload)
                } else {
                    PluginResponse::json(outcome.answer_status, &payload)
                }
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// The sweep — a notification, never a mechanism
// ---------------------------------------------------------------------------

/// Every six hours, notice the confirmed payments with no *booked* ledger entry
/// — including the ones whose intent is still in flight.
///
/// **It notifies; it does not write.** Writing here would be the plugin
/// reaching into another plugin's domain on a timer, which is the same
/// privileged-shortcut problem as the webhook, and it would need a credential
/// nobody has. Nor does it deliver an intent: the core's relay does that, with
/// backoff and a recorded answer. What the sweep adds is the *threshold*: an
/// intent still not delivered after `unbooked_after_minutes` is a stalled money
/// path, and one event names those payments so a treasurer can act with their
/// own authority.
///
/// One query.
async fn unbooked_sweep(c: &PluginContext, cfg: &StripeConfig) -> Result<(), SdkError> {
    let minutes = cfg.unbooked_after_minutes();
    let rows = c
        .db
        .query(
            sql_unbooked(c),
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
        .and_then(|row| row["total_unbooked"].as_i64())
        .unwrap_or(rows.len() as i64);
    let oldest = rows
        .iter()
        .filter_map(|row| row["confirmed_at"].as_str())
        .min()
        .unwrap_or_default()
        .to_string();
    let mut by_status = serde_json::Map::new();
    for status in LEDGER_STATUSES {
        let count = rows
            .iter()
            .filter(|row| row["ledger_status"].as_str() == Some(status))
            .count();
        if count > 0 {
            by_status.insert(status.to_string(), json!(count));
        }
    }
    c.events
        .publish(
            "stripe.ledger.unbooked",
            json!({
                "unbooked": rows.len(),
                "total_unbooked": total,
                "in_flight": rows
                    .iter()
                    .filter(|row| row["ledger_status"].as_str() == Some(LEDGER_INTENT_ENQUEUED))
                    .count(),
                "older_than_minutes": minutes,
                "oldest_confirmed_at": oldest,
                "by_status": Value::Object(by_status),
                "payments": rows,
                "next": "a row with 'intent_enqueued' is not a hand-job: the core's relay is \
                         delivering it, its state is in the intent_* fields, and a stalled one \
                         belongs to the operator (GET /api/outbox/intents, core:admin). A row \
                         with no intent at all is the real work — a caller holding finance:write \
                         books it with POST /api/stripe/payment/{id}/book. This notification is \
                         never the ledger write (plugin-to-plugin.md §3.2)",
            }),
        )
        .await
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// The caller's user id, when the request is authenticated.
fn caller_of(req: &PluginRequest) -> Option<String> {
    req.identity
        .as_ref()
        .map(|identity| identity.user_id.clone())
        .filter(|user_id| !user_id.is_empty())
}

/// A `PluginRequest` never carries an identity for a webhook, and the audit
/// entry is written with `identity: None`. Kept as a named helper so the
/// `DateTime` import is used for exactly one purpose: nothing here needs the
/// clock except the timestamp check.
pub fn timestamp_utc(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_guess_a_purpose() {
        assert_eq!(normalize_purpose("dues"), PURPOSE_DUES);
        assert_eq!(normalize_purpose(" DUES "), PURPOSE_DUES);
        assert_eq!(normalize_purpose("event_fee"), PURPOSE_EVENT_FEE);
        // Anything unrecognized is a donation: money into the general fund, not
        // mis-filed as dues against somebody's standing.
        assert_eq!(normalize_purpose("uniform"), PURPOSE_DONATION);
        assert_eq!(normalize_purpose(""), PURPOSE_DONATION);
    }

    #[test]
    fn maps_each_purpose_to_finances_category() {
        assert_eq!(category_for(PURPOSE_DUES), CATEGORY_DUES);
        assert_eq!(category_for(PURPOSE_EVENT_FEE), CATEGORY_EVENT_FEE);
        assert_eq!(category_for(PURPOSE_DONATION), CATEGORY_DONATION);
    }

    #[test]
    fn parses_amounts_exactly_and_refuses_floats() {
        assert_eq!(parse_amount_to_cents("25").unwrap(), 2500);
        assert_eq!(parse_amount_to_cents("25.5").unwrap(), 2550);
        assert_eq!(parse_amount_to_cents("$1,234.56").unwrap(), 123456);
        assert!(parse_amount_to_cents("25.005").is_err());
        assert!(parse_amount_to_cents("25.").is_err());
        assert!(parse_amount_to_cents("-5").is_err());
        assert!(parse_amount_to_cents("five").is_err());
        assert!(parse_amount_to_cents("").is_err());
    }

    #[test]
    fn reads_a_session_reference_only_from_its_own_prefix() {
        assert_eq!(session_ref_from_reference("stripe-session-42"), Some(42));
        assert_eq!(session_ref_from_reference("  stripe-session-7 "), Some(7));
        assert_eq!(session_ref_from_reference("cs_test_abc"), None);
        assert_eq!(session_ref_from_reference("stripe-session-"), None);
        assert_eq!(session_ref_from_reference(""), None);
    }

    #[test]
    fn a_malformed_webhook_has_no_envelope() {
        assert!(webhook_envelope(&json!({ "type": "x" })).is_err());
        assert!(webhook_envelope(&json!({ "id": "  " })).is_err());
        let envelope = webhook_envelope(&json!({
            "id": "evt_1", "type": "payment_intent.succeeded", "livemode": true,
            "api_version": "2024-06-20", "created": 1_700_000_000
        }))
        .unwrap();
        assert_eq!(envelope.event_type, "payment_intent.succeeded");
        assert_eq!(timestamp_utc(envelope.created.unwrap()).unwrap().to_string(),
                   "2023-11-14 22:13:20 UTC");
    }
}
