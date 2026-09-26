//! # adjutant-finance — funds, the ledger, budgets vs. actuals, sliding-scale dues
//! (SPEC §7.5).
//!
//! A troop's books are the least forgiving part of the software. This plugin
//! owns them: the six funds, every transaction, the budget envelope each fund
//! spends against, and the sliding-scale dues the Accords set so that cost never
//! decides who belongs.
//!
//! ## The rules this crate is built around
//!
//! **Money is an integer number of cents (`BIGINT`/`i64`) and nothing else.**
//! There is no `f64` anywhere in this crate — a float cannot represent `0.10`,
//! and a rounding error in a troop's books is a real defect, not a display
//! blemish. Amounts are formatted for humans only at the edge
//! ([`format_cents`]) and parsed from humans exactly ([`parse_dollars_to_cents`],
//! which refuses anything finer than a cent rather than rounding it). A JSON
//! body that sends a float is refused by `serde` before any handler sees it; the
//! wire format is `amount_cents` (an integer) or `amount` (a string like
//! `"12.50"`).
//!
//! **A balance is derived, never stored.** `finance.funds` holds identity —
//! code, name, purpose, whether it is restricted — and no balance column. A fund
//! balance is `SUM(amount_cents)` over its ledger rows, the budget's actual is
//! the same sum filtered, and whether a member's dues are settled is the sum of
//! the payments tagged to them. A denormalised `balance` or `remaining` can
//! drift from the ledger that produced it; this crate gives it nowhere to drift
//! *to*.
//!
//! **A transfer is two ledger entries, in one statement.** The host mediates
//! every database call (`HostDb`), and each call is its own statement on a
//! pooled connection — a plugin cannot open a transaction across calls. So the
//! transfer route is a *single* `INSERT … SELECT` over `unnest`ed arrays that
//! writes both legs (the negative out-leg and the positive in-leg) under one
//! `gen_random_uuid()` group, guarded so that a missing fund inserts **both
//! legs or neither**. Conservation of the total is therefore a property of the
//! statement, not of the handler remembering to compensate afterwards: the two
//! legs are `-a` and `+a`, and `ledger_integrity` re-checks on demand that
//! every group still has exactly two entries summing to zero.
//!
//! **The sliding scale is honor-system.** A scout self-reports their tier
//! ([`TIERS`]); nobody verifies anyone's income, and this crate has no field, no
//! route and no code path for doing so. The mandatory minimum is `$0`
//! ([`MINIMUM_DUES_CENTS`]) — a hardship tier assesses nothing and the scout
//! stays a scout. A self-report never changes the *base* cost it is computed
//! from (that is the treasurer's number), so self-reporting cannot inflate or
//! deflate an assessment.
//!
//! ## The six funds
//!
//! Migration 1 seeds General, Scholarship, Equipment, Expedition, Impact and
//! Commencement (SPEC §7.5). `kind` is one of those six — a classification this
//! plugin understands — while `code` is the stable slug a client addresses
//! (`/api/finance/funds`), so a troop may add a fund (a Lodge's own gear fund)
//! without inventing a seventh kind.
//!
//! ## Permissions
//!
//! SPEC §9.1 names `finance:read` (view fund balances), `finance:read_all`
//! (detailed financial data, Finance Subcouncil+) and `finance:manage` (manage
//! finances). Three additions are needed to state the real authorities:
//!
//! * `finance:write` — record income, expenses and transfers. The treasurer's
//!   daily book, and not the same authority as managing funds or budgets.
//! * `finance:manage_dues` — open an assessment, change a tier, waive dues.
//! * `finance:self_report` — report **your own** sliding-scale tier. The handler
//!   additionally requires that the member being reported is the caller (SPEC
//!   §9.2: "a caller reading their own record is an ownership check, not a
//!   grant") — the permission alone is not enough.
//!
//! Money is sensitive, so reads are scoped: balances and the sliding scale are
//! `finance:read`, while the ledger, the report and the dues list are
//! `finance:read_all`. A member's own dues are readable with `finance:read` at
//! any scope, because it is their own record; anybody else's needs `read_all`.
//!
//! ## Schema
//!
//! `finance.funds`, `finance.transactions`, `finance.budgets`, `finance.dues`
//! (SPEC §7.5). Vocabulary that reaches an arithmetic result is a database
//! constraint rather than a convention: a transaction's `kind`, the sign rule
//! per kind (`income` is positive, `expense` negative), the transfer pair rule
//! (group and counterparty present, or neither), a dues row's subject, and the
//! waive-with-zero rule.
//!
//! ## Integration
//!
//! `payment.received` (SPEC §5.4's event vocabulary) books an income entry into
//! the fund the payment names, keyed on the provider's payment id
//! (`external_ref`, unique) so a replayed event is a no-op rather than a second
//! deposit. A daily schedule re-checks the ledger's integrity and publishes
//! `finance.ledger.imbalanced` only when something is actually wrong.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{Datelike, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// The six funds (SPEC §7.5). `kind` classifies; `code` is the slug the API
/// addresses. The seeded funds use the same word for both.
pub const FUND_GENERAL: &str = "general";
/// Dues assistance and course fees, so cost never decides who attends.
pub const FUND_SCHOLARSHIP: &str = "scholarship";
/// Gear, maintenance and replacement.
pub const FUND_EQUIPMENT: &str = "equipment";
/// Expeditions and their travel.
pub const FUND_EXPEDITION: &str = "expedition";
/// Service projects and the community's share of impact work.
pub const FUND_IMPACT: &str = "impact";
/// Ceremony, honours and the transition out.
pub const FUND_COMMENCEMENT: &str = "commencement";

/// The six kinds, in the order the seed creates them.
pub const FUND_KINDS: [&str; 6] = [
    FUND_GENERAL,
    FUND_SCHOLARSHIP,
    FUND_EQUIPMENT,
    FUND_EXPEDITION,
    FUND_IMPACT,
    FUND_COMMENCEMENT,
];

/// Transaction kinds. `amount_cents` is signed: income is positive, expense is
/// negative, and a transfer's two legs carry either sign.
pub const KIND_INCOME: &str = "income";
/// An outflow. Stored negative, so a fund's balance is a plain `SUM`.
pub const KIND_EXPENSE: &str = "expense";
/// A move between two funds: two rows, one `transfer_group`, sum zero.
pub const KIND_TRANSFER: &str = "transfer";

/// The kinds a caller may record directly (`transfer` has its own route, because
/// it is two entries and must be atomic).
pub const DIRECT_KINDS: [&str; 2] = [KIND_INCOME, KIND_EXPENSE];

/// The directions a budget line may have (a transfer is never budgeted).
pub const DIRECTIONS: [&str; 2] = [KIND_INCOME, KIND_EXPENSE];

/// Reserved `category` codes. Categories are otherwise free text — a troop's
/// categories are its own — but these two are load-bearing: `dues` is what the
/// dues standing derives a member's payments from, and `transfer` is what a
/// transfer leg is filed under.
pub const CATEGORY_DUES: &str = "dues";
/// Filed on both legs of every transfer.
pub const CATEGORY_TRANSFER: &str = KIND_TRANSFER;

/// The sliding-scale tiers (Accords: honor-system self-reporting, tiers, and a
/// `$0` mandatory minimum).
pub const TIER_PATRON: &str = "patron";
/// The full membership cost.
pub const TIER_STANDARD: &str = "standard";
/// Half the membership cost.
pub const TIER_SUPPORTED: &str = "supported";
/// `$0` — the tier that keeps a scout a scout.
pub const TIER_HARDSHIP: &str = "hardship";

/// A tier and what it assesses, as a multiple of the troop's membership cost in
/// **basis points** (1 bp = 0.01%), so the scale is exact integer arithmetic all
/// the way down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Stable code stored in `finance.dues.tier`.
    pub code: &'static str,
    /// Display label for a client.
    pub label: &'static str,
    /// The tier's share of the membership cost, in basis points.
    pub bps: i64,
    /// One sentence a scout reads when choosing.
    pub description: &'static str,
}

/// The scale, in the order a client should show it.
pub const TIERS: [Tier; 4] = [
    Tier {
        code: TIER_PATRON,
        label: "Patron",
        bps: 20_000,
        description: "Twice the membership cost: covers the share of a scout who cannot",
    },
    Tier {
        code: TIER_STANDARD,
        label: "Standard",
        bps: 10_000,
        description: "The full membership cost",
    },
    Tier {
        code: TIER_SUPPORTED,
        label: "Supported",
        bps: 5_000,
        description: "Half the membership cost",
    },
    Tier {
        code: TIER_HARDSHIP,
        label: "Hardship",
        bps: 0,
        description: "No dues this year — nobody is turned away for hardship",
    },
];

/// The tier codes, in the order [`TIERS`] declares them.
pub const TIER_CODES: [&str; 4] = [TIER_PATRON, TIER_STANDARD, TIER_SUPPORTED, TIER_HARDSHIP];

/// The mandatory minimum assessed amount, in cents. Zero, deliberately: the
/// Accords forbid turning a scout away for hardship.
pub const MINIMUM_DUES_CENTS: i64 = 0;

/// The tier a scout is assessed at before they self-report — the full cost, so a
/// troop is never short because somebody did not answer.
pub const DEFAULT_TIER: &str = TIER_STANDARD;

/// Dues assessment states.
pub const STATUS_ASSESSED: &str = "assessed";
/// The scout reported the tier themselves (the honor system working).
pub const STATUS_SELF_REPORTED: &str = "self_reported";
/// Dues waived — the whole assessment is funded from `scholarship`, so the
/// member's own share (`assessed_cents - funded_cents`) is zero.
pub const STATUS_WAIVED: &str = "waived";

/// The statuses the API and the database both accept.
pub const DUES_STATUSES: [&str; 3] = [STATUS_ASSESSED, STATUS_SELF_REPORTED, STATUS_WAIVED];

/// Nothing to fund: the assessment is zero, or the tier funds nothing.
pub const DRAW_NONE: &str = "none";
/// Funded, and no `finance:write` holder has booked the draw yet.
pub const DRAW_UNBOOKED: &str = "unbooked";
/// A `finance:write` caller is booking it now.
pub const DRAW_ATTEMPTING: &str = "attempting";
/// The transfer landed; `draw_ref` carries its group.
pub const DRAW_BOOKED: &str = "booked";
/// Finance's own transfer route refused it (the configured dues fund IS
/// `scholarship`): nothing was booked, and the waiver still stands.
pub const DRAW_REFUSED: &str = "refused";
/// The transfer call errored: nothing was booked.
pub const DRAW_FAILED: &str = "failed";

/// The draw states, in the order they are reached — the same vocabulary the
/// store uses for its own scholarship draw (`plugins/store/src/lib.rs`), because
/// a draw is a draw whatever funded it.
pub const DRAW_STATUSES: [&str; 6] = [
    DRAW_NONE,
    DRAW_UNBOOKED,
    DRAW_ATTEMPTING,
    DRAW_BOOKED,
    DRAW_REFUSED,
    DRAW_FAILED,
];

/// The draw states that mean "funded, and the money has not moved yet" — what
/// the outstanding-draw worklist and the Annual Financial Report count.
pub const DRAW_UNSETTLED: [&str; 3] = [DRAW_UNBOOKED, DRAW_ATTEMPTING, DRAW_FAILED];

/// The description a booked dues draw carries into the ledger.
pub fn dues_draw_description(fiscal_year: i32, member_id: &str) -> String {
    format!("Dues {fiscal_year} scholarship draw — {member_id}")
}

/// The draw's **deterministic** idempotency key: one waiver, one transfer,
/// however many times the answer is lost. `finance.transactions.external_ref`
/// has a unique index, so a retry carrying the same reference writes nothing new
/// — the shape the store uses for its own draw reference.
pub fn dues_draw_reference(fiscal_year: i32, member_id: &str) -> String {
    format!("dues:{fiscal_year}:{member_id}")
}

/// What `scholarship` funds of one assessment, in cents.
///
/// A **waiver** funds the whole assessment (`funded_cents = assessed_cents`), so
/// the member's own share is zero. Anything else funds the **discount** — the
/// part of the membership cost the tier does not assess (`base_cents -
/// assessed_cents`) — clamped at zero, because a patron assesses *above* the base
/// cost and a subsidy cannot be negative.
///
/// It is also clamped **at the assessment**, because `funded_cents` is what
/// `scholarship` covers *of what is owed*: the database's
/// `dues_funded_within_assessment` refuses a subsidy larger than the assessment,
/// and a `$0` tier (hardship) is a legitimate zero — there is no assessment for
/// the fund to cover, so it funds nothing (the note's own outcome for hardship).
/// The clamp is why a self-reported `supported` tier funds half the membership
/// cost (its whole assessment) while a self-reported hardship funds nothing.
pub fn funded_cents_for(status: &str, base_cents: i64, assessed_cents: i64) -> i64 {
    let assessed = assessed_cents.max(0);
    if status == STATUS_WAIVED {
        return assessed;
    }
    base_cents
        .max(0)
        .saturating_sub(assessed)
        .clamp(0, assessed)
}

/// The draw state an assessment earns before anyone tries to book it: `none`
/// when there is nothing to fund, `unbooked` when there is.
pub fn unbooked_draw_state(funded_cents: i64) -> &'static str {
    if funded_cents > 0 {
        DRAW_UNBOOKED
    } else {
        DRAW_NONE
    }
}

/// A member's assessment row in `finance.dues`.
pub const DUES_KIND_MEMBER: &str = "member";
/// A Lodge's levy row in `finance.dues` — a fraction of the membership cost.
pub const DUES_KIND_LODGE: &str = "lodge";

/// The dues kinds, constrained in the database.
pub const DUES_KINDS: [&str; 2] = [DUES_KIND_MEMBER, DUES_KIND_LODGE];

/// A basis point is 1/100 of a percent: 10 000 bp = 100%. A Lodge's levy may be
/// at most twice the membership cost.
pub const MAX_SHARE_BPS: i64 = 100_000;

/// Config key: the month the troop's fiscal year starts (default January).
pub const CONFIG_FISCAL_YEAR_START_MONTH: &str = "fiscal_year_start_month";
/// Config key: the total membership cost one scout pays, in cents — the number
/// the sliding scale is a percentage of.
pub const CONFIG_MEMBERSHIP_COST_CENTS: &str = "membership_cost_cents";
/// Config key: the fund dues and donations land in by default.
pub const CONFIG_DUES_FUND_CODE: &str = "dues_fund_code";

/// `recorded_by` on the seed rows. Not a user id: no member is called this.
pub const SEEDED_BY: &str = "finance:seed";

/// The most ledger rows one page returns.
pub const MAX_LEDGER_LIMIT: i64 = 200;
/// The default page size for the ledger.
pub const DEFAULT_LEDGER_LIMIT: i64 = 50;
/// The most transaction entries a fund's detail view shows.
pub const RECENT_ENTRIES: i64 = 20;
/// The most dues rows one list returns.
pub const MAX_DUES_ROWS: i64 = 500;

// ---------------------------------------------------------------------------
// Money — exact, integer, cents-only
// ---------------------------------------------------------------------------

/// Format an amount for a human: `$1,234.56`, `-$0.05`, `$0.00`.
///
/// The only place money becomes a string. Every API field is `*_cents`; this
/// exists so a report can be read without a client-side formatter, and so tests
/// can state expected output in the form a treasurer would recognise.
pub fn format_cents(cents: i64) -> String {
    format_money(cents, "$")
}

/// [`format_cents`] with an explicit currency symbol.
pub fn format_money(cents: i64, symbol: &str) -> String {
    // `unsigned_abs` so `i64::MIN` does not overflow on negation.
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
        "{}{symbol}{grouped}.{remainder:02}",
        if cents < 0 { "-" } else { "" }
    )
}

/// Parse a human dollars-and-cents string into cents, **exactly**.
///
/// Accepts `"12"`, `"12.5"`, `"12.50"`, `"$1,234.56"`, `"-3.05"`, `"+4"`. A
/// string finer than a cent (`"12.345"`) is refused rather than rounded — a
/// silent round is how books stop adding up — as is a trailing decimal point, a
/// non-numeric character, or a value too large for `i64`.
pub fn parse_dollars_to_cents(raw: &str) -> Result<i64, String> {
    let cleaned = raw.trim();
    let stripped = match cleaned.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, cleaned.strip_prefix('+').unwrap_or(cleaned)),
    };
    let rest = stripped.1.strip_prefix('$').unwrap_or(stripped.1);
    let rest = rest.replace(',', "");
    parse_hundredths(rest.trim(), raw, "a cent is the smallest unit")
        .map(|hundredths| hundredths * stripped.0)
}

/// Parse a percentage into basis points, exactly: `"12.5"` → `1250`,
/// `"100"` → `10000`, `"0.25%"` → `25`.
///
/// Used for a Lodge's dues share, which the Accords express as a fraction of the
/// membership cost. A basis point is 0.01%, so a percentage with more than two
/// decimals is finer than the unit the database stores and is refused.
pub fn parse_percent_to_bps(raw: &str) -> Result<i64, String> {
    let cleaned = raw.trim().trim_end_matches('%');
    parse_hundredths(
        cleaned.trim(),
        raw,
        "a basis point (0.01%) is the smallest unit",
    )
}

/// Parse an exact decimal with at most two fraction digits into hundredths.
fn parse_hundredths(cleaned: &str, raw: &str, too_fine: &str) -> Result<i64, String> {
    if cleaned.is_empty() {
        return Err(format!("{raw:?} is not a number"));
    }
    let (whole, fraction) = match cleaned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (cleaned, None),
    };
    // The sign was stripped above, so every remaining character of the whole part
    // must be a digit: `"--3"` and `"1 2"` are refused rather than read as a
    // number with a sign somewhere.
    if !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{raw:?} is not a number"));
    }
    let whole_digits = if whole.is_empty() {
        0i128
    } else {
        whole
            .parse::<i128>()
            .map_err(|_| format!("{raw:?} is not a number"))?
    };
    let fraction_hundredths = match fraction {
        None => 0i128,
        Some("") => {
            return Err(format!("{raw:?} ends in a decimal point"));
        }
        Some(fraction) if fraction.len() > 2 => return Err(format!("{raw:?}: {too_fine}")),
        Some(fraction) if !fraction.bytes().all(|b| b.is_ascii_digit()) => {
            return Err(format!("{raw:?} is not a number"));
        }
        Some(fraction) if fraction.len() == 1 => {
            fraction
                .parse::<i128>()
                .map_err(|_| format!("{raw:?} is not a number"))?
                * 10
        }
        Some(fraction) => fraction
            .parse::<i128>()
            .map_err(|_| format!("{raw:?} is not a number"))?,
    };
    let total = whole_digits
        .checked_mul(100)
        .and_then(|scaled| scaled.checked_add(fraction_hundredths))
        .ok_or_else(|| format!("{raw:?} is too large to be money"))?;
    i64::try_from(total).map_err(|_| format!("{raw:?} is too large to be money"))
}

/// One amount, as a caller may state it: `amount_cents` (an integer number of
/// cents) **or** `amount` (a dollars string like `"12.50"`), never both.
///
/// Deliberately not `f64`: `serde` refuses a JSON float for either variant, so
/// `{"amount": 12.5}` is a 400 rather than a number this crate would have to
/// round.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum MoneySpec {
    /// An integer number of cents.
    Cents(i64),
    /// A human amount, parsed exactly (dollars for `amount`, cents for
    /// `amount_cents` — the distinction is enforced by [`AmountField::resolve`]).
    Text(String),
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// Resolve `amount_cents` / `amount` into cents.
///
/// `amount_cents` is an integer count of cents and **only** that — a string there
/// is refused rather than guessed at as dollars; `amount` is the human spelling
/// (a dollars string) and a bare integer there is refused for the same reason.
/// A JSON float is refused by `serde` before this is reached, so no amount in
/// this plugin is ever an `f64`.
fn resolve_amount(
    cents: &Option<MoneySpec>,
    dollars: &Option<MoneySpec>,
    what: &str,
) -> Result<i64, String> {
    match (cents, dollars) {
        (Some(_), Some(_)) => Err(format!(
            "give amount_cents or amount, not both ({what} is ambiguous)"
        )),
        (Some(MoneySpec::Cents(value)), None) => Ok(*value),
        (Some(MoneySpec::Text(_)), None) => Err(format!(
            "{what}: amount_cents is an integer number of cents — write it as a number, \
             or use \"amount\" for a dollar string"
        )),
        (None, Some(MoneySpec::Text(dollars))) => parse_dollars_to_cents(dollars),
        (None, Some(MoneySpec::Cents(_))) => Err(format!(
            "{what}: amount must be a string like \"12.50\"; use amount_cents for an \
             integer number of cents"
        )),
        (None, None) => Err(format!("{what} is required (amount_cents)")),
    }
}

#[derive(Debug, Deserialize)]
struct FundCreateBody {
    kind: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    restricted: Option<bool>,
    #[serde(default)]
    target_cents: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct FundEditBody {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    restricted: Option<bool>,
    #[serde(default)]
    active: Option<bool>,
    /// Absent leaves the target alone; `null` clears it. `Option<Option<_>>` is
    /// the only shape that tells those two apart.
    #[serde(default, deserialize_with = "optional_nullable")]
    target_cents: Option<Option<i64>>,
}

/// The `Option<Option<T>>` deserializer: absent → `None`, `null` → `Some(None)`,
/// a value → `Some(Some(v))`.
fn optional_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
struct TransactionBody {
    /// finance's own primary key for the fund. Optional now that `fund_code` is
    /// accepted: a producer that holds the fund's **code** (the reference
    /// `plugin-to-plugin.md` §3.5 asks for) can write without reading anything.
    #[serde(default)]
    fund_id: Option<i64>,
    /// The fund's **code** — resolved here, where the funds table lives, inside
    /// the same statement as the insert. Exactly one of `fund_id`/`fund_code`.
    #[serde(default)]
    fund_code: Option<String>,
    kind: String,
    #[serde(default)]
    amount_cents: Option<MoneySpec>,
    #[serde(default)]
    amount: Option<MoneySpec>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    member_id: Option<String>,
    #[serde(default)]
    occurred_on: Option<String>,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    allow_overdraft: Option<bool>,
    #[serde(default)]
    external_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TransferBody {
    /// The origin fund by finance's own primary key. Optional now that
    /// `from_fund_code` is accepted — exactly one of the two per leg.
    #[serde(default)]
    from_fund_id: Option<i64>,
    /// The origin fund by **code**, resolved inside the transfer's own
    /// statement. Exactly one of `from_fund_id`/`from_fund_code`.
    #[serde(default)]
    from_fund_code: Option<String>,
    /// The destination fund by id. Exactly one of `to_fund_id`/`to_fund_code`.
    #[serde(default)]
    to_fund_id: Option<i64>,
    /// The destination fund by **code**. Exactly one of
    /// `to_fund_id`/`to_fund_code`.
    #[serde(default)]
    to_fund_code: Option<String>,
    #[serde(default)]
    amount_cents: Option<MoneySpec>,
    #[serde(default)]
    amount: Option<MoneySpec>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    occurred_on: Option<String>,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    allow_overdraft: Option<bool>,
    /// An idempotency key for the **transfer as a whole** — a value the caller
    /// chooses that identifies this money move rather than the two rows it
    /// writes. `finance.transactions.external_ref` has a unique index, so a
    /// retry carrying the same reference is answered with the transfer it
    /// already made instead of a second pair of legs. The dues draw gives it a
    /// deterministic value ([`dues_draw_reference`]) for exactly that reason: a
    /// lost answer must not become a doubled subsidy.
    #[serde(default)]
    external_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BudgetBody {
    fund_id: i64,
    direction: String,
    #[serde(default)]
    amount_cents: Option<MoneySpec>,
    #[serde(default)]
    amount: Option<MoneySpec>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DuesAssessBody {
    member_id: String,
    tier: String,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    base_cents: Option<i64>,
    #[serde(default)]
    lodge_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LodgeDuesBody {
    lodge_id: String,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    share_bps: Option<i64>,
    /// A percentage as a string (`"10"`, `"12.5"`); equivalent to `share_bps`.
    #[serde(default)]
    share_percent: Option<String>,
    #[serde(default)]
    base_cents: Option<i64>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SelfReportBody {
    tier: String,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    note: Option<String>,
    /// Honoured only for a caller who holds `finance:manage_dues` (a treasurer
    /// recording a phoned-in answer); anyone else may report themselves only.
    #[serde(default)]
    member_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DuesPaymentBody {
    member_id: String,
    #[serde(default)]
    amount_cents: Option<MoneySpec>,
    #[serde(default)]
    amount: Option<MoneySpec>,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    fund_id: Option<i64>,
    #[serde(default)]
    occurred_on: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    external_ref: Option<String>,
}

/// The `payment.received` payload (SPEC §5.4) — what the payments integration
/// publishes and this plugin books.
///
/// `payment_id` is the provider's own id and is the idempotency key: the bus can
/// replay, so a second delivery must not be a second deposit.
#[derive(Debug, Clone, Deserialize)]
pub struct PaymentReceived {
    /// The provider's payment id (Stripe's `pi_…`).
    pub payment_id: String,
    /// The amount in cents. An integer, as the provider reports it — a float is
    /// refused rather than rounded into somebody's books.
    pub amount_cents: i64,
    /// The fund to book into; the configured dues fund (General) by default.
    #[serde(default)]
    pub fund_code: Option<String>,
    /// Free category; `donation` by default.
    #[serde(default)]
    pub category: Option<String>,
    /// Who paid, when the provider knows.
    #[serde(default)]
    pub member_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `YYYY-MM-DD`; today (UTC) by default.
    #[serde(default)]
    pub occurred_on: Option<String>,
    #[serde(default)]
    pub fiscal_year: Option<i32>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A trimmed, non-blank string, or `None`.
fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A fund `code`: lowercase slug, the shape the database's CHECK enforces. The
/// database is the authority; this turns a 500 into a 400.
fn normalize_code(raw: &str) -> Result<String, String> {
    let code = raw.trim().to_ascii_lowercase();
    let valid = !code.is_empty()
        && code.len() <= 32
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(code)
    } else {
        Err(format!(
            "fund code {raw:?} must be a lowercase slug: [a-z][a-z0-9_]{{0,31}}"
        ))
    }
}

fn normalize_kind(raw: &str) -> Result<String, String> {
    let kind = raw.trim().to_ascii_lowercase();
    if FUND_KINDS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!(
            "fund kind must be one of {}",
            FUND_KINDS.join(", ")
        ))
    }
}

/// A direction a caller may record directly (`income`/`expense`); a transfer is
/// two entries and has its own route.
fn normalize_direct_kind(raw: &str) -> Result<String, String> {
    let kind = raw.trim().to_ascii_lowercase();
    if DIRECT_KINDS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!(
            "kind must be one of {} — a transfer is recorded with POST /api/finance/transfer",
            DIRECT_KINDS.join(", ")
        ))
    }
}

fn normalize_direction(raw: &str) -> Result<String, String> {
    let direction = raw.trim().to_ascii_lowercase();
    if DIRECTIONS.contains(&direction.as_str()) {
        Ok(direction)
    } else {
        Err(format!(
            "direction must be one of {}",
            DIRECTIONS.join(", ")
        ))
    }
}

fn normalize_tier(raw: &str) -> Result<String, String> {
    let tier = raw.trim().to_ascii_lowercase();
    if TIER_CODES.contains(&tier.as_str()) {
        Ok(tier)
    } else {
        Err(format!(
            "tier must be one of {} — the scale is honor-system, and hardship is $0",
            TIER_CODES.join(", ")
        ))
    }
}

fn normalize_status(raw: &str) -> Result<String, String> {
    let status = raw.trim().to_ascii_lowercase();
    if DUES_STATUSES.contains(&status.as_str()) {
        Ok(status)
    } else {
        Err(format!(
            "status must be one of {}",
            DUES_STATUSES.join(", ")
        ))
    }
}

/// A date the caller stated (`YYYY-MM-DD`), or today in UTC.
fn occurred_on(raw: &Option<String>) -> Result<NaiveDate, String> {
    match trimmed(raw) {
        Some(value) => NaiveDate::parse_from_str(&value, "%Y-%m-%d")
            .map_err(|_| format!("occurred_on {value:?} is not a date (YYYY-MM-DD)")),
        None => Ok(Utc::now().date_naive()),
    }
}

/// The fiscal year an entry belongs to: the caller's, or the one the date falls
/// in. Refused outside the range the database constrains, so a typo is a 400
/// rather than a row no report will ever find.
fn fiscal_year_arg(stated: Option<i32>, date: NaiveDate, config: &Value) -> Result<i32, String> {
    let year = stated.unwrap_or_else(|| fiscal_year_of(date, fiscal_year_start_month(config)));
    if (2000..=2200).contains(&year) {
        Ok(year)
    } else {
        Err(format!("fiscal_year {year} is outside 2000..=2200"))
    }
}

/// The membership cost one scout pays, from config — the base the sliding scale
/// is a fraction of. `None` when the troop has not set one.
fn configured_membership_cost(config: &Value) -> Option<i64> {
    config_i64(config, CONFIG_MEMBERSHIP_COST_CENTS).filter(|cents| *cents >= 0)
}

/// The fund dues and donations land in by default.
fn configured_dues_fund(config: &Value) -> String {
    config_str(config, CONFIG_DUES_FUND_CODE).unwrap_or_else(|| FUND_GENERAL.to_string())
}

// ---------------------------------------------------------------------------
// What the annual report and the budget view compute (pure)
// ---------------------------------------------------------------------------

/// A budget line's variance against its actual.
///
/// `actual_cents` is the ledger's own signed number (income positive, expense
/// negative — never re-signed, so nobody has to guess which way round it is).
/// `variance_cents` is stated **favorably**: positive is good news, whichever
/// direction the line runs — ahead of plan on income (`actual - planned`) or
/// under plan on an expense (`planned + actual`). `adverse` is the flag a client
/// colours red.
pub fn budget_variance(direction: &str, planned_cents: i64, actual_cents: i64) -> (i64, bool) {
    let variance = if direction == KIND_INCOME {
        actual_cents - planned_cents
    } else {
        planned_cents + actual_cents
    };
    (variance, variance < 0)
}

/// The ledger's integrity: every transfer group has exactly two entries summing
/// to zero, and the sum of every fund's balance equals the ledger's total.
///
/// This is the check that says "the books add up" — the reason a transfer is one
/// statement rather than two writes, and the reason no balance is ever stored.
pub fn ledger_verdict(ledger_total_cents: i64, fund_total_cents: i64, groups: &[Value]) -> Value {
    let mut imbalanced: Vec<Value> = Vec::new();
    let mut groups_checked = 0i64;
    for group in groups {
        groups_checked += 1;
        let entries = group["entries"].as_i64().unwrap_or(0);
        let sum = group["group_sum_cents"].as_i64().unwrap_or(0);
        if entries != 2 || sum != 0 {
            imbalanced.push(json!({
                "transfer_group": group["transfer_group"],
                "entries": entries,
                "group_sum_cents": sum,
                "problem": if entries != 2 { "a transfer is two entries" } else { "the legs do not cancel" },
            }));
        }
    }
    json!({
        "balanced": imbalanced.is_empty() && ledger_total_cents == fund_total_cents,
        "ledger_total_cents": ledger_total_cents,
        "sum_of_fund_balances_cents": fund_total_cents,
        "difference_cents": ledger_total_cents - fund_total_cents,
        "transfer_groups": groups_checked,
        "imbalanced_groups": imbalanced,
        "checked": "every fund balance is the sum of its ledger rows, and every transfer \
                    group nets to zero",
    })
}

// ---------------------------------------------------------------------------
// SQL the routes share
//
// The SQL is here, in one place, because money arithmetic appearing twice is
// money arithmetic disagreeing. Every statement below was prepared against a
// real PostgreSQL 18 before it was written down: the host binds Rust types
// (`i64` → `int8`, `String` → `text`), so a date arrives as text and is cast
// (`$n::date`) — a bare `text` parameter cannot be inserted into a `date`
// column — and no parameter is cast inside a `VALUES` list (the SDK documents
// what that does to type inference).
// ---------------------------------------------------------------------------

/// Every fund column the API states. There is no stored balance: the figures
/// come from [`FUND_TOTALS`] over the fund's ledger rows.
const FUND_FIELDS: &str = r#"
    f.id, f.code, f.name, f.kind, f.purpose, f.restricted, f.active, f.target_cents,
    f.created_by, f.created_at::text AS created_at, f.updated_at::text AS updated_at
"#;

/// The ledger aggregates that turn a fund into its figures. `LEFT JOIN` keeps a
/// fund with no entries visible, at zero.
const FUND_TOTALS: &str = r#"
    COALESCE(SUM(t.amount_cents), 0)::bigint AS balance_cents,
    COALESCE(SUM(t.amount_cents) FILTER (WHERE t.kind = 'income'), 0)::bigint AS income_cents,
    COALESCE(SUM(t.amount_cents) FILTER (WHERE t.kind = 'expense'), 0)::bigint AS expense_cents,
    COALESCE(SUM(t.amount_cents) FILTER (WHERE t.kind = 'transfer'), 0)::bigint AS net_transfer_cents,
    COUNT(t.id)::bigint AS entry_count
"#;

/// A ledger row as the API states it, for `RETURNING` (alias `t`).
const TRANSACTION_FIELDS: &str = r#"
    t.id, t.fund_id, t.amount_cents, t.kind, t.transfer_group::text AS transfer_group,
    t.counterparty_fund_id, t.category, t.description, t.member_id, t.fiscal_year,
    t.occurred_on::text AS occurred_on, t.recorded_by, t.overdraft_authorized,
    t.external_ref, t.created_at::text AS created_at
"#;

/// A budget line as the API states it, for `RETURNING` (alias `b`).
const BUDGET_FIELDS: &str = r#"
    b.id, b.fund_id, b.fiscal_year, b.direction, b.category, b.planned_cents, b.note,
    b.created_by, b.created_at::text AS created_at, b.updated_at::text AS updated_at
"#;

/// A dues row as the API states it, for `RETURNING` (alias `d`).
const DUES_FIELDS: &str = r#"
    d.id, d.fiscal_year, d.dues_kind, d.member_id, d.lodge_id, d.tier, d.share_bps,
    d.base_cents, d.assessed_cents, d.funded_cents, d.draw_status,
    d.draw_ref::text AS draw_ref, d.self_reported, d.status, d.note, d.recorded_by,
    d.assessed_at::text AS assessed_at, d.updated_at::text AS updated_at
"#;

/// A dues row *plus what the ledger says about it*. `paid_cents` and
/// `outstanding_cents` are derived from the transactions tagged with this
/// member and year, never stored: a payment recorded by any route (the dues
/// route, the ledger route, a `payment.received` event) moves them, and a refund
/// moves them back.
///
/// `funded_cents` and the draw's state come off the row itself, because they are
/// the assessment's own figures: what `scholarship` covers, and whether the
/// balanced transfer that funds it has been booked yet.
const DUES_STANDING_FIELDS: &str = r#"
    d.id, d.fiscal_year, d.dues_kind, d.member_id, d.lodge_id, d.tier, d.share_bps,
    d.base_cents, d.assessed_cents, d.funded_cents, d.draw_status,
    d.draw_ref::text AS draw_ref, d.self_reported, d.status, d.note, d.recorded_by,
    d.assessed_at::text AS assessed_at, d.updated_at::text AS updated_at,
    COALESCE(p.paid_cents, 0)::bigint AS paid_cents,
    GREATEST(d.assessed_cents - d.funded_cents - COALESCE(p.paid_cents, 0), 0)::bigint
        AS outstanding_cents,
    (COALESCE(p.paid_cents, 0) >= d.assessed_cents - d.funded_cents) AS settled
"#;

/// A transfer's two legs, grouped, with their sum — the integrity check reads
/// this and expects `entries = 2, group_sum_cents = 0` for every row.
fn sql_transfer_groups(c: &PluginContext) -> String {
    format!(
        "SELECT transfer_group::text AS transfer_group, COUNT(*)::bigint AS entries, \
                COALESCE(SUM(amount_cents), 0)::bigint AS group_sum_cents \
         FROM {tx} WHERE transfer_group IS NOT NULL GROUP BY transfer_group \
         ORDER BY transfer_group",
        tx = c.db.table("transactions")
    )
}

/// The ledger's own total, and how many transfer entries are unpaired (a
/// transfer leg that somehow lost its group is a corrupt row).
fn sql_ledger_totals(c: &PluginContext) -> String {
    format!(
        "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS ledger_total_cents, \
                COUNT(*)::bigint AS entries, \
                COUNT(*) FILTER (WHERE kind = 'transfer' AND transfer_group IS NULL)::bigint \
                    AS unpaired_transfers \
         FROM {tx}",
        tx = c.db.table("transactions")
    )
}

/// The sum of every fund's derived balance. Must equal the ledger's total: the
/// books add up or the funds table is missing a transaction.
fn sql_fund_total(c: &PluginContext) -> String {
    format!(
        "SELECT COALESCE(SUM(balance), 0)::bigint AS fund_total_cents \
         FROM (SELECT SUM(amount_cents) AS balance FROM {tx} GROUP BY fund_id) b",
        tx = c.db.table("transactions")
    )
}

/// One fund and its derived figures, by id.
fn sql_fund_one(c: &PluginContext) -> String {
    format!(
        "SELECT {FUND_FIELDS}, {FUND_TOTALS} FROM {funds} f \
         LEFT JOIN {tx} t ON t.fund_id = f.id \
         WHERE f.id = $1 GROUP BY f.id",
        funds = c.db.table("funds"),
        tx = c.db.table("transactions")
    )
}

/// Every fund and its derived figures; `$1` = include inactive funds.
fn sql_fund_list(c: &PluginContext) -> String {
    format!(
        "SELECT {FUND_FIELDS}, {FUND_TOTALS} FROM {funds} f \
         LEFT JOIN {tx} t ON t.fund_id = f.id \
         WHERE ($1::bool OR f.active) GROUP BY f.id ORDER BY f.id",
        funds = c.db.table("funds"),
        tx = c.db.table("transactions")
    )
}

/// The funds named by an array of ids, with balances — what a refused transfer
/// reads to explain itself (`$1` = `bigint[]`).
fn sql_fund_balances(c: &PluginContext) -> String {
    format!(
        "SELECT f.id, f.code, f.name, f.active, \
                COALESCE(SUM(t.amount_cents), 0)::bigint AS balance_cents \
         FROM {funds} f LEFT JOIN {tx} t ON t.fund_id = f.id \
         WHERE f.id = ANY($1) GROUP BY f.id ORDER BY f.id",
        funds = c.db.table("funds"),
        tx = c.db.table("transactions")
    )
}

/// Budget lines against their actuals, for one fiscal year (or all when `$1` is
/// null). The actual is a `LATERAL` sum over the ledger, which is why nothing
/// has to be recomputed when a transaction is recorded: there is no stored
/// actual to drift from the ledger.
fn sql_budget_lines(c: &PluginContext) -> String {
    format!(
        "SELECT b.id, b.fund_id, f.code AS fund_code, f.name AS fund_name, f.kind AS fund_kind, \
                b.fiscal_year, b.direction, b.category, b.planned_cents, b.note, b.created_by, \
                b.created_at::text AS created_at, b.updated_at::text AS updated_at, \
                COALESCE(a.actual_cents, 0)::bigint AS actual_cents, \
                (b.category = '' AND EXISTS (SELECT 1 FROM {budgets} b2 \
                    WHERE b2.fund_id = b.fund_id AND b2.fiscal_year = b.fiscal_year \
                      AND b2.direction = b.direction AND b2.category <> '')) AS overlaps_categories \
         FROM {budgets} b JOIN {funds} f ON f.id = b.fund_id \
         LEFT JOIN LATERAL ( \
             SELECT SUM(t.amount_cents) AS actual_cents FROM {tx} t \
             WHERE t.fund_id = b.fund_id AND t.fiscal_year = b.fiscal_year \
               AND t.kind = b.direction AND (b.category = '' OR t.category = b.category) \
         ) a ON true \
         WHERE ($1::bigint IS NULL OR b.fiscal_year = $1) \
         ORDER BY f.id, b.direction, b.category",
        budgets = c.db.table("budgets"),
        funds = c.db.table("funds"),
        tx = c.db.table("transactions")
    )
}

/// A dues row with the payments the ledger shows: `SELECT … FROM dues d LEFT
/// JOIN LATERAL (the member's dues income) p ON true`, plus a `WHERE` clause the
/// caller supplies and its parameters follow.
fn sql_dues_standing(c: &PluginContext, where_clause: &str) -> String {
    format!(
        "SELECT {DUES_STANDING_FIELDS} FROM {dues} d \
         LEFT JOIN LATERAL ( \
             SELECT SUM(t.amount_cents) AS paid_cents FROM {tx} t \
             WHERE t.category = '{CATEGORY_DUES}' AND t.member_id = d.member_id \
               AND t.fiscal_year = d.fiscal_year \
         ) p ON true \
         WHERE {where_clause}",
        dues = c.db.table("dues"),
        tx = c.db.table("transactions")
    )
}

/// Per-fund figures for a fiscal year: opening balance (everything before the
/// year), the year's income and expenses, the two directions of transfer, and
/// the balance the year closes at. `$1`/`$2` are the year's first and last day
/// as text, cast here because that is what the host binds.
fn sql_annual_funds(c: &PluginContext) -> String {
    let in_year = "t.occurred_on >= $1::date AND t.occurred_on <= $2::date";
    format!(
        "SELECT f.id, f.code, f.name, f.kind, f.restricted, f.active, f.target_cents, \
                COALESCE(SUM(t.amount_cents) FILTER (WHERE t.occurred_on < $1::date), 0)::bigint \
                    AS opening_cents, \
                COALESCE(SUM(t.amount_cents) FILTER (WHERE {in_year} AND t.kind = 'income'), 0)::bigint \
                    AS income_cents, \
                COALESCE(SUM(t.amount_cents) FILTER (WHERE {in_year} AND t.kind = 'expense'), 0)::bigint \
                    AS expense_cents, \
                COALESCE(SUM(t.amount_cents) FILTER (WHERE {in_year} AND t.kind = 'transfer' \
                    AND t.amount_cents > 0), 0)::bigint AS transfers_in_cents, \
                COALESCE(SUM(t.amount_cents) FILTER (WHERE {in_year} AND t.kind = 'transfer' \
                    AND t.amount_cents < 0), 0)::bigint AS transfers_out_cents, \
                COALESCE(SUM(t.amount_cents), 0)::bigint AS closing_cents \
         FROM {funds} f LEFT JOIN {tx} t ON t.fund_id = f.id \
         GROUP BY f.id ORDER BY f.id",
        funds = c.db.table("funds"),
        tx = c.db.table("transactions")
    )
}

/// The year's dues by tier: how many scouts chose each tier, what it assessed,
/// **what `scholarship` funded**, and how many paid nothing at all. A tier nobody
/// chose is simply absent.
///
/// `funded_cents` is the figure the report was missing: the sum of what the troop
/// spent on access. `unbooked_draws` counts the funded rows whose balanced
/// transfer has not been booked yet — money that is owed to the dues fund and has
/// not moved, which is exactly what a treasurer needs to see rather than a zero.
fn sql_annual_dues(c: &PluginContext) -> String {
    format!(
        "SELECT tier, COUNT(*)::bigint AS members, \
                COALESCE(SUM(assessed_cents), 0)::bigint AS assessed_cents, \
                COALESCE(SUM(funded_cents), 0)::bigint AS funded_cents, \
                COUNT(*) FILTER (WHERE draw_status IN ('{unbooked}', '{attempting}', \
                                                       '{failed}'))::bigint AS unbooked_draws, \
                COUNT(*) FILTER (WHERE assessed_cents = 0)::bigint AS at_no_cost, \
                COUNT(*) FILTER (WHERE self_reported)::bigint AS self_reported, \
                COUNT(*) FILTER (WHERE status = '{STATUS_WAIVED}')::bigint AS waived \
         FROM {dues} WHERE dues_kind = '{DUES_KIND_MEMBER}' AND fiscal_year = $1 \
         GROUP BY tier ORDER BY tier",
        dues = c.db.table("dues"),
        unbooked = DRAW_UNBOOKED,
        attempting = DRAW_ATTEMPTING,
        failed = DRAW_FAILED,
    )
}

/// What the year actually collected in dues, straight from the ledger.
fn sql_annual_collected(c: &PluginContext) -> String {
    format!(
        "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS collected_cents \
         FROM {tx} WHERE category = '{CATEGORY_DUES}' AND fiscal_year = $1",
        tx = c.db.table("transactions")
    )
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct FinancePlugin {
    ctx: OnceLock<PluginContext>,
}

impl FinancePlugin {
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
        }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()")
    }
}

impl Default for FinancePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The schema (SPEC §7.5: `finance.funds`, `finance.transactions`,
/// `finance.budgets`, `finance.dues`).
///
/// Vocabulary that reaches an arithmetic result is a constraint, not a
/// convention: `kind` decides a sign, `direction` decides how a variance reads,
/// the subject rule decides whose payments a dues row counts, and
/// `dues_waived_is_funded` makes "waived but owing" unrepresentable while a
/// waiver names what was funded (migration 2, [`DUES_FUNDING_MIGRATION`]).
const MIGRATION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS funds (
    id BIGSERIAL PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    purpose TEXT NOT NULL DEFAULT '',
    restricted BOOLEAN NOT NULL DEFAULT false,
    active BOOLEAN NOT NULL DEFAULT true,
    target_cents BIGINT,
    created_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT funds_kind_valid CHECK (kind IN ('general', 'scholarship', 'equipment', 'expedition', 'impact', 'commencement')),
    CONSTRAINT funds_code_shape CHECK (code ~ '^[a-z][a-z0-9_]{0,31}$'),
    CONSTRAINT funds_target_valid CHECK (target_cents IS NULL OR target_cents >= 0)
);
CREATE INDEX IF NOT EXISTS idx_funds_kind ON funds(kind);
CREATE TABLE IF NOT EXISTS transactions (
    id BIGSERIAL PRIMARY KEY,
    fund_id BIGINT NOT NULL REFERENCES funds(id),
    amount_cents BIGINT NOT NULL,
    kind TEXT NOT NULL,
    transfer_group UUID,
    counterparty_fund_id BIGINT REFERENCES funds(id),
    category TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    member_id TEXT NOT NULL DEFAULT '',
    fiscal_year INTEGER NOT NULL,
    occurred_on DATE NOT NULL,
    recorded_by TEXT NOT NULL DEFAULT '',
    overdraft_authorized BOOLEAN NOT NULL DEFAULT false,
    external_ref TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT transactions_kind_valid CHECK (kind IN ('income', 'expense', 'transfer')),
    CONSTRAINT transactions_amount_nonzero CHECK (amount_cents <> 0),
    CONSTRAINT transactions_income_positive CHECK (kind <> 'income' OR amount_cents > 0),
    CONSTRAINT transactions_expense_negative CHECK (kind <> 'expense' OR amount_cents < 0),
    CONSTRAINT transactions_transfer_pair CHECK (
        (kind = 'transfer' AND transfer_group IS NOT NULL
         AND counterparty_fund_id IS NOT NULL AND counterparty_fund_id <> fund_id)
        OR (kind <> 'transfer' AND transfer_group IS NULL AND counterparty_fund_id IS NULL)),
    CONSTRAINT transactions_fiscal_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_transactions_external_ref ON transactions(external_ref);
CREATE INDEX IF NOT EXISTS idx_transactions_fund_date ON transactions(fund_id, occurred_on);
CREATE INDEX IF NOT EXISTS idx_transactions_year_kind ON transactions(fiscal_year, kind, category);
CREATE INDEX IF NOT EXISTS idx_transactions_member_year ON transactions(member_id, fiscal_year);
CREATE INDEX IF NOT EXISTS idx_transactions_transfer_group ON transactions(transfer_group);
CREATE TABLE IF NOT EXISTS budgets (
    id BIGSERIAL PRIMARY KEY,
    fund_id BIGINT NOT NULL REFERENCES funds(id) ON DELETE CASCADE,
    fiscal_year INTEGER NOT NULL,
    direction TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT '',
    planned_cents BIGINT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    created_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT budgets_direction_valid CHECK (direction IN ('income', 'expense')),
    CONSTRAINT budgets_planned_positive CHECK (planned_cents > 0),
    CONSTRAINT budgets_fiscal_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200),
    UNIQUE (fund_id, fiscal_year, direction, category)
);
CREATE TABLE IF NOT EXISTS dues (
    id BIGSERIAL PRIMARY KEY,
    fiscal_year INTEGER NOT NULL,
    dues_kind TEXT NOT NULL,
    member_id TEXT NOT NULL DEFAULT '',
    lodge_id TEXT NOT NULL DEFAULT '',
    tier TEXT NOT NULL DEFAULT '',
    share_bps INTEGER,
    base_cents BIGINT NOT NULL DEFAULT 0,
    assessed_cents BIGINT NOT NULL DEFAULT 0,
    self_reported BOOLEAN NOT NULL DEFAULT false,
    status TEXT NOT NULL DEFAULT 'assessed',
    note TEXT NOT NULL DEFAULT '',
    recorded_by TEXT NOT NULL DEFAULT '',
    assessed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT dues_kind_valid CHECK (dues_kind IN ('member', 'lodge')),
    CONSTRAINT dues_subject_present CHECK (
        (dues_kind = 'member' AND member_id <> '') OR
        (dues_kind = 'lodge' AND member_id = '' AND lodge_id <> '')),
    CONSTRAINT dues_tier_for_members CHECK (dues_kind <> 'member' OR tier <> ''),
    CONSTRAINT dues_share_for_lodges CHECK (
        dues_kind <> 'lodge'
        OR (share_bps IS NOT NULL AND share_bps >= 0 AND share_bps <= 100000)),
    CONSTRAINT dues_assessed_valid CHECK (assessed_cents >= 0),
    CONSTRAINT dues_base_valid CHECK (base_cents >= 0),
    CONSTRAINT dues_status_valid CHECK (status IN ('assessed', 'self_reported', 'waived')),
    CONSTRAINT dues_waived_is_zero CHECK (status <> 'waived' OR assessed_cents = 0),
    CONSTRAINT dues_fiscal_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200),
    UNIQUE (fiscal_year, dues_kind, member_id, lodge_id)
);
CREATE INDEX IF NOT EXISTS idx_dues_lodge_year ON dues(lodge_id, fiscal_year);
CREATE INDEX IF NOT EXISTS idx_dues_year_tier ON dues(fiscal_year, tier);
INSERT INTO funds (code, name, kind, purpose, restricted, created_by) VALUES
    ('general',      'General Fund',      'general',      'Operating costs, supplies and the year''s day-to-day business', false, 'finance:seed'),
    ('scholarship',  'Scholarship Fund',  'scholarship',  'Dues assistance and course fees, so cost never decides who attends', true, 'finance:seed'),
    ('equipment',    'Equipment Fund',    'equipment',    'Gear, maintenance and replacement', true, 'finance:seed'),
    ('expedition',   'Expedition Fund',   'expedition',   'Expeditions and their travel', true, 'finance:seed'),
    ('impact',       'Impact Fund',       'impact',       'Service projects and the community''s share of impact work', true, 'finance:seed'),
    ('commencement', 'Commencement Fund', 'commencement', 'Ceremony, honours and the transition out', true, 'finance:seed')
ON CONFLICT (code) DO NOTHING;
"#;

/// Migration 2's SQL, embedded at compile time from
/// `migrations/002_dues_funding.sql`.
///
/// It is a **new version**, never an edit to version 1: the core records applied
/// versions per schema and skips one it has already run without comparing its SQL
/// (`server/src/db.rs`, `server/src/plugin_runtime.rs`), so an amended version 1
/// would be invisible on every deployed database. It adds `funded_cents`, the
/// draw-state column and `draw_ref`, and replaces `dues_waived_is_zero` with the
/// three constraints that keep "waived but owing" unrepresentable while a waiver
/// names what was funded.
const DUES_FUNDING_MIGRATION: &str = include_str!("../migrations/002_dues_funding.sql");

#[async_trait]
impl AdjutantPlugin for FinancePlugin {
    fn id(&self) -> &str {
        "finance"
    }

    fn name(&self) -> &str {
        "Finance"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new("finance:read", "View fund balances and the sliding scale"),
            Permission::new(
                "finance:read_all",
                "View the ledger, budgets against actuals, dues and the Annual Financial Report",
            ),
            Permission::new(
                "finance:write",
                "Record income, expenses and transfers, and take dues payments",
            ),
            Permission::new(
                "finance:manage",
                "Create and edit funds, and set the budget a fund spends against",
            ),
            Permission::new(
                "finance:manage_dues",
                "Open dues assessments, set tiers, waive dues, and set a Lodge's levy",
            ),
            Permission::new(
                "finance:self_report",
                "Report your own sliding-scale dues tier (honor system)",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![
            Migration::new(1, "finance_schema", MIGRATION_SCHEMA),
            Migration::new(2, "dues_funding", DUES_FUNDING_MIGRATION),
        ]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        finance_routes(self.ctx())
    }

    fn subscriptions(&self) -> Vec<EventSubscription> {
        let ctx = self.ctx().clone(); // owned: the closure must not borrow self
        vec![EventSubscription::new(
            event_type::PAYMENT_RECEIVED,
            event_handler(move |ev| {
                let c = ctx.clone();
                async move { on_payment_received(&c, &ev).await }
            }),
        )]
    }

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone();
        vec![Schedule::new(
            "ledger_audit",
            std::time::Duration::from_secs(24 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                async move {
                    // Every day, re-derive the books and shout only if they do
                    // not add up: a silent schedule is a healthy one.
                    let verdict = ledger_integrity(&c).await?;
                    if verdict["balanced"] == json!(true) {
                        return Ok(());
                    }
                    c.events.publish("finance.ledger.imbalanced", verdict).await
                }
            }),
        )]
    }
}

/// Every route this plugin serves, in the order the API reference lists them.
fn finance_routes(ctx: &PluginContext) -> Vec<RouteDefinition> {
    vec![
        route_list_funds(ctx),
        route_create_fund(ctx),
        route_get_fund(ctx),
        route_edit_fund(ctx),
        route_list_transactions(ctx),
        route_record_transaction(ctx),
        route_transfer(ctx),
        route_list_budgets(ctx),
        route_set_budget(ctx),
        route_sliding_scale(ctx),
        route_assess_dues(ctx),
        route_repair_waived_dues(ctx),
        route_set_lodge_dues(ctx),
        route_get_lodge_dues(ctx),
        route_list_dues(ctx),
        route_get_member_dues(ctx),
        route_self_report(ctx),
        route_record_dues_payment(ctx),
        route_annual_report(ctx),
        route_health(ctx),
    ]
}

// ---------------------------------------------------------------------------
// Funds
// ---------------------------------------------------------------------------

/// `GET /api/finance/funds` — the six funds and what is in them.
///
/// One query. Every figure is derived from the ledger in that query, so a
/// balance cannot be stale.
fn route_list_funds(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/funds",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let include_inactive = req.query_bool("include_inactive");
                let rows =
                    c.db.query(sql_fund_list(&c), vec![SqlValue::Bool(include_inactive)])
                        .await?;
                let total: i64 = rows
                    .iter()
                    .map(|row| row["balance_cents"].as_i64().unwrap_or(0))
                    .sum();
                PluginResponse::json(
                    200,
                    &json!({
                        "funds": rows,
                        "total_cents": total,
                        "total_display": format_cents(total),
                        "include_inactive": include_inactive,
                        "note": "Balances are the sum of each fund's ledger, computed on read — \
                                 there is no stored balance to drift.",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/fund` — add a fund (a Lodge's own gear fund, say).
///
/// The six SPEC funds are seeded; this is for a troop that needs a seventh
/// envelope without inventing a seventh kind.
///
/// One query (`INSERT … ON CONFLICT (code) DO NOTHING RETURNING`; no row means
/// the code is taken), then the audit write.
fn route_create_fund(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/fund",
        "finance:manage",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: FundCreateBody = req.json()?;
                let kind = validate(normalize_kind(&body.kind))?;
                let code = match trimmed(&body.code) {
                    Some(raw) => validate(normalize_code(&raw))?,
                    None => kind.clone(),
                };
                let name = trimmed(&body.name).unwrap_or_else(|| default_fund_name(&kind));
                let purpose = trimmed(&body.purpose).unwrap_or_default();
                let restricted = body.restricted.unwrap_or_default();
                if body.target_cents.is_some_and(|target| target < 0) {
                    return PluginResponse::error(400, "target_cents must not be negative");
                }
                let creator = caller_of(&req).unwrap_or_default();

                let inserted =
                    c.db.query_one(
                        format!(
                            "INSERT INTO {funds} AS f \
                               (code, name, kind, purpose, restricted, target_cents, created_by) \
                             VALUES ($1, $2, $3, $4, $5, $6, $7) \
                             ON CONFLICT (code) DO NOTHING RETURNING {FUND_FIELDS}",
                            funds = c.db.table("funds")
                        ),
                        vec![
                            SqlValue::Text(code.clone()),
                            SqlValue::Text(name.clone()),
                            SqlValue::Text(kind.clone()),
                            SqlValue::Text(purpose.clone()),
                            SqlValue::Bool(restricted),
                            body.target_cents
                                .map(SqlValue::Int)
                                .unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(creator.clone()),
                        ],
                    )
                    .await?;
                let Some(fund) = inserted else {
                    return PluginResponse::error(
                        409,
                        format!("a fund with code {code:?} already exists"),
                    );
                };
                let id = fund["id"].as_i64().unwrap_or_default();
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "fund.create",
                        "fund",
                        &id.to_string(),
                        json!({
                            "code": code,
                            "kind": kind,
                            "restricted": restricted,
                            "target_cents": body.target_cents,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.fund.created",
                        json!({
                            "fund_id": id,
                            "code": fund["code"],
                            "kind": kind,
                            "restricted": restricted,
                            "created_by": creator,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/fund/{id}"),
                    &json!({
                        "fund": fund,
                        "balance_cents": 0,
                        "balance_display": format_cents(0),
                        "next": "record income with POST /api/finance/transaction",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/finance/fund/{id}` — one fund, its ledger page, and its budget.
///
/// Three queries: the fund and its derived figures; the fund's budget lines for
/// the fiscal year; the most recent entries.
fn route_get_fund(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/fund/{id}",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let fiscal_year = validate(fiscal_year_arg(
                    req.query_int("fiscal_year").map(|year| year as i32),
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                let Some(fund) =
                    c.db.query_one(sql_fund_one(&c), vec![SqlValue::Int(id)])
                        .await?
                else {
                    return PluginResponse::error(404, "no such fund");
                };
                let budgets =
                    c.db.query(
                        format!(
                            "{} ORDER BY b.direction, b.category",
                            sql_budget_lines_for_fund(&c)
                        ),
                        vec![SqlValue::Int(id), SqlValue::Int(i64::from(fiscal_year))],
                    )
                    .await?;
                let entries =
                    c.db.query(
                        format!(
                            "SELECT {TRANSACTION_FIELDS} FROM {tx} t \
                             WHERE t.fund_id = $1 ORDER BY t.id DESC LIMIT $2",
                            tx = c.db.table("transactions")
                        ),
                        vec![SqlValue::Int(id), SqlValue::Int(RECENT_ENTRIES)],
                    )
                    .await?;
                let budget_lines = budget_lines_with_variance(budgets);
                PluginResponse::json(
                    200,
                    &json!({
                        "fund": fund,
                        "fiscal_year": fiscal_year,
                        "budgets": budget_lines,
                        "entries": entries,
                    }),
                )
            }
        }),
    )
}

/// `PATCH /api/finance/fund/{id}` — rename, re-purpose, retire.
///
/// `code` is deliberately immutable: it is what a client and a configuration
/// name the fund by, and changing it would silently repoint
/// `dues_fund_code`. One query, then the audit write.
fn route_edit_fund(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::patch_protected(
        "/api/finance/fund/{id}",
        "finance:manage",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let id = req.int_param("id")?;
                let body: FundEditBody = req.json()?;
                let mut sets: Vec<String> = Vec::new();
                let mut params: Vec<SqlValue> = vec![SqlValue::Int(id)];
                if let Some(kind) = trimmed(&body.kind) {
                    set_clause(
                        &mut sets,
                        &mut params,
                        "kind",
                        SqlValue::Text(validate(normalize_kind(&kind))?),
                    );
                }
                if let Some(name) = trimmed(&body.name) {
                    set_clause(&mut sets, &mut params, "name", SqlValue::Text(name));
                }
                if let Some(purpose) = &body.purpose {
                    set_clause(
                        &mut sets,
                        &mut params,
                        "purpose",
                        SqlValue::Text(purpose.trim().to_string()),
                    );
                }
                if let Some(restricted) = body.restricted {
                    set_clause(
                        &mut sets,
                        &mut params,
                        "restricted",
                        SqlValue::Bool(restricted),
                    );
                }
                if let Some(active) = body.active {
                    set_clause(&mut sets, &mut params, "active", SqlValue::Bool(active));
                }
                if let Some(target) = body.target_cents {
                    if target.is_some_and(|cents| cents < 0) {
                        return PluginResponse::error(400, "target_cents must not be negative");
                    }
                    set_clause(
                        &mut sets,
                        &mut params,
                        "target_cents",
                        target.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                    );
                }
                if sets.is_empty() {
                    return PluginResponse::error(400, "no editable field was supplied");
                }
                let row =
                    c.db.query_one(
                        format!(
                            "UPDATE {funds} AS f SET {sets}, updated_at = now() \
                             WHERE f.id = $1 RETURNING {FUND_FIELDS}",
                            funds = c.db.table("funds"),
                            sets = sets.join(", ")
                        ),
                        params,
                    )
                    .await?;
                let Some(fund) = row else {
                    return PluginResponse::error(404, "no such fund");
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "fund.update",
                        "fund",
                        &id.to_string(),
                        json!({ "fields": sets.len(), "code": fund["code"] }),
                    )
                    .await?;
                PluginResponse::json(200, &json!({ "fund": fund }))
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// The filter every ledger query shares (`$1`…`$8`), so the page and the total
/// it reports cannot disagree about what they are counting.
const LEDGER_FILTER: &str = r#"
    ($1::bigint IS NULL OR t.fund_id = $1)
    AND ($2::bigint IS NULL OR t.fiscal_year = $2)
    AND ($3::text IS NULL OR t.kind = $3)
    AND ($4::text IS NULL OR t.category = $4)
    AND ($5::text IS NULL OR t.member_id = $5)
    AND ($6::text IS NULL OR t.transfer_group::text = $6)
    AND ($7::date IS NULL OR t.occurred_on >= $7::date)
    AND ($8::date IS NULL OR t.occurred_on <= $8::date)
"#;

/// The transfer statement: **one** `INSERT … SELECT`, so the two legs are all or
/// nothing.
///
/// Each leg is named by an **id or a code** — exactly one of the two — and both
/// are resolved here, inside this statement, where the funds table lives (`ref`).
/// That is what makes a transfer possible for a producer that holds only a fund
/// **code** (the reference `plugin-to-plugin.md` §3.5 asks for) without a read of
/// its own; an id is still accepted for a caller that already has one.
///
/// `$1`/`$2` are the origin reference (an id, or a code — exactly one non-null),
/// `$3`/`$4` the destination's, `$5` the two signed amounts `[-a, +a]` (computed
/// in Rust, where `-a` and `+a` are the same validated number), `$6`…`$10` the
/// shared fields, `$11` the overdraft flag, `$12` the transfer's own idempotency
/// key (nullable — a caller that gives none always writes). `gen_random_uuid()`
/// gives both legs one group; the guard requires both references to resolve *and*
/// the two funds to differ *and* the out-leg not to overdraw, and because it sits
/// on the single `SELECT` both rows pass it or neither is written.
///
/// `$12` is written on the **out-leg only**, because
/// `idx_transactions_external_ref` is unique and one transfer is one money move,
/// not two: a retry carrying the same key writes **no rows at all** (the
/// `NOT EXISTS` guard), and [`explain_no_transfer`] answers it with the transfer
/// it already made. Writing it on both legs would collide with itself.
const TRANSFER_SQL: &str = r#"
WITH ref AS (
  SELECT
    (SELECT f.id FROM {funds} f
      WHERE ($1::bigint IS NOT NULL AND f.id = $1)
         OR ($2::text IS NOT NULL AND f.code = $2)) AS from_id,
    (SELECT f.id FROM {funds} f
      WHERE ($3::bigint IS NOT NULL AND f.id = $3)
         OR ($4::text IS NOT NULL AND f.code = $4)) AS to_id
), g AS (SELECT gen_random_uuid() AS id)
INSERT INTO {tx} AS t
  (fund_id, amount_cents, kind, transfer_group, counterparty_fund_id, category, description,
   fiscal_year, occurred_on, recorded_by, overdraft_authorized, external_ref)
SELECT v.fund_id, v.amount_cents, 'transfer', g.id,
       CASE WHEN v.amount_cents < 0 THEN r.to_id ELSE r.from_id END,
       $6, $7, $8, $9::date, $10, $11,
       CASE WHEN v.amount_cents < 0 THEN $12::text ELSE NULL END
FROM ref r
CROSS JOIN g
CROSS JOIN LATERAL unnest($5::bigint[], ARRAY[r.from_id, r.to_id]::bigint[])
       AS v(amount_cents, fund_id)
WHERE r.from_id IS NOT NULL
  AND r.to_id IS NOT NULL
  AND r.from_id <> r.to_id
  AND ($12::text IS NULL
       OR NOT EXISTS (SELECT 1 FROM {tx} x WHERE x.external_ref = $12::text))
  AND ($11 OR (SELECT COALESCE(SUM(amount_cents), 0) FROM {tx} WHERE fund_id = r.from_id) + $5[1] >= 0)
RETURNING t.id, t.fund_id, t.amount_cents, t.kind, t.transfer_group::text AS transfer_group,
          t.counterparty_fund_id, t.category, t.description, t.fiscal_year,
          t.occurred_on::text AS occurred_on, t.recorded_by, t.external_ref,
          t.created_at::text AS created_at
"#;

/// `GET /api/finance/transactions` — the ledger, filtered and paged.
///
/// Two queries: the page (one row more than asked for, which is how `has_more`
/// is known without a `COUNT(*)` over the same filters) and the filtered total.
fn route_list_transactions(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/transactions",
        "finance:read_all",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let kind = match req.query_param("kind") {
                    Some(raw) => Some(validate(normalize_ledger_kind(raw))?),
                    None => None,
                };
                let (from, to) = match (req.query_param("from"), req.query_param("to")) {
                    (Some(from), Some(to)) => (
                        Some(validate(occurred_on(&Some(from.to_string())))?),
                        Some(validate(occurred_on(&Some(to.to_string())))?),
                    ),
                    (Some(from), None) => {
                        (Some(validate(occurred_on(&Some(from.to_string())))?), None)
                    }
                    (None, Some(to)) => (None, Some(validate(occurred_on(&Some(to.to_string())))?)),
                    (None, None) => (None, None),
                };
                let limit = req
                    .query_int("limit")
                    .unwrap_or(DEFAULT_LEDGER_LIMIT)
                    .clamp(1, MAX_LEDGER_LIMIT);
                let before_id = req.query_int("before_id");
                let fund_id = req.query_int("fund_id");
                let fiscal_year = req.query_int("fiscal_year");
                let category = req
                    .query_param("category")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let member_id = req
                    .query_param("member_id")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let transfer_group = req
                    .query_param("transfer_group")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);

                let shared = vec![
                    fund_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                    fiscal_year.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                    kind.clone().into(),
                    category.clone().into(),
                    member_id.clone().into(),
                    transfer_group.clone().into(),
                    from.map(|date| SqlValue::Text(date.to_string()))
                        .unwrap_or(SqlValue::Null),
                    to.map(|date| SqlValue::Text(date.to_string()))
                        .unwrap_or(SqlValue::Null),
                ];
                let mut page_params = shared.clone();
                page_params.push(before_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt));
                page_params.push(SqlValue::Int(limit + 1));

                let mut rows =
                    c.db.query(
                        format!(
                            "SELECT {TRANSACTION_LIST_FIELDS} FROM {tx} t \
                             JOIN {funds} f ON f.id = t.fund_id \
                             WHERE {LEDGER_FILTER} \
                               AND ($9::bigint IS NULL OR t.id < $9) \
                             ORDER BY t.id DESC LIMIT $10",
                            tx = c.db.table("transactions"),
                            funds = c.db.table("funds")
                        ),
                        page_params,
                    )
                    .await?;
                let has_more = rows.len() as i64 > limit;
                rows.truncate(limit as usize);
                let page_total: i64 = rows
                    .iter()
                    .map(|row| row["amount_cents"].as_i64().unwrap_or(0))
                    .sum();
                let totals =
                    c.db.query_one(
                        format!(
                            "SELECT COALESCE(SUM(t.amount_cents), 0)::bigint AS total_cents, \
                                    COUNT(*)::bigint AS entries \
                             FROM {tx} t WHERE {LEDGER_FILTER}",
                            tx = c.db.table("transactions")
                        ),
                        shared,
                    )
                    .await?
                    .unwrap_or_else(|| json!({ "total_cents": 0, "entries": 0 }));
                let next_before_id = if has_more {
                    rows.last().and_then(|row| row["id"].as_i64())
                } else {
                    None
                };
                PluginResponse::json(
                    200,
                    &json!({
                        "transactions": rows,
                        "count": rows.len(),
                        "has_more": has_more,
                        "next_before_id": next_before_id,
                        "page_total_cents": page_total,
                        "filtered_total_cents": totals["total_cents"],
                        "filtered_entries": totals["entries"],
                        "filters": {
                            "fund_id": fund_id,
                            "fiscal_year": fiscal_year,
                            "kind": kind,
                            "category": category,
                            "member_id": member_id,
                            "transfer_group": transfer_group,
                            "from": from.map(|date| date.to_string()),
                            "to": to.map(|date| date.to_string()),
                            "before_id": before_id,
                            "limit": limit,
                        },
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/transaction` — record one income or expense.
///
/// The amount is a **magnitude**: the sign is the `kind`'s business (`income` is
/// stored positive, `expense` negative), so a client cannot record an expense
/// that adds money.
///
/// The fund is named by `fund_id` **or** by `fund_code`, and resolved inside the
/// insert's own statement — so a producer that holds only a fund *code* (the
/// reference, §3.5) writes without reading finance first, and finance's primary
/// key never has to leave its own schema except as an answer. Exactly one of the
/// two: both together is refused, because finance would have to guess which one
/// the caller meant.
///
/// Queries: one guarded `INSERT … SELECT`; when it writes nothing, one more to
/// say *why* (unknown fund, a replayed `external_ref`, or an overdraft — see
/// [`explain_no_entry`]); then the fund's new balance. Then the audit write.
fn route_record_transaction(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/transaction",
        "finance:write",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: TransactionBody = req.json()?;
                let kind = validate(normalize_direct_kind(&body.kind))?;
                let magnitude = validate(resolve_amount(
                    &body.amount_cents,
                    &body.amount,
                    "amount_cents",
                ))?;
                if magnitude <= 0 {
                    return PluginResponse::error(
                        400,
                        format!(
                            "amount must be a positive magnitude in cents — the sign comes \
                             from kind ({kind}), so an {kind} cannot add money the other way"
                        ),
                    );
                }
                let date = validate(occurred_on(&body.occurred_on))?;
                let fiscal_year = validate(fiscal_year_arg(body.fiscal_year, date, &c.config))?;
                let fund_ref = match (body.fund_id, trimmed(&body.fund_code)) {
                    (Some(id), None) => FundRef::Id(id),
                    (None, Some(code)) => FundRef::Code(code),
                    (Some(_), Some(code)) => {
                        return PluginResponse::error(
                            400,
                            format!(
                                "pass either fund_id or fund_code, not both: {code:?} would be a \
                                 second answer to the same question, and finance would have to \
                                 choose which one you meant"
                            ),
                        )
                    }
                    (None, None) => {
                        return PluginResponse::error(
                            400,
                            "the entry names no fund: pass fund_id, or the fund's code as \
                             fund_code (either is resolved here, inside the same statement)",
                        )
                    }
                };
                let entry = LedgerEntry {
                    fund: fund_ref,
                    amount_cents: if kind == KIND_INCOME {
                        magnitude
                    } else {
                        -magnitude
                    },
                    kind: if kind == KIND_INCOME {
                        KIND_INCOME
                    } else {
                        KIND_EXPENSE
                    },
                    category: trimmed(&body.category).unwrap_or_default(),
                    description: trimmed(&body.description).unwrap_or_default(),
                    member_id: trimmed(&body.member_id).unwrap_or_default(),
                    fiscal_year,
                    occurred_on: date,
                    recorded_by: caller_of(&req).unwrap_or_default(),
                    overdraft_authorized: body.allow_overdraft.unwrap_or(false),
                    external_ref: trimmed(&body.external_ref),
                };
                let rows = insert_entry(&c, &entry).await?;
                let Some(recorded) = rows.first().cloned() else {
                    return explain_no_entry(&c, &entry).await;
                };
                // The fund the entry **landed in** — the row's own id, which is
                // what a `fund_code` entry has only after the insert resolved it.
                let fund_id = recorded["fund_id"].as_i64().unwrap_or_default();
                let balance = fund_balance_cents(&c, fund_id).await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "transaction.record",
                        "transaction",
                        &recorded["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "fund_id": fund_id,
                            "kind": entry.kind,
                            "amount_cents": entry.amount_cents,
                            "category": entry.category,
                            "member_id": entry.member_id,
                            "fiscal_year": entry.fiscal_year,
                            "occurred_on": entry.occurred_on.to_string(),
                            "overdraft_authorized": entry.overdraft_authorized,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.transaction.recorded",
                        json!({
                            "transaction_id": recorded["id"],
                            "fund_id": fund_id,
                            "kind": entry.kind,
                            "amount_cents": entry.amount_cents,
                            "category": entry.category,
                            "member_id": entry.member_id,
                            "recorded_by": entry.recorded_by,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/transaction/{}", recorded["id"]),
                    &json!({
                        "transaction": recorded,
                        "fund_id": fund_id,
                        "balance_cents": balance,
                        "balance_display": format_cents(balance),
                        "overdrawn": balance < 0,
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/transfer` — move money between two funds.
///
/// One statement ([`TRANSFER_SQL`]) writes both legs under one group, so a
/// failure writes neither and the sum of all funds is unchanged by the call.
/// Refused when the out-leg would take a fund negative unless
/// `allow_overdraft` is set — and then the entry records that it was authorised
/// (`overdraft_authorized`), because an overdraft is a decision somebody made,
/// not a rounding artefact.
///
/// Queries: the statement; when it does not return two rows, one more to explain
/// (`explain_no_transfer`); then the two funds' balances. Then the audit write.
fn route_transfer(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/transfer",
        "finance:write",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: TransferBody = req.json()?;
                let magnitude = validate(resolve_amount(
                    &body.amount_cents,
                    &body.amount,
                    "amount_cents",
                ))?;
                if magnitude <= 0 {
                    return PluginResponse::error(
                        400,
                        "amount must be a positive magnitude in cents — a transfer moves money \
                         one way, and the destination names the direction",
                    );
                }
                let from_ref = match (body.from_fund_id, trimmed(&body.from_fund_code)) {
                    (Some(id), None) => FundRef::Id(id),
                    (None, Some(code)) => FundRef::Code(code),
                    (Some(_), Some(code)) => {
                        return PluginResponse::error(
                            400,
                            format!(
                                "pass either from_fund_id or from_fund_code, not both: {code:?} \
                                 would be a second answer to the same question, and finance would \
                                 have to choose which one you meant"
                            ),
                        )
                    }
                    (None, None) => {
                        return PluginResponse::error(
                            400,
                            "the transfer names no origin fund: pass from_fund_id, or the fund's \
                             code as from_fund_code (either is resolved here, inside the same \
                             statement)",
                        )
                    }
                };
                let to_ref = match (body.to_fund_id, trimmed(&body.to_fund_code)) {
                    (Some(id), None) => FundRef::Id(id),
                    (None, Some(code)) => FundRef::Code(code),
                    (Some(_), Some(code)) => {
                        return PluginResponse::error(
                            400,
                            format!(
                                "pass either to_fund_id or to_fund_code, not both: {code:?} would \
                                 be a second answer to the same question, and finance would have \
                                 to choose which one you meant"
                            ),
                        )
                    }
                    (None, None) => {
                        return PluginResponse::error(
                            400,
                            "the transfer names no destination fund: pass to_fund_id, or the \
                             fund's code as to_fund_code (either is resolved here, inside the \
                             same statement)",
                        )
                    }
                };
                // Two references that are *the same kind* can be compared here, and
                // refusing them before the statement keeps the existing 400. Two of
                // different kinds (an id and a code) can only be compared once the
                // code is resolved, which the statement's own guard does — and
                // `explain_no_transfer` says so in the same words.
                let same_reference = match (&from_ref, &to_ref) {
                    (FundRef::Id(from), FundRef::Id(to)) => from == to,
                    (FundRef::Code(from), FundRef::Code(to)) => from == to,
                    _ => false,
                };
                if same_reference {
                    return PluginResponse::error(
                        400,
                        "a transfer needs two different funds (use a transaction to correct a \
                         fund's own entry)",
                    );
                }
                let date = validate(occurred_on(&body.occurred_on))?;
                let fiscal_year = validate(fiscal_year_arg(body.fiscal_year, date, &c.config))?;
                let overdraft = body.allow_overdraft.unwrap_or(false);
                let legs = vec![-magnitude, magnitude];
                let category = CATEGORY_TRANSFER.to_string();
                let description = trimmed(&body.description).unwrap_or_default();
                let external_ref = trimmed(&body.external_ref);
                let recorder = caller_of(&req).unwrap_or_default();
                let (from_id_bind, from_code_bind) = from_ref.bind();
                let (to_id_bind, to_code_bind) = to_ref.bind();

                let rows =
                    c.db.query(
                        // `.replace`, not `format!`: the const's `{tx}`/`{funds}`
                        // are in its *value*, and a format string never looks
                        // inside the value it interpolates.
                        TRANSFER_SQL
                            .replace("{tx}", &c.db.table("transactions"))
                            .replace("{funds}", &c.db.table("funds")),
                        vec![
                            from_id_bind,
                            from_code_bind,
                            to_id_bind,
                            to_code_bind,
                            SqlValue::IntArray(legs),
                            SqlValue::Text(category.clone()),
                            SqlValue::Text(description.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                            SqlValue::Text(date.to_string()),
                            SqlValue::Text(recorder.clone()),
                            SqlValue::Bool(overdraft),
                            external_ref.clone().into(),
                        ],
                    )
                    .await?;
                if rows.len() != 2 {
                    // A reference the ledger already holds is not a refusal, it is
                    // the **same transfer**: the caller retried a money move whose
                    // answer it lost. Answer with the transfer that already exists
                    // rather than a second pair of legs — which is why the draw
                    // carries a deterministic reference.
                    if let Some(reference) = &external_ref {
                        if let Some(existing) = existing_transfer(&c, reference).await? {
                            return PluginResponse::json(
                                200,
                                &json!({
                                    "duplicate": true,
                                    "transfer_group": existing["transfer_group"],
                                    "entries": existing["entries"],
                                    "group_sum_cents": existing["group_sum_cents"],
                                    "amount_cents": magnitude,
                                    "external_ref": reference,
                                    "note": format!(
                                        "external_ref {reference:?} already holds this transfer — \
                                         nothing was written a second time"
                                    ),
                                }),
                            );
                        }
                    }
                    return explain_no_transfer(&c, &from_ref, &to_ref, magnitude, overdraft).await;
                }
                // The ids the statement resolved, read from its own rows: the out
                // leg is the negative one, so this is where a code-named fund's id
                // comes from without a second statement.
                let leg_fund = |sign_positive: bool| {
                    rows.iter()
                        .find(|row| {
                            row["amount_cents"].as_i64().unwrap_or(0).is_positive()
                                == sign_positive
                        })
                        .and_then(|row| row["fund_id"].as_i64())
                        .unwrap_or_default()
                };
                let from_fund_id = leg_fund(false);
                let to_fund_id = leg_fund(true);
                let funds = vec![from_fund_id, to_fund_id];
                // Defence in depth: the statement's two legs are `-a` and `+a`,
                // and this says so out loud rather than trusting that.
                let legs_sum: i64 = rows
                    .iter()
                    .map(|row| row["amount_cents"].as_i64().unwrap_or(0))
                    .sum();
                if legs_sum != 0 {
                    return Err(SdkError::Internal(format!(
                        "a transfer's legs summed to {legs_sum} cents, which is not a transfer"
                    )));
                }
                let balances =
                    c.db.query(
                        sql_fund_balances(&c),
                        vec![SqlValue::IntArray(funds.clone())],
                    )
                    .await?;
                let group = rows[0]["transfer_group"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "transaction.transfer",
                        "transfer",
                        &group,
                        json!({
                            "from_fund_id": from_fund_id,
                            "to_fund_id": to_fund_id,
                            "amount_cents": magnitude,
                            "fiscal_year": fiscal_year,
                            "occurred_on": date.to_string(),
                            "overdraft_authorized": overdraft,
                            "entries": rows.len(),
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.transfer.recorded",
                        json!({
                            "transfer_group": rows[0]["transfer_group"],
                            "from_fund_id": from_fund_id,
                            "to_fund_id": to_fund_id,
                            "amount_cents": magnitude,
                            "entries": rows.len(),
                            "sum_cents": legs_sum,
                            "recorded_by": recorder,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/transactions?transfer_group={group}"),
                    &json!({
                        "transfer_group": rows[0]["transfer_group"],
                        "entries": rows,
                        "amount_cents": magnitude,
                        "sum_cents": legs_sum,
                        "balances": balances,
                        "note": "One statement wrote both legs: the sum of every fund is \
                                 unchanged by this call.",
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// `GET /api/finance/budgets` — the year's plan, next to what actually happened.
///
/// One query. The actual is summed from the ledger on read (the `LATERAL` in
/// [`sql_budget_lines`]), and the variance is computed in
/// [`budget_variance`] — so there is no stored "remaining" anywhere to drift.
fn route_list_budgets(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/budgets",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let fiscal_year = match req.query_int("fiscal_year") {
                    Some(year) => Some(validate(fiscal_year_arg(
                        Some(year as i32),
                        Utc::now().date_naive(),
                        &c.config,
                    ))?),
                    None => None,
                };
                let rows =
                    c.db.query(
                        sql_budget_lines(&c),
                        vec![fiscal_year
                            .map(|year| SqlValue::Int(i64::from(year)))
                            .unwrap_or(SqlValue::NullInt)],
                    )
                    .await?;
                let lines = budget_lines_with_variance(rows);
                PluginResponse::json(
                    200,
                    &json!({
                        "fiscal_year": fiscal_year.map(|year| year.to_string()).unwrap_or_else(|| "all".to_string()),
                        "lines": lines,
                        "totals": budget_totals(&lines),
                        "note": "variance_cents is favourable-positive: ahead of plan on income, \
                                 under plan on an expense. A whole-fund line (category \"\") that \
                                 sits beside category lines reports overlaps_categories, because \
                                 its actual then includes theirs.",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/budget` — set (or replace) a budget line.
///
/// A line is one fund, one year, one direction, one category — the uniqueness the
/// database enforces, so posting the same line again revises it rather than
/// creating a second one. An empty category is the fund's envelope for that
/// direction.
///
/// One query (the upsert; no row means the fund does not exist), then the audit
/// write.
fn route_set_budget(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/budget",
        "finance:manage",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: BudgetBody = req.json()?;
                let direction = validate(normalize_direction(&body.direction))?;
                let planned = validate(resolve_amount(
                    &body.amount_cents,
                    &body.amount,
                    "amount_cents",
                ))?;
                if planned <= 0 {
                    return PluginResponse::error(
                        400,
                        "planned_cents must be positive — a budget plans a direction, and the \
                         direction says which way the money moves",
                    );
                }
                let date = Utc::now().date_naive();
                let fiscal_year = validate(fiscal_year_arg(body.fiscal_year, date, &c.config))?;
                let category = trimmed(&body.category).unwrap_or_default();
                let note = trimmed(&body.note).unwrap_or_default();
                let author = caller_of(&req).unwrap_or_default();
                let row =
                    c.db.query_one(
                        format!(
                            "INSERT INTO {budgets} AS b \
                               (fund_id, fiscal_year, direction, category, planned_cents, note, \
                                created_by) \
                             SELECT $1, $2, $3, $4, $5, $6, $7 \
                             WHERE EXISTS (SELECT 1 FROM {funds} f WHERE f.id = $1) \
                             ON CONFLICT (fund_id, fiscal_year, direction, category) DO UPDATE \
                               SET planned_cents = EXCLUDED.planned_cents, \
                                   note = EXCLUDED.note, updated_at = now() \
                             RETURNING {BUDGET_FIELDS}",
                            budgets = c.db.table("budgets"),
                            funds = c.db.table("funds")
                        ),
                        vec![
                            SqlValue::Int(body.fund_id),
                            SqlValue::Int(i64::from(fiscal_year)),
                            SqlValue::Text(direction.clone()),
                            SqlValue::Text(category.clone()),
                            SqlValue::Int(planned),
                            SqlValue::Text(note.clone()),
                            SqlValue::Text(author.clone()),
                        ],
                    )
                    .await?;
                let Some(line) = row else {
                    return PluginResponse::error(404, "no such fund");
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "budget.set",
                        "budget",
                        &line["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "fund_id": body.fund_id,
                            "fiscal_year": fiscal_year,
                            "direction": direction,
                            "category": category,
                            "planned_cents": planned,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.budget.set",
                        json!({
                            "budget_id": line["id"],
                            "fund_id": body.fund_id,
                            "fiscal_year": fiscal_year,
                            "direction": direction,
                            "category": category,
                            "planned_cents": planned,
                            "set_by": author,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/budgets?fiscal_year={fiscal_year}"),
                    &json!({ "budget": line, "planned_display": format_cents(planned) }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// The sliding scale
// ---------------------------------------------------------------------------

/// `GET /api/finance/sliding-scale` — the whole scale for a membership cost.
///
/// No database call at all: the scale is a constant table and the arithmetic is
/// [`assessed_cents`]. `base_cents` may be given; otherwise the troop's
/// configured membership cost is used, and a troop that has configured nothing
/// sees a scale of zeroes rather than a made-up number.
fn route_sliding_scale(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/sliding-scale",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let configured = configured_membership_cost(&c.config);
                let base_cents = req
                    .query_int("base_cents")
                    .map(|base| base.max(0))
                    .or(configured)
                    .unwrap_or(0);
                PluginResponse::json(
                    200,
                    &json!({
                        "base_cents": base_cents,
                        "base_display": format_cents(base_cents),
                        "base_configured": base_cents > 0,
                        "minimum_cents": MINIMUM_DUES_CENTS,
                        "minimum_display": format_cents(MINIMUM_DUES_CENTS),
                        "tiers": scale_table(base_cents),
                        "honor_system": true,
                        "note": "A scout reports their own tier — this software has no income \
                                 verification and no field for one. Hardship assesses $0 and \
                                 nobody is turned away for it.",
                        "next": "self-report with POST /api/finance/dues/self-report \
                                 {\"tier\": \"supported\"}",
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Dues
// ---------------------------------------------------------------------------

/// `POST /api/finance/dues/assess` — open or revise a member's assessment.
///
/// This is the treasurer's route: the tier here is the one the troop records for
/// a scout who has not self-reported (or the one a scout asks for in person). A
/// waiver is `status: "waived"`, and it is **not** a price of zero: the tier's
/// assessment is written honestly and the whole of it is funded from
/// `scholarship`, so the row says both what the member was measured at and who
/// paid for it. The member's own share is `assessed_cents - funded_cents`, which
/// is zero for a waiver and can never go negative.
///
/// The draw is booked **in this same flow when the caller also holds
/// `finance:write`**, as the caller: the transfer is called with their own
/// request, their identity and nothing added. When they do not hold it, the
/// waiver is neither refused nor silently unfunded — the row records the funded
/// amount and an **outstanding** draw, and a `finance:write` holder books it
/// later with `POST /api/finance/transfer`. No credential is minted or
/// substituted for either case.
///
/// Queries: the permission check when there is something to fund, the upsert,
/// then — when the caller can book — the transfer's own statements and the
/// second half of the booking.
fn route_assess_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/dues/assess",
        "finance:manage_dues",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: DuesAssessBody = req.json()?;
                let member = body.member_id.trim();
                if member.is_empty() {
                    return PluginResponse::error(400, "member_id is required");
                }
                let tier = validate(normalize_tier(&body.tier))?;
                let status = match trimmed(&body.status) {
                    Some(raw) => validate(normalize_status(&raw))?,
                    None => STATUS_ASSESSED.to_string(),
                };
                let base_cents = body
                    .base_cents
                    .or_else(|| configured_membership_cost(&c.config))
                    .unwrap_or(0);
                if base_cents < 0 {
                    return PluginResponse::error(400, "base_cents must not be negative");
                }
                let Some(assessed_cents) = tier_assessment(base_cents, &tier) else {
                    return Err(SdkError::Internal(format!(
                        "tier {tier:?} validated but carries no share"
                    )));
                };
                let fiscal_year = validate(fiscal_year_arg(
                    body.fiscal_year,
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                // The tier's assessment is written whatever the status says: a
                // waiver funds all of it, anything else funds the discount the
                // tier gives (and never more than what is owed).
                let funded_cents = funded_cents_for(&status, base_cents, assessed_cents);
                let draw_reference = dues_draw_reference(fiscal_year, member);
                let dues_fund = configured_dues_fund(&c.config);
                // `finance:manage_dues` waives; `finance:write` books the ledger
                // write. They are separate permissions and not the same caller by
                // default, so the draw is booked only when this caller holds both.
                let can_book = funded_cents > 0
                    && c.permissions
                        .has_in_scope(req.identity.as_ref(), "finance:write", &Scope::troop())
                        .await;
                let opening_state = if funded_cents <= 0 {
                    DRAW_NONE
                } else if can_book {
                    DRAW_ATTEMPTING
                } else {
                    DRAW_UNBOOKED
                };
                let assessment = Assessment {
                    fiscal_year,
                    dues_kind: DUES_KIND_MEMBER,
                    member_id: member.to_string(),
                    lodge_id: trimmed(&body.lodge_id).unwrap_or_default(),
                    tier: tier.clone(),
                    share_bps: None,
                    base_cents,
                    assessed_cents,
                    funded_cents,
                    draw_status: opening_state.to_string(),
                    draw_ref: None,
                    self_reported: false,
                    status: status.clone(),
                    note: trimmed(&body.note).unwrap_or_default(),
                    recorded_by: caller_of(&req).unwrap_or_default(),
                };
                let Some(mut dues) = upsert_dues(&c, &assessment).await? else {
                    return Err(SdkError::Internal("the dues upsert returned no row".into()));
                };
                // The draw, booked as this caller when they hold `finance:write`.
                let mut draw_note: Option<String> = None;
                if can_book {
                    let (state, reference, note) = match book_dues_draw(
                        &c,
                        &req,
                        member,
                        fiscal_year,
                        funded_cents,
                        &dues_fund,
                        &draw_reference,
                    )
                    .await?
                    {
                        DrawOutcome::Booked(group) => (DRAW_BOOKED, Some(group), None),
                        DrawOutcome::Refused(message) => (DRAW_REFUSED, None, Some(message)),
                        DrawOutcome::Failed(message) => (DRAW_FAILED, None, Some(message)),
                    };
                    draw_note = note;
                    if let Some(updated) =
                        set_dues_draw_state(&c, fiscal_year, member, state, reference.as_deref())
                            .await?
                    {
                        dues = updated;
                    }
                }
                let draw_status = dues["draw_status"]
                    .as_str()
                    .unwrap_or(opening_state)
                    .to_string();
                let draw_ref = dues["draw_ref"].as_str().map(str::to_string);
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "dues.assess",
                        "dues",
                        &dues["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "member_id": member,
                            "fiscal_year": fiscal_year,
                            "tier": tier,
                            "status": status,
                            "base_cents": base_cents,
                            "assessed_cents": assessed_cents,
                            "funded_cents": funded_cents,
                            "draw_status": draw_status,
                            "draw_ref": draw_ref,
                            "draw_reference": draw_reference,
                            "draw_note": draw_note,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.dues.assessed",
                        json!({
                            "member_id": member,
                            "fiscal_year": fiscal_year,
                            "tier": tier,
                            "status": status,
                            "assessed_cents": assessed_cents,
                            "funded_cents": funded_cents,
                            "draw_status": draw_status,
                            "draw_ref": draw_ref,
                            "draw_fund_code": dues_fund,
                            "self_reported": false,
                            "assessed_by": assessment.recorded_by,
                        }),
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "dues": dues,
                        "assessed_cents": assessed_cents,
                        "assessed_display": format_cents(assessed_cents),
                        "funded_cents": funded_cents,
                        "funded_display": format_cents(funded_cents),
                        "draw_status": draw_status,
                        "draw_ref": draw_ref,
                        "draw_reference": draw_reference,
                        "draw_fund_code": dues_fund,
                        "draw_note": draw_note,
                        "member_share_cents": (assessed_cents - funded_cents).max(0),
                        "scale": scale_table(base_cents),
                        "next": "the scout may change their own tier at \
                                 POST /api/finance/dues/self-report",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/dues/lodge` — a Lodge's levy, as a fraction of the
/// membership cost (SPEC §7.5).
///
/// Scoped: the caller needs `finance:manage_dues` **covering that Lodge**. One
/// permission query, then the upsert, then the audit write.
fn route_set_lodge_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/finance/dues/lodge",
        "finance:manage_dues",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: LodgeDuesBody = req.json()?;
                let lodge = body.lodge_id.trim().to_string();
                if lodge.is_empty() {
                    return PluginResponse::error(
                        400,
                        "lodge_id is required — a Lodge's levy is the Lodge's own",
                    );
                }
                let share_bps = match (body.share_bps, trimmed(&body.share_percent)) {
                    (Some(bps), None) => bps,
                    (None, Some(percent)) => validate(parse_percent_to_bps(&percent))?,
                    (Some(_), Some(_)) => {
                        return PluginResponse::error(
                            400,
                            "give share_bps or share_percent, not both",
                        )
                    }
                    (None, None) => {
                        return PluginResponse::error(
                            400,
                            "share_bps (or share_percent, e.g. \"10\" or \"12.5\") is required",
                        )
                    }
                };
                if !(0..=MAX_SHARE_BPS).contains(&share_bps) {
                    return PluginResponse::error(
                        400,
                        format!("share_bps must be between 0 and {MAX_SHARE_BPS} (1000%)"),
                    );
                }
                let base_cents = body
                    .base_cents
                    .or_else(|| configured_membership_cost(&c.config))
                    .unwrap_or(0);
                if base_cents < 0 {
                    return PluginResponse::error(400, "base_cents must not be negative");
                }
                let fiscal_year = validate(fiscal_year_arg(
                    body.fiscal_year,
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                c.permissions
                    .reach(
                        req.identity.as_ref(),
                        "finance:manage_dues",
                        &Scope::lodge(&lodge),
                    )
                    .await?;
                let assessed_cents = lodge_levy_cents(base_cents, share_bps);
                let assessment = Assessment {
                    fiscal_year,
                    dues_kind: DUES_KIND_LODGE,
                    member_id: String::new(),
                    lodge_id: lodge.clone(),
                    tier: String::new(),
                    share_bps: Some(share_bps),
                    base_cents,
                    assessed_cents,
                    // A Lodge's levy is the Lodge's own fraction of the membership
                    // cost, not a member's access: nothing funds it, so there is
                    // no draw to book.
                    funded_cents: 0,
                    draw_status: DRAW_NONE.to_string(),
                    draw_ref: None,
                    self_reported: false,
                    status: STATUS_ASSESSED.to_string(),
                    note: trimmed(&body.note).unwrap_or_default(),
                    recorded_by: caller_of(&req).unwrap_or_default(),
                };
                let row = upsert_dues(&c, &assessment).await?;
                let Some(dues) = row else {
                    return Err(SdkError::Internal("the dues upsert returned no row".into()));
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "dues.assess.lodge",
                        "dues",
                        &dues["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "lodge_id": lodge,
                            "fiscal_year": fiscal_year,
                            "share_bps": share_bps,
                            "share_percent": format_percent(share_bps),
                            "base_cents": base_cents,
                            "assessed_cents": assessed_cents,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.dues.assessed",
                        json!({
                            "lodge_id": lodge,
                            "fiscal_year": fiscal_year,
                            "share_bps": share_bps,
                            "assessed_cents": assessed_cents,
                            "assessed_by": assessment.recorded_by,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/dues/lodge/{lodge}?fiscal_year={fiscal_year}"),
                    &json!({
                        "dues": dues,
                        "share_percent": format_percent(share_bps),
                        "assessed_display": format_cents(assessed_cents),
                        "arithmetic": format!(
                            "{} of {} = {}",
                            format_percent(share_bps),
                            format_cents(base_cents),
                            format_cents(assessed_cents)
                        ),
                    }),
                )
            }
        }),
    )
}

/// `GET /api/finance/dues/lodge/{lodge}` — a Lodge's levy and its members' dues.
///
/// One permission query (the caller needs `finance:read` covering that Lodge),
/// then the levy row and the Lodge's members with their standing.
fn route_get_lodge_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/finance/dues/lodge/{lodge}",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let Some(lodge) = req
                    .param("lodge")
                    .map(str::trim)
                    .filter(|lodge| !lodge.is_empty())
                    .map(str::to_string)
                else {
                    return Err(SdkError::Internal(
                        "route /api/finance/dues/lodge/{lodge} has no {lodge} capture".into(),
                    ));
                };
                let fiscal_year = validate(fiscal_year_arg(
                    req.query_int("fiscal_year").map(|year| year as i32),
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                c.permissions
                    .reach(req.identity.as_ref(), "finance:read", &Scope::lodge(&lodge))
                    .await?;
                let levy =
                    c.db.query_one(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'lodge' AND d.lodge_id = $1 AND d.fiscal_year = $2",
                        ),
                        vec![
                            SqlValue::Text(lodge.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                        ],
                    )
                    .await?;
                let members =
                    c.db.query(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.lodge_id = $1 AND d.fiscal_year = $2 \
                             ORDER BY d.member_id LIMIT $3",
                        ),
                        vec![
                            SqlValue::Text(lodge.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                            SqlValue::Int(MAX_DUES_ROWS),
                        ],
                    )
                    .await?;
                let per_member_levy_cents = levy
                    .as_ref()
                    .and_then(|row| row["share_bps"].as_i64())
                    .map(|share_bps| {
                        lodge_levy_cents(
                            levy.as_ref()
                                .and_then(|row| row["base_cents"].as_i64())
                                .unwrap_or(0),
                            share_bps,
                        )
                    });
                PluginResponse::json(
                    200,
                    &json!({
                        "lodge_id": lodge,
                        "fiscal_year": fiscal_year,
                        "levy": levy,
                        "per_member_levy_cents": per_member_levy_cents,
                        "members": members,
                        "totals": dues_totals(&members),
                        "note": "A Lodge's levy is a fraction of the total membership cost; \
                                 each member's own dues are the sliding scale's.",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/finance/dues` — the year's assessments with what the ledger shows.
///
/// One query. `paid_cents` and `outstanding_cents` come from the transactions
/// tagged with each member and year — recording a payment anywhere moves them.
fn route_list_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/dues",
        "finance:read_all",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let fiscal_year = validate(fiscal_year_arg(
                    req.query_int("fiscal_year").map(|year| year as i32),
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                let tier = match req.query_param("tier") {
                    Some(raw) => Some(validate(normalize_tier(raw))?),
                    None => None,
                };
                let status = match req.query_param("status") {
                    Some(raw) => Some(validate(normalize_status(raw))?),
                    None => None,
                };
                let lodge = req
                    .query_param("lodge_id")
                    .map(str::trim)
                    .filter(|lodge| !lodge.is_empty())
                    .map(str::to_string);
                let rows =
                    c.db.query(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.fiscal_year = $1 \
                             AND ($2::text IS NULL OR d.lodge_id = $2) \
                             AND ($3::text IS NULL OR d.tier = $3) \
                             AND ($4::text IS NULL OR d.status = $4) \
                             ORDER BY d.member_id LIMIT $5",
                        ),
                        vec![
                            SqlValue::Int(i64::from(fiscal_year)),
                            lodge.clone().into(),
                            tier.clone().into(),
                            status.clone().into(),
                            SqlValue::Int(MAX_DUES_ROWS),
                        ],
                    )
                    .await?;
                let totals = dues_totals(&rows);
                PluginResponse::json(
                    200,
                    &json!({
                        "fiscal_year": fiscal_year,
                        "dues": rows,
                        "count": rows.len(),
                        "truncated": rows.len() as i64 >= MAX_DUES_ROWS,
                        "totals": totals,
                        "by_tier": dues_by_tier(&rows),
                        "filters": { "lodge_id": lodge, "tier": tier, "status": status },
                        "honor_system": true,
                        "note": "paid_cents is derived from the ledger, so it moves when a \
                                 payment is recorded by any route — including a payment.received \
                                 event from the payments integration.",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/finance/dues/member/{member}` — one scout's dues and payments.
///
/// A member may read their own record with `finance:read` at any scope — that is
/// an ownership check, not a grant (SPEC §9.2) — and anybody else's needs
/// `finance:read_all` covering the troop, which is one permission query. Then the
/// standing, then the payments themselves.
fn route_get_member_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/finance/dues/member/{member}",
        "finance:read",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let Some(member) = req
                    .param("member")
                    .map(str::trim)
                    .filter(|member| !member.is_empty())
                    .map(str::to_string)
                else {
                    return Err(SdkError::Internal(
                        "route /api/finance/dues/member/{member} has no {member} capture".into(),
                    ));
                };
                let caller = caller_of(&req).unwrap_or_default();
                if member != caller {
                    c.permissions
                        .reach(req.identity.as_ref(), "finance:read_all", &Scope::troop())
                        .await?;
                }
                let fiscal_year = validate(fiscal_year_arg(
                    req.query_int("fiscal_year").map(|year| year as i32),
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                let dues =
                    c.db.query_one(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.member_id = $1 AND d.fiscal_year = $2",
                        ),
                        vec![
                            SqlValue::Text(member.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                        ],
                    )
                    .await?;
                let payments =
                    c.db.query(
                        format!(
                            "SELECT {TRANSACTION_FIELDS} FROM {tx} t \
                             WHERE t.member_id = $1 AND t.fiscal_year = $2 \
                               AND t.category = $3 \
                             ORDER BY t.id DESC LIMIT 50",
                            tx = c.db.table("transactions")
                        ),
                        vec![
                            SqlValue::Text(member.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                            SqlValue::Text(CATEGORY_DUES.to_string()),
                        ],
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "member_id": member,
                        "fiscal_year": fiscal_year,
                        "dues": dues,
                        "payments": payments,
                        "honor_system": true,
                        "next": if dues.is_none() {
                            "no assessment yet — report your tier with \
                             POST /api/finance/dues/self-report"
                        } else {
                            "self-report a different tier with POST /api/finance/dues/self-report"
                        },
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/dues/self-report` — a scout reports their own tier.
///
/// The honor system's one write. A scout may report **only themselves** unless
/// they also hold `finance:manage_dues` (a treasurer recording a phoned-in
/// answer), and a self-report **never sets the base cost** it is a fraction of:
/// the treasurer's assessment or the troop's configured membership cost does.
/// That is what keeps a self-reported tier from being a self-set price.
///
/// Queries: the permission check when reporting for somebody else, then the
/// existing assessment (whose base is reused), then the upsert, then the standing
/// the scout actually cares about. Then the audit write.
fn route_self_report(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/finance/dues/self-report",
        "finance:self_report",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: SelfReportBody = req.json()?;
                let Some(caller) = caller_of(&req) else {
                    return PluginResponse::error(
                        401,
                        "a self-report needs an authenticated member",
                    );
                };
                let tier = validate(normalize_tier(&body.tier))?;
                let subject = match trimmed(&body.member_id) {
                    Some(other) if other != caller => {
                        c.permissions
                            .reach(
                                req.identity.as_ref(),
                                "finance:manage_dues",
                                &Scope::troop(),
                            )
                            .await?;
                        other
                    }
                    _ => caller.clone(),
                };
                let fiscal_year = validate(fiscal_year_arg(
                    body.fiscal_year,
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                let existing =
                    c.db.query_one(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.member_id = $1 AND d.fiscal_year = $2",
                        ),
                        vec![
                            SqlValue::Text(subject.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                        ],
                    )
                    .await?;
                // The base is the treasurer's number, never the scout's.
                let Some(base_cents) = existing
                    .as_ref()
                    .and_then(|row| row["base_cents"].as_i64())
                    .or_else(|| configured_membership_cost(&c.config))
                else {
                    return PluginResponse::error(
                        409,
                        format!(
                            "no assessment and no configured membership cost for {subject} in \
                             {fiscal_year} — ask the treasurer to open one with \
                             POST /api/finance/dues/assess (a self-report never sets its own \
                             base cost)"
                        ),
                    );
                };
                let Some(assessed_cents) = tier_assessment(base_cents, &tier) else {
                    return Err(SdkError::Internal(format!(
                        "tier {tier:?} validated but carries no share"
                    )));
                };
                // A self-report lowers what the member pays, and the reduction is
                // a scholarship draw like any other — but this caller holds
                // `finance:self_report` and must **not** be given `finance:write`
                // (the sliding scale would become a spending authority). So the
                // draw is recorded as outstanding with its amount visible and a
                // `finance:write` holder books it later: the machine-originated
                // case is named, not solved by minting a credential.
                let funded_cents = funded_cents_for(STATUS_SELF_REPORTED, base_cents, assessed_cents);
                let draw_status = unbooked_draw_state(funded_cents);
                let assessment = Assessment {
                    fiscal_year,
                    dues_kind: DUES_KIND_MEMBER,
                    member_id: subject.clone(),
                    lodge_id: existing
                        .as_ref()
                        .and_then(|row| row["lodge_id"].as_str())
                        .unwrap_or_default()
                        .to_string(),
                    tier: tier.clone(),
                    share_bps: None,
                    base_cents,
                    assessed_cents,
                    funded_cents,
                    draw_status: draw_status.to_string(),
                    draw_ref: None,
                    self_reported: true,
                    status: STATUS_SELF_REPORTED.to_string(),
                    note: trimmed(&body.note).unwrap_or_default(),
                    recorded_by: caller.clone(),
                };
                let row = upsert_dues(&c, &assessment).await?;
                let Some(dues) = row else {
                    return Err(SdkError::Internal("the dues upsert returned no row".into()));
                };
                let standing =
                    c.db.query_one(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.member_id = $1 AND d.fiscal_year = $2",
                        ),
                        vec![
                            SqlValue::Text(subject.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                        ],
                    )
                    .await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "dues.self_report",
                        "dues",
                        &dues["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "member_id": subject,
                            "fiscal_year": fiscal_year,
                            "tier": tier,
                            "base_cents": base_cents,
                            "assessed_cents": assessed_cents,
                            "funded_cents": funded_cents,
                            "draw_status": draw_status,
                            "reported_by": caller,
                            "on_behalf": subject != caller,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.dues.self_reported",
                        json!({
                            "member_id": subject,
                            "fiscal_year": fiscal_year,
                            "tier": tier,
                            "assessed_cents": assessed_cents,
                            "funded_cents": funded_cents,
                            "draw_status": draw_status,
                            "reported_by": caller,
                        }),
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "dues": dues,
                        "standing": standing,
                        "assessed_cents": assessed_cents,
                        "assessed_display": format_cents(assessed_cents),
                        "funded_cents": funded_cents,
                        "funded_display": format_cents(funded_cents),
                        "draw_status": draw_status,
                        "draw_reference": dues_draw_reference(fiscal_year, &subject),
                        "honor_system": true,
                        "note": "Self-reported: no income verification is asked for or \
                                 recorded. Hardship assesses $0. A reduction is funded from \
                                 scholarship by a finance:write holder (POST \
                                 /api/finance/transfer) — this route never books it.",
                    }),
                )
            }
        }),
    )
}

/// The body of the waiver repair — both fields optional, so a bare `POST` (no
/// body at all) means "every year, really do it".
#[derive(Debug, Deserialize)]
struct WaiverRepairBody {
    /// Repair one year; absent repairs every year that has a candidate.
    #[serde(default)]
    fiscal_year: Option<i32>,
    /// Report what would change, and change nothing.
    #[serde(default)]
    dry_run: Option<bool>,
}

/// `POST /api/finance/dues/repair-waivers` — recompute the rows already waived at
/// zero, **on demand and only on demand**.
///
/// Migration 2 leaves a deployed database's historic waivers at
/// `assessed_cents = 0`: the tier's share is a Rust computation
/// ([`tier_assessment`]) and SQL must not guess it. This route is the repair that
/// note §4.3 asks for, and it is deliberately manual — recomputing a row changes
/// what a *past* year's Annual Financial Report says, and that is the owner's
/// call, not a startup step's. Nothing runs this automatically.
///
/// It touches only `status = 'waived' AND assessed_cents = 0` rows and computes
/// each one's assessment from **its own** `base_cents` and `tier`. A row whose
/// `base_cents` is 0 had no membership cost to fund and is left alone; so is a
/// row whose tier assesses nothing. The repair is **idempotent** by construction:
/// once a row's assessment is non-zero it no longer matches the worklist, and the
/// `UPDATE` re-states the same guard, so a concurrent or repeated call writes at
/// most once. The recomputed row's draw is left **outstanding** (`unbooked`): this
/// route repairs figures, it does not move money.
///
/// Queries: the worklist, then one guarded `UPDATE … RETURNING` per repaired row
/// (skipped entirely for a dry run).
fn route_repair_waived_dues(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/dues/repair-waivers",
        "finance:manage",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: WaiverRepairBody = if req.body.is_empty() {
                    WaiverRepairBody {
                        fiscal_year: None,
                        dry_run: None,
                    }
                } else {
                    req.json()?
                };
                let dry_run = body.dry_run.unwrap_or(false);
                let candidates =
                    c.db.query(
                        format!(
                            "SELECT id, member_id, fiscal_year, base_cents, tier FROM {dues} \
                             WHERE status = '{STATUS_WAIVED}' AND dues_kind = '{DUES_KIND_MEMBER}' \
                               AND assessed_cents = 0 AND base_cents > 0 \
                               AND ($1::bigint IS NULL OR fiscal_year = $1) \
                             ORDER BY fiscal_year, member_id LIMIT $2",
                            dues = c.db.table("dues")
                        ),
                        vec![
                            body.fiscal_year
                                .map(|year| SqlValue::Int(i64::from(year)))
                                .unwrap_or(SqlValue::NullInt),
                            SqlValue::Int(MAX_DUES_ROWS),
                        ],
                    )
                    .await?;
                let mut repaired: Vec<Value> = Vec::new();
                let mut nothing_to_fund = 0i64;
                let mut unrepairable = 0i64;
                for row in &candidates {
                    let Some(id) = row["id"].as_i64() else {
                        continue;
                    };
                    let member = row["member_id"].as_str().unwrap_or_default().to_string();
                    let fiscal_year = row["fiscal_year"].as_i64().unwrap_or_default() as i32;
                    let base_cents = row["base_cents"].as_i64().unwrap_or(0);
                    let tier = row["tier"].as_str().unwrap_or_default().to_string();
                    let assessed = match tier_assessment(base_cents, &tier) {
                        Some(cents) if cents > 0 => cents,
                        // A tier that assesses nothing (hardship by its own base,
                        // or a base of 0) has nothing to fund.
                        _ => {
                            nothing_to_fund += 1;
                            continue;
                        }
                    };
                    if dry_run {
                        repaired.push(json!({
                            "id": id,
                            "member_id": member,
                            "fiscal_year": fiscal_year,
                            "tier": tier,
                            "base_cents": base_cents,
                            "assessed_cents": assessed,
                            "funded_cents": assessed,
                            "applied": false,
                        }));
                        continue;
                    }
                    let updated = c
                        .db
                        .query_one(
                            format!(
                                "UPDATE {dues} AS d SET assessed_cents = $2, funded_cents = $2, \
                                   draw_status = $3, updated_at = now() \
                                 WHERE d.id = $1 AND d.status = '{STATUS_WAIVED}' \
                                   AND d.assessed_cents = 0 AND d.base_cents > 0 \
                                 RETURNING {DUES_FIELDS}",
                                dues = c.db.table("dues")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Int(assessed),
                                SqlValue::Text(DRAW_UNBOOKED.to_string()),
                            ],
                        )
                        .await?;
                    match updated {
                        Some(dues) => repaired.push(json!({
                            "dues": dues,
                            "assessed_cents": assessed,
                            "funded_cents": assessed,
                            "applied": true,
                        })),
                        // The guard did not match: another caller repaired it, or
                        // its base moved under us. Nothing to write, nothing lost.
                        None => unrepairable += 1,
                    }
                }
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "dues.repair_waivers",
                        "dues",
                        &body
                            .fiscal_year
                            .map(|year| year.to_string())
                            .unwrap_or_else(|| "all".to_string()),
                        json!({
                            "fiscal_year": body.fiscal_year,
                            "dry_run": dry_run,
                            "repaired": repaired.len(),
                            "nothing_to_fund": nothing_to_fund,
                            "unrepairable": unrepairable,
                        }),
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "dry_run": dry_run,
                        "fiscal_year": body.fiscal_year,
                        "candidates": candidates.len(),
                        "repaired": repaired.len(),
                        "nothing_to_fund": nothing_to_fund,
                        "unrepairable": unrepairable,
                        "rows": repaired,
                        "note": "Recomputes only rows that are 'waived' with an assessment of \
                                 zero, from each row's own base_cents and tier. A repair of a \
                                 past year changes what that year's report says, so it is \
                                 manual and idempotent, never a startup step. The draw is left \
                                 unbooked for a finance:write holder to book.",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/finance/dues/payment` — take a dues payment.
///
/// A payment is an *income entry* in the fund dues land in (the configured
/// `dues_fund_code`, General by default), tagged `category: "dues"` and with the
/// member it came from — which is exactly what makes a member's
/// `paid_cents`/`outstanding_cents` move. Nothing about the payment is stored on
/// the dues row, so a payment recorded here, through the ledger route, or by a
/// `payment.received` event are all equally "real".
///
/// Queries: the fund (by id, else the configured code), then the entry, then the
/// member's standing. Then the audit write.
fn route_record_dues_payment(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected(
        "/api/finance/dues/payment",
        "finance:write",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let body: DuesPaymentBody = req.json()?;
                let member = body.member_id.trim().to_string();
                if member.is_empty() {
                    return PluginResponse::error(400, "member_id is required");
                }
                let amount = validate(resolve_amount(
                    &body.amount_cents,
                    &body.amount,
                    "amount_cents",
                ))?;
                if amount <= 0 {
                    return PluginResponse::error(400, "amount must be positive");
                }
                let date = validate(occurred_on(&body.occurred_on))?;
                let fiscal_year = validate(fiscal_year_arg(body.fiscal_year, date, &c.config))?;
                let dues_fund = configured_dues_fund(&c.config);
                let fund =
                    c.db.query_one(
                        format!(
                            "SELECT f.id, f.code, f.name FROM {funds} f \
                             WHERE ($1::bigint IS NULL AND f.code = $2) OR f.id = $1",
                            funds = c.db.table("funds")
                        ),
                        vec![
                            body.fund_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(dues_fund.clone()),
                        ],
                    )
                    .await?;
                let Some(fund) = fund else {
                    return PluginResponse::error(
                        404,
                        match body.fund_id {
                            Some(id) => format!("no such fund: {id}"),
                            None => format!(
                                "no fund with the configured dues code {dues_fund:?} — create it \
                                 or set finance.dues_fund_code"
                            ),
                        },
                    );
                };
                let fund_id = fund["id"].as_i64().unwrap_or_default();
                let entry = LedgerEntry {
                    fund: FundRef::Id(fund_id),
                    amount_cents: amount,
                    kind: KIND_INCOME,
                    category: CATEGORY_DUES.to_string(),
                    description: trimmed(&body.description)
                        .unwrap_or_else(|| format!("Dues {fiscal_year} — {member}")),
                    member_id: member.clone(),
                    fiscal_year,
                    occurred_on: date,
                    recorded_by: caller_of(&req).unwrap_or_default(),
                    overdraft_authorized: false,
                    external_ref: trimmed(&body.external_ref),
                };
                let rows = insert_entry(&c, &entry).await?;
                let Some(payment) = rows.first().cloned() else {
                    // The only way a positive income entry writes nothing is a
                    // missing fund or a replayed external_ref.
                    return explain_no_entry(&c, &entry).await;
                };
                let standing =
                    c.db.query_one(
                        sql_dues_standing(
                            &c,
                            "d.dues_kind = 'member' AND d.member_id = $1 AND d.fiscal_year = $2",
                        ),
                        vec![
                            SqlValue::Text(member.clone()),
                            SqlValue::Int(i64::from(fiscal_year)),
                        ],
                    )
                    .await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "dues.payment",
                        "transaction",
                        &payment["id"].as_i64().unwrap_or_default().to_string(),
                        json!({
                            "member_id": member,
                            "fiscal_year": fiscal_year,
                            "fund_id": fund_id,
                            "amount_cents": amount,
                            "external_ref": entry.external_ref,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        "finance.dues.payment",
                        json!({
                            "transaction_id": payment["id"],
                            "member_id": member,
                            "fiscal_year": fiscal_year,
                            "fund_id": fund_id,
                            "amount_cents": amount,
                            "paid_cents": standing.as_ref().and_then(|row| row["paid_cents"].as_i64()),
                            "outstanding_cents": standing.as_ref().and_then(|row| row["outstanding_cents"].as_i64()),
                            "recorded_by": entry.recorded_by,
                        }),
                    )
                    .await?;
                PluginResponse::created(
                    &format!("/api/finance/dues/member/{member}?fiscal_year={fiscal_year}"),
                    &json!({
                        "payment": payment,
                        "fund": fund,
                        "standing": standing,
                        "settled": standing
                            .as_ref()
                            .and_then(|row| row["settled"].as_bool())
                            .unwrap_or(false),
                        "next": if standing.is_none() {
                            "no assessment is open for this member and year — the payment is \
                             recorded, and POST /api/finance/dues/assess opens the assessment"
                        } else {
                            "the standing above is derived from the ledger, not stored"
                        },
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// `GET /api/finance/report/annual` — the Annual Financial Report (SPEC §7.5).
///
/// Seven queries, then arithmetic that is all derived: per-fund opening/income/
/// expense/transfers/closing, the budget lines with their variances, the dues
/// section, and the ledger's integrity verdict. Nothing in the report is stored,
/// so it cannot disagree with the ledger it describes — and the verdict is
/// included because this is the document a treasurer hands to an outside body.
///
/// Audited: money leaving the troop as a document is worth a line in the log.
fn route_annual_report(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/report/annual",
        "finance:read_all",
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let fiscal_year = validate(fiscal_year_arg(
                    req.query_int("fiscal_year").map(|year| year as i32),
                    Utc::now().date_naive(),
                    &c.config,
                ))?;
                let start_month = fiscal_year_start_month(&c.config);
                let (start, end) = fiscal_year_bounds(fiscal_year, start_month);

                let funds =
                    c.db.query(
                        sql_annual_funds(&c),
                        vec![
                            SqlValue::Text(start.to_string()),
                            SqlValue::Text(end.to_string()),
                        ],
                    )
                    .await?;
                let budget_lines = budget_lines_with_variance(
                    c.db.query(
                        sql_budget_lines(&c),
                        vec![SqlValue::Int(i64::from(fiscal_year))],
                    )
                    .await?,
                );
                let dues_by_tier =
                    c.db.query(
                        sql_annual_dues(&c),
                        vec![SqlValue::Int(i64::from(fiscal_year))],
                    )
                    .await?;
                let collected =
                    c.db.query_one(
                        sql_annual_collected(&c),
                        vec![SqlValue::Int(i64::from(fiscal_year))],
                    )
                    .await?
                    .unwrap_or_else(|| json!({ "collected_cents": 0 }));
                // The ledger's own verdict — three queries, the same check
                // `/api/finance/health` reports.
                let verdict = ledger_integrity(&c).await?;

                let closing_total: i64 = funds
                    .iter()
                    .map(|fund| fund["closing_cents"].as_i64().unwrap_or(0))
                    .sum();
                let income_total: i64 = funds
                    .iter()
                    .map(|fund| fund["income_cents"].as_i64().unwrap_or(0))
                    .sum();
                let expense_total: i64 = funds
                    .iter()
                    .map(|fund| fund["expense_cents"].as_i64().unwrap_or(0))
                    .sum();
                let opening_total: i64 = funds
                    .iter()
                    .map(|fund| fund["opening_cents"].as_i64().unwrap_or(0))
                    .sum();
                let transfers_in_total: i64 = funds
                    .iter()
                    .map(|fund| fund["transfers_in_cents"].as_i64().unwrap_or(0))
                    .sum();
                let transfers_out_total: i64 = funds
                    .iter()
                    .map(|fund| fund["transfers_out_cents"].as_i64().unwrap_or(0))
                    .sum();
                let assessed_total: i64 = dues_by_tier
                    .iter()
                    .map(|tier| tier["assessed_cents"].as_i64().unwrap_or(0))
                    .sum();
                let funded_total: i64 = dues_by_tier
                    .iter()
                    .map(|tier| tier["funded_cents"].as_i64().unwrap_or(0))
                    .sum();
                let unbooked_draws: i64 = dues_by_tier
                    .iter()
                    .map(|tier| tier["unbooked_draws"].as_i64().unwrap_or(0))
                    .sum();
                let collected_cents = collected["collected_cents"].as_i64().unwrap_or(0);
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "report.annual",
                        "report",
                        &fiscal_year.to_string(),
                        json!({
                            "fiscal_year": fiscal_year,
                            "funds": funds.len(),
                            "closing_cents": closing_total,
                            "balanced": verdict["balanced"],
                        }),
                    )
                    .await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "fiscal_year": fiscal_year,
                        "period": { "start": start.to_string(), "end": end.to_string() },
                        "generated_at": Utc::now().to_rfc3339(),
                        "funds": funds,
                        "totals": {
                            "opening_cents": opening_total,
                            "opening_display": format_cents(opening_total),
                            "income_cents": income_total,
                            "income_display": format_cents(income_total),
                            "expense_cents": expense_total,
                            "expense_display": format_cents(expense_total),
                            "transfers_in_cents": transfers_in_total,
                            "transfers_out_cents": transfers_out_total,
                            "net_transfers_cents": transfers_in_total + transfers_out_total,
                            "net_movement_cents": income_total + expense_total,
                            "closing_cents": closing_total,
                            "closing_display": format_cents(closing_total),
                            "fund_count": funds.len(),
                        },
                        "budgets": { "lines": budget_lines, "totals": budget_totals(&budget_lines) },
                        "dues": {
                            "fiscal_year": fiscal_year,
                            "assessed_cents": assessed_total,
                            "assessed_display": format_cents(assessed_total),
                            "funded_cents": funded_total,
                            "funded_display": format_cents(funded_total),
                            "unbooked_draws": unbooked_draws,
                            "collected_cents": collected_cents,
                            "collected_display": format_cents(collected_cents),
                            "outstanding_cents": (assessed_total - funded_total - collected_cents).max(0),
                            "members_assessed": dues_by_tier
                                .iter()
                                .map(|tier| tier["members"].as_i64().unwrap_or(0))
                                .sum::<i64>(),
                            "at_no_cost": dues_by_tier
                                .iter()
                                .map(|tier| tier["at_no_cost"].as_i64().unwrap_or(0))
                                .sum::<i64>(),
                            "by_tier": dues_by_tier,
                            "honor_system": true,
                            "funding": "Anything free, deducted or discounted draws from the \
                                        scholarship fund: funded_cents is what the troop spent \
                                        on access, and a draw is a transfer, so it does not \
                                        appear in collected_cents.",
                        },
                        "integrity": verdict,
                        "ledger": {
                            "entries": verdict["entries"],
                            "unpaired_transfers": verdict["unpaired_transfers"],
                        },
                        "note": "Every figure here is derived from the ledger at read time. \
                                 Transfers move money between funds and leave the total \
                                 unchanged, so incomes less expenses plus the opening balance \
                                 equals the closing balance for the troop.",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/finance/health` — do the books add up?
///
/// Three queries ([`ledger_integrity`]). A troop's total is the sum of its funds'
/// balances by construction, so this checks the two things that could still be
/// wrong: a transfer group that is not two entries summing to zero, and a ledger
/// total that disagrees with the funds it is spread across.
fn route_health(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected(
        "/api/finance/health",
        "finance:read",
        route_handler(move |_req| {
            let c = c.clone();
            async move {
                let verdict = ledger_integrity(&c).await?;
                PluginResponse::json(
                    200,
                    &json!({
                        "integrity": verdict,
                        "ledger_entries": verdict["entries"],
                        "unpaired_transfers": verdict["unpaired_transfers"],
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A ledger row with its fund's code, for the ledger list (alias `t`, `f`).
const TRANSACTION_LIST_FIELDS: &str = r#"
    t.id, t.fund_id, f.code AS fund_code, f.name AS fund_name, t.amount_cents, t.kind,
    t.transfer_group::text AS transfer_group, t.counterparty_fund_id, t.category,
    t.description, t.member_id, t.fiscal_year, t.occurred_on::text AS occurred_on,
    t.recorded_by, t.overdraft_authorized, t.external_ref, t.created_at::text AS created_at
"#;

/// A validation failure is the caller's input, so it crosses the plugin boundary
/// as a 400 rather than a 500 — the safe path is the short one.
fn validate<T>(result: Result<T, String>) -> Result<T, SdkError> {
    result.map_err(SdkError::BadRequest)
}

/// The authenticated caller's id, or `None` for an anonymous request.
fn caller_of(req: &PluginRequest) -> Option<String> {
    req.identity
        .as_ref()
        .map(|identity| identity.user_id.clone())
        .filter(|user| !user.trim().is_empty())
}

/// Append `column = $n` to a dynamic `SET` list, keeping the placeholder number
/// and the bind order in step by construction (they are the same push).
fn set_clause(sets: &mut Vec<String>, params: &mut Vec<SqlValue>, column: &str, value: SqlValue) {
    params.push(value);
    sets.push(format!("{column} = ${}", params.len()));
}

/// The display name a fund gets when the caller does not give one.
fn default_fund_name(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => format!("{}{} Fund", first.to_ascii_uppercase(), chars.as_str()),
        None => "Fund".to_string(),
    }
}

/// Every kind a ledger *filter* may name — `transfer` included, unlike the kinds
/// a caller may record directly.
fn normalize_ledger_kind(raw: &str) -> Result<String, String> {
    let kind = raw.trim().to_ascii_lowercase();
    let all = [KIND_INCOME, KIND_EXPENSE, KIND_TRANSFER];
    if all.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!("kind must be one of {}", all.join(", ")))
    }
}

/// One budget line's plan against its actual, for one fund and one year —
/// [`sql_budget_lines`] narrowed to a fund, which is what a fund's page shows.
fn sql_budget_lines_for_fund(c: &PluginContext) -> String {
    format!(
        "SELECT b.id, b.fund_id, b.fiscal_year, b.direction, b.category, b.planned_cents, \
                b.note, b.created_by, b.created_at::text AS created_at, \
                b.updated_at::text AS updated_at, \
                COALESCE(a.actual_cents, 0)::bigint AS actual_cents, \
                (b.category = '' AND EXISTS (SELECT 1 FROM {budgets} b2 \
                    WHERE b2.fund_id = b.fund_id AND b2.fiscal_year = b.fiscal_year \
                      AND b2.direction = b.direction AND b2.category <> '')) AS overlaps_categories \
         FROM {budgets} b \
         LEFT JOIN LATERAL ( \
             SELECT SUM(t.amount_cents) AS actual_cents FROM {tx} t \
             WHERE t.fund_id = b.fund_id AND t.fiscal_year = b.fiscal_year \
               AND t.kind = b.direction AND (b.category = '' OR t.category = b.category) \
         ) a ON true \
         WHERE b.fund_id = $1 AND b.fiscal_year = $2",
        budgets = c.db.table("budgets"),
        tx = c.db.table("transactions")
    )
}

// ---------------------------------------------------------------------------
// Writing the ledger
// ---------------------------------------------------------------------------

/// How an entry names the fund it lands in.
///
/// **The code is the reference, the id is finance's key** (`plugin-to-plugin.md`
/// §3.5): a producer that holds a fund *code* — the thing finance seeds and its
/// own config names — can write an entry without reading anything first, and the
/// code→id resolution happens here, where the funds table lives, inside the same
/// statement as the insert. An id is still accepted, for a caller that already
/// has one.
#[derive(Debug, Clone)]
enum FundRef {
    Id(i64),
    Code(String),
}

impl FundRef {
    /// The pair every statement binds for one leg: `$n` an id (or NULL) and
    /// `$n+1` a code (or NULL), which is how a reference is resolved *inside* the
    /// statement that needs the fund's id.
    fn bind(&self) -> (SqlValue, SqlValue) {
        match self {
            FundRef::Id(id) => (SqlValue::Int(*id), SqlValue::Null),
            FundRef::Code(code) => (SqlValue::NullInt, SqlValue::Text(code.clone())),
        }
    }

    /// The reference as a reader of an error message should see it.
    fn describe(&self) -> String {
        match self {
            FundRef::Id(id) => format!("id {id}"),
            FundRef::Code(code) => format!("code {code:?}"),
        }
    }
}

/// One entry the handlers write. `amount_cents` is **signed** (income positive,
/// expense negative) and the sign is computed from the kind, never taken from a
/// request body.
struct LedgerEntry {
    fund: FundRef,
    amount_cents: i64,
    kind: &'static str,
    category: String,
    description: String,
    member_id: String,
    fiscal_year: i32,
    occurred_on: NaiveDate,
    recorded_by: String,
    overdraft_authorized: bool,
    external_ref: Option<String>,
}

/// Write one entry, guarded — **one statement**.
///
/// Two guards ride along with the `INSERT … SELECT`, which is what makes them
/// sound: the fund must exist — named by id **or by code**, resolved by the same
/// statement (`FROM funds f`) — and the fund must not be taken negative by this
/// entry (unless `overdraft_authorized` says a person decided otherwise).
/// `ON CONFLICT (external_ref) DO NOTHING` makes a replayed entry — the same
/// provider payment arriving twice — a no-op rather than a second deposit; a null
/// `external_ref` never conflicts, so an un-referenced entry always writes.
///
/// An empty result is therefore one of three things, and the caller asks
/// [`explain_no_entry`] which.
async fn insert_entry(c: &PluginContext, entry: &LedgerEntry) -> Result<Vec<Value>, SdkError> {
    // The fund is named by **id or code** and resolved here, in the same
    // statement: `FROM funds f` is the fund row the entry lands in, so a caller
    // that holds only the code needs no read of its own (`$1` an id, `$12` a
    // code, exactly one of them non-null).
    let (fund_id, fund_code) = match &entry.fund {
        FundRef::Id(id) => (SqlValue::Int(*id), SqlValue::Null),
        FundRef::Code(code) => (SqlValue::NullInt, SqlValue::Text(code.clone())),
    };
    c.db.query(
        format!(
            "INSERT INTO {tx} AS t \
               (fund_id, amount_cents, kind, category, description, member_id, fiscal_year, \
                occurred_on, recorded_by, overdraft_authorized, external_ref) \
             SELECT f.id, $2, $3, $4, $5, $6, $7, $8::date, $9, $10, $11 \
               FROM {funds} f \
             WHERE (($1::bigint IS NOT NULL AND f.id = $1) \
                    OR ($12::text IS NOT NULL AND f.code = $12)) \
               AND ($10 OR (SELECT COALESCE(SUM(amount_cents), 0)::bigint FROM {tx} \
                            WHERE fund_id = f.id) + $2 >= 0) \
             ON CONFLICT (external_ref) DO NOTHING \
             RETURNING {TRANSACTION_FIELDS}",
            tx = c.db.table("transactions"),
            funds = c.db.table("funds")
        ),
        vec![
            fund_id,
            SqlValue::Int(entry.amount_cents),
            SqlValue::Text(entry.kind.to_string()),
            SqlValue::Text(entry.category.clone()),
            SqlValue::Text(entry.description.clone()),
            SqlValue::Text(entry.member_id.clone()),
            SqlValue::Int(i64::from(entry.fiscal_year)),
            SqlValue::Text(entry.occurred_on.to_string()),
            SqlValue::Text(entry.recorded_by.clone()),
            SqlValue::Bool(entry.overdraft_authorized),
            entry.external_ref.clone().into(),
            fund_code,
        ],
    )
    .await
}

/// A fund's balance, summed from its ledger. `0` when the fund has no entries
/// (or does not exist — the caller has already established that).
async fn fund_balance_cents(c: &PluginContext, fund_id: i64) -> Result<i64, SdkError> {
    let row =
        c.db.query_one(
            format!(
                "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS balance_cents FROM {tx} \
                 WHERE fund_id = $1",
                tx = c.db.table("transactions")
            ),
            vec![SqlValue::Int(fund_id)],
        )
        .await?;
    Ok(row
        .and_then(|row| row["balance_cents"].as_i64())
        .unwrap_or(0))
}

/// The insert wrote nothing. Say which of the three reasons it was.
///
/// Queries: the fund and its balance, then — when the entry carried an
/// `external_ref` — the row that already holds it.
async fn explain_no_entry(
    c: &PluginContext,
    entry: &LedgerEntry,
) -> Result<PluginResponse, SdkError> {
    let (fund_id, fund_code) = match &entry.fund {
        FundRef::Id(id) => (SqlValue::Int(*id), SqlValue::Null),
        FundRef::Code(code) => (SqlValue::NullInt, SqlValue::Text(code.clone())),
    };
    let fund =
        c.db.query_one(
            format!(
                "SELECT f.id, f.code, f.name, f.active, \
                        COALESCE(SUM(t.amount_cents), 0)::bigint AS balance_cents \
                 FROM {funds} f LEFT JOIN {tx} t ON t.fund_id = f.id \
                 WHERE ($1::bigint IS NOT NULL AND f.id = $1) \
                    OR ($2::text IS NOT NULL AND f.code = $2) \
                 GROUP BY f.id",
                funds = c.db.table("funds"),
                tx = c.db.table("transactions")
            ),
            vec![fund_id, fund_code],
        )
        .await?;
    let Some(fund) = fund else {
        return PluginResponse::error(
            404,
            match &entry.fund {
                FundRef::Id(id) => format!("no such fund: {id}"),
                FundRef::Code(code) => format!("no such fund: code {code:?}"),
            },
        );
    };
    if let Some(reference) = &entry.external_ref {
        let existing =
            c.db.query_one(
                format!(
                    "SELECT {TRANSACTION_FIELDS} FROM {tx} t WHERE t.external_ref = $1",
                    tx = c.db.table("transactions")
                ),
                vec![SqlValue::Text(reference.clone())],
            )
            .await?;
        if let Some(existing) = existing {
            return PluginResponse::json(
                200,
                &json!({
                    "recorded": false,
                    "duplicate": true,
                    "transaction": existing,
                    "fund": fund,
                    "note": format!(
                        "external_ref {reference:?} is already recorded — the entry was not \
                         written twice"
                    ),
                }),
            );
        }
    }
    let balance = fund["balance_cents"].as_i64().unwrap_or(0);
    let code = fund["code"].as_str().unwrap_or("?");
    PluginResponse::error(
        409,
        format!(
            "{} would take fund {code} from {} to {}: a fund may not go negative. Record the \
             income first, or pass allow_overdraft to say the overdraft is authorised.",
            format_cents(entry.amount_cents),
            format_cents(balance),
            format_cents(balance + entry.amount_cents)
        ),
    )
}

/// The transfer statement returned fewer than two rows. Say why.
///
/// Three cases, in the order the statement could have refused them: a reference
/// that resolves to no fund (both funds of a transfer must exist), two references
/// that resolve to *the same* fund (a transfer to itself is not a transfer), and
/// an out-leg that would take its fund negative. The references are resolved by
/// this query too — by id **or by code** — so a code-named leg is explained in
/// the same words an id-named one is.
///
/// One query.
async fn explain_no_transfer(
    c: &PluginContext,
    from_ref: &FundRef,
    to_ref: &FundRef,
    amount_cents: i64,
    overdraft: bool,
) -> Result<PluginResponse, SdkError> {
    let (from_id_bind, from_code_bind) = from_ref.bind();
    let (to_id_bind, to_code_bind) = to_ref.bind();
    let rows =
        c.db.query(
            format!(
                "SELECT f.id, f.code, f.name, f.active, \
                        COALESCE(SUM(t.amount_cents), 0)::bigint AS balance_cents \
                 FROM {funds} f LEFT JOIN {tx} t ON t.fund_id = f.id \
                 WHERE (($1::bigint IS NOT NULL AND f.id = $1) \
                        OR ($2::text IS NOT NULL AND f.code = $2)) \
                    OR (($3::bigint IS NOT NULL AND f.id = $3) \
                        OR ($4::text IS NOT NULL AND f.code = $4)) \
                 GROUP BY f.id",
                funds = c.db.table("funds"),
                tx = c.db.table("transactions")
            ),
            vec![from_id_bind, from_code_bind, to_id_bind, to_code_bind],
        )
        .await?;
    // Which of the resolved rows is which leg. A reference is matched by what it
    // named: an id by id, a code by code.
    let matches = |row: &Value, reference: &FundRef| match reference {
        FundRef::Id(id) => row["id"].as_i64() == Some(*id),
        FundRef::Code(code) => row["code"].as_str() == Some(code.as_str()),
    };
    let from = rows.iter().find(|row| matches(row, from_ref));
    let to = rows.iter().find(|row| matches(row, to_ref));
    if from.is_none() || to.is_none() {
        let missing = if from.is_none() {
            from_ref.describe()
        } else {
            to_ref.describe()
        };
        return PluginResponse::error(
            404,
            format!(
                "no such fund: {missing} — both funds of a transfer must exist, and neither leg \
                 is written when one does not"
            ),
        );
    }
    let (Some(from), Some(to)) = (from, to) else {
        return Err(SdkError::Internal(
            "a transfer was explained with no funds".into(),
        ));
    };
    if from["id"].as_i64() == to["id"].as_i64() {
        return PluginResponse::error(
            409,
            "a transfer needs two different funds (use a transaction to correct a fund's own \
             entry): the two references resolve to the same fund, and neither leg is written",
        );
    }
    let balance = from["balance_cents"].as_i64().unwrap_or(0);
    let code = from["code"].as_str().unwrap_or("?").to_string();
    PluginResponse::error(
        409,
        format!(
            "a transfer of {} would leave fund {code} at {} (it holds {}): a fund may not go \
             negative. Record the income first, or pass allow_overdraft to say the overdraft is \
             authorised (this one was {}).",
            format_cents(amount_cents),
            format_cents(balance - amount_cents),
            format_cents(balance),
            if overdraft { "already set" } else { "not set" }
        ),
    )
}

// ---------------------------------------------------------------------------
// Dues rows
// ---------------------------------------------------------------------------

/// The transfer a reference already made, read back as its own two legs.
///
/// This is what a retried draw is answered with: an `external_ref` that is
/// already in the ledger names a money move that has happened, so the retry must
/// not write a second pair of legs. One query.
async fn existing_transfer(c: &PluginContext, reference: &str) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT t.transfer_group::text AS transfer_group, \
                    (SELECT COUNT(*)::bigint FROM {tx} g \
                      WHERE g.transfer_group = t.transfer_group) AS entries, \
                    (SELECT COALESCE(SUM(g.amount_cents), 0)::bigint FROM {tx} g \
                      WHERE g.transfer_group = t.transfer_group) AS group_sum_cents, \
                    (SELECT COUNT(*)::bigint FROM {tx} g2 \
                      WHERE g2.transfer_group = t.transfer_group AND g2.fund_id = t.fund_id \
                        AND g2.amount_cents > 0) AS in_legs \
             FROM {tx} t WHERE t.external_ref = $1",
            tx = c.db.table("transactions")
        ),
        vec![SqlValue::Text(reference.to_string())],
    )
    .await
}

/// Record a draw's state on its own dues row — the second half of a booking —
/// and hand the row back as the API states it.
///
/// It is deliberately a separate statement from the transfer: money that moved
/// and a row that does not know it is exactly the state `attempting` names, and
/// the row can be told the truth afterwards (or by the next caller, whose retry
/// is idempotent through the draw's deterministic reference). One query.
async fn set_dues_draw_state(
    c: &PluginContext,
    fiscal_year: i32,
    member_id: &str,
    draw_status: &str,
    draw_ref: Option<&str>,
) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "UPDATE {dues} AS d SET draw_status = $3, draw_ref = $4::text::uuid, \
               updated_at = now() \
             WHERE d.dues_kind = '{DUES_KIND_MEMBER}' AND d.member_id = $1 \
               AND d.fiscal_year = $2 \
             RETURNING {DUES_FIELDS}",
            dues = c.db.table("dues")
        ),
        vec![
            SqlValue::Text(member_id.to_string()),
            SqlValue::Int(i64::from(fiscal_year)),
            SqlValue::Text(draw_status.to_string()),
            draw_ref
                .map(|r| SqlValue::Text(r.to_string()))
                .unwrap_or(SqlValue::Null),
        ],
    )
    .await
}

/// What booking a dues draw came to.
enum DrawOutcome {
    /// The transfer landed; the group it landed under.
    Booked(String),
    /// Finance's own transfer route refused it (a 4xx): nothing was booked.
    Refused(String),
    /// The call errored: nothing was booked, and nothing is claimed to be.
    Failed(String),
}

/// Book one draw: a **balanced transfer from `scholarship`** into the fund the
/// dues would have landed in, through finance's own transfer route.
///
/// The route is called **as the caller**: the request is the caller's own, with
/// their identity and nothing added, so the ledger write is authorised by
/// `finance:write` — the caller's, not a credential this plugin minted. That is
/// the discipline `plugin-to-plugin.md` §3.1 states, and the reason a caller who
/// does not hold `finance:write` records an outstanding draw instead of a booked
/// one.
///
/// The draw carries its deterministic reference ([`dues_draw_reference`]), so if
/// the answer to this call is lost and the caller retries, the retry is answered
/// with the transfer that already exists rather than a second pair of legs.
async fn book_dues_draw(
    c: &PluginContext,
    req: &PluginRequest,
    member_id: &str,
    fiscal_year: i32,
    amount_cents: i64,
    dues_fund: &str,
    reference: &str,
) -> Result<DrawOutcome, SdkError> {
    let mut draw = req.clone();
    draw.method = "POST".to_string();
    draw.path = "/api/finance/transfer".to_string();
    draw.params = Default::default();
    draw.query = Vec::new();
    draw.headers = Default::default();
    draw.body = serde_json::to_vec(&json!({
        "from_fund_code": FUND_SCHOLARSHIP,
        "to_fund_code": dues_fund,
        "amount_cents": amount_cents,
        "description": dues_draw_description(fiscal_year, member_id),
        "fiscal_year": fiscal_year,
        "external_ref": reference,
    }))
    .map_err(|e| SdkError::Internal(format!("the draw body would not serialize: {e}")))?;
    let transfer = route_transfer(c);
    match (transfer.handler)(draw).await {
        Ok(response) => {
            let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
            if response.status < 300 {
                Ok(DrawOutcome::Booked(
                    body["transfer_group"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ))
            } else {
                Ok(DrawOutcome::Refused(
                    body["error"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("the transfer was refused ({})", response.status)),
                ))
            }
        }
        Err(e) => Ok(DrawOutcome::Failed(e.to_string())),
    }
}

/// One assessment being written. `share_bps` is set for a Lodge's levy and empty
/// for a member's; the database's `dues_subject_present` and
/// `dues_share_for_lodges` keep the two shapes apart.
struct Assessment {
    fiscal_year: i32,
    dues_kind: &'static str,
    member_id: String,
    lodge_id: String,
    tier: String,
    share_bps: Option<i64>,
    base_cents: i64,
    assessed_cents: i64,
    /// What `scholarship` covers of this assessment — the figure the Annual
    /// Financial Report needs to say what the troop spent on access.
    funded_cents: i64,
    /// The draw's state, in [`DRAW_STATUSES`]' vocabulary.
    draw_status: String,
    /// The transfer group the draw landed under, once it has one.
    draw_ref: Option<String>,
    self_reported: bool,
    status: String,
    note: String,
    recorded_by: String,
}

/// Create or replace one assessment — **one statement**.
///
/// One row per (year, kind, member, lodge): posting again revises, which is what
/// makes "the treasurer opens it, the scout revises their own tier" a single
/// record rather than a history of drafts.
///
/// The funding figures come from the assessment itself, with one exception the
/// `DO UPDATE` states: a draw that is already **booked** stays booked while the
/// status and the funded amount are unchanged. A re-assessment that recomputes
/// the same funding has not moved money, and the row must not forget that it did
/// — the draw's deterministic reference makes a re-booking a no-op, but a row
/// that lost its `booked` state would claim a subsidy twice.
async fn upsert_dues(
    c: &PluginContext,
    assessment: &Assessment,
) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "INSERT INTO {dues} AS d \
               (fiscal_year, dues_kind, member_id, lodge_id, tier, share_bps, base_cents, \
                assessed_cents, funded_cents, draw_status, draw_ref, self_reported, status, \
                note, recorded_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::text::uuid, $12, $13, \
                $14, $15) \
             ON CONFLICT (fiscal_year, dues_kind, member_id, lodge_id) DO UPDATE \
               SET tier = EXCLUDED.tier, share_bps = EXCLUDED.share_bps, \
                   base_cents = EXCLUDED.base_cents, \
                   assessed_cents = EXCLUDED.assessed_cents, \
                   funded_cents = EXCLUDED.funded_cents, \
                   draw_status = CASE WHEN d.status = EXCLUDED.status \
                                       AND d.funded_cents = EXCLUDED.funded_cents \
                                       AND d.draw_status = '{DRAW_BOOKED}' \
                                      THEN d.draw_status ELSE EXCLUDED.draw_status END, \
                   draw_ref = CASE WHEN d.status = EXCLUDED.status \
                                    AND d.funded_cents = EXCLUDED.funded_cents \
                                    AND d.draw_status = '{DRAW_BOOKED}' \
                                   THEN d.draw_ref ELSE EXCLUDED.draw_ref END, \
                   self_reported = EXCLUDED.self_reported, status = EXCLUDED.status, \
                   note = EXCLUDED.note, recorded_by = EXCLUDED.recorded_by, \
                   updated_at = now() \
             RETURNING {DUES_FIELDS}",
            dues = c.db.table("dues")
        ),
        vec![
            SqlValue::Int(i64::from(assessment.fiscal_year)),
            SqlValue::Text(assessment.dues_kind.to_string()),
            SqlValue::Text(assessment.member_id.clone()),
            SqlValue::Text(assessment.lodge_id.clone()),
            SqlValue::Text(assessment.tier.clone()),
            assessment
                .share_bps
                .map(SqlValue::Int)
                .unwrap_or(SqlValue::NullInt),
            SqlValue::Int(assessment.base_cents),
            SqlValue::Int(assessment.assessed_cents),
            SqlValue::Int(assessment.funded_cents),
            SqlValue::Text(assessment.draw_status.clone()),
            assessment
                .draw_ref
                .clone()
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null),
            SqlValue::Bool(assessment.self_reported),
            SqlValue::Text(assessment.status.clone()),
            SqlValue::Text(assessment.note.clone()),
            SqlValue::Text(assessment.recorded_by.clone()),
        ],
    )
    .await
}

/// A set of dues rows as a whole: what was assessed, what the ledger says was
/// collected, and what is still owed.
fn dues_totals(rows: &[Value]) -> Value {
    let sum = |field: &str| -> i64 {
        rows.iter()
            .map(|row| row[field].as_i64().unwrap_or(0))
            .sum()
    };
    let count = |predicate: fn(&Value) -> bool| -> i64 {
        rows.iter().filter(|row| predicate(row)).count() as i64
    };
    let assessed = sum("assessed_cents");
    let funded = sum("funded_cents");
    let collected = sum("paid_cents");
    let outstanding = sum("outstanding_cents");
    json!({
        "members": rows.len(),
        "assessed_cents": assessed,
        "assessed_display": format_cents(assessed),
        "funded_cents": funded,
        "funded_display": format_cents(funded),
        "collected_cents": collected,
        "collected_display": format_cents(collected),
        "outstanding_cents": outstanding,
        "outstanding_display": format_cents(outstanding),
        "at_no_cost": count(|row| row["assessed_cents"].as_i64().unwrap_or(0) == 0),
        "self_reported": count(|row| row["self_reported"].as_bool().unwrap_or(false)),
        "waived": count(|row| row["status"].as_str() == Some(STATUS_WAIVED)),
        "draws_unbooked": count(|row| {
            DRAW_UNSETTLED.contains(&row["draw_status"].as_str().unwrap_or_default())
        }),
        "settled": count(|row| row["settled"].as_bool().unwrap_or(false)),
        "honor_system": true,
    })
}

/// The same rows grouped by tier, in the scale's own order.
fn dues_by_tier(rows: &[Value]) -> Vec<Value> {
    TIERS
        .iter()
        .filter_map(|tier| {
            let members: Vec<&Value> = rows
                .iter()
                .filter(|row| row["tier"].as_str() == Some(tier.code))
                .collect();
            if members.is_empty() {
                return None;
            }
            let sum = |field: &str| -> i64 {
                members
                    .iter()
                    .map(|row| row[field].as_i64().unwrap_or(0))
                    .sum()
            };
            Some(json!({
                "tier": tier.code,
                "label": tier.label,
                "share_bps": tier.bps,
                "members": members.len(),
                "assessed_cents": sum("assessed_cents"),
                "funded_cents": sum("funded_cents"),
                "collected_cents": sum("paid_cents"),
                "outstanding_cents": sum("outstanding_cents"),
                "at_no_cost": members
                    .iter()
                    .filter(|row| row["assessed_cents"].as_i64().unwrap_or(0) == 0)
                    .count(),
                "self_reported": members
                    .iter()
                    .filter(|row| row["self_reported"].as_bool().unwrap_or(false))
                    .count(),
                "draws_unbooked": members
                    .iter()
                    .filter(|row| {
                        DRAW_UNSETTLED.contains(&row["draw_status"].as_str().unwrap_or_default())
                    })
                    .count(),
            }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Budget arithmetic and the ledger verdict
// ---------------------------------------------------------------------------

/// Add each line's variance (and its display form) to the rows a query returned.
fn budget_lines_with_variance(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let direction = row["direction"].as_str().unwrap_or(KIND_EXPENSE);
            let planned = row["planned_cents"].as_i64().unwrap_or(0);
            let actual = row["actual_cents"].as_i64().unwrap_or(0);
            let (variance, adverse) = budget_variance(direction, planned, actual);
            let mut line = row;
            if let Some(map) = line.as_object_mut() {
                map.insert("variance_cents".into(), json!(variance));
                map.insert("variance_display".into(), json!(format_cents(variance)));
                map.insert("adverse".into(), json!(adverse));
                map.insert("planned_display".into(), json!(format_cents(planned)));
                map.insert("actual_display".into(), json!(format_cents(actual)));
            }
            line
        })
        .collect()
}

/// The plan and the actual for a whole set of lines, plus how many lines are
/// running the wrong way.
fn budget_totals(lines: &[Value]) -> Value {
    let mut planned_income = 0i64;
    let mut actual_income = 0i64;
    let mut planned_expense = 0i64;
    let mut actual_expense = 0i64;
    let mut adverse = 0i64;
    let mut overlapping = 0i64;
    for line in lines {
        let planned = line["planned_cents"].as_i64().unwrap_or(0);
        let actual = line["actual_cents"].as_i64().unwrap_or(0);
        if line["direction"].as_str() == Some(KIND_INCOME) {
            planned_income += planned;
            actual_income += actual;
        } else {
            planned_expense += planned;
            actual_expense += actual;
        }
        if line["adverse"].as_bool().unwrap_or(false) {
            adverse += 1;
        }
        if line["overlaps_categories"].as_bool().unwrap_or(false) {
            overlapping += 1;
        }
    }
    let (income_variance, income_adverse) =
        budget_variance(KIND_INCOME, planned_income, actual_income);
    let (expense_variance, expense_adverse) =
        budget_variance(KIND_EXPENSE, planned_expense, actual_expense);
    json!({
        "planned_income_cents": planned_income,
        "actual_income_cents": actual_income,
        "income_variance_cents": income_variance,
        "income_adverse": income_adverse,
        "planned_expense_cents": planned_expense,
        "actual_expense_cents": actual_expense,
        "expense_variance_cents": expense_variance,
        "expense_adverse": expense_adverse,
        "lines": lines.len(),
        "adverse_lines": adverse,
        "lines_overlapping_categories": overlapping,
        "note": "A whole-fund line whose overlaps_categories is true sits beside category \
                 lines, so this total counts its actual twice — read the lines, or drop the \
                 whole-fund line.",
    })
}

/// Re-derive the books and judge them. Three queries: the ledger's totals, the
/// sum of every fund's balance, and the transfer groups.
async fn ledger_integrity(c: &PluginContext) -> Result<Value, SdkError> {
    let totals =
        c.db.query_one(sql_ledger_totals(c), vec![])
            .await?
            .unwrap_or_else(
                || json!({ "ledger_total_cents": 0, "entries": 0, "unpaired_transfers": 0 }),
            );
    let fund_total =
        c.db.query_one(sql_fund_total(c), vec![])
            .await?
            .unwrap_or_else(|| json!({ "fund_total_cents": 0 }));
    let groups = c.db.query(sql_transfer_groups(c), vec![]).await?;
    let mut verdict = ledger_verdict(
        totals["ledger_total_cents"].as_i64().unwrap_or(0),
        fund_total["fund_total_cents"].as_i64().unwrap_or(0),
        &groups,
    );
    if let Some(map) = verdict.as_object_mut() {
        map.insert("entries".into(), totals["entries"].clone());
        map.insert(
            "unpaired_transfers".into(),
            totals["unpaired_transfers"].clone(),
        );
    }
    Ok(verdict)
}

// ---------------------------------------------------------------------------
// payment.received → a booked income entry
// ---------------------------------------------------------------------------

/// `payment.received` (SPEC §5.4) → income in the fund the payment names.
///
/// **Idempotent**: the bus is a broadcast with replay, so the provider's payment
/// id is written to `external_ref` (unique) and a redelivery finds it and stops.
/// A payment names a fund by code, or lands wherever `dues_fund_code` says.
///
/// Queries: the idempotency probe, the fund, the entry. Then the audit write.
async fn on_payment_received(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    let payment: PaymentReceived = serde_json::from_value(ev.payload.clone()).map_err(|e| {
        SdkError::BadRequest(format!(
            "payment.received payload does not match PaymentReceived: {e}"
        ))
    })?;
    let reference = payment.payment_id.trim();
    if reference.is_empty() {
        return Err(SdkError::BadRequest(
            "payment.received payload has no payment_id — without it a replay cannot be told \
             from a second payment"
                .into(),
        ));
    }
    if payment.amount_cents <= 0 {
        return Err(SdkError::BadRequest(format!(
            "payment.received amount_cents is {}, which is not a deposit",
            payment.amount_cents
        )));
    }
    let already =
        c.db.exists(
            format!(
                "SELECT 1 FROM {tx} WHERE external_ref = $1",
                tx = c.db.table("transactions")
            ),
            vec![SqlValue::Text(reference.to_string())],
        )
        .await?;
    if already {
        return Ok(());
    }
    let code = payment
        .fund_code
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .unwrap_or(&configured_dues_fund(&c.config))
        .to_string();
    let fund =
        c.db.query_one(
            format!(
                "SELECT f.id, f.code, f.name FROM {funds} f WHERE f.code = $1",
                funds = c.db.table("funds")
            ),
            vec![SqlValue::Text(code.clone())],
        )
        .await?;
    let Some(fund) = fund else {
        return Err(SdkError::BadRequest(format!(
            "payment {reference:?} names fund {code:?}, which does not exist — create the fund \
             or send a fund_code that does"
        )));
    };
    let date = occurred_on(&payment.occurred_on).map_err(SdkError::BadRequest)?;
    let fiscal_year =
        fiscal_year_arg(payment.fiscal_year, date, &c.config).map_err(SdkError::BadRequest)?;
    let entry = LedgerEntry {
        fund: FundRef::Id(fund["id"].as_i64().unwrap_or_default()),
        amount_cents: payment.amount_cents,
        kind: KIND_INCOME,
        category: payment
            .category
            .as_deref()
            .map(str::trim)
            .filter(|category| !category.is_empty())
            .unwrap_or(CATEGORY_DUES)
            .to_string(),
        description: payment
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .unwrap_or("Payment received")
            .to_string(),
        member_id: payment
            .member_id
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        fiscal_year,
        occurred_on: date,
        recorded_by: "payment.received".to_string(),
        overdraft_authorized: false,
        external_ref: Some(reference.to_string()),
    };
    let rows = insert_entry(c, &entry).await?;
    let Some(recorded) = rows.first().cloned() else {
        // The `ON CONFLICT` caught a payment that arrived while this one was
        // being written: the wanted end state (one entry) holds either way.
        return Ok(());
    };
    c.audit
        .log(
            None,
            "payment.received",
            "transaction",
            &recorded["id"].as_i64().unwrap_or_default().to_string(),
            json!({
                "payment_id": reference,
                "fund_id": fund["id"],
                "fund_code": fund["code"],
                "amount_cents": entry.amount_cents,
                "category": entry.category,
                "member_id": entry.member_id,
                "fiscal_year": entry.fiscal_year,
                "source": ev.source,
            }),
        )
        .await?;
    c.events
        .publish(
            "finance.payment.recorded",
            json!({
                "transaction_id": recorded["id"],
                "payment_id": reference,
                "fund_id": fund["id"],
                "amount_cents": entry.amount_cents,
                "category": entry.category,
                "member_id": entry.member_id,
            }),
        )
        .await?;
    Ok(())
}

export_plugin!(FinancePlugin);

/// The tier a code names, or `None` for a code this plugin does not know.
pub fn tier_of(code: &str) -> Option<&'static Tier> {
    let code = code.trim().to_ascii_lowercase();
    TIERS.iter().find(|tier| tier.code == code)
}

/// What a share of a base cost comes to, in cents, rounded half-up to the cent.
///
/// `base * bps / 10 000` computed in `i128` and rounded once, at the end: the
/// multiplication happens before any division so no precision is lost to an
/// intermediate floor, and the result is clamped rather than wrapping.
pub fn assessed_cents(base_cents: i64, bps: i64) -> i64 {
    let base = i128::from(base_cents.max(0));
    let share = i128::from(bps.max(0));
    let rounded = (base * share + 5_000) / 10_000;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

/// What one tier assesses against a membership cost.
pub fn tier_assessment(base_cents: i64, tier: &str) -> Option<i64> {
    tier_of(tier).map(|tier| assessed_cents(base_cents, tier.bps))
}

/// A Lodge's levy: a fraction of the total membership cost (SPEC §7.5 "a Lodge
/// may levy dues as a fraction of total membership cost").
pub fn lodge_levy_cents(base_cents: i64, share_bps: i64) -> i64 {
    assessed_cents(base_cents, share_bps)
}

/// The whole scale for a base cost — what a client shows a scout who is choosing
/// a tier. Pure, so the numbers a scout sees are testable without a database.
pub fn scale_table(base_cents: i64) -> Vec<Value> {
    TIERS
        .iter()
        .map(|tier| {
            let cents = assessed_cents(base_cents, tier.bps);
            json!({
                "tier": tier.code,
                "label": tier.label,
                "share_bps": tier.bps,
                "share_percent": format_percent(tier.bps),
                "assessed_cents": cents,
                "assessed_display": format_cents(cents),
                "description": tier.description,
                "self_reportable": true,
            })
        })
        .collect()
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

// ---------------------------------------------------------------------------
// The fiscal year
// ---------------------------------------------------------------------------

/// The month a troop's fiscal year starts, from plugin config (default
/// January — a calendar year, which is what a troop that never configures this
/// expects). Read from `finance.<key>` or the top level, so an operator can
/// nest it under the plugin's own section or set it globally.
pub fn fiscal_year_start_month(config: &Value) -> u32 {
    config_i64(config, CONFIG_FISCAL_YEAR_START_MONTH)
        .filter(|month| (1..=12).contains(month))
        .map(|month| month as u32)
        .unwrap_or(1)
}

/// The fiscal year a date falls in. With a January start this is the calendar
/// year; otherwise a date before the start month belongs to the year before.
pub fn fiscal_year_of(date: NaiveDate, start_month: u32) -> i32 {
    if date.month() >= start_month.clamp(1, 12) {
        date.year()
    } else {
        date.year() - 1
    }
}

/// The first and last day of a fiscal year, inclusive — what the annual report
/// sums between.
pub fn fiscal_year_bounds(fiscal_year: i32, start_month: u32) -> (NaiveDate, NaiveDate) {
    let start_month = start_month.clamp(1, 12);
    let start = NaiveDate::from_ymd_opt(fiscal_year, start_month, 1).unwrap_or(NaiveDate::MIN);
    let end = NaiveDate::from_ymd_opt(fiscal_year + 1, start_month, 1)
        .and_then(|first_of_next| first_of_next.pred_opt())
        .unwrap_or(NaiveDate::MAX);
    (start, end)
}

/// An `i64` config value, under `finance.<key>` or the top level.
fn config_i64(config: &Value, key: &str) -> Option<i64> {
    config
        .get("finance")
        .and_then(|section| section.get(key))
        .or_else(|| config.get(key))
        .and_then(Value::as_i64)
}

/// A string config value, under `finance.<key>` or the top level.
fn config_str(config: &Value, key: &str) -> Option<String> {
    config
        .get("finance")
        .and_then(|section| section.get(key))
        .or_else(|| config.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// __PART2__

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formats_grouped_and_signed() {
        assert_eq!(format_cents(0), "$0.00");
        assert_eq!(format_cents(5), "$0.05");
        assert_eq!(format_cents(123_456), "$1,234.56");
        assert_eq!(format_cents(-4), "-$0.04");
        assert_eq!(format_cents(-123_456_789), "-$1,234,567.89");
        // A display form is never the storage form: cents round-trip exactly.
        for cents in [0i64, 1, 99, 100, 10_000, -1, -99, -100, 987_654_321] {
            assert_eq!(parse_dollars_to_cents(&format_cents(cents)), Ok(cents));
        }
    }

    #[test]
    fn money_parses_exactly_and_refuses_what_it_cannot() {
        assert_eq!(parse_dollars_to_cents("12"), Ok(1200));
        assert_eq!(parse_dollars_to_cents("12.5"), Ok(1250));
        assert_eq!(parse_dollars_to_cents("$1,234.56"), Ok(123_456));
        assert_eq!(parse_dollars_to_cents(" -3.05 "), Ok(-305));
        assert_eq!(parse_dollars_to_cents(".5"), Ok(50));
        for bad in ["", "abc", "12.345", "1.2.3", "12.", "$", "--3", "1e3"] {
            assert!(
                parse_dollars_to_cents(bad).is_err(),
                "{bad:?} must be refused rather than rounded"
            );
        }
        // Beyond i64 cents is refused, not wrapped.
        assert!(parse_dollars_to_cents("99999999999999999999").is_err());
    }

    #[test]
    fn the_sliding_scale_is_exact_and_never_demands_more_than_a_tier_says() {
        assert_eq!(assessed_cents(60_000, 20_000), 120_000); // patron: double
        assert_eq!(assessed_cents(60_000, 10_000), 60_000); // standard: the full cost
        assert_eq!(assessed_cents(60_000, 5_000), 30_000); // supported: half
        assert_eq!(assessed_cents(60_000, 0), 0); // hardship: nothing
        assert_eq!(MINIMUM_DUES_CENTS, 0);
        // Rounding is once, at the end, half-up.
        assert_eq!(assessed_cents(6_667, 5_000), 3_334); // 3333.5 → 3334
        assert_eq!(assessed_cents(1, 5_000), 1); // 0.5 → 1
        assert_eq!(assessed_cents(0, 20_000), 0);
        // A huge base cannot overflow into a wrong number.
        assert!(assessed_cents(i64::MAX, 20_000) > 0);
        assert_eq!(tier_assessment(60_000, "hardship"), Some(0));
        assert_eq!(tier_assessment(60_000, "nope"), None);
        assert_eq!(tier_of("HARDSHIP").map(|tier| tier.code), Some("hardship"));
    }

    #[test]
    fn the_scale_table_states_every_tier_with_its_arithmetic() {
        let table = scale_table(60_000);
        assert_eq!(table.len(), TIERS.len());
        let hardship = table
            .iter()
            .find(|tier| tier["tier"] == json!("hardship"))
            .expect("the hardship tier is on the scale");
        assert_eq!(hardship["assessed_cents"], json!(0));
        assert_eq!(hardship["assessed_display"], json!("$0.00"));
        assert_eq!(hardship["share_percent"], json!("0%"));
        let patron = table
            .iter()
            .find(|tier| tier["tier"] == json!("patron"))
            .expect("the patron tier is on the scale");
        assert_eq!(patron["assessed_display"], json!("$1,200.00"));
        assert_eq!(format_percent(1_250), "12.5%");
    }

    #[test]
    fn percentages_parse_to_basis_points_exactly() {
        assert_eq!(parse_percent_to_bps("10"), Ok(1_000));
        assert_eq!(parse_percent_to_bps("12.5%"), Ok(1_250));
        assert_eq!(parse_percent_to_bps("0.25"), Ok(25));
        assert_eq!(parse_percent_to_bps("100"), Ok(10_000));
        assert!(parse_percent_to_bps("0.005").is_err());
        // A Lodge's levy is a fraction of the total membership cost.
        assert_eq!(lodge_levy_cents(60_000, 1_000), 6_000);
        assert_eq!(lodge_levy_cents(60_000, 2_500), 15_000);
    }

    #[test]
    fn the_fiscal_year_follows_the_configured_start_month() {
        let january = json!({});
        assert_eq!(fiscal_year_start_month(&january), 1);
        let date = NaiveDate::from_ymd_opt(2026, 3, 15).expect("a date");
        assert_eq!(fiscal_year_of(date, 1), 2026);
        assert_eq!(
            fiscal_year_bounds(2026, 1),
            (
                NaiveDate::from_ymd_opt(2026, 1, 1).expect("a date"),
                NaiveDate::from_ymd_opt(2026, 12, 31).expect("a date")
            )
        );
        // A July fiscal year: March belongs to the year before.
        let july = json!({ "finance": { "fiscal_year_start_month": 7 } });
        assert_eq!(fiscal_year_start_month(&july), 7);
        assert_eq!(fiscal_year_of(date, 7), 2025);
        assert_eq!(
            fiscal_year_of(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), 7),
            2026
        );
        assert_eq!(
            fiscal_year_bounds(2026, 7),
            (
                NaiveDate::from_ymd_opt(2026, 7, 1).expect("a date"),
                NaiveDate::from_ymd_opt(2027, 6, 30).expect("a date")
            )
        );
        // A nonsense month falls back to January rather than panicking.
        assert_eq!(
            fiscal_year_start_month(&json!({ "fiscal_year_start_month": 99 })),
            1
        );
        // Config is read from `finance.<key>` or the top level.
        assert_eq!(
            configured_membership_cost(&json!({ "membership_cost_cents": 60_000 })),
            Some(60_000)
        );
        assert_eq!(
            configured_dues_fund(&json!({ "finance": { "dues_fund_code": "scholarship" } })),
            "scholarship"
        );
        assert_eq!(configured_dues_fund(&json!({})), FUND_GENERAL);
    }

    #[test]
    fn variance_is_favourable_positive_in_both_directions() {
        // Income: ahead of plan is good.
        assert_eq!(
            budget_variance(KIND_INCOME, 100_000, 85_000),
            (-15_000, true)
        );
        assert_eq!(
            budget_variance(KIND_INCOME, 100_000, 120_000),
            (20_000, false)
        );
        // Expense: under plan is good, and the actual is the ledger's own negative.
        assert_eq!(
            budget_variance(KIND_EXPENSE, 40_000, -15_000),
            (25_000, false)
        );
        assert_eq!(
            budget_variance(KIND_EXPENSE, 20_000, -25_000),
            (-5_000, true)
        );
        // Exactly to plan is not adverse.
        assert_eq!(budget_variance(KIND_EXPENSE, 20_000, -20_000), (0, false));
    }

    #[test]
    fn the_ledger_verdict_fails_closed() {
        let groups = vec![
            json!({ "transfer_group": "a", "entries": 2, "group_sum_cents": 0 }),
            json!({ "transfer_group": "b", "entries": 2, "group_sum_cents": 0 }),
        ];
        let verdict = ledger_verdict(70_000, 70_000, &groups);
        assert_eq!(verdict["balanced"], json!(true));
        assert_eq!(verdict["transfer_groups"], json!(2));
        assert_eq!(verdict["difference_cents"], json!(0));
        // A leg that does not cancel is not balanced.
        let broken = vec![json!({ "transfer_group": "c", "entries": 2, "group_sum_cents": 1 })];
        let verdict = ledger_verdict(70_000, 70_000, &broken);
        assert_eq!(verdict["balanced"], json!(false));
        assert_eq!(
            verdict["imbalanced_groups"][0]["transfer_group"],
            json!("c")
        );
        // A halved transfer is not balanced either.
        let halved = vec![json!({ "transfer_group": "d", "entries": 1, "group_sum_cents": -500 })];
        let verdict = ledger_verdict(70_000, 70_000, &halved);
        assert_eq!(verdict["balanced"], json!(false));
        // A funds total that disagrees with the ledger is not balanced.
        assert_eq!(
            ledger_verdict(70_000, 69_999, &groups)["balanced"],
            json!(false)
        );
    }

    #[test]
    fn the_vocabulary_the_api_accepts_is_the_vocabulary_constrained() {
        assert_eq!(FUND_KINDS[0], "general");
        assert!(FUND_KINDS.contains(&"commencement"));
        assert_eq!(DIRECT_KINDS, ["income", "expense"]);
        assert_eq!(DIRECTIONS, ["income", "expense"]);
        assert_eq!(DUES_STATUSES, ["assessed", "self_reported", "waived"]);
        assert_eq!(DUES_KINDS, ["member", "lodge"]);
        assert_eq!(normalize_kind("GENERAL"), Ok("general".to_string()));
        assert!(normalize_kind("meshcore").is_err());
        assert!(normalize_direct_kind("transfer").is_err());
        assert_eq!(
            normalize_ledger_kind("transfer"),
            Ok("transfer".to_string())
        );
        assert!(normalize_code("9lodge").is_err());
        assert_eq!(normalize_code("Lodge_4"), Ok("lodge_4".to_string()));
        assert_eq!(default_fund_name("general"), "General Fund");
        // The reserved category codes the dues arithmetic keys on.
        assert_eq!(CATEGORY_DUES, "dues");
        assert_eq!(CATEGORY_TRANSFER, KIND_TRANSFER);
    }
}
