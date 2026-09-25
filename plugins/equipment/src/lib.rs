//! # adjutant-equipment — gear inventory, checkout, and maintenance (SPEC §7.6).
//!
//! SPEC §7.6 gives this plugin five responsibilities: an inventory catalog
//! (items, condition, location), checkout/checkin tracking, maintenance
//! schedules, replacement flagging, and an availability view for mission
//! planning. This crate is those five, plus the schema (`equipment.items`,
//! `equipment.checkouts`) they live on.
//!
//! ## An item is one physical thing, not a quantity
//!
//! Three identical tents are **three rows**. That single decision is what makes
//! everything else honest: a checkout names the actual item (`checkouts.item_id`
//! is a uuid-less but unambiguous identity), so "who has the good tent" has an
//! answer and "which tent came back damaged" has an attribution. A catalog that
//! stored `quantity = 3` would be able to say neither. Category, condition and
//! location are therefore properties of an individual item, not of a model.
//!
//! ## Checkout/checkin is a state machine, and the database enforces it
//!
//! An item is *out* exactly while it has an **open** checkout
//! (`checked_in_at IS NULL`). Two rules follow, and both are enforced twice —
//! once in the handler so the caller gets a useful `409`, and once in the
//! database so a race or a direct SQL session cannot get around them:
//!
//! * **An item cannot be checked out twice.** `idx_checkouts_open_item` is a
//!   partial unique index on `item_id WHERE checked_in_at IS NULL`. The handler
//!   checks first and answers `409` naming the holder; the index is the backstop
//!   for two simultaneous requests.
//! * **A checkin references an open checkout.** `checked_in_at` and
//!   `condition_in` arrive together (`checkouts_returned_consistent`), so a row
//!   is either open and awaiting a condition, or closed and carrying one. A
//!   checkin with no open checkout is a `409`, never a silent second row.
//!
//! The checkout log is append-only in practice: nothing is edited except to
//! close a checkout, and nothing is deleted except with the item. Who held what,
//! when, and in what condition is the record.
//!
//! ## Condition is recorded twice, so damage is attributable
//!
//! Every checkout records `condition_out` (the grade the item was in when it
//! left) and every checkin records `condition_in` (the grade it came back in,
//! and it is *required* — an unchecked checkin cannot happen). The pair brackets
//! a period of use: a downgrade between them happened while that member had it,
//! and [`is_downgrade`] is the arithmetic that says so. The checkin also carries
//! the item's condition forward and increments `service_count`, which is the
//! input to replacement flagging.
//!
//! ## Maintenance takes an item out of the pool without losing it
//!
//! Three item states: `available`, `maintenance`, `retired`.
//! `POST …/maintenance` moves `available → maintenance` (`maintenance_since`,
//! `maintenance_until`, `maintenance_note`) and thereby removes the item from
//! every availability answer; `POST …/return-to-service` moves it back and also
//! clears a retirement (a mis-retired item does not need SQL to recover).
//! `retired` is the terminal-ish state for an item that is gone.
//!
//! Maintenance is *not* an automatic expiry: `maintenance_until` is the date the
//! troop expects the item back, not a date on which it silently becomes
//! available again. A month in a repair queue is not a month of availability.
//! Separately, `next_service_on` is a **maintenance schedule** on an item that is
//! still in the pool (oil the lantern, re-seal the tent), surfaced by
//! `GET /api/equipment/maintenance` and by the daily `maintenance_due` schedule,
//! which publishes one event when something is due rather than nothing at all.
//!
//! ## Replacement flagging: derived, with a human override
//!
//! An item is a replacement **candidate** when a rule fires — its condition is
//! `poor`/`unserviceable`, or its `service_count` / age crosses a threshold
//! ([`Thresholds`], configurable through the plugin's `core.plugins.config`).
//! That set is computed, never stored, so it cannot go stale: fix the condition
//! and the candidate disappears. `replacement_flagged` is the separate **human**
//! decision ("this one, when the budget allows"), and shows up as its own reason.
//! A checkin that *creates* a candidate publishes `equipment.replacement.flagged`
//! — only on the transition, so the event means "this just became a problem".
//!
//! ## Availability is what a mission planner asks
//!
//! `GET /api/equipment/availability?from=&to=` answers "what can I take on these
//! dates?" by partitioning the pool into available and unavailable, and naming
//! the reason for each refusal (`checked_out`, `in_maintenance`, `retired`,
//! `unserviceable`) plus who holds it and when it is due back. The window
//! arithmetic is [`overlaps`] — an inclusive day range — and it is computed in
//! Rust over the fetched rows rather than in one clever `LEFT JOIN`, so the part
//! a planner depends on is unit-tested rather than eyeballed.
//!
//! A checkout blocks the whole window it touches. A closed checkout ends on its
//! return date; an **open** one ends on its `due_on` *when that date is still in
//! the future*, and otherwise blocks indefinitely ([`checkout_end`]). An overdue
//! open checkout is not a promise anyone should plan against.
//!
//! ## Permissions
//!
//! `equipment:read` (catalogue, checkouts, availability), `equipment:write`
//! (add/edit catalogue entries), `equipment:checkout` (take gear out, bring it
//! back), `equipment:manage` (maintenance, retirement, deletion). Every route is
//! gated at **troop** scope: SPEC §7.6 gives equipment no Lodge-scoped authority
//! — the gear pool is the troop's, and `location` is where it physically lives,
//! not an ownership boundary. Role grants are the operator's business, not the
//! plugin's (see `docs/api-reference.md`).
//!
//! ## Bind values: every date is text, then cast
//!
//! `SqlValue::Null` is a **TEXT** null, and the SDK's own note records what
//! happens when one is bound where a typed column is expected. So every date
//! parameter here is bound as text and cast in SQL — `($7::text)::date` — which
//! is correct for `NULL` (`NULL::text::date` is `NULL`) and for `2026-10-01`
//! alike, and never depends on PostgreSQL inferring a parameter type. The same
//! reasoning makes `(c.checked_out_at AT TIME ZONE 'UTC')::date` the way a
//! timestamp becomes a day: the session's `TimeZone` setting must not decide
//! which day a checkout started.
//!
//! ## DDL notes
//!
//! * `"condition"` is quoted. It is the domain's word for the column and is safe
//!   in PostgreSQL, but quoting it removes the question.
//! * CHECK constraints here are immutable expressions only. `due_on >= the day
//!   of checkout` is *not* a CHECK (a timestamp→day conversion is not immutable,
//!   so it does not belong in one); the handler enforces it and answers `400`.
//! * Indexes are `IF NOT EXISTS` so a re-run of the migration is a no-op.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

/// Declare one route: the constructor, the path, the permission, and the async
/// handler the core calls. Each declaration hands the core a closure owning a
/// clone of the plugin's context — the same shape every plugin has, so it is
/// written once here rather than sixteen times in the manifest.
macro_rules! route {
    ($ctx:expr, $ctor:ident, $path:expr, $permission:expr, $handler:ident) => {{
        let c = $ctx.clone();
        RouteDefinition::$ctor(
            $path,
            $permission,
            route_handler(move |req| {
                let c = c.clone();
                async move { $handler(&c, req).await }
            }),
        )
    }};
}

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// In the pool and usable.
pub const STATUS_AVAILABLE: &str = "available";
/// Flagged for service: out of the available pool, not lost.
pub const STATUS_MAINTENANCE: &str = "maintenance";
/// Gone from the pool (sold, scrapped, lost).
pub const STATUS_RETIRED: &str = "retired";

pub const ITEM_STATUSES: [&str; 3] = [STATUS_AVAILABLE, STATUS_MAINTENANCE, STATUS_RETIRED];

/// Condition grades, best to worst. The order is the ranking [`condition_rank`]
/// encodes — a grade list that disagrees with the ranking is a bug, so both live
/// next to each other.
pub const CONDITION_NEW: &str = "new";
pub const CONDITION_GOOD: &str = "good";
pub const CONDITION_FAIR: &str = "fair";
pub const CONDITION_POOR: &str = "poor";
pub const CONDITION_UNSERVICEABLE: &str = "unserviceable";

pub const CONDITIONS: [&str; 5] = [
    CONDITION_NEW,
    CONDITION_GOOD,
    CONDITION_FAIR,
    CONDITION_POOR,
    CONDITION_UNSERVICEABLE,
];

/// What a new catalogue entry is assumed to be when nobody says otherwise.
pub const DEFAULT_CONDITION: &str = CONDITION_GOOD;
/// What an item with no category is filed under.
pub const DEFAULT_CATEGORY: &str = "other";

/// The grades that, by themselves, make an item a replacement candidate.
pub const POOR_CONDITIONS: [&str; 2] = [CONDITION_POOR, CONDITION_UNSERVICEABLE];

/// Catalogue categories. Free text would fragment ("tent", "tents", "Tents") and
/// the availability view filters on this.
pub const CATEGORIES: [&str; 15] = [
    "tent",
    "shelter",
    "pack",
    "sleeping",
    "cooking",
    "stove",
    "rope",
    "climbing",
    "water",
    "first_aid",
    "tool",
    "radio",
    "navigation",
    "uniform",
    "other",
];

/// Default availability window (days) when the caller names neither end.
pub const DEFAULT_AVAILABILITY_DAYS: i64 = 7;
/// Longest availability window the API will answer for.
pub const MAX_AVAILABILITY_DAYS: i64 = 365;
/// Default page size for list routes.
pub const DEFAULT_LIMIT: i64 = 50;
/// Largest page size for list routes.
pub const MAX_LIMIT: i64 = 200;

/// Event types this plugin publishes. The bus does not validate names, so
/// constants are the only thing keeping a publisher and a subscriber agreeing.
pub mod event {
    /// A catalogue entry was created.
    pub const ITEM_CREATED: &str = "equipment.item.created";
    /// A catalogue entry was edited.
    pub const ITEM_UPDATED: &str = "equipment.item.updated";
    /// An item was retired.
    pub const ITEM_RETIRED: &str = "equipment.item.retired";
    /// An item was deleted (only ever one with no history).
    pub const ITEM_DELETED: &str = "equipment.item.deleted";
    /// Gear went out.
    pub const CHECKED_OUT: &str = "equipment.checked_out";
    /// Gear came back — with the condition it came back in.
    pub const CHECKED_IN: &str = "equipment.checked_in";
    /// Item moved into maintenance (out of the pool).
    pub const MAINTENANCE_FLAGGED: &str = "equipment.maintenance.flagged";
    /// Item came back into the pool.
    pub const MAINTENANCE_CLEARED: &str = "equipment.maintenance.cleared";
    /// A service date was set on an item that is still in the pool.
    pub const MAINTENANCE_SCHEDULED: &str = "equipment.maintenance.scheduled";
    /// The daily schedule found something in maintenance or due for service.
    pub const MAINTENANCE_DUE: &str = "equipment.maintenance.due";
    /// An item just *became* a replacement candidate.
    pub const REPLACEMENT_FLAGGED: &str = "equipment.replacement.flagged";
}

// ---------------------------------------------------------------------------
// Pure arithmetic — the parts a planner and a quartermaster depend on
// ---------------------------------------------------------------------------

/// Today, on the server's clock. The plugin has no timezone database of its own
/// and none of this arithmetic is finer than a day.
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Parse a `YYYY-MM-DD` date. Strict on purpose: a date that PostgreSQL would
/// read differently from what the caller meant is worse than a `400`.
pub fn parse_date(raw: &str) -> Result<NaiveDate, String> {
    let value = raw.trim();
    if value.len() != 10 {
        return Err(format!("{value:?} is not a date — use YYYY-MM-DD"));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{value:?} is not a date — use YYYY-MM-DD"))
}

/// Parse an optional date. `Some("")` and `Some("  ")` are `None` (a blank form
/// field means "not given", not "invalid").
pub fn parse_optional_date(raw: Option<&str>) -> Result<Option<NaiveDate>, String> {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => parse_date(value).map(Some),
        None => Ok(None),
    }
}

/// A grade's rank, best (`new`, 0) to worst (`unserviceable`, 4).
///
/// An unrecognised grade ranks **worst**, not middle: the database constrains
/// the column, so this only fires on a corrupt or hand-edited row, and a
/// comparison that cannot read a grade must not conclude "no damage".
pub fn condition_rank(grade: &str) -> i32 {
    match grade.trim().to_ascii_lowercase().as_str() {
        CONDITION_NEW => 0,
        CONDITION_GOOD => 1,
        CONDITION_FAIR => 2,
        CONDITION_POOR => 3,
        _ => 4,
    }
}

/// Did the item get worse between two grades — the damage question.
pub fn is_downgrade(from: &str, to: &str) -> bool {
    condition_rank(to) > condition_rank(from)
}

/// Is this grade unserviceable — out of the pool even when its status says
/// `available`? (A grade that cannot be read ranks worst, so an unreadable
/// grade is treated as unserviceable: fail closed, not open.)
pub fn is_unserviceable(grade: &str) -> bool {
    condition_rank(grade) >= condition_rank(CONDITION_UNSERVICEABLE)
}

/// Completed years between two dates (a birthday-style count, not
/// `days / 365`).
pub fn age_years(acquired: NaiveDate, on: NaiveDate) -> i64 {
    let mut years = on.year() as i64 - acquired.year() as i64;
    if (on.month(), on.day()) < (acquired.month(), acquired.day()) {
        years -= 1;
    }
    years.max(0)
}

/// Do two inclusive day ranges touch?
///
/// `end` is `None` for a checkout that has not come back and has no reliable
/// return date — that blocks every window that starts on or after it went out.
/// An inclusive range is what a planner means by "from the 3rd to the 5th": the
/// item is wanted on both of those days.
pub fn overlaps(from: NaiveDate, to: NaiveDate, start: NaiveDate, end: Option<NaiveDate>) -> bool {
    match end {
        Some(end) => start <= to && end >= from,
        None => start <= to,
    }
}

/// The day a checkout stops occupying its item; `None` means "indefinitely".
///
/// * a **closed** checkout ends the day it came back;
/// * an **open** checkout ends on its `due_on` *only while that date is still in
///   the future*. An overdue open checkout is a promise already broken, so it
///   blocks indefinitely rather than quietly freeing the item.
pub fn checkout_end(
    open: bool,
    checked_in_on: Option<NaiveDate>,
    due_on: Option<NaiveDate>,
    on: NaiveDate,
) -> Option<NaiveDate> {
    if !open {
        return checked_in_on;
    }
    match due_on {
        Some(due) if due >= on => Some(due),
        _ => None,
    }
}

/// Is an open checkout past its due date? (A closed one is never overdue: it
/// came back, however late.)
pub fn is_overdue(open: bool, due_on: Option<NaiveDate>, on: NaiveDate) -> bool {
    open && due_on.is_some_and(|due| due < on)
}

/// The `[start, end)` a checkout row occupies, read from the columns
/// `CHECKOUT_FIELDS` selects. `None` when the row has no parsable start date.
pub fn checkout_window(row: &Value, on: NaiveDate) -> Option<(NaiveDate, Option<NaiveDate>)> {
    let start = date_field(row, "checked_out_on")?;
    let end = checkout_end(
        row["open"].as_bool().unwrap_or(false),
        date_field(row, "checked_in_on"),
        date_field(row, "due_on"),
        on,
    );
    Some((start, end))
}

fn date_field(row: &Value, key: &str) -> Option<NaiveDate> {
    let raw = row[key].as_str()?;
    parse_date(raw).ok()
}

// ---------------------------------------------------------------------------
// Thresholds (the plugin's config)
// ---------------------------------------------------------------------------

/// When an item's condition, use, or age says "replace it".
///
/// Read from the plugin's `core.plugins.config`:
///
/// ```json
/// { "replacement_service_count": 40, "replacement_age_years": 10,
///   "maintenance_lead_days": 14, "overdue_grace_days": 0 }
/// ```
///
/// Every value is clamped to a sane range, so a typo cannot produce a threshold
/// that flags the whole catalogue (or none of it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Thresholds {
    /// Completed checkouts at or above which the item is a candidate.
    pub replacement_service_count: i64,
    /// Years since `acquired_on` at or above which the item is a candidate.
    pub replacement_age_years: i64,
    /// Grades that are, on their own, a reason to replace.
    pub replacement_conditions: Vec<String>,
    /// How far ahead a service date counts as "due".
    pub maintenance_lead_days: i64,
    /// Days past a due date before an open checkout is called overdue.
    pub overdue_grace_days: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            replacement_service_count: 40,
            replacement_age_years: 10,
            replacement_conditions: POOR_CONDITIONS.iter().map(|c| c.to_string()).collect(),
            maintenance_lead_days: 14,
            overdue_grace_days: 0,
        }
    }
}

impl Thresholds {
    /// Read thresholds from the plugin's config, falling back to the defaults
    /// for anything absent or unreadable.
    pub fn from_config(config: &Value) -> Self {
        let defaults = Self::default();
        let grades = config
            .get("replacement_conditions")
            .and_then(Value::as_array)
            .map(|values| {
                let mut grades: Vec<String> = values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|g| g.trim().to_ascii_lowercase())
                    .filter(|g| CONDITIONS.contains(&g.as_str()))
                    .collect();
                grades.sort();
                grades.dedup();
                grades
            })
            .filter(|grades| !grades.is_empty())
            .unwrap_or(defaults.replacement_conditions);
        Self {
            replacement_service_count: config_i64(config, "replacement_service_count", 1, 10_000)
                .unwrap_or(defaults.replacement_service_count),
            replacement_age_years: config_i64(config, "replacement_age_years", 1, 200)
                .unwrap_or(defaults.replacement_age_years),
            replacement_conditions: grades,
            maintenance_lead_days: config_i64(config, "maintenance_lead_days", 0, 365)
                .unwrap_or(defaults.maintenance_lead_days),
            overdue_grace_days: config_i64(config, "overdue_grace_days", 0, 365)
                .unwrap_or(defaults.overdue_grace_days),
        }
    }
}

fn config_i64(config: &Value, key: &str, min: i64, max: i64) -> Option<i64> {
    config.get(key).and_then(Value::as_i64).map(|v| v.clamp(min, max))
}

/// The thresholds in force for this plugin instance.
pub fn thresholds(c: &PluginContext) -> Thresholds {
    Thresholds::from_config(&c.config)
}

// ---------------------------------------------------------------------------
// Replacement flagging
// ---------------------------------------------------------------------------

/// Why an item is a replacement candidate; empty means it is fine.
///
/// The reasons are stable codes, not sentences: a client decides how to phrase
/// "condition_unserviceable" (and may sort by it), whereas a sentence baked in
/// here would be un-translatable and un-testable.
pub fn replacement_reasons(item: &Value, on: NaiveDate, t: &Thresholds) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    if item["replacement_flagged"].as_bool().unwrap_or(false) {
        reasons.push("flagged".to_string());
    }
    let grade = item["condition"].as_str().unwrap_or(DEFAULT_CONDITION);
    if t.replacement_conditions.iter().any(|c| c == grade) {
        reasons.push(format!("condition_{grade}"));
    }
    let service_count = item["service_count"].as_i64().unwrap_or(0);
    if service_count >= t.replacement_service_count {
        reasons.push("service_count".to_string());
    }
    if let Some(acquired) = date_field(item, "acquired_on") {
        if age_years(acquired, on) >= t.replacement_age_years {
            reasons.push("age".to_string());
        }
    }
    reasons
}

/// The replacement candidates in `items`, worst first.
///
/// Sorted by number of reasons, then by condition, then by id — a stable order
/// so two calls agree, and so the item that is both unserviceable and worn out
/// leads the list.
pub fn replacement_candidates(items: &[Value], on: NaiveDate, t: &Thresholds) -> Vec<Value> {
    let mut candidates: Vec<Value> = items
        .iter()
        .filter_map(|item| {
            let reasons = replacement_reasons(item, on, t);
            if reasons.is_empty() {
                return None;
            }
            let grade = item["condition"].as_str().unwrap_or(DEFAULT_CONDITION);
            Some(json!({
                "item": item,
                "reasons": reasons,
                "condition_rank": condition_rank(grade),
                "service_count": item["service_count"].as_i64().unwrap_or(0),
                "age_years": date_field(item, "acquired_on").map(|a| age_years(a, on)),
            }))
        })
        .collect();
    candidates.sort_by(|a, b| {
        let ar = a["reasons"].as_array().map(Vec::len).unwrap_or(0);
        let br = b["reasons"].as_array().map(Vec::len).unwrap_or(0);
        br.cmp(&ar)
            .then_with(|| {
                b["condition_rank"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["condition_rank"].as_i64().unwrap_or(0))
            })
            .then_with(|| {
                a["item"]["id"].as_i64().unwrap_or(0).cmp(&b["item"]["id"].as_i64().unwrap_or(0))
            })
    });
    candidates
}

// ---------------------------------------------------------------------------
// Maintenance report
// ---------------------------------------------------------------------------

/// The maintenance picture: what is out of the pool, what is due, what is
/// scheduled, and what has no schedule at all despite being in poor shape.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    /// `status = maintenance` — out of the pool right now.
    pub in_service: Vec<Value>,
    /// A `next_service_on` in the past.
    pub overdue: Vec<Value>,
    /// A `next_service_on` inside the lead window.
    pub due: Vec<Value>,
    /// A `next_service_on` further out than the lead window.
    pub scheduled: Vec<Value>,
    /// Poor/unserviceable but with no `next_service_on` — somebody should decide.
    pub needs_schedule: Vec<Value>,
}

impl MaintenanceReport {
    /// Does anything in here want attention today?
    pub fn has_due_work(&self) -> bool {
        !self.in_service.is_empty() || !self.overdue.is_empty() || !self.due.is_empty()
    }

    /// The items called out loudest, for an event payload.
    pub fn due_item_ids(&self) -> Vec<i64> {
        self.in_service
            .iter()
            .chain(self.overdue.iter())
            .chain(self.due.iter())
            .filter_map(|item| item["id"].as_i64())
            .collect()
    }
}

/// Bucket items by maintenance state. Pure: the buckets are the whole point of
/// the maintenance view, and they are testable without a clock or a database.
pub fn maintenance_report(items: &[Value], on: NaiveDate, t: &Thresholds) -> MaintenanceReport {
    let lead = on + chrono::Duration::days(t.maintenance_lead_days);
    let mut report = MaintenanceReport::default();
    for item in items {
        if item["status"].as_str().unwrap_or(STATUS_AVAILABLE) == STATUS_MAINTENANCE {
            report.in_service.push(item.clone());
            continue;
        }
        match date_field(item, "next_service_on") {
            Some(when) if when < on => report.overdue.push(item.clone()),
            Some(when) if when <= lead => report.due.push(item.clone()),
            Some(_) => report.scheduled.push(item.clone()),
            None => {
                let grade = item["condition"].as_str().unwrap_or(DEFAULT_CONDITION);
                if t.replacement_conditions.iter().any(|c| c == grade) {
                    report.needs_schedule.push(item.clone());
                }
            }
        }
    }
    report
}

/// The report as JSON, with the counts a dashboard shows and the thresholds it
/// was computed with (so a surprising answer is explicable).
pub fn maintenance_report_json(
    report: &MaintenanceReport,
    on: NaiveDate,
    t: &Thresholds,
) -> Value {
    json!({
        "today": on.to_string(),
        "lead_days": t.maintenance_lead_days,
        "in_service": report.in_service,
        "overdue": report.overdue,
        "due": report.due,
        "scheduled": report.scheduled,
        "needs_schedule": report.needs_schedule,
        "counts": {
            "in_service": report.in_service.len(),
            "overdue": report.overdue.len(),
            "due": report.due.len(),
            "scheduled": report.scheduled.len(),
            "needs_schedule": report.needs_schedule.len(),
        },
        "thresholds": t,
    })
}

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

/// The pool, partitioned for one window.
#[derive(Debug, Clone, Default)]
pub struct Availability {
    /// In the pool, in usable condition, and not out across the window.
    pub available: Vec<Value>,
    /// Everything else, each with the reasons why.
    pub unavailable: Vec<Value>,
}

impl Availability {
    /// How many items each reason accounts for. An item can carry more than one
    /// reason (unserviceable *and* out), so these do not sum to `unavailable`.
    pub fn reason_counts(&self) -> Value {
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for entry in &self.unavailable {
            if let Some(reasons) = entry["reasons"].as_array() {
                for reason in reasons.iter().filter_map(Value::as_str) {
                    *counts.entry(reason.to_string()).or_insert(0) += 1;
                }
            }
        }
        let mut out = serde_json::Map::new();
        for (reason, count) in counts {
            out.insert(reason, json!(count));
        }
        Value::Object(out)
    }
}

/// Partition the pool for the inclusive window `from..=to`.
///
/// The order of the reasons matters — it is the order a planner should read
/// them in: an item that is retired *and* on a checkout is, first of all, gone.
pub fn partition_availability(
    items: &[Value],
    checkouts: &[Value],
    from: NaiveDate,
    to: NaiveDate,
    on: NaiveDate,
) -> Availability {
    let mut by_item: BTreeMap<i64, Vec<&Value>> = BTreeMap::new();
    for checkout in checkouts {
        if let Some(item_id) = checkout["item_id"].as_i64() {
            by_item.entry(item_id).or_default().push(checkout);
        }
    }

    let mut result = Availability::default();
    for item in items {
        let mut reasons: Vec<&str> = Vec::new();
        let mut blocking: Vec<Value> = Vec::new();
        match item["status"].as_str().unwrap_or(STATUS_AVAILABLE) {
            STATUS_RETIRED => reasons.push("retired"),
            STATUS_MAINTENANCE => reasons.push("in_maintenance"),
            _ => {}
        }
        if is_unserviceable(item["condition"].as_str().unwrap_or(DEFAULT_CONDITION)) {
            reasons.push("unserviceable");
        }
        if let Some(item_id) = item["id"].as_i64() {
            if let Some(rows) = by_item.get(&item_id) {
                for checkout in rows {
                    let Some((start, end)) = checkout_window(checkout, on) else {
                        continue;
                    };
                    if overlaps(from, to, start, end) {
                        reasons.push("checked_out");
                        blocking.push(json!({
                            "checkout_id": checkout["id"],
                            "checked_out_on": start.to_string(),
                            "due_on": checkout["due_on"],
                            "held_by": checkout["checked_out_by"],
                            "purpose": checkout["purpose"],
                            "mission_id": checkout["mission_id"],
                            "condition_out": checkout["condition_out"],
                            "open": checkout["open"],
                            // The raw promise, not the window end: an overdue open
                            // checkout has no reliable end, which is exactly why
                            // the promise bit is worth reporting.
                            "overdue": is_overdue(
                                checkout["open"].as_bool().unwrap_or(false),
                                date_field(checkout, "due_on"),
                                on,
                            ),
                        }));
                        break;
                    }
                }
            }
        }
        if reasons.is_empty() {
            result.available.push(item.clone());
        } else {
            result.unavailable.push(json!({
                "item": item,
                "reasons": reasons,
                "blocking": blocking,
            }));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Trim to a non-empty value, or `None` (a blank form field is "not given").
fn trimmed(value: &Option<String>) -> Option<String> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

fn normalize_category(value: &Option<String>) -> Result<String, String> {
    let category = trimmed(value)
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_CATEGORY.to_string());
    if !CATEGORIES.contains(&category.as_str()) {
        return Err(format!("category must be one of {}", CATEGORIES.join(", ")));
    }
    Ok(category)
}

fn normalize_condition(value: &Option<String>) -> Result<String, String> {
    let grade = trimmed(value)
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_CONDITION.to_string());
    if !CONDITIONS.contains(&grade.as_str()) {
        return Err(format!("condition must be one of {}", CONDITIONS.join(", ")));
    }
    Ok(grade)
}

fn validate_grade(raw: &str, field: &str) -> Result<String, String> {
    let grade = raw.trim().to_ascii_lowercase();
    if !CONDITIONS.contains(&grade.as_str()) {
        return Err(format!("{field} must be one of {}", CONDITIONS.join(", ")));
    }
    Ok(grade)
}

fn caller(req: &PluginRequest) -> String {
    req.identity.as_ref().map(|i| i.user_id.clone()).unwrap_or_default()
}

fn limit_of(req: &PluginRequest) -> i64 {
    req.query_int("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Optional date query/body field with a field name in the error message.
fn optional_date(value: &Option<String>, field: &str) -> Result<Option<NaiveDate>, String> {
    parse_optional_date(value.as_deref()).map_err(|e| format!("{field}: {e}"))
}

/// Append `column = $n` to a dynamic `SET` list, keeping the placeholder number
/// and the bind order in step by construction (they are the same push).
fn set_clause(sets: &mut Vec<String>, params: &mut Vec<SqlValue>, column: &str, value: SqlValue) {
    params.push(value);
    sets.push(format!("{column} = ${}", params.len()));
}

// ---------------------------------------------------------------------------
// Row shapes and fetch helpers
// ---------------------------------------------------------------------------

/// Every item column the API states.
const ITEM_FIELDS: &str = r#"
    i.id, i.name, i.asset_tag, i.category, i.description, i."condition",
    i.location, i.acquired_on::text AS acquired_on, i.source, i.service_count,
    i.next_service_on::text AS next_service_on, i.status,
    i.maintenance_since::text AS maintenance_since,
    i.maintenance_until::text AS maintenance_until, i.maintenance_note,
    i.replacement_flagged, i.replacement_note,
    i.retired_at::text AS retired_at, i.retired_reason,
    i.created_by, i.created_at::text AS created_at, i.updated_at::text AS updated_at
"#;

/// Every checkout column the API states, joined to the item it names.
const CHECKOUT_FIELDS: &str = r#"
    c.id, c.item_id, i.name AS item_name, i.asset_tag, i.category,
    c.checked_out_by, c.checked_out_at::text AS checked_out_at,
    (c.checked_out_at AT TIME ZONE 'UTC')::date::text AS checked_out_on,
    c.due_on::text AS due_on, c.purpose, c.mission_id, c.destination,
    c.condition_out, c.note_out,
    c.checked_in_at::text AS checked_in_at,
    (c.checked_in_at AT TIME ZONE 'UTC')::date::text AS checked_in_on,
    c.checked_in_by, c.condition_in, c.note_in, c.damaged,
    (c.checked_in_at IS NULL) AS open
"#;

async fn fetch_item(c: &PluginContext, id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {} FROM {} i WHERE i.id = $1",
            ITEM_FIELDS,
            c.db.table("items")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// The item's open checkout, if it has one — the state-machine question every
/// mutating handler asks before it writes.
async fn open_checkout(c: &PluginContext, item_id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {} FROM {checkouts} c JOIN {items} i ON i.id = c.item_id \
             WHERE c.item_id = $1 AND c.checked_in_at IS NULL \
             ORDER BY c.checked_out_at DESC LIMIT 1",
            CHECKOUT_FIELDS,
            checkouts = c.db.table("checkouts"),
            items = c.db.table("items")
        ),
        vec![SqlValue::Int(item_id)],
    )
    .await
}

/// The item's checkout log, newest first.
async fn item_checkouts(
    c: &PluginContext,
    item_id: i64,
    limit: i64,
) -> Result<Vec<Value>, SdkError> {
    c.db.query(
        format!(
            "SELECT {} FROM {checkouts} c JOIN {items} i ON i.id = c.item_id \
             WHERE c.item_id = $1 ORDER BY c.checked_out_at DESC, c.id DESC LIMIT $2",
            CHECKOUT_FIELDS,
            checkouts = c.db.table("checkouts"),
            items = c.db.table("items")
        ),
        vec![SqlValue::Int(item_id), SqlValue::Int(limit.clamp(1, MAX_LIMIT))],
    )
    .await
}

/// The derived replacement/availability/maintenance block a single item's
/// detail response carries.
fn item_flags(item: &Value, on: NaiveDate, t: &Thresholds) -> Value {
    let reasons = replacement_reasons(item, on, t);
    let grade = item["condition"].as_str().unwrap_or(DEFAULT_CONDITION);
    let status = item["status"].as_str().unwrap_or(STATUS_AVAILABLE);
    let next_service_on = date_field(item, "next_service_on");
    json!({
        "status": status,
        "in_pool": status == STATUS_AVAILABLE && !is_unserviceable(grade),
        "condition_rank": condition_rank(grade),
        "replacement": {
            "candidate": !reasons.is_empty(),
            "reasons": reasons,
            "flagged": item["replacement_flagged"].as_bool().unwrap_or(false),
            "service_count": item["service_count"].as_i64().unwrap_or(0),
            "age_years": date_field(item, "acquired_on").map(|a| age_years(a, on)),
            "thresholds": { "service_count": t.replacement_service_count, "age_years": t.replacement_age_years },
        },
        "maintenance": {
            "in_service": status == STATUS_MAINTENANCE,
            "since": item["maintenance_since"],
            "until": item["maintenance_until"],
            "note": item["maintenance_note"],
            "next_service_on": item["next_service_on"],
            "service_due": next_service_on.is_some_and(|when| when <= on + chrono::Duration::days(t.maintenance_lead_days)),
            "service_overdue": next_service_on.is_some_and(|when| when < on),
        },
    })
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ItemBody {
    name: String,
    #[serde(default)]
    asset_tag: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    acquired_on: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    next_service_on: Option<String>,
    #[serde(default)]
    replacement_note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ItemEditBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    asset_tag: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    acquired_on: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    next_service_on: Option<String>,
    /// The human replacement decision.
    #[serde(default)]
    replacement_flagged: Option<bool>,
    #[serde(default)]
    replacement_note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckoutBody {
    /// Who is taking it. Defaults to the caller.
    #[serde(default)]
    checked_out_by: Option<String>,
    #[serde(default)]
    due_on: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    mission_id: Option<i64>,
    #[serde(default)]
    destination: Option<String>,
    /// The grade the item leaves in. Defaults to the item's current condition.
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckinBody {
    /// Required: a checkin that does not state the condition cannot attribute
    /// damage, which is the point of recording it.
    condition: String,
    #[serde(default)]
    note: Option<String>,
    /// Overrides the computed answer (a downgrade in grade).
    #[serde(default)]
    damaged: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MaintenanceBody {
    /// Why it is out of the pool.
    reason: String,
    /// When it is expected back, if anyone knows.
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    next_service_on: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReturnBody {
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// A new service date, if the item should keep one.
    #[serde(default)]
    next_service_on: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScheduleServiceBody {
    on: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RetireBody {
    reason: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/equipment/item` — add one physical thing to the catalogue.
async fn create_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let body: ItemBody = req.json()?;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return PluginResponse::error(400, "name is required");
    }
    let category = match normalize_category(&body.category) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let grade = match normalize_condition(&body.condition) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let acquired_on = match optional_date(&body.acquired_on, "acquired_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let next_service_on = match optional_date(&body.next_service_on, "next_service_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let asset_tag = trimmed(&body.asset_tag);

    // An asset tag identifies one item; two items with the same tag makes the
    // tag useless. Checked here so the caller gets a 409 rather than the unique
    // index's generic database error; the index is still the arbiter.
    if let Some(tag) = &asset_tag {
        let taken =
            c.db.exists(
                format!(
                    "SELECT 1 FROM {} WHERE asset_tag = $1",
                    c.db.table("items")
                ),
                vec![SqlValue::Text(tag.clone())],
            )
            .await?;
        if taken {
            return PluginResponse::error(
                409,
                format!("asset tag {tag:?} is already on another item"),
            );
        }
    }

    let creator = caller(&req);
    let row = c
        .db
        .query_one(
            format!(
                "INSERT INTO {items} AS i \
                   (name, asset_tag, category, description, \"condition\", location, \
                    acquired_on, source, next_service_on, replacement_note, created_by) \
                 VALUES ($1, $2, $3, COALESCE($4, ''), $5, COALESCE($6, ''), \
                         ($7::text)::date, COALESCE($8, ''), ($9::text)::date, \
                         COALESCE($10, ''), $11) \
                 RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![
                SqlValue::Text(name.clone()),
                asset_tag.clone().into(),
                SqlValue::Text(category.clone()),
                body.description.clone().into(),
                SqlValue::Text(grade.clone()),
                body.location.clone().into(),
                acquired_on.map(|d| SqlValue::Text(d.to_string())).unwrap_or(SqlValue::Null),
                body.source.clone().into(),
                next_service_on
                    .map(|d| SqlValue::Text(d.to_string()))
                    .unwrap_or(SqlValue::Null),
                body.replacement_note.clone().into(),
                SqlValue::Text(creator.clone()),
            ],
        )
        .await?
        .ok_or_else(|| SdkError::Internal("item insert returned no row".into()))?;
    let id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            req.identity.as_ref(),
            "item.create",
            "equipment_item",
            &id.to_string(),
            json!({
                "name": name,
                "asset_tag": asset_tag,
                "category": category,
                "condition": grade,
                "location": body.location,
            }),
        )
        .await?;
    c.events
        .publish(
            event::ITEM_CREATED,
            json!({
                "item_id": id,
                "name": row["name"],
                "asset_tag": row["asset_tag"],
                "category": row["category"],
                "condition": row["condition"],
                "created_by": creator,
            }),
        )
        .await?;
    PluginResponse::created(
        &format!("/api/equipment/item/{id}"),
        &json!({ "item": row, "one_physical_thing": "kits are counts; this row is one item" }),
    )
}

/// `GET /api/equipment/items` — the catalogue.
async fn list_items(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let include_retired = req.query_bool("include_retired");
    let status = match req.query_param("status") {
        Some(raw) => {
            let value = raw.trim().to_ascii_lowercase();
            if !ITEM_STATUSES.contains(&value.as_str()) {
                return PluginResponse::error(
                    400,
                    format!("status must be one of {}", ITEM_STATUSES.join(", ")),
                );
            }
            Some(value)
        }
        None => None,
    };
    let condition = match req.query_param("condition") {
        Some(raw) => {
            let value = raw.trim().to_ascii_lowercase();
            if !CONDITIONS.contains(&value.as_str()) {
                return PluginResponse::error(
                    400,
                    format!("condition must be one of {}", CONDITIONS.join(", ")),
                );
            }
            Some(value)
        }
        None => None,
    };
    let search = trimmed(&req.query_param("q").map(str::to_string));
    let rows = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {items} i \
                 WHERE ($1::bool OR i.status <> 'retired') \
                   AND ($2::text IS NULL OR i.status = $2) \
                   AND ($3::text IS NULL OR i.category = $3) \
                   AND ($4::text IS NULL OR i.\"condition\" = $4) \
                   AND ($5::text IS NULL OR i.location = $5) \
                   AND ($6::text IS NULL OR i.name ILIKE '%' || $6 || '%' \
                        OR i.asset_tag ILIKE '%' || $6 || '%') \
                 ORDER BY i.category, i.name, i.id LIMIT $7",
                fields = ITEM_FIELDS,
                items = c.db.table("items")
            ),
            vec![
                SqlValue::Bool(include_retired),
                status.clone().into(),
                req.query_param("category").map(str::to_string).into(),
                condition.clone().into(),
                req.query_param("location").map(str::to_string).into(),
                search.clone().into(),
                SqlValue::Int(limit_of(&req)),
            ],
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "items": rows,
            "count": rows.len(),
            "include_retired": include_retired,
        }),
    )
}

/// `GET /api/equipment/item/{id}` — one item, its open checkout, its recent log,
/// and the derived flags.
async fn get_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let on = today();
    let t = thresholds(c);
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    // Order matters: open checkout, then the log.
    let open = open_checkout(c, id).await?;
    let history = item_checkouts(c, id, 20).await?;
    PluginResponse::json(
        200,
        &json!({
            "item": item,
            "open_checkout": open,
            "checkouts": history,
            "flags": item_flags(&item, on, &t),
        }),
    )
}

/// `GET /api/equipment/item/{id}/history` — the full checkout log, paged.
async fn get_item_history(
    c: &PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    let history = item_checkouts(c, id, limit_of(&req)).await?;
    PluginResponse::json(
        200,
        &json!({
            "item_id": id,
            "name": item["name"],
            "asset_tag": item["asset_tag"],
            "checkouts": history,
            "count": history.len(),
        }),
    )
}

/// `PATCH /api/equipment/item/{id}` — correct the catalogue record.
///
/// Deliberately cannot change `status`: leaving the pool, coming back, and being
/// retired are state transitions with their own routes, an audit entry each, and
/// the item's availability hanging off them.
async fn edit_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: ItemEditBody = req.json()?;
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };

    let category = match normalize_category(&body.category) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let grade = match normalize_condition(&body.condition) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let acquired_on = match optional_date(&body.acquired_on, "acquired_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let next_service_on = match optional_date(&body.next_service_on, "next_service_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let asset_tag = trimmed(&body.asset_tag);

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = vec![SqlValue::Int(id)];
    if let Some(name) = trimmed(&body.name) {
        set_clause(&mut sets, &mut params, "name", SqlValue::Text(name));
    }
    if body.asset_tag.is_some() {
        set_clause(
            &mut sets,
            &mut params,
            "asset_tag",
            asset_tag.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
        );
    }
    if body.category.is_some() {
        set_clause(&mut sets, &mut params, "category", SqlValue::Text(category.clone()));
    }
    if let Some(description) = &body.description {
        set_clause(
            &mut sets,
            &mut params,
            "description",
            SqlValue::Text(description.trim().to_string()),
        );
    }
    if body.condition.is_some() {
        set_clause(&mut sets, &mut params, "\"condition\"", SqlValue::Text(grade.clone()));
    }
    if let Some(location) = &body.location {
        set_clause(
            &mut sets,
            &mut params,
            "location",
            SqlValue::Text(location.trim().to_string()),
        );
    }
    if body.acquired_on.is_some() {
        set_clause(
            &mut sets,
            &mut params,
            "acquired_on",
            date_param(acquired_on),
        );
    }
    if let Some(source) = &body.source {
        set_clause(&mut sets, &mut params, "source", SqlValue::Text(source.trim().to_string()));
    }
    if body.next_service_on.is_some() {
        set_clause(&mut sets, &mut params, "next_service_on", date_param(next_service_on));
    }
    if let Some(flagged) = body.replacement_flagged {
        set_clause(&mut sets, &mut params, "replacement_flagged", SqlValue::Bool(flagged));
    }
    if let Some(note) = &body.replacement_note {
        set_clause(
            &mut sets,
            &mut params,
            "replacement_note",
            SqlValue::Text(note.trim().to_string()),
        );
    }
    if sets.is_empty() {
        return PluginResponse::error(400, "no editable field was supplied");
    }

    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET {sets}, updated_at = now() \
                 WHERE i.id = $1 RETURNING {fields}",
                items = c.db.table("items"),
                sets = sets.join(", "),
                fields = ITEM_FIELDS
            ),
            params,
        )
        .await?;
    let Some(row) = rows.first().cloned() else {
        return PluginResponse::error(404, "no such item");
    };
    let on = today();
    let t = thresholds(c);
    c.audit
        .log(
            req.identity.as_ref(),
            "item.update",
            "equipment_item",
            &id.to_string(),
            json!({ "fields": sets.len() }),
        )
        .await?;
    c.events
        .publish(
            event::ITEM_UPDATED,
            json!({
                "item_id": id,
                "name": row["name"],
                "status": row["status"],
                "condition": row["condition"],
                "fields": sets.len(),
            }),
        )
        .await?;

    // A promotion to candidate that a human caused: the derived answer changed,
    // and the flag is the part another plugin would care about.
    let before = replacement_reasons(&item, on, &t);
    let after = replacement_reasons(&row, on, &t);
    if before.is_empty() && !after.is_empty() {
        c.events
            .publish(
                event::REPLACEMENT_FLAGGED,
                json!({ "item_id": id, "name": row["name"], "reasons": after, "by": "patch" }),
            )
            .await?;
    }
    PluginResponse::json(
        200,
        &json!({ "item": row, "flags": item_flags(&row, on, &t) }),
    )
}

/// The `SqlValue` for an optional date column: text, cast in SQL.
fn date_param(value: Option<NaiveDate>) -> SqlValue {
    value.map(|d| SqlValue::Text(d.to_string())).unwrap_or(SqlValue::Null)
}

/// `POST /api/equipment/item/{id}/checkout` — an item leaves, with its condition.
async fn checkout_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: CheckoutBody = req.json()?;
    let on = today();
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };

    // The state machine, in the order a caller should hear about it.
    match item["status"].as_str().unwrap_or(STATUS_AVAILABLE) {
        STATUS_RETIRED => {
            return PluginResponse::error(409, "this item is retired and cannot be checked out")
        }
        STATUS_MAINTENANCE => {
            return PluginResponse::error(
                409,
                "this item is flagged for maintenance and is out of the pool — \
                 return it to service first (POST /api/equipment/item/{id}/return-to-service)",
            )
        }
        _ => {}
    }
    let item_grade = item["condition"].as_str().unwrap_or(DEFAULT_CONDITION).to_string();
    if is_unserviceable(&item_grade) {
        return PluginResponse::error(
            409,
            "this item is unserviceable — it is not in the pool until its condition is corrected \
             or it is retired",
        );
    }

    // One open checkout per item. The partial unique index is the backstop for a
    // race; this check is what turns it into a 409 that names the holder.
    if let Some(open) = open_checkout(c, id).await? {
        return PluginResponse::error(
            409,
            format!(
                "item {id} is already checked out to {} since {} — check it in first \
                 (POST /api/equipment/item/{id}/checkin)",
                open["checked_out_by"].as_str().unwrap_or("?"),
                open["checked_out_on"].as_str().unwrap_or("?")
            ),
        );
    }

    let holder = trimmed(&body.checked_out_by).or_else(|| {
        let who = caller(&req);
        if who.trim().is_empty() {
            None
        } else {
            Some(who)
        }
    });
    let Some(holder) = holder else {
        return PluginResponse::error(
            400,
            "checked_out_by is required: there is no authenticated caller to default to",
        );
    };
    let grade = match normalize_condition(&body.condition) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let due_on = match optional_date(&body.due_on, "due_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    if due_on.is_some_and(|due| due < on) {
        return PluginResponse::error(400, "due_on must not be in the past");
    }

    let row = c
        .db
        .query_one(
            format!(
                "INSERT INTO {checkouts} c \
                   (item_id, checked_out_by, due_on, purpose, mission_id, destination, \
                    condition_out, note_out) \
                 VALUES ($1, $2, ($3::text)::date, COALESCE($4, ''), $5, COALESCE($6, ''), \
                         $7, COALESCE($8, '')) \
                 RETURNING c.id, c.item_id, c.checked_out_by, \
                           c.checked_out_at::text AS checked_out_at, \
                           (c.checked_out_at AT TIME ZONE 'UTC')::date::text AS checked_out_on, \
                           c.due_on::text AS due_on, c.purpose, c.mission_id, c.destination, \
                           c.condition_out, c.note_out, (c.checked_in_at IS NULL) AS open",
                checkouts = c.db.table("checkouts")
            ),
            vec![
                SqlValue::Int(id),
                SqlValue::Text(holder.clone()),
                due_on.map(|d| SqlValue::Text(d.to_string())).unwrap_or(SqlValue::Null),
                body.purpose.clone().into(),
                body.mission_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                body.destination.clone().into(),
                SqlValue::Text(grade.clone()),
                body.note.clone().into(),
            ],
        )
        .await?
        .ok_or_else(|| SdkError::Internal("checkout insert returned no row".into()))?;
    let checkout_id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            req.identity.as_ref(),
            "item.checkout",
            "equipment_item",
            &id.to_string(),
            json!({
                "checkout_id": checkout_id,
                "checked_out_by": holder,
                "condition_out": grade,
                "item_condition": item_grade,
                "due_on": due_on.map(|d| d.to_string()),
                "mission_id": body.mission_id,
            }),
        )
        .await?;
    c.events
        .publish(
            event::CHECKED_OUT,
            json!({
                "item_id": id,
                "item_name": item["name"],
                "asset_tag": item["asset_tag"],
                "checkout_id": checkout_id,
                "checked_out_by": holder,
                "condition_out": grade,
                "condition_downgrade": is_downgrade(&item_grade, &grade),
                "due_on": due_on.map(|d| d.to_string()),
                "mission_id": body.mission_id,
                "recorded_by": caller(&req),
            }),
        )
        .await?;
    PluginResponse::created(
        &format!("/api/equipment/item/{id}/history"),
        &json!({
            "checkout": row,
            "item": item,
            "notes": if is_downgrade(&item_grade, &grade) {
                "condition_out is worse than the catalogue grade — the item is now recorded at \
                 that grade; set it back on checkin if this was a mistake"
            } else {
                "bring it back at POST /api/equipment/item/{id}/checkin, stating its condition"
            },
        }),
    )
}

/// `POST /api/equipment/item/{id}/checkin` — the item returns, with its
/// condition, closing the open checkout.
async fn checkin_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: CheckinBody = req.json()?;
    let on = today();
    let t = thresholds(c);
    let grade = match validate_grade(&body.condition, "condition") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    // No open checkout, no checkin: a second close would be a phantom row, and
    // `checkouts_returned_consistent` would not catch it because it would be
    // complete on its own.
    let Some(open) = open_checkout(c, id).await? else {
        return PluginResponse::error(
            409,
            format!("item {id} has no open checkout — there is nothing to check in"),
        );
    };
    let checkout_id = open["id"].as_i64().unwrap_or_default();
    let condition_out = open["condition_out"].as_str().unwrap_or(DEFAULT_CONDITION).to_string();
    let damaged = body.damaged.unwrap_or_else(|| is_downgrade(&condition_out, &grade));
    let back_by = caller(&req);

    let closed = c
        .db
        .query_one(
            format!(
                "UPDATE {checkouts} c SET checked_in_at = now(), checked_in_by = $2, \
                        condition_in = $3, note_in = COALESCE($4, ''), damaged = $5, \
                        updated_at = now() \
                 WHERE c.id = $1 AND c.checked_in_at IS NULL \
                 RETURNING c.id, c.item_id, c.checked_out_by, \
                           c.checked_out_at::text AS checked_out_at, \
                           (c.checked_out_at AT TIME ZONE 'UTC')::date::text AS checked_out_on, \
                           c.due_on::text AS due_on, c.purpose, c.mission_id, \
                           c.condition_out, c.note_out, c.checked_in_at::text AS checked_in_at, \
                           (c.checked_in_at AT TIME ZONE 'UTC')::date::text AS checked_in_on, \
                           c.checked_in_by, c.condition_in, c.note_in, c.damaged",
                checkouts = c.db.table("checkouts")
            ),
            vec![
                SqlValue::Int(checkout_id),
                SqlValue::Text(back_by.clone()),
                SqlValue::Text(grade.clone()),
                body.note.clone().into(),
                SqlValue::Bool(damaged),
            ],
        )
        .await?
        .ok_or_else(|| {
            SdkError::Conflict(
                "the checkout was closed by another request — reload the item".into(),
            )
        })?;

    // The item carries the condition forward and banks the service.
    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET \"condition\" = $2, \
                        service_count = i.service_count + 1, updated_at = now() \
                 WHERE i.id = $1 RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![SqlValue::Int(id), SqlValue::Text(grade.clone())],
        )
        .await?;
    let Some(updated) = rows.first().cloned() else {
        return PluginResponse::error(404, "no such item");
    };

    // Did this checkin *create* a replacement candidate? The event is the
    // transition, not the state — a repeated "still a candidate" is noise.
    let before = replacement_reasons(&item, on, &t);
    let after = replacement_reasons(&updated, on, &t);

    c.audit
        .log(
            req.identity.as_ref(),
            "item.checkin",
            "equipment_item",
            &id.to_string(),
            json!({
                "checkout_id": checkout_id,
                "checked_out_by": open["checked_out_by"],
                "checked_in_by": back_by,
                "condition_out": condition_out,
                "condition_in": grade,
                "damaged": damaged,
                "days_out": days_between(open["checked_out_on"].as_str(), on),
            }),
        )
        .await?;
    c.events
        .publish(
            event::CHECKED_IN,
            json!({
                "item_id": id,
                "item_name": updated["name"],
                "asset_tag": updated["asset_tag"],
                "checkout_id": checkout_id,
                "checked_out_by": open["checked_out_by"],
                "checked_in_by": back_by,
                "condition_out": condition_out,
                "condition_in": grade,
                "damaged": damaged,
                "service_count": updated["service_count"],
                "replacement_reasons": after,
            }),
        )
        .await?;
    if before.is_empty() && !after.is_empty() {
        c.events
            .publish(
                event::REPLACEMENT_FLAGGED,
                json!({
                    "item_id": id,
                    "name": updated["name"],
                    "asset_tag": updated["asset_tag"],
                    "reasons": after,
                    "by": "checkin",
                    "condition_out": condition_out,
                    "condition_in": grade,
                }),
            )
            .await?;
    }

    let mut item = updated.clone();
    if damaged && !is_unserviceable(item["condition"].as_str().unwrap_or(DEFAULT_CONDITION)) {
        item["maintenance_hint"] = json!(
            "this checkin recorded damage — flag it for service \
             (POST /api/equipment/item/{id}/maintenance) so it leaves the pool"
        );
    }
    PluginResponse::json(
        200,
        &json!({
            "checkout": closed,
            "item": item,
            "damaged": damaged,
            "service_count": updated["service_count"],
            "flags": item_flags(&updated, on, &t),
            "new_replacement_candidate": before.is_empty() && !after.is_empty(),
        }),
    )
}

fn days_between(start: Option<&str>, on: NaiveDate) -> Option<i64> {
    parse_optional_date(start).ok().flatten().map(|start| (on - start).num_days())
}

/// `POST /api/equipment/item/{id}/maintenance` — out of the pool, not lost.
async fn flag_maintenance(
    c: &PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: MaintenanceBody = req.json()?;
    let reason = body.reason.trim().to_string();
    if reason.is_empty() {
        return PluginResponse::error(400, "reason is required: why is it out of the pool?");
    }
    let on = today();
    let t = thresholds(c);
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    match item["status"].as_str().unwrap_or(STATUS_AVAILABLE) {
        STATUS_MAINTENANCE => {
            return PluginResponse::error(
                409,
                "this item is already flagged for maintenance — return it to service \
                 (or leave it where it is)",
            )
        }
        STATUS_RETIRED => {
            return PluginResponse::error(409, "this item is retired; it is already out of the pool")
        }
        _ => {}
    }
    // An item that is physically out cannot also be out for service: the two
    // states would disagree about where it is.
    if let Some(open) = open_checkout(c, id).await? {
        return PluginResponse::error(
            409,
            format!(
                "item {id} is checked out to {} — check it in before flagging it for service",
                open["checked_out_by"].as_str().unwrap_or("?")
            ),
        );
    }
    let until = match optional_date(&body.until, "until") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let next_service_on = match optional_date(&body.next_service_on, "next_service_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };

    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET status = 'maintenance', \
                        maintenance_since = ($2::text)::date, \
                        maintenance_until = ($3::text)::date, \
                        maintenance_note = $4, \
                        next_service_on = COALESCE(($5::text)::date, i.next_service_on), \
                        updated_at = now() \
                 WHERE i.id = $1 AND i.status = 'available' RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![
                SqlValue::Int(id),
                SqlValue::Text(on.to_string()),
                date_param(until),
                SqlValue::Text(reason.clone()),
                date_param(next_service_on),
            ],
        )
        .await?;
    let Some(row) = rows.first().cloned() else {
        return PluginResponse::error(409, "the item is no longer available to flag");
    };
    c.audit
        .log(
            req.identity.as_ref(),
            "item.maintenance.flag",
            "equipment_item",
            &id.to_string(),
            json!({ "reason": reason, "until": until.map(|d| d.to_string()), "since": on.to_string() }),
        )
        .await?;
    c.events
        .publish(
            event::MAINTENANCE_FLAGGED,
            json!({
                "item_id": id,
                "name": row["name"],
                "asset_tag": row["asset_tag"],
                "reason": reason,
                "since": on.to_string(),
                "until": until.map(|d| d.to_string()),
                "condition": row["condition"],
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "item": row,
            "flags": item_flags(&row, on, &t),
            "notes": "the item is out of the availability pool until it is returned to service; \
                      a maintenance_until date is when the troop expects it back, not a date it \
                      silently becomes available again",
        }),
    )
}

/// `POST /api/equipment/item/{id}/return-to-service` — back into the pool, and
/// the way back from a mistaken retirement.
async fn return_to_service(
    c: &PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: ReturnBody = req.json()?;
    let on = today();
    let t = thresholds(c);
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    let status = item["status"].as_str().unwrap_or(STATUS_AVAILABLE);
    if status == STATUS_AVAILABLE {
        return PluginResponse::error(
            409,
            "this item is not out of the pool — there is no maintenance or retirement to clear",
        );
    }
    let grade = match &body.condition {
        Some(raw) => match validate_grade(raw, "condition") {
            Ok(v) => v,
            Err(e) => return PluginResponse::error(400, e),
        },
        None => String::new(),
    };
    let next_service_on = match optional_date(&body.next_service_on, "next_service_on") {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, e),
    };
    let note = trimmed(&body.note).unwrap_or_default();

    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET status = 'available', maintenance_since = NULL, \
                        maintenance_until = NULL, maintenance_note = '', \
                        retired_at = NULL, retired_reason = '', \
                        \"condition\" = COALESCE($2, i.\"condition\"), \
                        next_service_on = ($3::text)::date, updated_at = now() \
                 WHERE i.id = $1 AND i.status <> 'available' RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![
                SqlValue::Int(id),
                if grade.is_empty() {
                    SqlValue::Null
                } else {
                    SqlValue::Text(grade.clone())
                },
                date_param(next_service_on),
            ],
        )
        .await?;
    let Some(row) = rows.first().cloned() else {
        return PluginResponse::error(409, "the item is already in the pool");
    };
    let action = if status == STATUS_RETIRED {
        "item.unretire"
    } else {
        "item.maintenance.clear"
    };
    c.audit
        .log(
            req.identity.as_ref(),
            action,
            "equipment_item",
            &id.to_string(),
            json!({
                "from_status": status,
                "condition": row["condition"],
                "note": note,
                "next_service_on": next_service_on.map(|d| d.to_string()),
            }),
        )
        .await?;
    c.events
        .publish(
            event::MAINTENANCE_CLEARED,
            json!({
                "item_id": id,
                "name": row["name"],
                "asset_tag": row["asset_tag"],
                "from_status": status,
                "condition": row["condition"],
                "note": note,
                "returned_by": caller(&req),
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "item": row,
            "flags": item_flags(&row, on, &t),
            "notes": if is_unserviceable(row["condition"].as_str().unwrap_or(DEFAULT_CONDITION)) {
                "the item is back in the pool by status but its condition is unserviceable, so \
                 availability still refuses it — correct the condition or retire it"
            } else {
                "the item is back in the availability pool"
            },
        }),
    )
}

/// `POST /api/equipment/item/{id}/schedule-service` — a service date on an item
/// that stays in the pool.
async fn schedule_service(
    c: &PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: ScheduleServiceBody = req.json()?;
    let on = today();
    let when = match parse_date(&body.on) {
        Ok(v) => v,
        Err(e) => return PluginResponse::error(400, format!("on: {e}")),
    };
    if when < on {
        return PluginResponse::error(
            400,
            format!("on {} is in the past — the item is already overdue, not scheduled", when),
        );
    }
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    if item["status"].as_str() == Some(STATUS_RETIRED) {
        return PluginResponse::error(409, "this item is retired; there is nothing to service");
    }
    let note = trimmed(&body.note).unwrap_or_default();
    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET next_service_on = ($2::text)::date, \
                        maintenance_note = CASE WHEN i.status = 'maintenance' THEN i.maintenance_note \
                                               ELSE $3 END, \
                        updated_at = now() \
                 WHERE i.id = $1 RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![
                SqlValue::Int(id),
                SqlValue::Text(when.to_string()),
                SqlValue::Text(note.clone()),
            ],
        )
        .await?;
    let Some(row) = rows.first().cloned() else {
        return PluginResponse::error(404, "no such item");
    };
    c.audit
        .log(
            req.identity.as_ref(),
            "item.maintenance.schedule",
            "equipment_item",
            &id.to_string(),
            json!({ "next_service_on": when.to_string(), "note": note, "status": row["status"] }),
        )
        .await?;
    c.events
        .publish(
            event::MAINTENANCE_SCHEDULED,
            json!({
                "item_id": id,
                "name": row["name"],
                "next_service_on": when.to_string(),
                "note": note,
                "status": row["status"],
                "scheduled_by": caller(&req),
            }),
        )
        .await?;
    let t = thresholds(c);
    let due_in = (when - on).num_days();
    PluginResponse::json(
        200,
        &json!({
            "item": row,
            "due_in_days": due_in,
            "due": due_in <= t.maintenance_lead_days,
            "flags": item_flags(&row, on, &t),
        }),
    )
}

/// `POST /api/equipment/item/{id}/retire` — out of the pool for good.
async fn retire_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: RetireBody = req.json()?;
    let reason = body.reason.trim().to_string();
    if reason.is_empty() {
        return PluginResponse::error(400, "reason is required: why is it leaving the pool?");
    }
    let on = today();
    let t = thresholds(c);
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    if item["status"].as_str() == Some(STATUS_RETIRED) {
        return PluginResponse::error(409, "this item is already retired");
    }
    if let Some(open) = open_checkout(c, id).await? {
        return PluginResponse::error(
            409,
            format!(
                "item {id} is checked out to {} — an item that is somewhere else cannot be \
                 retired; check it in first",
                open["checked_out_by"].as_str().unwrap_or("?")
            ),
        );
    }
    let rows = c
        .db
        .query(
            format!(
                "UPDATE {items} AS i SET status = 'retired', retired_at = now(), \
                        retired_reason = $2, maintenance_since = NULL, \
                        maintenance_until = NULL, maintenance_note = '', updated_at = now() \
                 WHERE i.id = $1 AND i.status <> 'retired' RETURNING {fields}",
                items = c.db.table("items"),
                fields = ITEM_FIELDS
            ),
            vec![SqlValue::Int(id), SqlValue::Text(reason.clone())],
        )
        .await?;
    let Some(row) = rows.first().cloned() else {
        return PluginResponse::error(409, "this item is already retired");
    };
    c.audit
        .log(
            req.identity.as_ref(),
            "item.retire",
            "equipment_item",
            &id.to_string(),
            json!({ "reason": reason, "was_status": item["status"], "condition": row["condition"] }),
        )
        .await?;
    c.events
        .publish(
            event::ITEM_RETIRED,
            json!({
                "item_id": id,
                "name": row["name"],
                "asset_tag": row["asset_tag"],
                "reason": reason,
                "condition": row["condition"],
                "service_count": row["service_count"],
                "retired_by": caller(&req),
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "item": row,
            "flags": item_flags(&row, on, &t),
            "notes": "a retirement can be undone with POST /api/equipment/item/{id}/return-to-service",
        }),
    )
}

/// `DELETE /api/equipment/item/{id}` — only ever an entry with no history.
///
/// Deleting a catalogue entry that has been checked out would delete the record
/// of who held what, which is the audit trail the checkout log exists to be.
/// Retiring is the answer for an item that is gone; delete is for the one that
/// was never really there (a typo, a duplicate).
async fn delete_item(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(item) = fetch_item(c, id).await? else {
        return PluginResponse::error(404, "no such item");
    };
    let history =
        c.db.exists(
            format!("SELECT 1 FROM {} WHERE item_id = $1 LIMIT 1", c.db.table("checkouts")),
            vec![SqlValue::Int(id)],
        )
        .await?;
    if history {
        return PluginResponse::error(
            409,
            format!(
                "item {id} has checkout history — deleting it would delete who held what. Retire it \
                 instead (POST /api/equipment/item/{id}/retire)"
            ),
        );
    }
    c.db.execute(
        format!("DELETE FROM {} WHERE id = $1", c.db.table("items")),
        vec![SqlValue::Int(id)],
    )
    .await?;
    c.audit
        .log(
            req.identity.as_ref(),
            "item.delete",
            "equipment_item",
            &id.to_string(),
            json!({ "name": item["name"], "asset_tag": item["asset_tag"], "status": item["status"] }),
        )
        .await?;
    c.events
        .publish(
            event::ITEM_DELETED,
            json!({ "item_id": id, "name": item["name"], "asset_tag": item["asset_tag"] }),
        )
        .await?;
    PluginResponse::json(200, &json!({ "deleted": id, "name": item["name"] }))
}

/// `GET /api/equipment/checkouts` — the checkout log, and the who-has-what view.
async fn list_checkouts(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let on = today();
    let state = match req.query_param("state") {
        None => "all".to_string(),
        Some(raw) => {
            let value = raw.trim().to_ascii_lowercase();
            if !["all", "open", "closed"].contains(&value.as_str()) {
                return PluginResponse::error(400, "state must be all, open or closed");
            }
            value
        }
    };
    let overdue_only = req.query_bool("overdue");
    let rows = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {checkouts} c JOIN {items} i ON i.id = c.item_id \
                 WHERE ($1::bigint IS NULL OR c.item_id = $1) \
                   AND ($2::text IS NULL OR c.checked_out_by = $2) \
                   AND ($3::bigint IS NULL OR c.mission_id = $3) \
                   AND ($4::text = 'all' \
                        OR ($4::text = 'open' AND c.checked_in_at IS NULL) \
                        OR ($4::text = 'closed' AND c.checked_in_at IS NOT NULL)) \
                   AND ($5::bool IS NOT TRUE \
                        OR (c.checked_in_at IS NULL AND c.due_on IS NOT NULL \
                            AND c.due_on < (($6::text)::date - $7::int))) \
                 ORDER BY c.checked_out_at DESC, c.id DESC LIMIT $8",
                fields = CHECKOUT_FIELDS,
                checkouts = c.db.table("checkouts"),
                items = c.db.table("items")
            ),
            vec![
                req.query_int("item_id").map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                trimmed(&req.query_param("member").map(str::to_string)).into(),
                req.query_int("mission_id").map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                SqlValue::Text(state.clone()),
                SqlValue::Bool(overdue_only),
                SqlValue::Text(on.to_string()),
                SqlValue::Int(thresholds(c).overdue_grace_days),
                SqlValue::Int(limit_of(&req)),
            ],
        )
        .await?;
    let open = rows
        .iter()
        .filter(|r| r["open"].as_bool().unwrap_or(false))
        .count();
    PluginResponse::json(
        200,
        &json!({
            "checkouts": rows,
            "count": rows.len(),
            "open": open,
            "state": state,
            "overdue_only": overdue_only,
            "today": on.to_string(),
        }),
    )
}

/// `GET /api/equipment/availability?from=&to=` — what a mission planner asks.
async fn availability(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let on = today();
    let from = match req.query_param("from") {
        Some(raw) => match parse_date(raw) {
            Ok(v) => v,
            Err(e) => return PluginResponse::error(400, format!("from: {e}")),
        },
        None => on,
    };
    let days = req
        .query_int("days")
        .unwrap_or(DEFAULT_AVAILABILITY_DAYS)
        .clamp(1, MAX_AVAILABILITY_DAYS);
    let to = match req.query_param("to") {
        Some(raw) => match parse_date(raw) {
            Ok(v) => v,
            Err(e) => return PluginResponse::error(400, format!("to: {e}")),
        },
        None => from + chrono::Duration::days(days),
    };
    if to < from {
        return PluginResponse::error(400, "to must not be before from");
    }
    let include_retired = req.query_bool("include_retired");
    let category = req.query_param("category").map(str::to_string);
    let location = req.query_param("location").map(str::to_string);

    // Order matters: the pool, then the checkouts that touch the window.
    let items = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {items} i \
                 WHERE ($1::bool OR i.status <> 'retired') \
                   AND ($2::text IS NULL OR i.category = $2) \
                   AND ($3::text IS NULL OR i.location = $3) \
                 ORDER BY i.category, i.name, i.id LIMIT $4",
                fields = ITEM_FIELDS,
                items = c.db.table("items")
            ),
            vec![
                SqlValue::Bool(include_retired),
                category.clone().into(),
                location.clone().into(),
                SqlValue::Int(limit_of(&req)),
            ],
        )
        .await?;
    let checkouts = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {checkouts} c JOIN {items} i ON i.id = c.item_id \
                 WHERE (c.checked_out_at AT TIME ZONE 'UTC')::date <= ($1::text)::date \
                   AND COALESCE((c.checked_in_at AT TIME ZONE 'UTC')::date, c.due_on, \
                                'infinity'::date) >= ($2::text)::date \
                 ORDER BY c.item_id, c.checked_out_at LIMIT $3",
                fields = CHECKOUT_FIELDS,
                checkouts = c.db.table("checkouts"),
                items = c.db.table("items")
            ),
            vec![
                SqlValue::Text(to.to_string()),
                SqlValue::Text(from.to_string()),
                SqlValue::Int(MAX_LIMIT),
            ],
        )
        .await?;

    let pool = partition_availability(&items, &checkouts, from, to, on);
    let t = thresholds(c);
    PluginResponse::json(
        200,
        &json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "days": (to - from).num_days(),
            "today": on.to_string(),
            "available": pool.available,
            "unavailable": pool.unavailable,
            "out": checkouts,
            "counts": {
                "available": pool.available.len(),
                "unavailable": pool.unavailable.len(),
                "in_pool": items.len(),
                "by_reason": pool.reason_counts(),
            },
            "thresholds": t,
            "notice": "An item is available when it is in the pool, is not unserviceable, and no \
                       checkout touches the window. A due date is a promise: an open checkout \
                       blocks its item until the future due date, and indefinitely once that date \
                       has passed or when no due date was given.",
        }),
    )
}

/// `GET /api/equipment/replacements` — the replacement candidates, worst first.
async fn replacements(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let on = today();
    let t = thresholds(c);
    let rows = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {items} i WHERE i.status <> 'retired' \
                 ORDER BY i.id LIMIT $1",
                fields = ITEM_FIELDS,
                items = c.db.table("items")
            ),
            vec![SqlValue::Int(limit_of(&req))],
        )
        .await?;
    let candidates = replacement_candidates(&rows, on, &t);
    PluginResponse::json(
        200,
        &json!({
            "candidates": candidates,
            "count": candidates.len(),
            "examined": rows.len(),
            "today": on.to_string(),
            "thresholds": t,
            "notice": "Candidates are computed, not stored: reasons are `condition_poor`, \
                       `condition_unserviceable`, `service_count`, `age`, or `flagged` (a human \
                       decision recorded on the item with PATCH /api/equipment/item/{id}).",
        }),
    )
}

/// `GET /api/equipment/maintenance` — what is out of the pool and what is due.
async fn maintenance_view(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let on = today();
    let t = thresholds(c);
    let rows = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {items} i \
                 WHERE i.status <> 'retired' \
                   AND (i.status = 'maintenance' OR i.next_service_on IS NOT NULL \
                        OR i.replacement_flagged OR i.\"condition\" = ANY($1)) \
                 ORDER BY i.next_service_on NULLS LAST, i.name, i.id LIMIT $2",
                fields = ITEM_FIELDS,
                items = c.db.table("items")
            ),
            vec![
                SqlValue::TextArray(t.replacement_conditions.clone()),
                SqlValue::Int(limit_of(&req)),
            ],
        )
        .await?;
    let report = maintenance_report(&rows, on, &t);
    PluginResponse::json(200, &maintenance_report_json(&report, on, &t))
}

/// The daily schedule: publish one event when something wants attention.
///
/// Silent when nothing is due — an empty reminder every morning is how a troop
/// learns to ignore reminders.
async fn publish_maintenance_due(c: &PluginContext) -> Result<(), SdkError> {
    let on = today();
    let t = thresholds(c);
    let rows = c
        .db
        .query(
            format!(
                "SELECT {fields} FROM {items} i \
                 WHERE i.status <> 'retired' \
                   AND (i.status = 'maintenance' OR i.next_service_on IS NOT NULL) \
                 ORDER BY i.next_service_on NULLS LAST, i.id LIMIT $1",
                fields = ITEM_FIELDS,
                items = c.db.table("items")
            ),
            vec![SqlValue::Int(MAX_LIMIT)],
        )
        .await?;
    let report = maintenance_report(&rows, on, &t);
    if !report.has_due_work() {
        return Ok(());
    }
    c.events
        .publish(
            event::MAINTENANCE_DUE,
            json!({
                "today": on.to_string(),
                "lead_days": t.maintenance_lead_days,
                "in_service": report.in_service.len(),
                "overdue": report.overdue.len(),
                "due": report.due.len(),
                "item_ids": report.due_item_ids(),
                "source": "maintenance_due",
            }),
        )
        .await
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct EquipmentPlugin {
    ctx: OnceLock<PluginContext>,
}

impl EquipmentPlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()/schedules()")
    }
}

impl Default for EquipmentPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for EquipmentPlugin {
    fn id(&self) -> &str {
        "equipment"
    }

    fn name(&self) -> &str {
        "Equipment"
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
            Permission::new(
                "equipment:read",
                "View the catalogue, the checkout log, and availability",
            ),
            Permission::new(
                "equipment:write",
                "Add catalogue entries and correct their details",
            ),
            Permission::new(
                "equipment:checkout",
                "Check equipment out and back in, recording its condition",
            ),
            Permission::new(
                "equipment:manage",
                "Flag and clear maintenance, schedule service, retire and delete items",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "equipment_schema",
            "CREATE TABLE IF NOT EXISTS items (\
                 id BIGSERIAL PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 asset_tag TEXT, \
                 category TEXT NOT NULL DEFAULT 'other', \
                 description TEXT NOT NULL DEFAULT '', \
                 \"condition\" TEXT NOT NULL DEFAULT 'good', \
                 location TEXT NOT NULL DEFAULT '', \
                 acquired_on DATE, \
                 source TEXT NOT NULL DEFAULT '', \
                 service_count INTEGER NOT NULL DEFAULT 0, \
                 next_service_on DATE, \
                 maintenance_since DATE, \
                 maintenance_until DATE, \
                 maintenance_note TEXT NOT NULL DEFAULT '', \
                 status TEXT NOT NULL DEFAULT 'available', \
                 replacement_flagged BOOLEAN NOT NULL DEFAULT false, \
                 replacement_note TEXT NOT NULL DEFAULT '', \
                 retired_at TIMESTAMPTZ, \
                 retired_reason TEXT NOT NULL DEFAULT '', \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT items_name_not_blank CHECK (btrim(name) <> ''), \
                 CONSTRAINT items_condition_valid CHECK (\"condition\" IN ( \
                     'new', 'good', 'fair', 'poor', 'unserviceable')), \
                 CONSTRAINT items_status_valid CHECK (status IN ( \
                     'available', 'maintenance', 'retired')), \
                 CONSTRAINT items_service_count_valid CHECK (service_count >= 0), \
                 CONSTRAINT items_maintenance_consistent CHECK ( \
                     (status = 'maintenance' AND maintenance_since IS NOT NULL) OR \
                     (status <> 'maintenance' AND maintenance_since IS NULL)), \
                 CONSTRAINT items_retired_consistent CHECK ( \
                     (status = 'retired') = (retired_at IS NOT NULL)), \
                 CONSTRAINT items_maintenance_window_valid CHECK ( \
                     maintenance_since IS NULL OR maintenance_until IS NULL \
                     OR maintenance_until >= maintenance_since) \
             );\
             CREATE UNIQUE INDEX IF NOT EXISTS idx_items_asset_tag \
               ON items(asset_tag) WHERE asset_tag IS NOT NULL;\
             CREATE INDEX IF NOT EXISTS idx_items_status ON items(status);\
             CREATE INDEX IF NOT EXISTS idx_items_category ON items(category);\
             CREATE INDEX IF NOT EXISTS idx_items_next_service ON items(next_service_on) \
               WHERE next_service_on IS NOT NULL;\
             CREATE TABLE IF NOT EXISTS checkouts (\
                 id BIGSERIAL PRIMARY KEY, \
                 item_id BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE, \
                 checked_out_by TEXT NOT NULL, \
                 checked_out_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 due_on DATE, \
                 purpose TEXT NOT NULL DEFAULT '', \
                 mission_id BIGINT, \
                 destination TEXT NOT NULL DEFAULT '', \
                 condition_out TEXT NOT NULL, \
                 note_out TEXT NOT NULL DEFAULT '', \
                 checked_in_at TIMESTAMPTZ, \
                 checked_in_by TEXT, \
                 condition_in TEXT, \
                 note_in TEXT NOT NULL DEFAULT '', \
                 damaged BOOLEAN NOT NULL DEFAULT false, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT checkouts_holder_not_blank CHECK (btrim(checked_out_by) <> ''), \
                 CONSTRAINT checkouts_condition_out_valid CHECK (condition_out IN ( \
                     'new', 'good', 'fair', 'poor', 'unserviceable')), \
                 CONSTRAINT checkouts_condition_in_valid CHECK ( \
                     condition_in IS NULL OR condition_in IN ( \
                         'new', 'good', 'fair', 'poor', 'unserviceable')), \
                 CONSTRAINT checkouts_returned_consistent CHECK ( \
                     (checked_in_at IS NULL) = (condition_in IS NULL)), \
                 CONSTRAINT checkouts_return_after_out CHECK ( \
                     checked_in_at IS NULL OR checked_in_at >= checked_out_at) \
             );\
             CREATE UNIQUE INDEX IF NOT EXISTS idx_checkouts_open_item \
               ON checkouts(item_id) WHERE checked_in_at IS NULL;\
             CREATE INDEX IF NOT EXISTS idx_checkouts_item ON checkouts(item_id);\
             CREATE INDEX IF NOT EXISTS idx_checkouts_holder ON checkouts(checked_out_by);\
             CREATE INDEX IF NOT EXISTS idx_checkouts_mission ON checkouts(mission_id) \
               WHERE mission_id IS NOT NULL;",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        // Every route is gated at troop scope: SPEC §7.6 gives equipment no
        // Lodge-scoped authority, and `location` is where an item physically
        // lives rather than who owns the decision about it. Each declaration
        // hands the core a closure that owns a clone of the plugin's context.
        let ctx = self.ctx();
        vec![
            route!(ctx, post_protected, "/api/equipment/item", "equipment:write", create_item),
            route!(ctx, get_protected, "/api/equipment/items", "equipment:read", list_items),
            route!(ctx, get_protected, "/api/equipment/availability", "equipment:read", availability),
            route!(ctx, get_protected, "/api/equipment/replacements", "equipment:read", replacements),
            route!(ctx, get_protected, "/api/equipment/maintenance", "equipment:read", maintenance_view),
            route!(ctx, get_protected, "/api/equipment/checkouts", "equipment:read", list_checkouts),
            route!(ctx, get_protected, "/api/equipment/item/{id}", "equipment:read", get_item),
            route!(ctx, get_protected, "/api/equipment/item/{id}/history", "equipment:read", get_item_history),
            route!(ctx, patch_protected, "/api/equipment/item/{id}", "equipment:write", edit_item),
            route!(ctx, post_protected, "/api/equipment/item/{id}/checkout", "equipment:checkout", checkout_item),
            route!(ctx, post_protected, "/api/equipment/item/{id}/checkin", "equipment:checkout", checkin_item),
            route!(ctx, post_protected, "/api/equipment/item/{id}/maintenance", "equipment:manage", flag_maintenance),
            route!(ctx, post_protected, "/api/equipment/item/{id}/return-to-service", "equipment:manage", return_to_service),
            route!(ctx, post_protected, "/api/equipment/item/{id}/schedule-service", "equipment:manage", schedule_service),
            route!(ctx, post_protected, "/api/equipment/item/{id}/retire", "equipment:manage", retire_item),
            // Destructive, so the SDK requires a troop-covering grant even for
            // an any-scope declaration — this route is troop-scoped regardless.
            route!(ctx, delete_protected, "/api/equipment/item/{id}", "equipment:manage", delete_item),
        ]
    }

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone();
        vec![Schedule::new(
            "maintenance_due",
            std::time::Duration::from_secs(24 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                async move { publish_maintenance_due(&c).await }
            }),
        )]
    }
}

export_plugin!(EquipmentPlugin);
