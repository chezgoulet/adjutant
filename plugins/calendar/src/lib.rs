//! # adjutant-calendar — events, recurrence, RSVPs, and quorum projection
//! (SPEC §7.7).
//!
//! A troop's daily life is its calendar (docs/plugin-roadmap.md §4: the calendar
//! is part of the first release, "without events and RSVPs the troop keeps
//! planning in a group chat"). This plugin owns that:
//!
//! * **Events** at troop-wide or Lodge scope, using the scoped-permission system
//!   (SPEC §9.2, `docs/design/scoped-permissions.md`): a Lodge event is created,
//!   edited and cancelled by a grant that covers *that Lodge*, and a troop-wide
//!   event by a grant that covers the troop.
//! * **Recurrence** in iCal `RRULE` form for the weekly and monthly meetings a
//!   troop actually holds, expanded on the wall clock of the event's timezone
//!   (`src/recurrence.rs` explains the subset and why it is hand-rolled).
//! * **RSVPs** (`going`, `not_going`, `maybe`, `pending`), per occurrence for a
//!   recurring event, with `EXDATE` support so one cancelled meeting does not
//!   cancel the series.
//! * **Quorum projection for Congress** — whether enough members have said they
//!   are coming to reach one-third of registered scouts.
//! * **Seasonal awareness** (`src/season.rs`) and a weekly schedule that
//!   publishes the season's upcoming tasks on the event bus.
//! * **Integration**: `mission.completed` auto-creates a debrief event; a
//!   governing-body event can carry governance's `meeting_id` so the client can
//!   show the attendance-based number beside the RSVP projection.
//!
//! ## The quorum question, answered once
//!
//! SPEC §7.7 gives this plugin "quorum calculation for Congress" and SPEC §7.4
//! gives governance the same arithmetic for its meetings. `docs/milestones/
//! M4-missions-governance.md` says the duplication "needs settling when M5
//! starts: the meeting (and its attendance) belongs to governance, so calendar
//! should ask governance rather than keep a second tally."
//!
//! It is settled here as follows:
//!
//! * [`compute_quorum`] is **the same arithmetic governance uses**, and the same
//!   vocabulary (`one_third_registered`, `majority_members`, `fixed`), so the
//!   two plugins cannot disagree about how many people "one-third of registered
//!   scouts" is. The difference is only what is counted.
//! * Calendar counts **intent**: RSVPs marked `going` for one occurrence (a
//!   series-level RSVP covers every occurrence, and a member's
//!   occurrence-specific answer wins over their series answer). This is a
//!   *projection* — it is what the event's page should show a week out, when
//!   waiting for the room to fill is not an option.
//! * Governance counts **attendance**, which is the number that decides a
//!   motion. There is no cross-schema read: a plugin role owns its schema only
//!   (`docs/plugin-development.md` §Schema isolation), so calendar cannot and
//!   does not read `governance.attendance`.
//! * An event may carry `meeting_id`, and the quorum response names the
//!   authoritative number's address
//!   (`/api/governance/meeting/{id}/quorum`) so a client shows both without
//!   either plugin guessing.
//! * Registered-scout counts are **recorded on the event** (`expected_voters`)
//!   rather than read from `membership`, for the same isolation reason. An
//!   unconfigured event fails closed: `required == 0` is never "met".
//!
//! ## Permissions
//!
//! SPEC §9.1 names `calendar:read` and `calendar:create`. `calendar:manage` and
//! `calendar:rsvp` are the M5 additions: editing or cancelling somebody else's
//! event is not the same authority as creating one, and answering an invitation
//! is something every member does (SPEC §9.1 grants read/create to "Chief+",
//! which cannot be the whole RSVP story).
//!
//! **Role grants are not the plugin's to make.** The core seeds `chief` with
//! every permission; a troop maps other roles through `core.role_permissions`
//! (see `docs/api-reference.md`).
//!
//! ## Schema
//!
//! `events` and `rsvps` are SPEC §7.7's tables. Vocabulary is constrained in the
//! database (`scope_type`, `status`, `response`, `quorum_basis`), because a
//! response code that reaches a tally is a constraint, not a convention.

mod recurrence;
mod season;

pub use recurrence::{
    expand, expand_rrule, is_excluded, is_midnight, midnight, parse_exdates, parse_local_timestamp,
    parse_rrule, render_exdates, render_local, ByDay, Expansion, Freq, Recurrence, MAX_PERIODS,
    PART_NAMES,
};
pub use season::{
    due_date, season_of, seasonal_tasks_between, seasonal_tasks_for_month, SeasonalTask,
    SEASONAL_TASKS,
};

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// Troop-wide scope (SPEC §7.7 "troop-wide … events").
pub const SCOPE_TROOP: &str = "troop";
/// Lodge-level scope.
pub const SCOPE_LODGE: &str = "lodge";

pub const SCOPE_TYPES: [&str; 2] = [SCOPE_TROOP, SCOPE_LODGE];

/// Not a governing-body meeting.
pub const BODY_NONE: &str = "";
/// Governance's stable body codes (SPEC §7.4), reused so a client can correlate
/// a Congress event with the motions decided in it.
pub const BODY_CODES: [&str; 4] = ["congress", "tc", "lodge", "committee"];

/// Event categories.
pub const CATEGORIES: [&str; 9] = [
    "meeting", "training", "service", "mission", "debrief", "social", "ceremony", "camp", "other",
];
pub const DEFAULT_CATEGORY: &str = "other";

/// RSVP answers.
pub const RESPONSE_GOING: &str = "going";
/// Answered no.
pub const RESPONSE_NOT_GOING: &str = "not_going";
/// Answered maybe.
pub const RESPONSE_MAYBE: &str = "maybe";
/// Invited, no answer yet.
pub const RESPONSE_PENDING: &str = "pending";

pub const RESPONSES: [&str; 4] = [
    RESPONSE_GOING,
    RESPONSE_NOT_GOING,
    RESPONSE_MAYBE,
    RESPONSE_PENDING,
];

/// Event states.
pub const STATUS_SCHEDULED: &str = "scheduled";
/// Cancelled (kept, with its RSVPs, so the record survives).
pub const STATUS_CANCELLED: &str = "cancelled";

pub const STATUSES: [&str; 2] = [STATUS_SCHEDULED, STATUS_CANCELLED];

/// Quorum bases — governance's vocabulary, plus `none` for an ordinary event.
pub const QUORUM_NONE: &str = "none";
/// Congress: one third of registered scouts, rounded up (3rd Congress).
pub const QUORUM_ONE_THIRD_REGISTERED: &str = "one_third_registered";
/// A majority of the body's members.
pub const QUORUM_MAJORITY_MEMBERS: &str = "majority_members";
/// An explicit number the operator sets.
pub const QUORUM_FIXED: &str = "fixed";

pub const QUORUM_BASES: [&str; 4] = [
    QUORUM_NONE,
    QUORUM_ONE_THIRD_REGISTERED,
    QUORUM_MAJORITY_MEMBERS,
    QUORUM_FIXED,
];

/// The `created_by` value on an event this plugin created for itself (a mission
/// debrief). Deliberately not a user id: the visibility rule matches
/// `created_by = caller`, and no member is called this.
pub const AUTO_CREATED_BY: &str = "calendar:auto";

/// Days after a mission closes that its debrief is provisionally scheduled for.
pub const DEBRIEF_LEAD_DAYS: i64 = 7;
/// The hour (local) a debrief is provisionally scheduled at.
pub const DEBRIEF_HOUR: u32 = 18;
/// How far ahead seasonal tasks are surfaced by the weekly schedule.
pub const SEASON_LEAD_DAYS: i64 = 14;
/// The most occurrences one expansion may return through the API.
pub const MAX_OCCURRENCES: usize = 100;
/// How many occurrences of a series are searched when checking that a caller's
/// occurrence really belongs to it (see `record_rsvp`).
pub const SERIES_PROBE_LIMIT: usize = 500;

// ---------------------------------------------------------------------------
// Quorum arithmetic (pure — the same numbers governance computes)
// ---------------------------------------------------------------------------

/// The number of responding members required for quorum.
///
/// * `one_third_registered` — `ceil(expected / 3)`: the Congress rule the 3rd
///   Catamount Congress locked ("quorum = one-third of registered scouts");
/// * `majority_members` — `floor(expected / 2) + 1`;
/// * `fixed` — the configured number, or `0` when none is set;
/// * `none` (and anything unrecognised) — `0`, no quorum rule.
///
/// An unconfigured event therefore has a required quorum of `0`, and
/// [`quorum_met`] treats that as **not met**: an event with no quorum rule fails
/// closed rather than reporting a quorum of nobody. This is governance's
/// arithmetic verbatim, so the two plugins' numbers agree whenever
/// `expected_voters` is the same population.
pub fn compute_quorum(basis: &str, expected: i64, configured: Option<i64>) -> i64 {
    let expected = expected.max(0);
    match basis {
        // Guarded on `expected > 0`: a third of nobody is not one person.
        QUORUM_ONE_THIRD_REGISTERED if expected > 0 => (expected + 2) / 3,
        QUORUM_MAJORITY_MEMBERS if expected > 0 => expected / 2 + 1,
        QUORUM_FIXED => configured.unwrap_or(0).max(0),
        _ => 0,
    }
}

/// Is quorum met? A required count of `0` is never met (see [`compute_quorum`]).
pub fn quorum_met(going: i64, required: i64) -> bool {
    required > 0 && going >= required
}

/// The scope an event's authority lives at.
///
/// A Lodge event whose `scope_id` is missing is **troop-wide**, not "lodge with
/// no id": the database refuses that shape on write, and a read that meets one
/// must not silently widen or narrow the check.
pub fn event_scope(scope_type: &str, scope_id: Option<&str>) -> Scope {
    match (scope_type, scope_id) {
        (SCOPE_LODGE, Some(id)) if !id.trim().is_empty() => Scope::lodge(id),
        _ => Scope::troop(),
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct CalendarPlugin {
    ctx: OnceLock<PluginContext>,
}

impl CalendarPlugin {
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

impl Default for CalendarPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Row shape and fetch helpers
// ---------------------------------------------------------------------------

/// Every event column the API states, with the conversions the host needs.
///
/// The host decodes json/bool/int/text/date/timestamp and nothing else, so
/// `timestamptz` columns come back as `::text` (absolute, with offset) and the
/// **wall clock** in the event's own timezone is rendered beside it — the
/// occurrence times the recurrence engine and the client both speak.
const EVENT_FIELDS: &str = r#"
    e.id, e.title, e.description, e.category, e.scope_type, e.scope_id, e.body, e.meeting_id,
    e.location, e.timezone, e.all_day, e.rrule, e.exdates,
    e.starts_at::text AS starts_at,
    to_char(e.starts_at AT TIME ZONE COALESCE(NULLIF(e.timezone, ''), 'UTC'),
            'YYYY-MM-DD"T"HH24:MI:SS') AS starts_local,
    e.ends_at::text AS ends_at,
    to_char(e.ends_at AT TIME ZONE COALESCE(NULLIF(e.timezone, ''), 'UTC'),
            'YYYY-MM-DD"T"HH24:MI:SS') AS ends_local,
    to_char(now() AT TIME ZONE COALESCE(NULLIF(e.timezone, ''), 'UTC'),
            'YYYY-MM-DD"T"HH24:MI:SS') AS now_local,
    e.status, e.quorum_basis, e.expected_voters, e.quorum_required, e.source_mission_id,
    e.cancelled_at::text AS cancelled_at, e.cancelled_by, e.created_by,
    e.created_at::text AS created_at, e.updated_at::text AS updated_at
"#;

fn local_of(row: &Value, key: &str) -> Option<NaiveDateTime> {
    row[key]
        .as_str()
        .and_then(|s| parse_local_timestamp(s).ok())
}

async fn fetch_event(c: &PluginContext, id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {} FROM {} e WHERE e.id = $1",
            EVENT_FIELDS,
            c.db.table("events")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// The event's own wall clock at "now" (rendered by PostgreSQL in the event's
/// timezone, so DST is the database's business, not this crate's).
fn now_local_of(event: &Value) -> NaiveDateTime {
    local_of(event, "now_local").unwrap_or_else(|| Utc::now().naive_utc())
}

/// The `(troop-wide, lodge ids)` the caller covers — the visibility half of the
/// scoped-permission rule. Management is checked separately, per object.
async fn visibility(c: &PluginContext, identity: Option<&Identity>) -> (bool, Vec<String>) {
    let troop_wide = c
        .permissions
        .has_in_scope(identity, "calendar:read", &Scope::troop())
        .await;
    if troop_wide {
        return (true, Vec::new());
    }
    let lodges = identity
        .map(|i| {
            i.grants
                .iter()
                .filter(|g| g.scope.scope_type == ScopeType::Lodge)
                .filter_map(|g| g.scope.scope_id.clone())
                .collect()
        })
        .unwrap_or_default();
    (false, lodges)
}

/// May this caller see this event?
///
/// Troop-wide events are for the troop — anybody with `calendar:read` at some
/// scope sees them. A Lodge event needs a grant covering that Lodge, and a
/// member always sees what they created.
async fn can_read_event(c: &PluginContext, identity: Option<&Identity>, event: &Value) -> bool {
    let Some(identity) = identity else {
        return false;
    };
    if c.permissions
        .has_in_scope(Some(identity), "calendar:read", &Scope::troop())
        .await
    {
        return true;
    }
    if event["scope_type"].as_str().unwrap_or(SCOPE_TROOP) == SCOPE_TROOP {
        return true;
    }
    let scope = event_scope(
        event["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        event["scope_id"].as_str(),
    );
    if c.permissions
        .has_in_scope(Some(identity), "calendar:read", &scope)
        .await
    {
        return true;
    }
    event["created_by"].as_str() == Some(identity.user_id.as_str())
}

/// The occurrences of a fetched event from `from` on its wall clock.
///
/// A non-recurring event has exactly one occurrence: its start. `EXDATE`
/// entries are filtered out here, so every caller sees the same series.
fn occurrences_for(
    event: &Value,
    from: NaiveDateTime,
    limit: usize,
) -> Result<Expansion, SdkError> {
    let Some(dtstart) = local_of(event, "starts_local") else {
        return Err(SdkError::Internal(
            "event has no parsable starts_at (the row or its timezone is corrupt)".into(),
        ));
    };
    let all_day = event["all_day"].as_bool().unwrap_or(false);
    let exdates = parse_exdates(event["exdates"].as_str().unwrap_or_default())
        .map_err(|e| SdkError::Internal(format!("stored exdates did not parse: {e}")))?;
    let rrule = event["rrule"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();

    // Ask for the excluded ones too: the series is expanded before the EXDATEs
    // are removed, so stopping at `limit` would hand back fewer occurrences than
    // the caller asked for — a client listing "the next three meetings" after a
    // cancelled week got two. Each `exdate` removes at most one occurrence.
    let wanted = limit.saturating_add(exdates.len());
    let mut expansion = if rrule.is_empty() {
        Expansion {
            occurrences: if dtstart >= from {
                vec![dtstart]
            } else {
                Vec::new()
            },
            truncated: false,
            periods: 1,
        }
    } else {
        expand_rrule(&rrule, dtstart, from, wanted)
            .map_err(|e| SdkError::Internal(format!("stored rrule did not parse: {e}")))?
    };
    expansion
        .occurrences
        .retain(|o| !is_excluded(&exdates, *o, all_day));
    expansion.occurrences.truncate(limit);
    Ok(expansion)
}

/// The count of RSVPs that apply to one occurrence: occurrence-specific answers
/// override a series-level answer for the same member, and a series-level answer
/// counts for every occurrence.
async fn rsvp_counts(
    c: &PluginContext,
    event_id: i64,
    occurrence: NaiveDateTime,
    timezone: &str,
) -> Result<Value, SdkError> {
    let row =
        c.db.query_one(
            format!(
                "SELECT COUNT(*) FILTER (WHERE t.response = $4) AS going, \
                        COUNT(*) FILTER (WHERE t.response = $5) AS not_going, \
                        COUNT(*) FILTER (WHERE t.response = $6) AS maybe, \
                        COUNT(*) FILTER (WHERE t.response = $7) AS pending, \
                        COUNT(*) AS responded \
                 FROM (SELECT DISTINCT ON (member_id) member_id, response \
                       FROM {rsvps} \
                       WHERE event_id = $1 \
                         AND (occurrence_at = ($2::timestamp AT TIME ZONE $3) \
                              OR occurrence_at = '-infinity'::timestamptz) \
                       ORDER BY member_id, (occurrence_at <> '-infinity'::timestamptz) DESC) t",
                rsvps = c.db.table("rsvps")
            ),
            vec![
                SqlValue::Int(event_id),
                SqlValue::Text(render_local(occurrence)),
                SqlValue::Text(timezone.to_string()),
                SqlValue::Text(RESPONSE_GOING.to_string()),
                SqlValue::Text(RESPONSE_NOT_GOING.to_string()),
                SqlValue::Text(RESPONSE_MAYBE.to_string()),
                SqlValue::Text(RESPONSE_PENDING.to_string()),
            ],
        )
        .await?;
    Ok(row.unwrap_or_else(|| {
        json!({
            "going": 0, "not_going": 0, "maybe": 0, "pending": 0, "responded": 0
        })
    }))
}

/// The quorum picture for one occurrence: the rule, the projection, and how the
/// two compare.
fn quorum_state(event: &Value, counts: &Value) -> Value {
    let basis = event["quorum_basis"].as_str().unwrap_or(QUORUM_NONE);
    let expected = event["expected_voters"].as_i64().unwrap_or(0);
    let configured = event["quorum_required"].as_i64();
    let required = compute_quorum(basis, expected, configured);
    let going = counts["going"].as_i64().unwrap_or(0);
    json!({
        "basis": basis,
        "rule": basis != QUORUM_NONE,
        "expected": expected,
        "required": required,
        "going": going,
        "met": quorum_met(going, required),
        "short": (required - going).max(0),
        "basis_configured": required > 0,
        "kind": "rsvp_projection",
    })
}

/// The occurrence a quorum question is about when the caller does not name one:
/// the event's own start if it does not recur, otherwise its next occurrence
/// (falling back to the start when the series has run out).
fn default_occurrence(event: &Value) -> Result<NaiveDateTime, SdkError> {
    let base = local_of(event, "starts_local")
        .ok_or_else(|| SdkError::Internal("event has no parsable starts_at".into()))?;
    let rrule = event["rrule"].as_str().unwrap_or_default().trim();
    if rrule.is_empty() {
        return Ok(base);
    }
    let expansion = occurrences_for(event, now_local_of(event), 1)?;
    Ok(expansion.occurrences.first().copied().unwrap_or(base))
}

/// Where the authoritative (attendance-based) number for a Congress event lives,
/// when the event names a governance meeting.
fn governance_quorum_hint(event: &Value) -> Option<String> {
    event["meeting_id"]
        .as_i64()
        .map(|id| format!("/api/governance/meeting/{id}/quorum"))
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EventBody {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope_type: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    /// A governing-body code (`congress`, `tc`, `lodge`, `committee`).
    #[serde(default)]
    body: Option<String>,
    /// Governance's meeting id, when this event *is* that meeting.
    #[serde(default)]
    meeting_id: Option<i64>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    all_day: Option<bool>,
    /// Wall clock: `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS]`.
    starts_at: String,
    #[serde(default)]
    ends_at: Option<String>,
    #[serde(default)]
    rrule: Option<String>,
    #[serde(default)]
    exdates: Option<String>,
    #[serde(default)]
    quorum_basis: Option<String>,
    #[serde(default)]
    expected_voters: Option<i64>,
    #[serde(default)]
    quorum_required: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct EventEditBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope_type: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    meeting_id: Option<i64>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    all_day: Option<bool>,
    #[serde(default)]
    starts_at: Option<String>,
    #[serde(default)]
    ends_at: Option<String>,
    #[serde(default)]
    rrule: Option<String>,
    #[serde(default)]
    exdates: Option<String>,
    #[serde(default)]
    quorum_basis: Option<String>,
    #[serde(default)]
    expected_voters: Option<i64>,
    #[serde(default)]
    quorum_required: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RsvpBody {
    response: String,
    /// The occurrence being answered for (wall clock). Absent = the whole series.
    #[serde(default)]
    occurrence: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OccurrenceBody {
    /// The occurrence to cancel, on the event's wall clock.
    occurrence: String,
    #[serde(default)]
    reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Validation helpers (shared by create and edit)
// ---------------------------------------------------------------------------

fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn normalize_scope_type(value: &Option<String>) -> Result<String, String> {
    let scope = trimmed(value)
        .unwrap_or_else(|| SCOPE_TROOP.to_string())
        .to_ascii_lowercase();
    if !SCOPE_TYPES.contains(&scope.as_str()) {
        return Err(format!(
            "scope_type must be one of {}",
            SCOPE_TYPES.join(", ")
        ));
    }
    Ok(scope)
}

fn normalize_body_code(value: &Option<String>) -> Result<String, String> {
    let code = trimmed(value)
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    if !code.is_empty() && !BODY_CODES.contains(&code.as_str()) {
        return Err(format!(
            "body must be one of {} (or omitted for an ordinary event)",
            BODY_CODES.join(", ")
        ));
    }
    Ok(code)
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

fn normalize_quorum_basis(value: &Option<String>, body_code: &str) -> Result<String, String> {
    let basis = match trimmed(value) {
        Some(given) => given.to_ascii_lowercase(),
        // A Congress event's quorum rule is the Accords' one; an ordinary event
        // has no quorum rule at all.
        None if body_code == "congress" => QUORUM_ONE_THIRD_REGISTERED.to_string(),
        None => QUORUM_NONE.to_string(),
    };
    if !QUORUM_BASES.contains(&basis.as_str()) {
        return Err(format!(
            "quorum_basis must be one of {}",
            QUORUM_BASES.join(", ")
        ));
    }
    Ok(basis)
}

fn validate_voters(expected: Option<i64>, required: Option<i64>) -> Result<(), String> {
    if expected.is_some_and(|n| n < 0) {
        return Err("expected_voters must not be negative".into());
    }
    if required.is_some_and(|n| n < 0) {
        return Err("quorum_required must not be negative".into());
    }
    Ok(())
}

/// Validate a `starts_at`/`ends_at` pair on the wall clock, returning
/// `(starts, ends)`.
fn validate_window(
    starts_at: &str,
    ends_at: &Option<String>,
    all_day: bool,
) -> Result<(NaiveDateTime, Option<NaiveDateTime>), String> {
    let mut start = parse_local_timestamp(starts_at)?;
    if all_day {
        start = midnight(start);
    }
    let end = match trimmed(ends_at) {
        Some(raw) => {
            let mut end = parse_local_timestamp(&raw)?;
            if all_day {
                end = midnight(end);
            }
            if end <= start {
                return Err("ends_at must be after starts_at".into());
            }
            Some(end)
        }
        None => None,
    };
    Ok((start, end))
}

/// The event's `rrule`, canonicalised; `""` when it does not recur.
fn validate_rrule(value: &Option<String>) -> Result<String, String> {
    match trimmed(value) {
        Some(raw) => {
            let rule = parse_rrule(&raw)?;
            if !rule.recurs() {
                // `COUNT=1` is a series of one: store it as non-recurring rather
                // than as a rule that never fires twice.
                return Ok(String::new());
            }
            Ok(rule.to_ical())
        }
        None => Ok(String::new()),
    }
}

fn validate_exdates(value: &Option<String>) -> Result<Vec<NaiveDateTime>, String> {
    match trimmed(value) {
        Some(raw) => parse_exdates(&raw),
        None => Ok(Vec::new()),
    }
}

async fn timezone_exists(c: &PluginContext, tz: &str) -> Result<bool, SdkError> {
    c.db.exists(
        "SELECT 1 FROM pg_timezone_names WHERE name = $1",
        vec![SqlValue::Text(tz.to_string())],
    )
    .await
}

fn limit_of(req: &PluginRequest, default: i64, max: i64) -> usize {
    req.query_int("limit").unwrap_or(default).clamp(1, max) as usize
}

/// Append `column = $n` to a dynamic `SET` list, keeping the placeholder number
/// and the bind order in step by construction (they are the same push).
fn set_clause(sets: &mut Vec<String>, params: &mut Vec<SqlValue>, column: &str, value: SqlValue) {
    params.push(value);
    sets.push(format!("{column} = ${}", params.len()));
}

/// The payload the weekly season reminder publishes, or `None` when nothing is
/// due in the window (a quiet week publishes nothing rather than an empty
/// announcement).
///
/// Pure on purpose: the interesting part of "proactive surfacing" is *which*
/// prompts fall in a window, and that is testable without waiting for the clock
/// to reach the right week.
pub fn seasonal_prompt(today: NaiveDate, window_days: i64) -> Option<Value> {
    let tasks = seasonal_tasks_between(today, today + chrono::Duration::days(window_days));
    if tasks.is_empty() {
        return None;
    }
    let payload: Vec<Value> = tasks
        .iter()
        .map(|(date, task)| {
            json!({
                "key": task.key,
                "date": date.to_string(),
                "season": season_of(*date),
                "title": task.title,
                "detail": task.detail,
            })
        })
        .collect();
    Some(json!({ "window_days": window_days, "tasks": payload }))
}

// ---------------------------------------------------------------------------
// Plugin declaration
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for CalendarPlugin {
    fn id(&self) -> &str {
        "calendar"
    }

    fn name(&self) -> &str {
        "Calendar"
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
            Permission::new("calendar:read", "View events and their RSVP summaries"),
            Permission::new("calendar:create", "Create events at a scope they hold"),
            Permission::new(
                "calendar:manage",
                "Edit, cancel or delete events, and record another member's RSVP",
            ),
            Permission::new(
                "calendar:rsvp",
                "Respond to an invitation (going/not_going/maybe)",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "calendar_schema",
            "CREATE TABLE IF NOT EXISTS events (\
                 id BIGSERIAL PRIMARY KEY, \
                 title TEXT NOT NULL, \
                 description TEXT NOT NULL DEFAULT '', \
                 category TEXT NOT NULL DEFAULT 'other', \
                 scope_type TEXT NOT NULL DEFAULT 'troop', \
                 scope_id TEXT, \
                 body TEXT NOT NULL DEFAULT '', \
                 meeting_id BIGINT, \
                 location TEXT, \
                 timezone TEXT NOT NULL DEFAULT 'UTC', \
                 all_day BOOLEAN NOT NULL DEFAULT false, \
                 starts_at TIMESTAMPTZ NOT NULL, \
                 ends_at TIMESTAMPTZ, \
                 rrule TEXT NOT NULL DEFAULT '', \
                 exdates TEXT NOT NULL DEFAULT '', \
                 status TEXT NOT NULL DEFAULT 'scheduled', \
                 quorum_basis TEXT NOT NULL DEFAULT 'none', \
                 expected_voters INTEGER NOT NULL DEFAULT 0, \
                 quorum_required INTEGER, \
                 source_mission_id BIGINT, \
                 created_by TEXT NOT NULL, \
                 cancelled_at TIMESTAMPTZ, \
                 cancelled_by TEXT, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT events_scope_type_valid CHECK (scope_type IN ('troop', 'lodge')), \
                 CONSTRAINT events_scope_id_present CHECK ( \
                     (scope_type = 'troop' AND scope_id IS NULL) OR \
                     (scope_type = 'lodge' AND scope_id IS NOT NULL AND scope_id <> '') \
                 ), \
                 CONSTRAINT events_category_valid CHECK (category IN (\
                     'meeting', 'training', 'service', 'mission', 'debrief', 'social', \
                     'ceremony', 'camp', 'other')), \
                 CONSTRAINT events_status_valid CHECK (status IN ('scheduled', 'cancelled')), \
                 CONSTRAINT events_quorum_basis_valid CHECK (quorum_basis IN (\
                     'none', 'one_third_registered', 'majority_members', 'fixed')), \
                 CONSTRAINT events_expected_voters_valid CHECK (expected_voters >= 0), \
                 CONSTRAINT events_window_valid CHECK (ends_at IS NULL OR ends_at > starts_at) \
             );\
             CREATE INDEX IF NOT EXISTS idx_events_starts ON events(starts_at);\
             CREATE INDEX IF NOT EXISTS idx_events_scope ON events(scope_type, scope_id);\
             CREATE INDEX IF NOT EXISTS idx_events_body ON events(body);\
             CREATE UNIQUE INDEX IF NOT EXISTS idx_events_source_mission \
               ON events(source_mission_id);\
             CREATE TABLE IF NOT EXISTS rsvps (\
                 id BIGSERIAL PRIMARY KEY, \
                 event_id BIGINT NOT NULL REFERENCES events(id) ON DELETE CASCADE, \
                 member_id TEXT NOT NULL, \
                 response TEXT NOT NULL DEFAULT 'pending', \
                 occurrence_at TIMESTAMPTZ NOT NULL DEFAULT '-infinity', \
                 note TEXT NOT NULL DEFAULT '', \
                 responded_by TEXT NOT NULL, \
                 responded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT rsvps_response_valid CHECK (\
                     response IN ('going', 'not_going', 'maybe', 'pending')), \
                 UNIQUE (event_id, occurrence_at, member_id) \
             );\
             CREATE INDEX IF NOT EXISTS idx_rsvps_event ON rsvps(event_id);",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- create ----------------------------------------------------------
        // Object-shaped: the caller needs `calendar:create` at *some* scope and
        // the handler checks the event's own scope (a troop grant covers every
        // Lodge; a Lodge grant covers that Lodge's events only).
        let c = ctx.clone();
        let create_event = RouteDefinition::post_protected_any_scope(
            "/api/calendar/event",
            "calendar:create",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: EventBody = req.json()?;
                    if body.title.trim().is_empty() {
                        return PluginResponse::error(400, "title is required");
                    }
                    let scope_type = match normalize_scope_type(&body.scope_type) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, e),
                    };
                    let scope_id = trimmed(&body.scope_id);
                    if scope_type == SCOPE_LODGE && scope_id.is_none() {
                        return PluginResponse::error(
                            400,
                            "a Lodge event needs scope_id (the Lodge it belongs to)",
                        );
                    }
                    if scope_type == SCOPE_TROOP && scope_id.is_some() {
                        return PluginResponse::error(
                            400,
                            "a troop-wide event carries no scope_id (scope_id is for Lodge events)",
                        );
                    }
                    let body_code = match normalize_body_code(&body.body) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, e),
                    };
                    // A Congress, Troop Council or committee meeting is the
                    // troop's, never one Lodge's.
                    if !body_code.is_empty() && scope_type != SCOPE_TROOP {
                        return PluginResponse::error(
                            400,
                            format!(
                                "a {body_code} event is troop-wide: it cannot be scoped to a Lodge"
                            ),
                        );
                    }
                    let category = match normalize_category(&body.category) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, e),
                    };
                    let basis = match normalize_quorum_basis(&body.quorum_basis, &body_code) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, e),
                    };
                    if let Err(e) = validate_voters(body.expected_voters, body.quorum_required) {
                        return PluginResponse::error(400, e);
                    }
                    let all_day = body.all_day.unwrap_or(false);
                    let (starts_local, ends_local) =
                        match validate_window(&body.starts_at, &body.ends_at, all_day) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        };
                    let rrule = match validate_rrule(&body.rrule) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, format!("rrule: {e}")),
                    };
                    let exdates = match validate_exdates(&body.exdates) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, e),
                    };
                    let timezone = trimmed(&body.timezone).unwrap_or_else(|| "UTC".to_string());

                    let scope = event_scope(&scope_type, scope_id.as_deref());
                    c.permissions
                        .reach(req.identity.as_ref(), "calendar:create", &scope)
                        .await?;

                    // The zone database lives in the database: ask it, so a typo
                    // is a 400 here rather than a 500 from the first to_char().
                    match timezone_exists(&c, &timezone).await {
                        Ok(true) => {}
                        Ok(false) => {
                            return PluginResponse::error(
                                400,
                                format!(
                                    "timezone {timezone:?} is not an IANA name PostgreSQL knows \
                                     (e.g. America/New_York, UTC)"
                                ),
                            )
                        }
                        Err(e) => return Err(e),
                    }

                    let creator = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row =
                        c.db.query_one(
                            format!(
                                "INSERT INTO {events} AS e \
                                   (title, description, category, scope_type, scope_id, body, \
                                    meeting_id, location, timezone, all_day, starts_at, ends_at, \
                                    rrule, exdates, quorum_basis, expected_voters, \
                                    quorum_required, created_by) \
                                 VALUES ($1, COALESCE($2, ''), $3, $4, $5, $6, $7, $8, $9, $10, \
                                         ($11::timestamp AT TIME ZONE $9), \
                                         ($12::timestamp AT TIME ZONE $9), \
                                         $13, $14, $15, COALESCE($16, 0), $17, $18) \
                                 RETURNING {fields}",
                                events = c.db.table("events"),
                                fields = EVENT_FIELDS
                            ),
                            vec![
                                SqlValue::Text(body.title.trim().to_string()),
                                body.description.clone().into(),
                                SqlValue::Text(category.clone()),
                                SqlValue::Text(scope_type.clone()),
                                scope_id.clone().into(),
                                SqlValue::Text(body_code.clone()),
                                body.meeting_id
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                                body.location.clone().into(),
                                SqlValue::Text(timezone.clone()),
                                SqlValue::Bool(all_day),
                                SqlValue::Text(render_local(starts_local)),
                                ends_local
                                    .map(|e| SqlValue::Text(render_local(e)))
                                    .unwrap_or(SqlValue::Null),
                                SqlValue::Text(rrule.clone()),
                                SqlValue::Text(render_exdates(&exdates)),
                                SqlValue::Text(basis.clone()),
                                body.expected_voters
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                                body.quorum_required
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(creator.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    let id = row["id"].as_i64().unwrap_or_default();
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "event.create",
                            "event",
                            &id.to_string(),
                            json!({
                                "title": body.title,
                                "scope_type": scope_type,
                                "scope_id": scope_id,
                                "rrule": rrule,
                                "starts_local": render_local(starts_local),
                                "timezone": timezone,
                            }),
                        )
                        .await?;
                    c.events
                        .publish(
                            event_type::EVENT_CREATED,
                            json!({
                                "event_id": id,
                                "title": body.title,
                                "category": category,
                                "scope_type": scope_type,
                                "scope_id": scope_id,
                                "body": body_code,
                                "starts_at": row["starts_at"],
                                "all_day": all_day,
                                "recurring": !rrule.is_empty(),
                                "created_by": creator,
                            }),
                        )
                        .await?;
                    let expansion = occurrences_for(&row, now_local_of(&row), 3)?;
                    let counts = rsvp_counts(&c, id, starts_local, &timezone).await?;
                    PluginResponse::created(
                        &format!("/api/calendar/event/{id}"),
                        &json!({
                            "event": row,
                            "occurrences": expansion.occurrences.iter().map(|o| render_local(*o)).collect::<Vec<_>>(),
                            "quorum": quorum_state(&row, &counts),
                            "next": if basis == QUORUM_ONE_THIRD_REGISTERED
                                && row["expected_voters"].as_i64().unwrap_or(0) == 0 {
                                "set expected_voters to the number of registered scouts, or the \
                                 quorum rule fails closed (PATCH /api/calendar/event/{id})"
                            } else {
                                "members RSVP at POST /api/calendar/event/{id}/rsvp"
                            },
                        }),
                    )
                }
            }),
        );

        // --- list ------------------------------------------------------------
        let c = ctx.clone();
        let list_events = RouteDefinition::get_protected_any_scope(
            "/api/calendar/events",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let identity = req.identity.as_ref();
                    let (troop_wide, lodges) = visibility(&c, identity).await;
                    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
                    let status = match req.query_param("status") {
                        None => Some(STATUS_SCHEDULED.to_string()),
                        Some("all") => None,
                        Some(other) => {
                            let value = other.trim().to_ascii_lowercase();
                            if !STATUSES.contains(&value.as_str()) {
                                return PluginResponse::error(
                                    400,
                                    format!(
                                        "status must be one of {}, or 'all'",
                                        STATUSES.join(", ")
                                    ),
                                );
                            }
                            Some(value)
                        }
                    };
                    let from = match req.query_param("from") {
                        Some(raw) => match parse_local_timestamp(raw) {
                            Ok(v) => Some(v),
                            Err(e) => return PluginResponse::error(400, format!("from: {e}")),
                        },
                        None => None,
                    };
                    let to = match req.query_param("to") {
                        Some(raw) => match parse_local_timestamp(raw) {
                            Ok(v) => Some(v),
                            Err(e) => return PluginResponse::error(400, format!("to: {e}")),
                        },
                        None => None,
                    };
                    let rows =
                        c.db.query(
                            format!(
                                "SELECT {fields} FROM {events} e \
                                 WHERE ($1::bool OR e.scope_type = 'troop' \
                                        OR e.scope_id = ANY($2) OR e.created_by = $3) \
                                   AND ($4::text IS NULL OR e.status = $4) \
                                   AND ($5::text IS NULL OR e.scope_type = $5) \
                                   AND ($6::text IS NULL OR e.scope_id = $6) \
                                   AND ($7::text IS NULL OR e.body = $7) \
                                   AND ($8::text IS NULL OR e.category = $8) \
                                   AND ($9::timestamp IS NULL OR e.starts_at >=\
                                        ($9::timestamp AT TIME ZONE \
                                         COALESCE(NULLIF(e.timezone, ''), 'UTC'))) \
                                   AND ($10::timestamp IS NULL OR e.starts_at <=\
                                        ($10::timestamp AT TIME ZONE \
                                         COALESCE(NULLIF(e.timezone, ''), 'UTC'))) \
                                 ORDER BY e.starts_at, e.id \
                                 LIMIT $11",
                                fields = EVENT_FIELDS,
                                events = c.db.table("events")
                            ),
                            vec![
                                SqlValue::Bool(troop_wide),
                                SqlValue::TextArray(lodges),
                                SqlValue::Text(caller),
                                status.clone().into(),
                                req.query_param("scope_type").map(String::from).into(),
                                req.query_param("scope_id").map(String::from).into(),
                                req.query_param("body").map(String::from).into(),
                                req.query_param("category").map(String::from).into(),
                                from.map(|f| SqlValue::Text(render_local(f)))
                                    .unwrap_or(SqlValue::Null),
                                to.map(|t| SqlValue::Text(render_local(t)))
                                    .unwrap_or(SqlValue::Null),
                                SqlValue::Int(req.query_int("limit").unwrap_or(50).clamp(1, 200)),
                            ],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &json!({
                            "events": rows,
                            "scope": if troop_wide { "troop" } else { "scoped" },
                            "status": status.unwrap_or_else(|| "all".to_string()),
                        }),
                    )
                }
            }),
        );

        // --- upcoming (with the season's tasks) -------------------------------
        // The "what is coming" view the client's home screen shows: the next
        // occurrences across every event the caller can see, plus the seasonal
        // prompts that fall inside the same window.
        let c = ctx.clone();
        let upcoming = RouteDefinition::get_protected_any_scope(
            "/api/calendar/upcoming",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let identity = req.identity.as_ref();
                    let (troop_wide, lodges) = visibility(&c, identity).await;
                    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
                    let days = req.query_int("days").unwrap_or(21).clamp(1, 180);
                    let per_event = limit_of(&req, 10, MAX_OCCURRENCES as i64);
                    let total_limit = req.query_int("total").unwrap_or(40).clamp(1, 400);
                    let from_arg = match req.query_param("from") {
                        Some(raw) => match parse_local_timestamp(raw) {
                            Ok(v) => Some(v),
                            Err(e) => return PluginResponse::error(400, format!("from: {e}")),
                        },
                        None => None,
                    };
                    let rows =
                        c.db.query(
                            format!(
                                "SELECT {fields} FROM {events} e \
                                 WHERE e.status = 'scheduled' \
                                   AND (e.rrule <> '' OR e.starts_at >= now()) \
                                   AND ($1::bool OR e.scope_type = 'troop' \
                                        OR e.scope_id = ANY($2) OR e.created_by = $3) \
                                   AND ($4::text IS NULL OR e.scope_type = $4) \
                                   AND ($5::text IS NULL OR e.scope_id = $5) \
                                   AND ($6::text IS NULL OR e.category = $6) \
                                 ORDER BY e.starts_at, e.id \
                                 LIMIT 200",
                                fields = EVENT_FIELDS,
                                events = c.db.table("events")
                            ),
                            vec![
                                SqlValue::Bool(troop_wide),
                                SqlValue::TextArray(lodges),
                                SqlValue::Text(caller),
                                req.query_param("scope_type").map(String::from).into(),
                                req.query_param("scope_id").map(String::from).into(),
                                req.query_param("category").map(String::from).into(),
                            ],
                        )
                        .await?;

                    let mut items: Vec<Value> = Vec::new();
                    // Seasonal prompts, deduplicated by (date, key): two events in
                    // different zones in the same window must not show the same
                    // task twice.
                    let mut seasonal: std::collections::BTreeMap<(String, String), Value> =
                        std::collections::BTreeMap::new();
                    let mut truncated = false;
                    for row in &rows {
                        let now_local = now_local_of(row);
                        let from = from_arg.unwrap_or(now_local);
                        let to = from + chrono::Duration::days(days);
                        let expansion = occurrences_for(row, from, per_event)?;
                        truncated |= expansion.truncated;
                        for occurrence in expansion
                            .occurrences
                            .iter()
                            .filter(|occurrence| **occurrence <= to)
                        {
                            items.push(json!({
                                "event_id": row["id"],
                                "title": row["title"],
                                "category": row["category"],
                                "scope_type": row["scope_type"],
                                "scope_id": row["scope_id"],
                                "body": row["body"],
                                "meeting_id": row["meeting_id"],
                                "location": row["location"],
                                "timezone": row["timezone"],
                                "all_day": row["all_day"],
                                "recurring": row["rrule"].as_str().is_some_and(|r| !r.is_empty()),
                                "occurrence_local": render_local(*occurrence),
                                "starts_local": row["starts_local"],
                                "starts_at": row["starts_at"],
                                "is_series_start": row["starts_local"] == json!(render_local(*occurrence)),
                            }));
                        }
                        let window_from = from.date();
                        let window_to = to.date();
                        for (date, task) in seasonal_tasks_between(window_from, window_to) {
                            seasonal
                                .entry((date.to_string(), task.key.to_string()))
                                .or_insert_with(|| {
                                    json!({
                                        "key": task.key,
                                        "date": date.to_string(),
                                        "season": season_of(date),
                                        "title": task.title,
                                        "detail": task.detail,
                                    })
                                });
                        }
                    }
                    items.sort_by_key(|item| {
                        item["occurrence_local"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string()
                    });
                    items.truncate(total_limit as usize);
                    let seasonal: Vec<Value> = seasonal.into_values().collect();
                    PluginResponse::json(
                        200,
                        &json!({
                            "window_days": days,
                            "occurrences": items,
                            "seasonal": seasonal,
                            "truncated": truncated,
                            "scope": if troop_wide { "troop" } else { "scoped" },
                        }),
                    )
                }
            }),
        );

        // --- season ----------------------------------------------------------
        let c = ctx.clone();
        let season = RouteDefinition::get_protected(
            "/api/calendar/season",
            "calendar:read",
            route_handler(move |_req| {
                let _c = c.clone();
                async move {
                    let req = _req;
                    let on = match req.query_param("on") {
                        Some(raw) => raw.trim().to_string(),
                        None => Utc::now().date_naive().to_string(),
                    };
                    // `on=2026-10` is a month view; `on=2026-10-06` starts a window.
                    if on.len() == 7 {
                        let parts: Vec<&str> = on.split('-').collect();
                        let month = parts.get(1).and_then(|m| m.parse::<u32>().ok());
                        if parts.len() != 2 || month.is_none() {
                            return PluginResponse::error(
                                400,
                                format!("on {on:?} is not YYYY-MM or YYYY-MM-DD"),
                            );
                        }
                        let month = month.expect("checked above");
                        let tasks: Vec<Value> = seasonal_tasks_for_month(month)
                            .iter()
                            .map(|task| {
                                json!({
                                    "key": task.key,
                                    "date": due_date(2000, task).map(|d| d.to_string()),
                                    "title": task.title,
                                    "detail": task.detail,
                                })
                            })
                            .collect();
                        let season = if tasks.is_empty() {
                            "none"
                        } else {
                            season_of(NaiveDate::from_ymd_opt(2000, month, 1).expect("valid month"))
                        };
                        return PluginResponse::json(
                            200,
                            &json!({ "on": on, "month": month, "season": season, "tasks": tasks }),
                        );
                    }
                    let from = match parse_local_timestamp(&on) {
                        Ok(v) => v.date(),
                        Err(e) => return PluginResponse::error(400, format!("on: {e}")),
                    };
                    let days = req.query_int("days").unwrap_or(90).clamp(1, 365);
                    let to = from + chrono::Duration::days(days);
                    let tasks: Vec<Value> = seasonal_tasks_between(from, to)
                        .iter()
                        .map(|(date, task)| {
                            json!({
                                "key": task.key,
                                "date": date.to_string(),
                                "season": season_of(*date),
                                "title": task.title,
                                "detail": task.detail,
                            })
                        })
                        .collect();
                    PluginResponse::json(
                        200,
                        &json!({
                            "on": from.to_string(),
                            "window_days": days,
                            "until": to.to_string(),
                            "season": season_of(from),
                            "tasks": tasks,
                        }),
                    )
                }
            }),
        );

        // --- detail ----------------------------------------------------------
        let c = ctx.clone();
        let get_event = RouteDefinition::get_protected_any_scope(
            "/api/calendar/event/{id}",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        // Absent and forbidden are the same answer for a caller
                        // who cannot see it at all.
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    };
                    if !can_read_event(&c, req.identity.as_ref(), &event).await {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    }
                    let rsvps = c
                        .db
                        .query(
                            format!(
                                "SELECT id, member_id, response, \
                                        occurrence_at::text AS occurrence_at, \
                                        to_char(occurrence_at AT TIME ZONE COALESCE(NULLIF($2, ''), 'UTC'), \
                                                'YYYY-MM-DD\"T\"HH24:MI:SS') AS occurrence_local, \
                                        note, responded_by, responded_at::text AS responded_at \
                                 FROM {rsvps} WHERE event_id = $1 \
                                 ORDER BY member_id, occurrence_at",
                                rsvps = c.db.table("rsvps")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(event["timezone"].as_str().unwrap_or("UTC").to_string()),
                            ],
                        )
                        .await?;
                    let occurrence = default_occurrence(&event)?;
                    let counts = rsvp_counts(
                        &c,
                        id,
                        occurrence,
                        event["timezone"].as_str().unwrap_or("UTC"),
                    )
                    .await?;
                    let expansion = occurrences_for(&event, now_local_of(&event), 5)?;
                    PluginResponse::json(
                        200,
                        &json!({
                            "event": event,
                            "occurrences": expansion.occurrences.iter().map(|o| render_local(*o)).collect::<Vec<_>>(),
                            "rsvps": rsvps,
                            "counts": counts,
                            "quorum": quorum_state(&event, &counts),
                            "quorum_at": render_local(occurrence),
                            "governance_quorum": governance_quorum_hint(&event),
                        }),
                    )
                }
            }),
        );

        // --- occurrences -----------------------------------------------------
        let c = ctx.clone();
        let occurrences = RouteDefinition::get_protected_any_scope(
            "/api/calendar/event/{id}/occurrences",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    };
                    if !can_read_event(&c, req.identity.as_ref(), &event).await {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    }
                    let from = match req.query_param("from") {
                        Some(raw) => match parse_local_timestamp(raw) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, format!("from: {e}")),
                        },
                        None => now_local_of(&event),
                    };
                    let limit = limit_of(&req, 10, MAX_OCCURRENCES as i64);
                    let expansion = occurrences_for(&event, from, limit)?;
                    let exdates = parse_exdates(event["exdates"].as_str().unwrap_or_default())
                        .unwrap_or_default();
                    PluginResponse::json(
                        200,
                        &json!({
                            "event_id": id,
                            "timezone": event["timezone"],
                            "starts_local": event["starts_local"],
                            "rrule": event["rrule"],
                            "occurrences": expansion.occurrences.iter().map(|o| render_local(*o)).collect::<Vec<_>>(),
                            "exdates": exdates.iter().map(|d| render_local(*d)).collect::<Vec<_>>(),
                            "truncated": expansion.truncated,
                        }),
                    )
                }
            }),
        );

        // --- quorum ----------------------------------------------------------
        let c = ctx.clone();
        let quorum = RouteDefinition::get_protected_any_scope(
            "/api/calendar/event/{id}/quorum",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    };
                    if !can_read_event(&c, req.identity.as_ref(), &event).await {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    }
                    let occurrence = match req.query_param("occurrence") {
                        Some(raw) => match parse_local_timestamp(raw) {
                            Ok(v) => v,
                            Err(e) => {
                                return PluginResponse::error(400, format!("occurrence: {e}"))
                            }
                        },
                        None => default_occurrence(&event)?,
                    };
                    let timezone = event["timezone"].as_str().unwrap_or("UTC").to_string();
                    let counts = rsvp_counts(&c, id, occurrence, &timezone).await?;
                    let hint = governance_quorum_hint(&event);
                    PluginResponse::json(
                        200,
                        &json!({
                            "event_id": id,
                            "counted": {
                                "occurrence": render_local(occurrence),
                                "timezone": timezone,
                                "recurring": event["rrule"].as_str().is_some_and(|r| !r.is_empty()),
                                "includes_series_rsvps": true,
                            },
                            "quorum": quorum_state(&event, &counts),
                            "rsvps": counts,
                            "governance_meeting_id": event["meeting_id"],
                            "governance_quorum": hint,
                            "note": "An RSVP projection: it counts intent ('going') for this \
                                     occurrence, including series-level answers. The number that \
                                     decides a motion is attendance, recorded in governance.",
                        }),
                    )
                }
            }),
        );

        // --- RSVPs -----------------------------------------------------------
        let c = ctx.clone();
        let rsvp = RouteDefinition::post_protected_any_scope(
            "/api/calendar/event/{id}/rsvp",
            "calendar:rsvp",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: RsvpBody = req.json()?;
                    let Some(caller) = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .filter(|u| !u.trim().is_empty())
                    else {
                        return PluginResponse::error(401, "rsvp requires an authenticated member");
                    };
                    record_rsvp(&c, &req, id, &caller, &body, false).await
                }
            }),
        );

        let c = ctx.clone();
        let rsvp_for_member = RouteDefinition::post_protected_any_scope(
            "/api/calendar/event/{id}/rsvp/{member}",
            "calendar:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: RsvpBody = req.json()?;
                    let Some(member) = req.param("member").map(str::to_string) else {
                        return PluginResponse::error(
                            400,
                            "route has no {member} capture — declare it in the path",
                        );
                    };
                    if member.trim().is_empty() {
                        return PluginResponse::error(400, "member must not be blank");
                    }
                    record_rsvp(&c, &req, id, member.trim(), &body, true).await
                }
            }),
        );

        let c = ctx.clone();
        let list_rsvps = RouteDefinition::get_protected_any_scope(
            "/api/calendar/event/{id}/rsvps",
            "calendar:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    };
                    if !can_read_event(&c, req.identity.as_ref(), &event).await {
                        return PluginResponse::error(403, "no such event, or you cannot see it");
                    }
                    let timezone = event["timezone"].as_str().unwrap_or("UTC").to_string();
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT id, member_id, response, \
                                        occurrence_at::text AS occurrence_at, \
                                        to_char(occurrence_at AT TIME ZONE COALESCE(NULLIF($2, ''), 'UTC'), \
                                                'YYYY-MM-DD\"T\"HH24:MI:SS') AS occurrence_local, \
                                        note, responded_by, responded_at::text AS responded_at \
                                 FROM {rsvps} WHERE event_id = $1 ORDER BY member_id",
                                rsvps = c.db.table("rsvps")
                            ),
                            vec![SqlValue::Int(id), SqlValue::Text(timezone.clone())],
                        )
                        .await?;
                    let occurrence = default_occurrence(&event)?;
                    let counts = rsvp_counts(&c, id, occurrence, &timezone).await?;
                    PluginResponse::json(
                        200,
                        &json!({
                            "event_id": id,
                            "rsvps": rows,
                            "counts": counts,
                            "quorum_at": render_local(occurrence),
                            "quorum": quorum_state(&event, &counts),
                        }),
                    )
                }
            }),
        );

        // --- edit / cancel / delete ------------------------------------------
        let c = ctx.clone();
        let edit_event = RouteDefinition::patch_protected_any_scope(
            "/api/calendar/event/{id}",
            "calendar:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: EventEditBody = req.json()?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(404, "no such event");
                    };
                    let scope = event_scope(
                        event["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
                        event["scope_id"].as_str(),
                    );
                    c.permissions
                        .reach(req.identity.as_ref(), "calendar:manage", &scope)
                        .await?;

                    // Effective values: what the patch supplies, otherwise what
                    // the event already has — so a partial patch is validated
                    // against the whole event, not against blanks.
                    let scope_type = match &body.scope_type {
                        Some(raw) => match normalize_scope_type(&Some(raw.clone())) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        },
                        None => event["scope_type"]
                            .as_str()
                            .unwrap_or(SCOPE_TROOP)
                            .to_string(),
                    };
                    let scope_id = trimmed(&body.scope_id)
                        .or_else(|| event["scope_id"].as_str().map(String::from));
                    if scope_type == SCOPE_LODGE && scope_id.is_none() {
                        return PluginResponse::error(
                            400,
                            "a Lodge event needs scope_id (the Lodge it belongs to)",
                        );
                    }
                    if scope_type == SCOPE_TROOP && trimmed(&body.scope_id).is_some() {
                        return PluginResponse::error(
                            400,
                            "a troop-wide event carries no scope_id",
                        );
                    }
                    let body_code = match &body.body {
                        Some(raw) => match normalize_body_code(&Some(raw.clone())) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        },
                        None => event["body"].as_str().unwrap_or(BODY_NONE).to_string(),
                    };
                    if !body_code.is_empty() && scope_type != SCOPE_TROOP {
                        return PluginResponse::error(
                            400,
                            format!("a {body_code} event is troop-wide"),
                        );
                    }
                    let category = match &body.category {
                        Some(raw) => match normalize_category(&Some(raw.clone())) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        },
                        None => event["category"]
                            .as_str()
                            .unwrap_or(DEFAULT_CATEGORY)
                            .to_string(),
                    };
                    let basis = match &body.quorum_basis {
                        Some(raw) => match normalize_quorum_basis(&Some(raw.clone()), &body_code) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        },
                        None => event["quorum_basis"]
                            .as_str()
                            .unwrap_or(QUORUM_NONE)
                            .to_string(),
                    };
                    if let Err(e) = validate_voters(body.expected_voters, body.quorum_required) {
                        return PluginResponse::error(400, e);
                    }
                    let all_day = body
                        .all_day
                        .unwrap_or_else(|| event["all_day"].as_bool().unwrap_or(false));
                    let timezone = trimmed(&body.timezone)
                        .unwrap_or_else(|| event["timezone"].as_str().unwrap_or("UTC").to_string());
                    // Re-validate the window with whatever the patch changes.
                    let effective_start = match &body.starts_at {
                        Some(raw) => {
                            let current_end = body
                                .ends_at
                                .clone()
                                .or_else(|| local_of(&event, "ends_local").map(render_local));
                            match validate_window(raw, &current_end, all_day) {
                                Ok((start, _)) => start,
                                Err(e) => return PluginResponse::error(400, e),
                            }
                        }
                        None => match local_of(&event, "starts_local") {
                            Some(start) => start,
                            None => {
                                return PluginResponse::error(
                                    400,
                                    "starts_at is required (the stored value did not parse)",
                                )
                            }
                        },
                    };
                    let ends_local = match &body.ends_at {
                        Some(raw) => {
                            let mut end = match parse_local_timestamp(raw) {
                                Ok(v) => v,
                                Err(e) => return PluginResponse::error(400, e),
                            };
                            if all_day {
                                end = midnight(end);
                            }
                            if end <= effective_start {
                                return PluginResponse::error(
                                    400,
                                    "ends_at must be after starts_at",
                                );
                            }
                            Some(end)
                        }
                        None => None,
                    };
                    let rrule = match &body.rrule {
                        Some(raw) => match validate_rrule(&Some(raw.clone())) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, format!("rrule: {e}")),
                        },
                        None => event["rrule"].as_str().unwrap_or_default().to_string(),
                    };
                    let exdates = match &body.exdates {
                        Some(raw) => match validate_exdates(&Some(raw.clone())) {
                            Ok(v) => v,
                            Err(e) => return PluginResponse::error(400, e),
                        },
                        None => parse_exdates(event["exdates"].as_str().unwrap_or_default())
                            .unwrap_or_default(),
                    };
                    if let Some(tz) = trimmed(&body.timezone) {
                        match timezone_exists(&c, &tz).await {
                            Ok(true) => {}
                            Ok(false) => {
                                return PluginResponse::error(
                                    400,
                                    format!("timezone {tz:?} is not an IANA name PostgreSQL knows"),
                                )
                            }
                            Err(e) => return Err(e),
                        }
                    }

                    // Nothing supplied is a client bug worth naming.
                    let mut sets: Vec<String> = Vec::new();
                    let mut params: Vec<SqlValue> = vec![SqlValue::Int(id)];
                    {
                        if let Some(title) = trimmed(&body.title) {
                            set_clause(&mut sets, &mut params, "title", SqlValue::Text(title));
                        }
                        if let Some(description) = &body.description {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "description",
                                SqlValue::Text(description.trim().to_string()),
                            );
                        }
                        if body.category.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "category",
                                SqlValue::Text(category),
                            );
                        }
                        if body.scope_type.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "scope_type",
                                SqlValue::Text(scope_type.clone()),
                            );
                            set_clause(
                                &mut sets,
                                &mut params,
                                "scope_id",
                                scope_id
                                    .clone()
                                    .map(SqlValue::Text)
                                    .unwrap_or(SqlValue::Null),
                            );
                        }
                        if body.body.is_some() {
                            set_clause(&mut sets, &mut params, "body", SqlValue::Text(body_code));
                        }
                        if body.meeting_id.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "meeting_id",
                                body.meeting_id
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                            );
                        }
                        if let Some(location) = &body.location {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "location",
                                SqlValue::Text(location.trim().to_string()),
                            );
                        }
                        if body.timezone.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "timezone",
                                SqlValue::Text(timezone.clone()),
                            );
                        }
                        if body.all_day.is_some() {
                            set_clause(&mut sets, &mut params, "all_day", SqlValue::Bool(all_day));
                        }
                        if body.starts_at.is_some()
                            || body.timezone.is_some()
                            || body.all_day.is_some()
                        {
                            params.push(SqlValue::Text(render_local(effective_start)));
                            let start_param = params.len();
                            params.push(SqlValue::Text(timezone.clone()));
                            let tz_param = params.len();
                            sets.push(format!(
                                "starts_at = (${start_param}::timestamp AT TIME ZONE ${tz_param})"
                            ));
                        }
                        if body.ends_at.is_some() || body.timezone.is_some() {
                            params.push(
                                ends_local
                                    .map(|e| SqlValue::Text(render_local(e)))
                                    .unwrap_or(SqlValue::Null),
                            );
                            let end_param = params.len();
                            params.push(SqlValue::Text(timezone.clone()));
                            let tz_param = params.len();
                            sets.push(format!(
                                "ends_at = (${end_param}::timestamp AT TIME ZONE ${tz_param})"
                            ));
                        }
                        if body.rrule.is_some() {
                            set_clause(&mut sets, &mut params, "rrule", SqlValue::Text(rrule));
                        }
                        if body.exdates.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "exdates",
                                SqlValue::Text(render_exdates(&exdates)),
                            );
                        }
                        if body.quorum_basis.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "quorum_basis",
                                SqlValue::Text(basis),
                            );
                        }
                        if body.expected_voters.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "expected_voters",
                                SqlValue::Int(body.expected_voters.unwrap_or(0)),
                            );
                        }
                        if body.quorum_required.is_some() {
                            set_clause(
                                &mut sets,
                                &mut params,
                                "quorum_required",
                                body.quorum_required
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                            );
                        }
                    }
                    if sets.is_empty() {
                        return PluginResponse::error(400, "no editable field was supplied");
                    }

                    let rows =
                        c.db.query(
                            format!(
                                "UPDATE {events} AS e SET {sets}, updated_at = now() \
                                 WHERE e.id = $1 RETURNING {fields}",
                                events = c.db.table("events"),
                                sets = sets.join(", "),
                                fields = EVENT_FIELDS
                            ),
                            params,
                        )
                        .await?;
                    let Some(row) = rows.first().cloned() else {
                        return PluginResponse::error(404, "no such event");
                    };
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "event.update",
                            "event",
                            &id.to_string(),
                            json!({ "fields": sets.len() }),
                        )
                        .await?;
                    c.events
                        .publish(
                            "event.updated",
                            json!({
                                "event_id": id,
                                "title": row["title"],
                                "scope_type": row["scope_type"],
                                "scope_id": row["scope_id"],
                                "starts_at": row["starts_at"],
                                "recurring": row["rrule"].as_str().is_some_and(|r| !r.is_empty()),
                            }),
                        )
                        .await?;
                    let occurrence = default_occurrence(&row)?;
                    let counts = rsvp_counts(
                        &c,
                        id,
                        occurrence,
                        row["timezone"].as_str().unwrap_or("UTC"),
                    )
                    .await?;
                    PluginResponse::json(
                        200,
                        &json!({
                            "event": row,
                            "quorum": quorum_state(&row, &counts),
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let cancel_event = RouteDefinition::post_protected_any_scope(
            "/api/calendar/event/{id}/cancel",
            "calendar:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(404, "no such event");
                    };
                    let scope = event_scope(
                        event["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
                        event["scope_id"].as_str(),
                    );
                    c.permissions
                        .reach(req.identity.as_ref(), "calendar:manage", &scope)
                        .await?;
                    if event["status"].as_str() == Some(STATUS_CANCELLED) {
                        return PluginResponse::error(409, "this event is already cancelled");
                    }
                    let caller = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let rows =
                        c.db.query(
                            format!(
                                "UPDATE {events} AS e SET status = 'cancelled', \
                                        cancelled_at = now(), cancelled_by = $2, \
                                        updated_at = now() \
                                 WHERE e.id = $1 RETURNING {fields}",
                                events = c.db.table("events"),
                                fields = EVENT_FIELDS
                            ),
                            vec![SqlValue::Int(id), SqlValue::Text(caller.clone())],
                        )
                        .await?;
                    let Some(row) = rows.first().cloned() else {
                        return PluginResponse::error(404, "no such event");
                    };
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "event.cancel",
                            "event",
                            &id.to_string(),
                            json!({ "title": row["title"] }),
                        )
                        .await?;
                    c.events
                        .publish(
                            "event.cancelled",
                            json!({
                                "event_id": id,
                                "title": row["title"],
                                "scope_type": row["scope_type"],
                                "scope_id": row["scope_id"],
                                "cancelled_by": caller,
                            }),
                        )
                        .await?;
                    PluginResponse::json(200, &json!({ "event": row }))
                }
            }),
        );

        // Cancelling one occurrence of a series (RFC 5545's `EXDATE`): a holiday
        // week does not cancel the weekly meeting.
        let c = ctx.clone();
        let cancel_occurrence = RouteDefinition::post_protected_any_scope(
            "/api/calendar/event/{id}/occurrence/cancel",
            "calendar:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: OccurrenceBody = req.json()?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(404, "no such event");
                    };
                    let scope = event_scope(
                        event["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
                        event["scope_id"].as_str(),
                    );
                    c.permissions
                        .reach(req.identity.as_ref(), "calendar:manage", &scope)
                        .await?;
                    let rrule = event["rrule"].as_str().unwrap_or_default().trim();
                    if rrule.is_empty() {
                        return PluginResponse::error(
                            409,
                            "this event does not recur — cancel the event instead \
                             (POST /api/calendar/event/{id}/cancel)",
                        );
                    }
                    let occurrence = match parse_local_timestamp(&body.occurrence) {
                        Ok(v) => v,
                        Err(e) => return PluginResponse::error(400, format!("occurrence: {e}")),
                    };
                    let expansion = occurrences_for(
                        &event,
                        local_of(&event, "starts_local").unwrap_or(occurrence),
                        SERIES_PROBE_LIMIT,
                    )?;
                    if !expansion.occurrences.contains(&occurrence) {
                        return PluginResponse::error(
                            400,
                            format!(
                                "{} is not an occurrence of this event ({} searched{})",
                                render_local(occurrence),
                                expansion.occurrences.len(),
                                if expansion.truncated {
                                    ", and the series is longer than that"
                                } else {
                                    ""
                                }
                            ),
                        );
                    }
                    let all_day = event["all_day"].as_bool().unwrap_or(false);
                    let mut exdates = parse_exdates(event["exdates"].as_str().unwrap_or_default())
                        .unwrap_or_default();
                    exdates.push(occurrence);
                    let exdates = render_exdates(&exdates);
                    let rows =
                        c.db.query(
                            format!(
                                "UPDATE {events} AS e SET exdates = $2, updated_at = now() \
                                 WHERE e.id = $1 RETURNING {fields}",
                                events = c.db.table("events"),
                                fields = EVENT_FIELDS
                            ),
                            vec![SqlValue::Int(id), SqlValue::Text(exdates.clone())],
                        )
                        .await?;
                    let Some(row) = rows.first().cloned() else {
                        return PluginResponse::error(404, "no such event");
                    };
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "event.occurrence.cancel",
                            "event",
                            &id.to_string(),
                            json!({
                                "occurrence": render_local(occurrence),
                                "reason": body.reason,
                                "all_day": all_day,
                            }),
                        )
                        .await?;
                    c.events
                        .publish(
                            "event.occurrence.cancelled",
                            json!({
                                "event_id": id,
                                "occurrence": render_local(occurrence),
                                "exdates": exdates,
                            }),
                        )
                        .await?;
                    PluginResponse::json(200, &json!({ "event": row, "exdates": exdates }))
                }
            }),
        );

        // Delete is destructive: the SDK gives it a troop-covering requirement
        // even from the `any_scope` constructor, so a Lodge grant cannot erase a
        // Lodge's record. Cancelling (above) is the scoped operation.
        let c = ctx.clone();
        let delete_event = RouteDefinition::delete_protected(
            "/api/calendar/event/{id}",
            "calendar:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(event) = fetch_event(&c, id).await? else {
                        return PluginResponse::error(404, "no such event");
                    };
                    c.db.execute(
                        format!("DELETE FROM {} WHERE id = $1", c.db.table("events")),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "event.delete",
                            "event",
                            &id.to_string(),
                            json!({ "title": event["title"], "scope_type": event["scope_type"] }),
                        )
                        .await?;
                    c.events
                        .publish(
                            "event.deleted",
                            json!({ "event_id": id, "title": event["title"] }),
                        )
                        .await?;
                    PluginResponse::json(200, &json!({ "deleted": id }))
                }
            }),
        );

        vec![
            create_event,
            list_events,
            upcoming,
            season,
            get_event,
            occurrences,
            quorum,
            rsvp,
            rsvp_for_member,
            list_rsvps,
            edit_event,
            cancel_event,
            cancel_occurrence,
            delete_event,
        ]
    }

    fn subscriptions(&self) -> Vec<EventSubscription> {
        let ctx = self.ctx().clone(); // owned: the closure must not borrow self
        vec![EventSubscription::new(
            event_type::MISSION_COMPLETED,
            event_handler(move |ev| {
                let c = ctx.clone();
                async move { on_mission_completed(&c, &ev).await }
            }),
        )]
    }

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone();
        vec![Schedule::new(
            "season_reminder",
            std::time::Duration::from_secs(7 * 24 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                async move {
                    // The server's clock is UTC and this plugin has no timezone
                    // database of its own (see src/recurrence.rs §Wall clock): the
                    // prompt is a week-wide window, so the day it lands on locally
                    // does not change what it says.
                    let Some(payload) = seasonal_prompt(Utc::now().date_naive(), SEASON_LEAD_DAYS)
                    else {
                        return Ok(());
                    };
                    c.events.publish("event.season.upcoming", payload).await
                }
            }),
        )]
    }
}

/// Record (or change) one member's RSVP, and answer with the projection it
/// produces.
///
/// `by_manager` is the difference between answering your own invitation
/// (`calendar:rsvp` at the event's scope) and recording somebody's answer for
/// them (`calendar:manage`, the phoned-in RSVP).
async fn record_rsvp(
    c: &PluginContext,
    req: &PluginRequest,
    event_id: i64,
    member: &str,
    body: &RsvpBody,
    by_manager: bool,
) -> Result<PluginResponse, SdkError> {
    let response = body.response.trim().to_ascii_lowercase();
    if !RESPONSES.contains(&response.as_str()) {
        return PluginResponse::error(
            400,
            format!("response must be one of {}", RESPONSES.join(", ")),
        );
    }
    let Some(event) = fetch_event(c, event_id).await? else {
        // Absent and forbidden are the same answer: an event you cannot see is
        // an event you cannot answer.
        return PluginResponse::error(403, "no such event, or you cannot see it");
    };
    let scope = event_scope(
        event["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        event["scope_id"].as_str(),
    );
    let permission = if by_manager {
        "calendar:manage"
    } else {
        "calendar:rsvp"
    };
    c.permissions
        .reach(req.identity.as_ref(), permission, &scope)
        .await?;

    let timezone = event["timezone"].as_str().unwrap_or("UTC").to_string();
    let rrule = event["rrule"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let base = local_of(&event, "starts_local")
        .ok_or_else(|| SdkError::Internal("event has no parsable starts_at".into()))?;
    // `None` = a series-level answer, stored as the `-infinity` sentinel that
    // counts for every occurrence unless this member answers one specifically.
    let occurrence = match trimmed(&body.occurrence) {
        Some(raw) => {
            let parsed = parse_local_timestamp(&raw)
                .map_err(|e| SdkError::BadRequest(format!("occurrence: {e}")))?;
            if rrule.is_empty() {
                return PluginResponse::error(
                    400,
                    "this event does not recur — RSVP for the event itself \
                     (omit `occurrence`)",
                );
            }
            let expansion = occurrences_for(&event, base, SERIES_PROBE_LIMIT)?;
            if !expansion.occurrences.contains(&parsed) {
                return PluginResponse::error(
                    400,
                    format!(
                        "{} is not an occurrence of this event ({} searched{})",
                        render_local(parsed),
                        expansion.occurrences.len(),
                        if expansion.truncated {
                            ", and the series is longer than that"
                        } else {
                            ""
                        }
                    ),
                );
            }
            Some(parsed)
        }
        None => None,
    };
    let counted = occurrence.unwrap_or(base);

    let recorder = req
        .identity
        .as_ref()
        .map(|i| i.user_id.clone())
        .unwrap_or_default();
    let note = trimmed(&body.note).unwrap_or_default();
    let row =
        c.db.query_one(
            format!(
                "INSERT INTO {rsvps} (event_id, member_id, response, occurrence_at, note, \
                                      responded_by) \
                 VALUES ($1, $2, $3, \
                         COALESCE(($4::timestamp AT TIME ZONE $5), '-infinity'::timestamptz), \
                         $6, $7) \
                 ON CONFLICT (event_id, occurrence_at, member_id) DO UPDATE SET \
                   response = EXCLUDED.response, \
                   note = EXCLUDED.note, \
                   responded_by = EXCLUDED.responded_by, \
                   responded_at = now() \
                 RETURNING id, event_id, member_id, response, \
                           occurrence_at::text AS occurrence_at, \
                           to_char(occurrence_at AT TIME ZONE COALESCE(NULLIF($5, ''), 'UTC'), \
                                   'YYYY-MM-DD\"T\"HH24:MI:SS') AS occurrence_local, \
                           note, responded_by, responded_at::text AS responded_at",
                rsvps = c.db.table("rsvps")
            ),
            vec![
                SqlValue::Int(event_id),
                SqlValue::Text(member.to_string()),
                SqlValue::Text(response.clone()),
                occurrence
                    .map(|o| SqlValue::Text(render_local(o)))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Text(timezone.clone()),
                SqlValue::Text(note),
                SqlValue::Text(recorder.clone()),
            ],
        )
        .await?
        .ok_or_else(|| SdkError::Internal("rsvp upsert returned no row".into()))?;

    let counts = rsvp_counts(c, event_id, counted, &timezone).await?;
    c.audit
        .log(
            req.identity.as_ref(),
            "event.rsvp",
            "event",
            &event_id.to_string(),
            json!({
                "member_id": member,
                "response": response,
                "occurrence": occurrence.map(render_local),
                "by_manager": by_manager,
            }),
        )
        .await?;
    c.events
        .publish(
            "event.rsvp",
            json!({
                "event_id": event_id,
                "member_id": member,
                "response": response,
                "occurrence": occurrence.map(render_local),
                "series_level": occurrence.is_none(),
                "going": counts["going"],
                "recorded_by": recorder,
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "rsvp": row,
            "counts": counts,
            "quorum": quorum_state(&event, &counts),
            "quorum_at": render_local(counted),
        }),
    )
}

/// `mission.completed` (SPEC §5.4) → a provisional debrief event.
///
/// The payload contract is [`MissionCompleted`]; the debrief lands one week
/// after the mission closed, at a fixed local hour, and the troop moves it if
/// that is wrong — a proposal on the calendar beats a mission nobody debriefs.
///
/// **Idempotent**: the bus can redeliver (it is a broadcast with replay), so the
/// `source_mission_id` unique index is the guard — a repeated event is a no-op,
/// not a second debrief.
async fn on_mission_completed(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    let mission: MissionCompleted = serde_json::from_value(ev.payload.clone()).map_err(|e| {
        SdkError::BadRequest(format!(
            "mission.completed payload does not match MissionCompleted: {e}"
        ))
    })?;
    let exists =
        c.db.exists(
            format!(
                "SELECT 1 FROM {} WHERE source_mission_id = $1",
                c.db.table("events")
            ),
            vec![SqlValue::Int(mission.mission_id)],
        )
        .await?;
    if exists {
        return Ok(());
    }

    let lodge = mission
        .lodge_id
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let (scope_type, scope_id) = match lodge {
        Some(lodge) => (SCOPE_LODGE, Some(lodge.to_string())),
        None => (SCOPE_TROOP, None),
    };
    let completed_local = mission.completed_at.naive_utc();
    let starts_local = (completed_local.date() + chrono::Duration::days(DEBRIEF_LEAD_DAYS))
        .and_hms_opt(DEBRIEF_HOUR, 0, 0)
        .expect("a fixed hour is a valid time");
    let title = format!("Debrief — {}", mission.title);
    let description = format!(
        "Auto-created from mission.completed (mission {}, closed {}). Review the report, then \
         confirm or move this debrief.",
        mission.mission_id, completed_local
    );

    let inserted =
        c.db.query_one(
            format!(
                "INSERT INTO {events} AS e \
                   (title, description, category, scope_type, scope_id, body, timezone, \
                    starts_at, rrule, exdates, quorum_basis, expected_voters, source_mission_id, \
                    created_by) \
                 VALUES ($1, $2, 'debrief', $3, $4, '', 'UTC', \
                         ($5::timestamp AT TIME ZONE 'UTC'), '', '', 'none', 0, $6, $7) \
                 ON CONFLICT (source_mission_id) DO NOTHING \
                 RETURNING {fields}",
                events = c.db.table("events"),
                fields = EVENT_FIELDS
            ),
            vec![
                SqlValue::Text(title.clone()),
                SqlValue::Text(description),
                SqlValue::Text(scope_type.to_string()),
                scope_id.clone().into(),
                SqlValue::Text(render_local(starts_local)),
                SqlValue::Int(mission.mission_id),
                SqlValue::Text(AUTO_CREATED_BY.to_string()),
            ],
        )
        .await?;

    // A `None` here is the unique index catching a concurrent delivery: the
    // debrief exists (or is being created), which is the wanted end state.
    let Some(row) = inserted else { return Ok(()) };
    let id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            None,
            "event.create.debrief",
            "event",
            &id.to_string(),
            json!({
                "mission_id": mission.mission_id,
                "title": mission.title,
                "starts_local": render_local(starts_local),
                "source": ev.source,
            }),
        )
        .await?;
    c.events
        .publish(
            event_type::EVENT_CREATED,
            json!({
                "event_id": id,
                "title": title,
                "category": "debrief",
                "scope_type": scope_type,
                "scope_id": scope_id,
                "starts_at": row["starts_at"],
                "recurring": false,
                "created_by": AUTO_CREATED_BY,
                "source_mission_id": mission.mission_id,
            }),
        )
        .await?;
    Ok(())
}

export_plugin!(CalendarPlugin);
