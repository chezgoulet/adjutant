//! # adjutant-conflicts — the staged resolution pathway (SPEC §7.9)
//!
//! A troop has disputes. The Accords' answer is restorative, not punitive: a
//! conflict is resolved as close to the parties as it can be, and only escalates
//! when the stage below it genuinely fails. This plugin is that pathway in
//! software:
//!
//! ```text
//! direct_conversation → facilitation → arbitration → troop_council
//! ```
//!
//! * **Conflict case management** — SPEC §7.9's `conflicts.cases`.
//! * **Stage transition tracking** — every move is an append-only row in
//!   `conflicts.stage_log`: who moved it, from where to where, when, and *why*
//!   (the reason is mandatory). The history is the accountability.
//! * **Anti-dropout** — a stage that sits beyond a configured threshold
//!   surfaces to the case's facilitators, on a schedule (the `stage_nudge`
//!   schedule), and is listed by `GET /api/conflicts/stalled`.
//! * **Resolution recording** — the outcome and the agreement, not a verdict
//!   against a person. `conflicts.cases.outcome` / `agreement`, plus a
//!   `resolution` (or `agreement`) row in the log.
//! * **Appeal/escalation** — a stage that fails advances the case; the log keeps
//!   the whole journey, including every intermediate stage that was tried.
//!
//! ## This is the most privacy-sensitive plugin in the system
//!
//! A case is a private matter between people. **Nothing here grants read access
//! to a case by role, by troop, or by lodge.** The design rules, in order of how
//! load-bearing they are:
//!
//! 1. **"Can read this case" is an object-level check on the case's own
//!    visibility list** — [`may_read`] / [`standing_of`]: the caller is one of
//!    `party_ids`, or one of the `facilitator_ids` assigned to *that* case.
//!    There is no branch that widens because the caller holds a permission at
//!    troop scope. `conflicts:read_own` gets a caller *to* the handler; the
//!    handler decides, per case, and refuses everyone else.
//! 2. **A case is deliberately not scoped.** It is not a troop object and not a
//!    lodge object, so it is not modelled as a [`Scope`] — there is no
//!    `Scope::troop()` check anywhere in this crate, because "the troop" is
//!    exactly who must never see it. Route gates are `any_scope` (`None`
//!    `required_scope`) and the object check is the real boundary.
//! 3. **Party names never leave the case.** Events this plugin publishes carry
//!    the case's opaque id and stage and nothing else ([`EVENT_CONFLICT_FILED`],
//!    [`event_type::CONFLICT_ESCALATED`], [`EVENT_CONFLICT_STAGE_STALLED`], …); the
//!    audit log — which is a troop-level, tamper-evident ledger — records counts
//!    and actions, never a party id or the text of an account. Who was assigned
//!    which case lives in `stage_log`, which is reachable only through the case.
//! 4. **Absent and forbidden answer the same 403** on every case route, so the
//!    route cannot be used to probe which case ids exist.
//! 5. **`conflicts:manage` does not confer read access.** The pathway's
//!    administrator (Troop Council, or whoever the troop charges with staffing)
//!    can appoint and release facilitators and can see which cases exist, but
//!    not one word of what is in them. A manage holder cannot appoint
//!    *themselves* either — `set_facilitator` refuses it — so there is no route
//!    from "administers the pathway" to "reads a case it is not on".
//! 6. **`stage_log` is append-only in the database**, not by convention: a
//!    trigger refuses `UPDATE` and `DELETE` (migration 2). A case's journey
//!    cannot be rewritten by the plugin, or by anybody with the plugin's role.
//!
//! ## Permissions — including why there is no `conflicts:read`
//!
//! SPEC §9.1 names the resource; these four are the whole vocabulary, and each
//! is narrow on purpose:
//!
//! | Permission | What it is | Why it is not more |
//! |---|---|---|
//! | `conflicts:file` | File a case; and act on a case you are already a party to — record the agreement you reached, add a party, withdraw. | It is the *member-level* action on your own case. It never widens what you can read: the handler still requires that you are in `party_ids`. |
//! | `conflicts:read_own` | Read a case you have **standing** in — you are one of its parties, or its assigned facilitator. | Named `read_own`, not `read`, because the alternative is a permission that reads *anybody's* case, which is the failure mode this plugin exists to avoid. It admits a caller to the route; the object check is what answers. |
//! | `conflicts:facilitate` | Carry a case you are **assigned** to: advance its stage, record its outcome, work your stalled queue. | Assignment is per case (`facilitator_ids`), so holding this permission does nothing on a case you are not on. |
//! | `conflicts:manage` | Administer the pathway: appoint/release facilitators, see which cases are stalled and uncarried, add parties on a case's behalf. | Deliberately excluded: reading case *content*. Staffing is a decision about people, not a licence to read their dispute. |
//!
//! A troop's roles map onto these through `core.role_permissions` — role grants
//! are the troop's business, not this plugin's. A `conflict_facilitator` role is
//! expected to hold `conflicts:facilitate` **plus** the member-level
//! `conflicts:file` / `conflicts:read_own`, because a facilitator is also a
//! member; the object checks still confine each route to the cases they carry.
//!
//! The four ids are declared **once**, in [`perms`], with these descriptions;
//! `permissions_granted()` returns that declaration and every route gate reads
//! its id from it. `perms::assert_routes_gate_declared` — called in this crate's
//! tests — is what makes "every gate names a declared permission" a test result
//! rather than a load-time surprise.
//!
//! ## Schema
//!
//! `conflicts.cases` (the case, its current stage, its visibility lists, its
//! outcome) and `conflicts.stage_log` (append-only). The DDL lives in
//! `plugins/conflicts/migrations/*.sql` and is embedded at compile time by
//! [`migrations!`], which binds each file to the version and name recorded in
//! `core.schema_migrations`. Vocabulary that reaches a
//! decision — stages, statuses, log kinds — is constrained in the database,
//! because a stage code that has reached the log is a record, not a convention.
//! The filer is constrained to be a party (`cases_filer_is_a_party`), so "I
//! cannot read the case I filed" is not representable.
//!
//! ## What this plugin deliberately does not do
//!
//! * No delete route. A case is withdrawn or resolved; the record survives. The
//!   log's trigger would refuse the erasure anyway.
//! * No `subscriptions()`. Nothing on the event bus may change a case's stage or
//!   visibility — the pathway is moved by people, and letting an event move a
//!   case would be an unaudited actor. The anti-dropout mechanism is a
//!   [`Schedule`], not a reactor.
//! * No notification delivery of its own. The nudge publishes an opaque event
//!   ([`EVENT_CONFLICT_STAGE_STALLED`]) and lists the case to its facilitators;
//!   how a facilitator is actually told is the troop's channel to choose.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The declaration — stated once, and everything else generated from it
// ---------------------------------------------------------------------------

/// The permission vocabulary, declared once.
///
/// The crate docs above say what each permission is and why it is not wider;
/// this module is where the four id strings live. `permissions_granted()`
/// returns [`granted`](crate::perms::granted), and every route gate reads its
/// permission from these declarations (`perms::FILE.id`) instead of repeating the
/// literal — so a gate cannot drift from the vocabulary it gates on. The tests
/// call
/// [`assert_routes_gate_declared`](crate::perms::assert_routes_gate_declared),
/// which is the same check the core's loader makes, as a failing test rather than
/// a plugin that will not load.
pub mod perms {
    adjutant_sdk::permissions! {
        /// File a case; and act on a case you are already a party to.
        FILE = "conflicts:file" => "File a conflict case, and act on a case you are a party to (record your agreement, add a party, withdraw)";
        /// Read a case you have standing in — a party, or its facilitator.
        READ_OWN = "conflicts:read_own" => "Read a conflict case you have standing in — you are one of its parties, or a facilitator assigned to it";
        /// Carry a case you are assigned to.
        FACILITATE = "conflicts:facilitate" => "Carry a conflict case you are assigned to: advance its stage and record its outcome";
        /// Administer the pathway (staffing it), never its content.
        MANAGE = "conflicts:manage" => "Administer the conflict pathway: appoint and release facilitators, and see which cases are stalled and uncarried";
    }
}

/// The migrations, with their SQL in `migrations/*.sql`.
///
/// The SQL is embedded at compile time (`include_str!`), and the path is relative
/// to *this* file — so `../migrations/…` is the crate's own directory. The
/// version and name that `core.schema_migrations` records are bound to the file
/// here, rather than restated beside a multi-kilobyte string literal, and the
/// macro refuses to build on a duplicate version, a missing file, a version below
/// 1, or a name declared twice. **These versions and names must not change:** a
/// deployed database has them recorded, and the version decides whether a
/// migration runs.
pub mod migrations {
    adjutant_sdk::migrations! {
        1 => "conflicts_schema" => "../migrations/001_conflicts_schema.sql";
        2 => "stage_log_append_only" => "../migrations/002_stage_log_append_only.sql";
    }
}

// ---------------------------------------------------------------------------
// The pathway
// ---------------------------------------------------------------------------

/// Stage 1 — the parties talk to each other. The restorative default: most
/// disputes never need to leave this stage.
pub const STAGE_DIRECT: &str = "direct_conversation";
/// Stage 2 — a facilitator helps the parties reach their own agreement.
pub const STAGE_FACILITATION: &str = "facilitation";
/// Stage 3 — a third party decides or brokers terms.
pub const STAGE_ARBITRATION: &str = "arbitration";
/// Stage 4 — the troop's council hears it. The last resort, and the only stage
/// the whole troop has an interest in.
pub const STAGE_COUNCIL: &str = "troop_council";

/// The four stages, in pathway order. Order is meaning: `is_later_stage` and
/// `advance_targets` are defined by it, so nothing else may reorder this.
pub const STAGES: [&str; 4] = [
    STAGE_DIRECT,
    STAGE_FACILITATION,
    STAGE_ARBITRATION,
    STAGE_COUNCIL,
];

/// The stage a freshly filed case starts at.
pub const STAGE_ENTRY: &str = STAGE_DIRECT;

/// The stage code's position in the pathway, or `None` for an unknown code.
pub fn stage_index(stage: &str) -> Option<usize> {
    STAGES.iter().position(|s| *s == stage)
}

/// The stage after `stage`, or `None` at the end of the pathway.
pub fn next_stage(stage: &str) -> Option<&'static str> {
    stage_index(stage).and_then(|i| STAGES.get(i + 1).copied())
}

/// Every stage a case at `stage` may advance to — all of the later ones, because
/// skipping a stage is sometimes right (a case the parties agree needs a
/// decision skips straight to arbitration) and the reason recorded with the move
/// is what makes the skip accountable.
pub fn advance_targets(stage: &str) -> Vec<&'static str> {
    match stage_index(stage) {
        Some(index) => STAGES[index + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// May a case move from `from` to `to`? Forward only, one step or several, never
/// backwards — a case that could walk back to `direct_conversation` after
/// arbitration would let a party undo another party's participation.
pub fn is_later_stage(from: &str, to: &str) -> bool {
    match (stage_index(from), stage_index(to)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// Case status.
pub const STATUS_OPEN: &str = "open";
pub const STATUS_RESOLVED: &str = "resolved";
pub const STATUS_WITHDRAWN: &str = "withdrawn";
pub const STATUSES: [&str; 3] = [STATUS_OPEN, STATUS_RESOLVED, STATUS_WITHDRAWN];

// ---------------------------------------------------------------------------
// stage_log kinds — the vocabulary of the ledger
// ---------------------------------------------------------------------------

/// The case was created.
pub const KIND_FILED: &str = "filed";
/// The case moved between stages.
pub const KIND_TRANSITION: &str = "transition";
/// A facilitator recorded the outcome and closed the case.
pub const KIND_RESOLUTION: &str = "resolution";
/// The parties recorded the agreement they reached themselves, at stage 1.
pub const KIND_AGREEMENT: &str = "agreement";
/// A party withdrew the case.
pub const KIND_WITHDRAWN: &str = "withdrawn";
/// A facilitator was appointed to carry the case.
pub const KIND_FACILITATOR_ASSIGNED: &str = "facilitator_assigned";
/// A facilitator was released from the case.
pub const KIND_FACILITATOR_RELEASED: &str = "facilitator_released";
/// Somebody's visibility list was widened by adding a party.
pub const KIND_PARTY_ADDED: &str = "party_added";
/// The anti-dropout mechanism nudged the case's facilitators.
pub const KIND_NUDGE: &str = "nudge";

/// Every kind the ledger accepts (mirrored by the `stage_log_kind_valid`
/// constraint, so a kind cannot be written that the CHECK does not know).
pub const LOG_KINDS: [&str; 9] = [
    KIND_FILED,
    KIND_TRANSITION,
    KIND_RESOLUTION,
    KIND_AGREEMENT,
    KIND_WITHDRAWN,
    KIND_FACILITATOR_ASSIGNED,
    KIND_FACILITATOR_RELEASED,
    KIND_PARTY_ADDED,
    KIND_NUDGE,
];

// ---------------------------------------------------------------------------
// Events — opaque by construction
// ---------------------------------------------------------------------------

/// `conflict.filed` — a case exists. Payload: `{ case_id, stage }`, nothing more.
pub const EVENT_CONFLICT_FILED: &str = "conflict.filed";
/// `conflict.resolved` — the case closed with an outcome. Payload:
/// `{ case_id, stage, resolved_by }`, where `resolved_by` is `facilitator` or
/// `parties` — a *kind* of actor, never a name.
pub const EVENT_CONFLICT_RESOLVED: &str = "conflict.resolved";
/// `conflict.withdrawn` — a party withdrew the case.
pub const EVENT_CONFLICT_WITHDRAWN: &str = "conflict.withdrawn";
/// `conflict.stage.stalled` — the anti-dropout mechanism nudged a stalled case.
/// Payload: `{ case_id, stage, days_stalled, nudge_count }`.
pub const EVENT_CONFLICT_STAGE_STALLED: &str = "conflict.stage.stalled";
/// `conflict.staffing.changed` — a facilitator was appointed or released.
/// Payload: `{ case_id, action }`; the identity of the facilitator stays in the
/// case's private log.
pub const EVENT_CONFLICT_STAFFING: &str = "conflict.staffing.changed";
/// `conflict.party.added` — the case's visibility list grew. Payload:
/// `{ case_id }`.
pub const EVENT_CONFLICT_PARTY_ADDED: &str = "conflict.party.added";

/// The plain-language statement of this plugin's privacy rule. Returned with a
/// case so a client never has to guess, and so the rule travels with the data.
pub const CASE_PRIVACY_NOTE: &str = "A conflict case is private to its parties and to the facilitators \
     assigned to it. No troop-wide or lodge-wide grant reaches it, and no permission reads a case its \
     holder is not on.";

// ---------------------------------------------------------------------------
// Anti-dropout configuration
// ---------------------------------------------------------------------------

/// How long a stage may sit before the case is stalled — 7 days, unless the
/// troop configures `stage_stall_hours`.
pub const DEFAULT_STALL_HOURS: i64 = 24 * 7;
/// The least time between two nudges for the same case — 3 days, unless the
/// troop configures `nudge_cooldown_hours`. Without a cooldown a stalled case
/// would be nudged every tick, which is noise, not pressure.
pub const DEFAULT_NUDGE_COOLDOWN_HOURS: i64 = 72;
/// Upper bound on either setting (a year), so a typo cannot silence the
/// mechanism for a decade.
pub const MAX_HOURS: i64 = 24 * 365;
/// How many stalled cases one nudge pass considers.
pub const NUDGE_BATCH: i64 = 100;
/// The actor recorded for a nudge: nobody moved the case, the clock did.
pub const AUTO_ACTOR: &str = "conflicts:auto";

/// Field-length caps. A conflict record is read by people under stress; a cap is
/// a courtesy and a guard against a paste of an entire group chat.
pub const MAX_TITLE: usize = 200;
pub const MAX_TEXT: usize = 8000;
pub const MAX_ID: usize = 200;
/// Route list cap.
pub const MAX_LIST: i64 = 200;

/// The configured stall threshold in hours (`stage_stall_hours`).
pub fn stall_threshold_hours(config: &Value) -> i64 {
    config
        .get("stage_stall_hours")
        .and_then(Value::as_i64)
        .map(|h| h.clamp(1, MAX_HOURS))
        .unwrap_or(DEFAULT_STALL_HOURS)
}

/// The configured nudge cooldown in hours (`nudge_cooldown_hours`).
pub fn nudge_cooldown_hours(config: &Value) -> i64 {
    config
        .get("nudge_cooldown_hours")
        .and_then(Value::as_i64)
        .map(|h| h.clamp(1, MAX_HOURS))
        .unwrap_or(DEFAULT_NUDGE_COOLDOWN_HOURS)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Whole hours from `from` to `to`, never negative.
///
/// A clock that jumped backwards (NTP, a restored snapshot) must not read as a
/// case that is "minus four days stalled" — a negative age would sail through
/// every threshold comparison.
pub fn hours_between(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    (to - from).num_hours().max(0)
}

/// How long the case has been in its current stage.
pub fn stage_age_hours(stage_since: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    hours_between(stage_since, now)
}

/// Parse a timestamp as the host renders one.
///
/// The core decodes `timestamptz` as text, so every query in this crate asks for
/// `x::text` and parses here. PostgreSQL's own rendering
/// (`2026-09-01 12:00:00.123456+00`) is the production case; RFC 3339 is
/// accepted because a stub host, a fixture or a test may produce it.
pub fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    let normalized = normalize_offset(text);
    if let Ok(parsed) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(parsed) = DateTime::parse_from_str(&normalized, format) {
            return Some(parsed.with_timezone(&Utc));
        }
    }
    // A rendering with no offset at all is read as UTC: the alternative is to
    // treat a real row as unparsable and skip the case in the nudge, which would
    // make one host's formatting choice a silent dropout.
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// PostgreSQL renders a whole-hour offset as `+00`, which chrono's `%:z` does not
/// accept (`+00:00`). Rewrite only that exact shape, so a timestamp with a real
/// minute offset (`+05:30`) or a compact one (`+0000`) is passed through
/// untouched.
fn normalize_offset(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 3 {
        let sign = bytes[bytes.len() - 3];
        let digits = &bytes[bytes.len() - 2..];
        if (sign == b'+' || sign == b'-') && digits.iter().all(u8::is_ascii_digit) {
            return format!("{text}:00");
        }
    }
    text.to_string()
}

/// Has the case been in its current stage at least `threshold_hours`?
pub fn is_stalled(stage_since: DateTime<Utc>, now: DateTime<Utc>, threshold_hours: i64) -> bool {
    stage_age_hours(stage_since, now) >= threshold_hours.max(1)
}

/// Should the anti-dropout mechanism nudge this case now?
///
/// Two conditions, and they are different questions:
///
/// * **is it stalled** — the stage has exceeded the threshold; and
/// * **has it been quiet long enough** — a nudge younger than the cooldown is
///   not repeated. A previously nudged case stays eligible forever, because
///   anti-dropout only works if it keeps surfacing a case nobody has touched;
///   dropping it after N nudges would be the dropout it exists to prevent.
pub fn should_nudge(
    stage_since: DateTime<Utc>,
    last_nudge_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    threshold_hours: i64,
    cooldown_hours: i64,
) -> bool {
    if !is_stalled(stage_since, now, threshold_hours) {
        return false;
    }
    match last_nudge_at {
        Some(last) => hours_between(last, now) >= cooldown_hours.max(1),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Standing — the object-level permission check
// ---------------------------------------------------------------------------

/// The relationship a caller has to one case. Nothing else about them matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The caller is one of the case's parties.
    Party,
    /// The caller is one of the case's assigned facilitators.
    Facilitator,
}

impl Standing {
    /// The stable code a response carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Party => "party",
            Standing::Facilitator => "facilitator",
        }
    }
}

/// Read a `text[]` column out of a row, tolerating a missing or odd-shaped one.
fn id_list(row: &Value, key: &str) -> Vec<String> {
    row.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The caller's standing in this case, or `None`.
///
/// **This is the permission check.** A case's visibility list is the object, and
/// there is no scope, role or troop-wide branch behind it: a caller who is not
/// in `party_ids` or `facilitator_ids` has no standing, whoever they are.
/// Party is checked first so a facilitator who is also a party answers as a
/// party — the case is theirs before it is a job.
pub fn standing_of(case: &Value, caller: &str) -> Option<Standing> {
    let caller = caller.trim();
    if caller.is_empty() {
        return None;
    }
    if id_list(case, "party_ids").iter().any(|id| id == caller) {
        return Some(Standing::Party);
    }
    if id_list(case, "facilitator_ids")
        .iter()
        .any(|id| id == caller)
    {
        return Some(Standing::Facilitator);
    }
    None
}

/// May this caller read the case at all?
pub fn may_read(case: &Value, caller: &str) -> bool {
    standing_of(case, caller).is_some()
}

/// May this caller move the case between stages? The assigned facilitator's job,
/// and only theirs — a party cannot advance their own case past themselves.
pub fn may_advance(case: &Value, caller: &str) -> bool {
    standing_of(case, caller) == Some(Standing::Facilitator)
}

/// May this caller write a stage_log entry as a facilitator?
pub fn may_keep_the_log(case: &Value, caller: &str) -> bool {
    standing_of(case, caller) == Some(Standing::Facilitator)
}

/// May this caller record the agreement the parties reached themselves?
///
/// Only at the entry stage: `direct_conversation` *is* the parties resolving it,
/// so they are the right person to write it down. From any later stage a
/// facilitator records the outcome, because by then a third party is in the
/// room and the record is theirs to keep.
pub fn may_record_agreement(case: &Value, caller: &str) -> bool {
    case.get("stage").and_then(Value::as_str) == Some(STAGE_DIRECT)
        && standing_of(case, caller) == Some(Standing::Party)
}

/// May this caller withdraw the case? The parties' decision, never a
/// facilitator's.
pub fn may_withdraw(case: &Value, caller: &str) -> bool {
    standing_of(case, caller) == Some(Standing::Party)
}

/// Is the case still live? A resolved or withdrawn case does not move, gain
/// parties, or gain a facilitator.
pub fn is_open(case: &Value) -> bool {
    case.get("status").and_then(Value::as_str) == Some(STATUS_OPEN)
}

// ---------------------------------------------------------------------------
// Anti-dropout reporting (metadata only — never a party)
// ---------------------------------------------------------------------------

/// One stalled case as the nudge and the facilitator's queue report it.
///
/// Deliberately built from an allowlist of fields rather than by echoing the
/// row: a case row carries `party_ids`, `title` and `summary`, and any one of
/// those in a stalled-case listing would put a name on a troop-visible surface.
/// Returns `None` when the case is not stalled, or when its stage timestamp is
/// unreadable (skip it rather than nudge on a guess).
pub fn stall_report(
    case: &Value,
    now: DateTime<Utc>,
    threshold_hours: i64,
    cooldown_hours: i64,
) -> Option<Value> {
    let stage_since = parse_timestamp(case.get("stage_since").and_then(Value::as_str)?)?;
    if !is_stalled(stage_since, now, threshold_hours) {
        return None;
    }
    let last_nudge = case
        .get("nudged_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp);
    let age = stage_age_hours(stage_since, now);
    Some(json!({
        "case_id": case.get("id"),
        "stage": case.get("stage"),
        "stage_since": case.get("stage_since"),
        "stage_age_hours": age,
        "days_stalled": age / 24,
        "nudge_count": case.get("nudge_count").cloned().unwrap_or(json!(0)),
        "last_nudged_at": case.get("nudged_at").cloned().unwrap_or(Value::Null),
        "needs_nudge": should_nudge(stage_since, last_nudge, now, threshold_hours, cooldown_hours),
    }))
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct ConflictsPlugin {
    ctx: OnceLock<PluginContext>,
}

impl ConflictsPlugin {
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
        }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/schedules()")
    }
}

impl Default for ConflictsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Row shape and fetch helpers
// ---------------------------------------------------------------------------

/// Every case column the API states. `timestamptz` columns come back as `::text`
/// (the host decodes text, not timestamps) and the stage's age is computed by
/// the database in hours, so a client and the nudge agree on what "stalled"
/// means.
const CASE_FIELDS: &str = r#"
    c.id, c.title, c.summary, c.stage, c.status, c.filed_by,
    c.party_ids, c.facilitator_ids, c.outcome, c.agreement,
    c.nudge_count,
    c.stage_since::text AS stage_since,
    c.nudged_at::text AS nudged_at,
    c.resolved_at::text AS resolved_at, c.resolved_by,
    c.withdrawn_at::text AS withdrawn_at, c.withdrawn_by,
    c.created_at::text AS created_at, c.updated_at::text AS updated_at,
    (EXTRACT(EPOCH FROM (now() - c.stage_since))::bigint / 3600) AS stage_age_hours
"#;

/// The columns the anti-dropout mechanism needs: enough to decide, and nothing
/// that names a party.
const STALL_PROBE_FIELDS: &str = "c.id, c.stage, c.facilitator_ids, c.nudge_count, \
     c.stage_since::text AS stage_since, c.nudged_at::text AS nudged_at";

async fn fetch_case(c: &PluginContext, id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {fields} FROM {cases} c WHERE c.id = $1",
            fields = CASE_FIELDS,
            cases = c.db.table("cases")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// Open cases whose current stage is older than `$1` hours.
///
/// The threshold filter is in SQL so the pass does not read the whole table; the
/// cooldown check stays in Rust ([`should_nudge`]), where it is testable and
/// where a previously nudged case is a first-class case rather than a WHERE
/// clause. Nothing here selects a party.
fn stalled_probe_sql(c: &PluginContext) -> String {
    format!(
        "SELECT {fields} FROM {cases} c \
         WHERE c.status = 'open' \
           AND c.stage_since < now() - ($1::int * INTERVAL '1 hour') \
         ORDER BY c.stage_since, c.id \
         LIMIT $2",
        fields = STALL_PROBE_FIELDS,
        cases = c.db.table("cases")
    )
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FileCaseBody {
    title: String,
    /// The filer's own account. Required: it is the "why" the ledger opens with.
    summary: String,
    /// The other parties, when the filer can name them. The filer is always a
    /// party whether or not they appear here.
    #[serde(default)]
    parties: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StageBody {
    stage: String,
    /// Why the case moved. Required — an unexplained escalation is the thing the
    /// log exists to make impossible.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResolutionBody {
    /// What was agreed or decided. The outcome, not a verdict against a person.
    outcome: String,
    #[serde(default)]
    agreement: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgreementBody {
    /// The agreement the parties reached between themselves.
    agreement: String,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WithdrawBody {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FacilitatorBody {
    user_id: String,
    /// `assign` (default) or `release`.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PartyBody {
    user_id: String,
    /// Required: widening a case's visibility list is an act that needs a why.
    #[serde(default)]
    reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// A required, trimmed, length-capped string.
fn bounded(value: &str, max: usize, label: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} is required"));
    }
    if trimmed.chars().count() > max {
        return Err(format!("{label} must be at most {max} characters"));
    }
    Ok(trimmed.to_string())
}

/// An optional, trimmed, length-capped string. Blank means absent.
fn optional_bounded(
    value: &Option<String>,
    max: usize,
    label: &str,
) -> Result<Option<String>, String> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => {
            if text.chars().count() > max {
                return Err(format!("{label} must be at most {max} characters"));
            }
            Ok(Some(text.to_string()))
        }
    }
}

fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// A stage code, canonicalised, or an error naming the pathway.
fn normalize_stage(value: &str) -> Result<&'static str, String> {
    let wanted = value.trim().to_ascii_lowercase();
    STAGES
        .iter()
        .copied()
        .find(|stage| *stage == wanted)
        .ok_or_else(|| format!("stage must be one of {}", STAGES.join(", ")))
}

/// A status filter for the list route: a status code, or `all`.
fn normalize_status_filter(value: &str) -> Result<Option<String>, String> {
    let wanted = value.trim().to_ascii_lowercase();
    if wanted == "all" {
        return Ok(None);
    }
    if STATUSES.contains(&wanted.as_str()) {
        return Ok(Some(wanted));
    }
    Err(format!(
        "status must be one of {}, or 'all'",
        STATUSES.join(", ")
    ))
}

/// The caller's user id, or a 401 that is already an [`SdkError`].
///
/// Every route here needs an authenticated member: standing is a fact about a
/// person, so an anonymous caller has none by construction.
fn caller_of(req: &PluginRequest) -> Result<String, SdkError> {
    req.identity
        .as_ref()
        .map(|identity| identity.user_id.trim().to_string())
        .filter(|user| !user.is_empty())
        .ok_or_else(|| SdkError::Unauthorized("this route requires an authenticated member".into()))
}

/// The two answers a case route gives about a case the caller cannot have:
/// identical, so the route cannot be walked to discover which case ids exist.
const NO_LIKE_THAT: &str = "no such case, or you cannot see it";

fn forbidden_case() -> Result<PluginResponse, SdkError> {
    PluginResponse::error(403, NO_LIKE_THAT)
}

/// Append a row to the case's ledger. The one place `stage_log` is written, so
/// there is one place to check that the reason is present and non-blank.
async fn log_entry(
    c: &PluginContext,
    case_id: i64,
    kind: &str,
    from_stage: &str,
    to_stage: &str,
    actor: &str,
    reason: &str,
) -> Result<(), SdkError> {
    if reason.trim().is_empty() {
        return Err(SdkError::Internal(
            "refusing to write a stage_log row with no reason".into(),
        ));
    }
    c.db.execute(
        format!(
            "INSERT INTO {log} (case_id, kind, from_stage, to_stage, actor, reason) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            log = c.db.table("stage_log")
        ),
        vec![
            SqlValue::Int(case_id),
            SqlValue::Text(kind.to_string()),
            SqlValue::Text(from_stage.to_string()),
            SqlValue::Text(to_stage.to_string()),
            SqlValue::Text(actor.to_string()),
            SqlValue::Text(reason.trim().to_string()),
        ],
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin declaration
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for ConflictsPlugin {
    fn id(&self) -> &str {
        "conflicts"
    }

    fn name(&self) -> &str {
        "Conflicts"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        perms::granted()
    }

    fn migrations(&self) -> Vec<Migration> {
        migrations::all()
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();
        vec![
            file_case(ctx),
            list_cases(ctx),
            get_case(ctx),
            case_log(ctx),
            advance_stage(ctx),
            record_resolution(ctx),
            record_agreement(ctx),
            withdraw_case(ctx),
            set_facilitator(ctx),
            add_party(ctx),
            stalled_cases(ctx),
        ]
    }

    // No subscriptions, on purpose: the pathway is moved by people. An event
    // that could advance a case or widen its visibility would be an unaudited
    // actor on a private record. The anti-dropout mechanism is a schedule.
    //
    // fn subscriptions(&self) -> Vec<EventSubscription> { Vec::new() }

    fn schedules(&self) -> Vec<Schedule> {
        let ctx = self.ctx().clone(); // owned: the closure must not borrow self
        vec![Schedule::new(
            "stage_nudge",
            std::time::Duration::from_secs(6 * 60 * 60),
            schedule_handler(move || {
                let c = ctx.clone();
                async move { nudge_stalled_cases(&c).await }
            }),
        )]
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /api/conflicts/case` — file a case (`conflicts:file`).
///
/// The filer is made a party by construction (`party_ids` always begins with
/// them, and the `cases_filer_is_a_party` constraint refuses a row where that is
/// not true), which is why filing implies "and I can read it".
///
/// A case is filed **unstaffed**: appointing a facilitator is a separate act by
/// `conflicts:manage`, and it lands in the log. A filer cannot appoint
/// themselves, and cannot appoint the person they are in conflict with.
fn file_case(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case",
        perms::FILE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let body: FileCaseBody = req.json()?;
                let title = match bounded(&body.title, MAX_TITLE, "title") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let summary = match bounded(&body.summary, MAX_TEXT, "summary") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };

                // The visibility list: the filer, then whoever else they name,
                // deduplicated. This list *is* the case's privacy boundary.
                let mut parties: Vec<String> = vec![caller.clone()];
                for raw in &body.parties {
                    let party = raw.trim();
                    if party.is_empty() {
                        continue;
                    }
                    if party.chars().count() > MAX_ID {
                        return PluginResponse::error(
                            400,
                            format!("a party id must be at most {MAX_ID} characters"),
                        );
                    }
                    if !parties.iter().any(|existing| existing == party) {
                        parties.push(party.to_string());
                    }
                }

                let row =
                    c.db.query_one(
                        format!(
                            "INSERT INTO {cases} AS c (title, summary, party_ids, filed_by) \
                             VALUES ($1, $2, $3, $4) RETURNING {fields}",
                            cases = c.db.table("cases"),
                            fields = CASE_FIELDS
                        ),
                        vec![
                            SqlValue::Text(title.clone()),
                            SqlValue::Text(summary.clone()),
                            parties.clone().into(),
                            SqlValue::Text(caller.clone()),
                        ],
                    )
                    .await?
                    .ok_or_else(|| SdkError::Internal("insert returned no case row".into()))?;
                let id = row["id"].as_i64().unwrap_or_default();

                // The opening entry. Its "why" is the filer's account.
                let reason = trimmed(&body.reason).unwrap_or_else(|| summary.clone());
                log_entry(&c, id, KIND_FILED, "", STAGE_ENTRY, &caller, &reason).await?;

                // The audit log is troop-level and tamper-evident, so it records
                // the act and its size — never a party, never the account.
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.filed",
                        "conflict_case",
                        &id.to_string(),
                        json!({
                            "stage": STAGE_ENTRY,
                            "parties": parties.len(),
                            "facilitators": 0,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_CONFLICT_FILED,
                        json!({ "case_id": id, "stage": STAGE_ENTRY }),
                    )
                    .await?;

                PluginResponse::created(
                    &format!("/api/conflicts/case/{id}"),
                    &json!({
                        "case": row,
                        "standing": Standing::Party.as_str(),
                        "advance_targets": advance_targets(STAGE_ENTRY),
                        "privacy": CASE_PRIVACY_NOTE,
                        "next": "the case is unstaffed: a facilitator is appointed with \
                                 conflicts:manage (POST /api/conflicts/case/<id>/facilitator) before it \
                                 can move stages. At this stage the parties may record their own \
                                 agreement (POST /api/conflicts/case/<id>/agreement).",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/conflicts/cases` — the cases the caller has standing in
/// (`conflicts:read_own`).
///
/// There is no troop branch in this query: it selects cases whose `party_ids` or
/// `facilitator_ids` contain the caller's id, and nothing else reaches the
/// caller. A caller with no standing gets an empty list, not a filtered view of
/// other people's disputes.
fn list_cases(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/conflicts/cases",
        perms::READ_OWN.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let status = match req.query_param("status") {
                    None => Some(STATUS_OPEN.to_string()),
                    Some(raw) => match normalize_status_filter(raw) {
                        Ok(value) => value,
                        Err(error) => return PluginResponse::error(400, error),
                    },
                };
                let role = req
                    .query_param("role")
                    .map(|value| value.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "all".to_string());
                if !["all", "party", "facilitator"].contains(&role.as_str()) {
                    return PluginResponse::error(
                        400,
                        "role must be one of all, party, facilitator",
                    );
                }
                let stage = match req.query_param("stage") {
                    None => None,
                    Some(raw) => match normalize_stage(raw) {
                        Ok(value) => Some(value.to_string()),
                        Err(error) => return PluginResponse::error(400, error),
                    },
                };
                let limit = req.query_int("limit").unwrap_or(50).clamp(1, MAX_LIST);

                let rows =
                    c.db.query(
                        format!(
                            "SELECT {fields} FROM {cases} c \
                             WHERE ($1 = ANY(c.party_ids) OR $1 = ANY(c.facilitator_ids)) \
                               AND ($2::text IS NULL OR c.status = $2) \
                               AND ($3::text IS NULL OR c.stage = $3) \
                               AND ($4::text = 'all' \
                                    OR ($4 = 'party' AND $1 = ANY(c.party_ids)) \
                                    OR ($4 = 'facilitator' AND $1 = ANY(c.facilitator_ids))) \
                             ORDER BY c.updated_at DESC, c.id DESC \
                             LIMIT $5",
                            fields = CASE_FIELDS,
                            cases = c.db.table("cases")
                        ),
                        vec![
                            SqlValue::Text(caller),
                            status.clone().into(),
                            stage.clone().into(),
                            SqlValue::Text(role.clone()),
                            SqlValue::Int(limit),
                        ],
                    )
                    .await?;
                let returned = rows.len();
                PluginResponse::json(
                    200,
                    &json!({
                        "cases": rows,
                        "role": role,
                        "status": status.unwrap_or_else(|| "all".to_string()),
                        "stage": stage,
                        "returned": returned,
                        "privacy": CASE_PRIVACY_NOTE,
                    }),
                )
            }
        }),
    )
}

/// `GET /api/conflicts/case/{id}` — one case, to somebody with standing
/// (`conflicts:read_own`).
///
/// The route gate admits a caller who holds `conflicts:read_own` somewhere; the
/// handler is the boundary, and it answers the same 403 whether the case does
/// not exist or the caller is a stranger to it.
fn get_case(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/conflicts/case/{id}",
        perms::READ_OWN.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                let Some(standing) = standing_of(&case, &caller) else {
                    return forbidden_case();
                };

                // Owned copies: `case` is moved into the response below.
                let stage = case["stage"].as_str().unwrap_or_default().to_string();
                let status = case["status"].as_str().unwrap_or_default().to_string();
                let open = status == STATUS_OPEN;
                let facilitator = standing == Standing::Facilitator;
                let party = standing == Standing::Party;
                let targets = if open {
                    advance_targets(&stage)
                } else {
                    Vec::new()
                };

                PluginResponse::json(
                    200,
                    &json!({
                        "case": case,
                        "standing": standing.as_str(),
                        "advance_targets": targets,
                        "may": {
                            "advance": open && facilitator,
                            "record_resolution": open && facilitator,
                            "record_agreement": open && party && stage == STAGE_DIRECT,
                            "add_party": open,
                            "withdraw": open && party,
                        },
                        "privacy": CASE_PRIVACY_NOTE,
                    }),
                )
            }
        }),
    )
}

/// `GET /api/conflicts/case/{id}/log` — the case's append-only history
/// (`conflicts:read_own`).
///
/// This is the accountability surface: every stage move with its actor, its
/// timestamp and its reason, in order. It is the same private standing check as
/// the case itself — the history of a dispute is the dispute.
fn case_log(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/conflicts/case/{id}/log",
        perms::READ_OWN.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                if !may_read(&case, &caller) {
                    return forbidden_case();
                }
                let rows =
                    c.db.query(
                        format!(
                            "SELECT id, case_id, kind, from_stage, to_stage, actor, reason, \
                                    occurred_at::text AS occurred_at \
                             FROM {log} WHERE case_id = $1 ORDER BY occurred_at, id",
                            log = c.db.table("stage_log")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                let returned = rows.len();
                PluginResponse::json(
                    200,
                    &json!({
                        "case_id": id,
                        "log": rows,
                        "returned": returned,
                        "append_only": true,
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/stage` — advance the case
/// (`conflicts:facilitate`, assigned facilitator only).
///
/// Forward only, with a mandatory reason. The whole journey is preserved: an
/// escalation does not replace the stage that failed, it follows it, which is
/// what makes the pathway look restorative in the record rather than punitive.
fn advance_stage(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/stage",
        perms::FACILITATE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                if !may_advance(&case, &caller) {
                    // Not "you cannot facilitate" but the same opaque refusal as
                    // every other case route: holding `conflicts:facilitate` is
                    // not standing, and the answer must not reveal which.
                    return forbidden_case();
                }
                let current = case["stage"].as_str().unwrap_or_default().to_string();
                let status = case["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_OPEN {
                    return PluginResponse::error(
                        409,
                        format!(
                            "this case is {status}: a {status} case has no next stage. \
                             The record of it stays in the log (GET /api/conflicts/case/{id}/log)."
                        ),
                    );
                }

                let body: StageBody = req.json()?;
                let target = match normalize_stage(&body.stage) {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                if !is_later_stage(&current, target) {
                    let targets = advance_targets(&current);
                    let valid = if targets.is_empty() {
                        "it is already at the last stage of the pathway".to_string()
                    } else {
                        format!("valid next stages: {}", targets.join(", "))
                    };
                    return PluginResponse::error(
                        409,
                        format!(
                            "this case is at {current}: it cannot move to {target} — the pathway \
                             only advances ({valid}). A case that failed a stage keeps that stage in \
                             its history."
                        ),
                    );
                }
                let reason = match bounded(
                    body.reason.as_deref().unwrap_or_default(),
                    MAX_TEXT,
                    "reason",
                ) {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };

                // A stage move restarts the dropout clock: the new stage has
                // just begun, so its nudge count starts at zero.
                let row =
                    c.db.query_one(
                        format!(
                            "UPDATE {cases} AS c \
                             SET stage = $2, stage_since = now(), nudged_at = NULL, \
                                 nudge_count = 0, updated_at = now() \
                             WHERE c.id = $1 RETURNING {fields}",
                            cases = c.db.table("cases"),
                            fields = CASE_FIELDS
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(target.to_string())],
                    )
                    .await?
                    .ok_or_else(|| SdkError::Internal("stage update returned no row".into()))?;

                log_entry(&c, id, KIND_TRANSITION, &current, target, &caller, &reason).await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.stage.advance",
                        "conflict_case",
                        &id.to_string(),
                        json!({
                            "from_stage": current,
                            "to_stage": target,
                            "skipped": stage_index(target).unwrap_or_default()
                                > stage_index(&current).unwrap_or_default() + 1,
                            "to_troop_council": target == STAGE_COUNCIL,
                        }),
                    )
                    .await?;
                c.events
                    .publish(
                        event_type::CONFLICT_ESCALATED,
                        json!({
                            "case_id": id,
                            "from_stage": current,
                            "to_stage": target,
                            "to_troop_council": target == STAGE_COUNCIL,
                        }),
                    )
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case": row,
                        "from_stage": current,
                        "advance_targets": advance_targets(target),
                        "note": "the move is recorded in the case's log with its reason, and the \
                                 dropout clock for the new stage starts now",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/resolution` — the facilitator records the
/// outcome and closes the case (`conflicts:facilitate`, assigned facilitator).
///
/// What is stored is the **outcome and any agreement** — what the parties will
/// do next — not a finding against a person. The case closes at whatever stage
/// it reached, and that stage is what the log says it was resolved at.
fn record_resolution(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/resolution",
        perms::FACILITATE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                if !may_keep_the_log(&case, &caller) {
                    return forbidden_case();
                }
                let stage = case["stage"].as_str().unwrap_or_default().to_string();
                let status = case["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_OPEN {
                    return PluginResponse::error(409, format!("this case is already {status}"));
                }

                let body: ResolutionBody = req.json()?;
                let outcome = match bounded(&body.outcome, MAX_TEXT, "outcome") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let agreement = match optional_bounded(&body.agreement, MAX_TEXT, "agreement") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let reason = trimmed(&body.reason)
                    .unwrap_or_else(|| format!("resolution recorded at {stage}"));

                let row =
                    c.db.query_one(
                        format!(
                            "UPDATE {cases} AS c \
                             SET status = 'resolved', outcome = $2, agreement = COALESCE($3, ''), \
                                 resolved_at = now(), resolved_by = $4, updated_at = now() \
                             WHERE c.id = $1 RETURNING {fields}",
                            cases = c.db.table("cases"),
                            fields = CASE_FIELDS
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(outcome.clone()),
                            agreement.clone().into(),
                            SqlValue::Text(caller.clone()),
                        ],
                    )
                    .await?
                    .ok_or_else(|| {
                        SdkError::Internal("resolution update returned no row".into())
                    })?;

                log_entry(&c, id, KIND_RESOLUTION, &stage, &stage, &caller, &reason).await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.resolved",
                        "conflict_case",
                        &id.to_string(),
                        // No outcome text here: the audit log is troop-level.
                        json!({ "stage": stage, "agreement_recorded": agreement.is_some() }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_CONFLICT_RESOLVED,
                        json!({ "case_id": id, "stage": stage, "resolved_by": "facilitator" }),
                    )
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case": row,
                        "resolution": { "stage": stage, "recorded_by": "facilitator" },
                        "note": "the record is the outcome and the agreement, not a verdict: \
                                 'resolved' describes the dispute, not a person",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/agreement` — a party records the agreement the
/// parties reached themselves (`conflicts:file`, party only, entry stage only).
///
/// This is the restorative core: at `direct_conversation` the people involved
/// are the ones who settle it, so they write it down. From any later stage a
/// facilitator records the outcome, because by then a third party is in the room.
fn record_agreement(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/agreement",
        perms::FILE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                if !may_withdraw(&case, &caller) {
                    return forbidden_case();
                }
                let status = case["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_OPEN {
                    return PluginResponse::error(409, format!("this case is already {status}"));
                }
                if !may_record_agreement(&case, &caller) {
                    let stage = case["stage"].as_str().unwrap_or_default().to_string();
                    return PluginResponse::error(
                        409,
                        format!(
                            "the parties record their own agreement at {STAGE_DIRECT}; this case is \
                             at {stage}. From a later stage a facilitator records the outcome \
                             (POST /api/conflicts/case/{id}/resolution)."
                        ),
                    );
                }

                let body: AgreementBody = req.json()?;
                let agreement = match bounded(&body.agreement, MAX_TEXT, "agreement") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let outcome = match optional_bounded(&body.outcome, MAX_TEXT, "outcome") {
                    Ok(Some(value)) => value,
                    Ok(None) => agreement.clone(),
                    Err(error) => return PluginResponse::error(400, error),
                };
                let reason = trimmed(&body.reason).unwrap_or_else(|| agreement.clone());

                let row =
                    c.db.query_one(
                        format!(
                            "UPDATE {cases} AS c \
                             SET status = 'resolved', agreement = $2, outcome = $3, \
                                 resolved_at = now(), resolved_by = $4, updated_at = now() \
                             WHERE c.id = $1 RETURNING {fields}",
                            cases = c.db.table("cases"),
                            fields = CASE_FIELDS
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(agreement),
                            SqlValue::Text(outcome),
                            SqlValue::Text(caller.clone()),
                        ],
                    )
                    .await?
                    .ok_or_else(|| SdkError::Internal("agreement update returned no row".into()))?;

                log_entry(
                    &c,
                    id,
                    KIND_AGREEMENT,
                    STAGE_DIRECT,
                    STAGE_DIRECT,
                    &caller,
                    &reason,
                )
                .await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.agreement",
                        "conflict_case",
                        &id.to_string(),
                        json!({ "stage": STAGE_DIRECT, "by": "party" }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_CONFLICT_RESOLVED,
                        json!({ "case_id": id, "stage": STAGE_DIRECT, "resolved_by": "parties" }),
                    )
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case": row,
                        "resolution": { "stage": STAGE_DIRECT, "recorded_by": "party" },
                        "note": "the parties settled it between themselves: the closest to the \
                                 parties a dispute can be resolved",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/withdraw` — a party withdraws the case
/// (`conflicts:file`, party only).
///
/// Withdrawing is not deleting: the case and its log stay. Nobody erases a
/// record of a dispute — the party who no longer wants to pursue it says so,
/// with a reason, and the pathway stops.
fn withdraw_case(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/withdraw",
        perms::FILE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                if !may_withdraw(&case, &caller) {
                    return forbidden_case();
                }
                let status = case["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_OPEN {
                    return PluginResponse::error(409, format!("this case is already {status}"));
                }
                let stage = case["stage"].as_str().unwrap_or_default().to_string();

                let body: WithdrawBody = req.json()?;
                let reason = match bounded(
                    body.reason.as_deref().unwrap_or_default(),
                    MAX_TEXT,
                    "reason",
                ) {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };

                let row =
                    c.db.query_one(
                        format!(
                            "UPDATE {cases} AS c \
                             SET status = 'withdrawn', withdrawn_at = now(), withdrawn_by = $2, \
                                 updated_at = now() \
                             WHERE c.id = $1 RETURNING {fields}",
                            cases = c.db.table("cases"),
                            fields = CASE_FIELDS
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(caller.clone())],
                    )
                    .await?
                    .ok_or_else(|| SdkError::Internal("withdraw update returned no row".into()))?;

                log_entry(&c, id, KIND_WITHDRAWN, &stage, &stage, &caller, &reason).await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.withdrawn",
                        "conflict_case",
                        &id.to_string(),
                        json!({ "stage": stage }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_CONFLICT_WITHDRAWN,
                        json!({ "case_id": id, "stage": stage }),
                    )
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case": row,
                        "note": "withdrawn, not deleted: the case and its history remain, and \
                                 either party may file again",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/facilitator` — appoint or release a facilitator
/// (`conflicts:manage`).
///
/// The staffing route, and the one place the privacy model has to hold a line:
///
/// * the handler answers 404 for a case that does not exist — the *existence* of
///   a case id is not private from the pathway's administrator, but its content
///   is, and this response never includes a party;
/// * **self-appointment is refused.** If a manage holder could add themselves as
///   a facilitator they could read any case, which would quietly turn
///   "administers the pathway" into "reads every dispute". Staffing somebody
///   else is the decision this permission describes.
fn set_facilitator(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/facilitator",
        perms::MANAGE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return PluginResponse::error(404, "no such case");
                };
                let stage = case["stage"].as_str().unwrap_or_default().to_string();
                let body: FacilitatorBody = req.json()?;
                let user_id = match bounded(&body.user_id, MAX_ID, "user_id") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let action = body
                    .action
                    .as_deref()
                    .map(|value| value.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "assign".to_string());
                if !["assign", "release"].contains(&action.as_str()) {
                    return PluginResponse::error(400, "action must be assign or release");
                }

                let facilitators = id_list(&case, "facilitator_ids");
                let already = facilitators.iter().any(|id| id == &user_id);
                if action == "assign" {
                    if user_id == caller {
                        return PluginResponse::error(
                            403,
                            "a facilitator is appointed by somebody else: staffing a case is \
                             distinct from carrying it, and self-appointment would turn \
                             conflicts:manage into read access to every case",
                        );
                    }
                    if already {
                        return PluginResponse::error(
                            409,
                            format!("{user_id} is already a facilitator on this case"),
                        );
                    }
                } else if !already {
                    return PluginResponse::error(
                        409,
                        format!("{user_id} is not a facilitator on this case"),
                    );
                }

                let sql = if action == "assign" {
                    format!(
                        "UPDATE {cases} SET facilitator_ids = array_append(facilitator_ids, $2), \
                         updated_at = now() WHERE id = $1",
                        cases = c.db.table("cases")
                    )
                } else {
                    format!(
                        "UPDATE {cases} SET facilitator_ids = array_remove(facilitator_ids, $2), \
                         updated_at = now() WHERE id = $1",
                        cases = c.db.table("cases")
                    )
                };
                c.db.execute(
                    sql,
                    vec![SqlValue::Int(id), SqlValue::Text(user_id.clone())],
                )
                .await?;

                let (kind, verb) = if action == "assign" {
                    (KIND_FACILITATOR_ASSIGNED, "appointed")
                } else {
                    (KIND_FACILITATOR_RELEASED, "released")
                };
                let default_reason = if action == "assign" {
                    "appointed to carry this case"
                } else {
                    "released from this case"
                };
                // The actor is the appointer and the *subject* is the appointee:
                // both belong in the ledger, or "who was put on this case" would
                // live only in a mutable array and the troop-level audit, which is
                // deliberately opaque. So the reason names the person and carries
                // the human's words.
                let reason = format!(
                    "{verb} {user_id} — {}",
                    trimmed(&body.reason).unwrap_or_else(|| default_reason.to_string())
                );
                log_entry(&c, id, kind, &stage, &stage, &caller, &reason).await?;

                // The audit entry stays opaque on purpose: who carries a private
                // case belongs in the case's log, not in the troop-level ledger.
                let after = if action == "assign" {
                    facilitators.len() + 1
                } else {
                    facilitators.len().saturating_sub(1)
                };
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.facilitator",
                        "conflict_case",
                        &id.to_string(),
                        json!({ "action": action, "facilitators": after }),
                    )
                    .await?;
                c.events
                    .publish(
                        EVENT_CONFLICT_STAFFING,
                        json!({ "case_id": id, "action": action }),
                    )
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case_id": id,
                        "action": action,
                        "facilitator_ids": if action == "assign" {
                            let mut next = facilitators.clone();
                            next.push(user_id.clone());
                            next
                        } else {
                            facilitators.iter().filter(|f| **f != user_id).cloned().collect()
                        },
                        "note": "appointing a facilitator gives them standing in this case, and \
                                 is recorded in its log with this reason",
                    }),
                )
            }
        }),
    )
}

/// `POST /api/conflicts/case/{id}/party` — add a party (`conflicts:file`,
/// standing required).
///
/// Adding a party **widens the case's visibility list**, which is the one
/// operation that can leak a private record, so it is deliberately not a manage
/// power: only somebody already on the case may widen it, and only with a reason
/// that lands in the log. The path from "administer the pathway" to "read a
/// case" stays closed.
fn add_party(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::post_protected_any_scope(
        "/api/conflicts/case/{id}/party",
        perms::FILE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let id = req.int_param("id")?;
                let Some(case) = fetch_case(&c, id).await? else {
                    return forbidden_case();
                };
                let Some(standing) = standing_of(&case, &caller) else {
                    return forbidden_case();
                };
                let stage = case["stage"].as_str().unwrap_or_default().to_string();
                let status = case["status"].as_str().unwrap_or_default().to_string();
                if status != STATUS_OPEN {
                    return PluginResponse::error(
                        409,
                        format!("this case is {status}: its parties are settled"),
                    );
                }

                let body: PartyBody = req.json()?;
                let user_id = match bounded(&body.user_id, MAX_ID, "user_id") {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };
                let parties = id_list(&case, "party_ids");
                if parties.iter().any(|existing| existing == &user_id) {
                    return PluginResponse::error(
                        409,
                        format!("{user_id} is already a party to this case"),
                    );
                }
                let reason = match bounded(
                    body.reason.as_deref().unwrap_or_default(),
                    MAX_TEXT,
                    "reason",
                ) {
                    Ok(value) => value,
                    Err(error) => return PluginResponse::error(400, error),
                };

                c.db.execute(
                    format!(
                        "UPDATE {cases} SET party_ids = array_append(party_ids, $2), \
                         updated_at = now() WHERE id = $1",
                        cases = c.db.table("cases")
                    ),
                    vec![SqlValue::Int(id), SqlValue::Text(user_id.clone())],
                )
                .await?;
                // The ledger names the person who was added as well as who added
                // them: widening a case's visibility list is a decision somebody
                // later has to be able to account for.
                let reason = format!("added {user_id} as a party — {reason}");
                log_entry(&c, id, KIND_PARTY_ADDED, &stage, &stage, &caller, &reason).await?;
                c.audit
                    .log(
                        req.identity.as_ref(),
                        "case.party.add",
                        "conflict_case",
                        &id.to_string(),
                        json!({ "parties": parties.len() + 1, "by": standing.as_str() }),
                    )
                    .await?;
                c.events
                    .publish(EVENT_CONFLICT_PARTY_ADDED, json!({ "case_id": id }))
                    .await?;

                PluginResponse::json(
                    200,
                    &json!({
                        "case_id": id,
                        "added": user_id,
                        "added_by": standing.as_str(),
                        "note": "a party can read the case from now on; the addition is in the log \
                                 with this reason",
                    }),
                )
            }
        }),
    )
}

/// `GET /api/conflicts/stalled` — the cases this facilitator carries that have
/// stopped moving (`conflicts:facilitate`).
///
/// The manual face of the anti-dropout mechanism, and metadata only: an opaque
/// case id, a stage, an age, a nudge count. A facilitator sees their own
/// stalled cases — not the troop's — because a case's existence is as private as
/// its content to everybody except the people on it.
fn stalled_cases(ctx: &PluginContext) -> RouteDefinition {
    let c = ctx.clone();
    RouteDefinition::get_protected_any_scope(
        "/api/conflicts/stalled",
        perms::FACILITATE.id,
        route_handler(move |req| {
            let c = c.clone();
            async move {
                let caller = caller_of(&req)?;
                let threshold = req
                    .query_int("hours")
                    .map(|hours| hours.clamp(1, MAX_HOURS))
                    .unwrap_or_else(|| stall_threshold_hours(&c.config));
                let cooldown = nudge_cooldown_hours(&c.config);
                let limit = req.query_int("limit").unwrap_or(50).clamp(1, MAX_LIST);
                let rows =
                    c.db.query(
                        stalled_probe_sql(&c),
                        vec![SqlValue::Int(threshold), SqlValue::Int(limit)],
                    )
                    .await?;
                let now = Utc::now();
                let cases: Vec<Value> = rows
                    .iter()
                    .filter(|row| id_list(row, "facilitator_ids").iter().any(|f| f == &caller))
                    .filter_map(|row| stall_report(row, now, threshold, cooldown))
                    .collect();
                let returned = cases.len();
                PluginResponse::json(
                    200,
                    &json!({
                        "as_of": now.to_rfc3339(),
                        "threshold_hours": threshold,
                        "cooldown_hours": cooldown,
                        "cases": cases,
                        "returned": returned,
                        "note": "the cases you carry that have stopped moving, by opaque id and \
                                 stage. The parties are not listed here — see the case itself.",
                    }),
                )
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Anti-dropout: the scheduled nudge
// ---------------------------------------------------------------------------

/// The `stage_nudge` schedule: find stalled cases and nudge their facilitators.
///
/// Runs on the plugin's own connection pool as its own role, like a request. One
/// pass reads the open cases past the threshold, keeps those whose cooldown has
/// expired ([`should_nudge`]), and for each: bumps its nudge counter, writes a
/// `nudge` row to the case's ledger (so the pressure is part of the record), and
/// publishes an **opaque** `conflict.stage.stalled`.
///
/// Nothing here reads or emits a party. A case with no facilitator yet is still
/// nudged — its ledger entry is the record that nobody has picked it up, and the
/// event is what tells whoever is listening.
async fn nudge_stalled_cases(c: &PluginContext) -> Result<(), SdkError> {
    let threshold = stall_threshold_hours(&c.config);
    let cooldown = nudge_cooldown_hours(&c.config);
    let now = Utc::now();
    let rows =
        c.db.query(
            stalled_probe_sql(c),
            vec![SqlValue::Int(threshold), SqlValue::Int(NUDGE_BATCH)],
        )
        .await?;

    for case in &rows {
        let Some(stage_since) = case
            .get("stage_since")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        else {
            // An unreadable stage timestamp is skipped rather than guessed at.
            continue;
        };
        let last_nudge = case
            .get("nudged_at")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        if !should_nudge(stage_since, last_nudge, now, threshold, cooldown) {
            continue;
        }
        let id = case.get("id").and_then(Value::as_i64).unwrap_or_default();
        let stage = case
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let age_hours = stage_age_hours(stage_since, now);
        let days = age_hours / 24;
        let nudges = case.get("nudge_count").and_then(Value::as_i64).unwrap_or(0) + 1;
        let reason = format!(
            "no movement in {stage} for {days} day(s) ({age_hours}h): the pathway has stalled"
        );

        c.db.execute(
            format!(
                // Deliberately does not touch `updated_at`: a nudge is the clock
                // talking, not a person, and a case should not jump to the top of
                // a list because a schedule looked at it.
                "UPDATE {cases} SET nudge_count = nudge_count + 1, nudged_at = now() \
                 WHERE id = $1",
                cases = c.db.table("cases")
            ),
            vec![SqlValue::Int(id)],
        )
        .await?;
        // The actor is the mechanism, not a person: nobody moved this case.
        log_entry(c, id, KIND_NUDGE, &stage, &stage, AUTO_ACTOR, &reason).await?;
        c.audit
            .log(
                None,
                "case.nudge",
                "conflict_case",
                &id.to_string(),
                json!({ "stage": stage, "days_stalled": days, "nudges": nudges }),
            )
            .await?;
        c.events
            .publish(
                EVENT_CONFLICT_STAGE_STALLED,
                json!({
                    "case_id": id,
                    "stage": stage,
                    "days_stalled": days,
                    "nudge_count": nudges,
                }),
            )
            .await?;
    }
    Ok(())
}

export_plugin!(ConflictsPlugin);
