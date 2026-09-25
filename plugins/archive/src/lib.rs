//! # adjutant-archive — the troop's memory (SPEC §7.8)
//!
//! `docs/plugin-roadmap.md` §4 puts this plugin's value plainly: "Archive in
//! particular is the strongest thing to show an outside body, because it is the
//! only part no vendor can sell back to us." A governance plugin that models
//! motions but forgets what the troop decided, in which meeting, and what came
//! of it, is a form generator. This plugin owns that memory:
//!
//! * **Records** (SPEC §7.8's `archive.records`) — Congress proceedings, Troop
//!   Council and Lodge minutes, mission reports, policies, impacts, and the
//!   correspondence that surrounds them.
//! * **Full-text search** over a real PostgreSQL index: a generated, weighted
//!   `tsvector` column with a GIN index, queried with `websearch_to_tsquery` (or
//!   `plainto_tsquery`) and ranked with `ts_rank_cd`. Not a `LIKE` scan.
//! * **A timeline** that is chronological across *all* record types, not
//!   per-type lists, with a keyset cursor so paging a decade of history is
//!   stable while new records are filed.
//! * **Relationship tracking** — the SPEC's chain
//!   `decisions → policies → missions → impact` — as explicit, typed edges, so
//!   the troop can answer *"why does this policy exist?"* and *"what did this
//!   decision actually produce?"* in one call (`/lineage`).
//!
//! ## Records are immutable. This is enforced, not promised.
//!
//! An archive that can be quietly edited is not an archive. Three things make
//! that structural rather than aspirational:
//!
//! 1. There is **no route that modifies or deletes a record** — not even for a
//!    Chair. Read the route table: only `POST` (file), `POST …/supersede`
//!    (correct), and `DELETE /api/archive/link/{id}` (unlink a mis-typed edge)
//!    change anything.
//! 2. The database **refuses** it. Migration 2 installs a `BEFORE UPDATE OR
//!    DELETE` trigger on `records` that raises unconditionally, so a future
//!    route, a native plugin, or a hand-typed `psql` UPDATE all fail loudly with
//!    a message that says what to do instead. The trigger is the enforcement;
//!    the missing route is good manners.
//! 3. A correction is **a new record that supersedes the old one**. The original
//!    survives with its own fingerprint — what it said, when it was filed, who
//!    filed it — and the correction carries `supersedes_id` and the reason.
//!    `/record/{id}/chain` walks the correction history in both directions.
//!
//! `superseded_by` is **derived** (`SELECT c.id FROM records c WHERE
//! c.supersedes_id = r.id`), never stored, so there is no bookkeeping pointer
//! that can fall out of step with the rows — and no UPDATE anywhere in the
//! plugin's query surface.
//!
//! ## Relationships are the point
//!
//! Edges read `from <relation> to`, and the direction is always **cause →
//! effect**:
//!
//! | relation | from | to | the question it answers |
//! |---|---|---|---|
//! | `outcome` | `motion` | `decision` | what did the body decide about this motion? |
//! | `decides` | `decision` | `policy` | why does this policy exist? |
//! | `authorizes` | `policy` | `mission_report` | what authorises this mission? |
//! | `produces` | `mission_report` | `impact` | what did this mission actually produce? |
//! | `amends` | record | record | what does this version amend? |
//! | `documents` | `minutes` | record | what do these minutes document? |
//! | `relates_to` | record | record | an edge the troop drew by hand |
//!
//! `/record/{id}/lineage` walks the graph in both directions (upstream = why it
//! exists, downstream = what it produced) with a recursive CTE, returns the
//! nodes and edges, and renders one plain-language sentence per step
//! ([`describe_edge`]) so the answer is legible without the client having to
//! understand the vocabulary.
//!
//! ## Ingestion is automatic, and idempotent
//!
//! The M6-era plugins publish on the bus; the archive listens and accrues the
//! record without anyone re-typing a minute:
//!
//! | subscription | events | record filed |
//! |---|---|---|
//! | `motion.` | `motion.proposed` | a `motion` record (the proposal) |
//! | | `motion.passed` / `motion.failed` | a `decision` record, linked `outcome` from the motion |
//! | `accords.` | `accords.adopted` | a `policy` record, linked `amends` to the previous version |
//! | `mission.` | `mission.completed` | a `mission_report` record, plus an `impact` record linked `produces` |
//!
//! Prefix filters are used deliberately: `motion.` covers proposed/passed/failed
//! with one subscription, and a future `motion.amended` is not silently dropped
//! work — it lands in a handler that already knows what to do with a motion.
//!
//! **Immutability shapes the model.** A motion that passes cannot be turned into
//! the decision it became by editing the motion record, so ingestion files two
//! records and links them. That is the honest history: the troop proposed X, and
//! later decided Y about it.
//!
//! **Idempotency is the database's.** Every ingested record carries a
//! `source_ref` `<plugin>:<entity>:<id>` under a unique index, and every write is
//! `ON CONFLICT (source_ref) DO NOTHING RETURNING …`. The bus is a broadcast with
//! replay, so a redelivered event finds the row already there, returns no row,
//! and the handler stops — no duplicate, no second audit entry. Manual records
//! have a NULL `source_ref`, and PostgreSQL's unique indexes do not constrain
//! NULLs, so hand-filed records never collide with each other.
//!
//! ## Permissions
//!
//! | permission | what it reaches |
//! |---|---|
//! | `archive:read` | records, timeline, chain, lineage, stats |
//! | `archive:search` | full-text search (its own permission: a troop can open search to everyone and keep the chronological view to the archive keeper) |
//! | `archive:write` | file a record, file a correction |
//! | `archive:link` | draw a relationship between two records |
//! | `archive:manage` | remove a mis-typed link (destructive, troop-covering by the SDK) |
//!
//! Writes are tightly held by design: `archive:write` is meant for an archive
//! keeper and the chairs, not for every scout. Every read is **scoped** exactly
//! as the rest of the system is (SPEC §9.2): a troop-covering grant of the read
//! permission sees the whole archive, a Lodge-covering grant sees troop-wide
//! records plus its own Lodge's, and the author of a record always sees what
//! they filed. The same predicate is applied inside `/search` — a search that
//! leaked a Lodge's minutes to the troop would undo the point of scoping it.
//!
//! **Role grants are the operator's, not the plugin's.** The core seeds `chief`
//! with every permission; a troop maps other roles through `core.role_permissions`
//! (see `docs/api-reference.md`).
//!
//! ## Schema
//!
//! `records` is SPEC §7.8's table. `links` is the edge table underneath the
//! relationship tracking. Vocabulary that reaches a constraint is a constraint:
//! `kind`, `body_code`, `outcome`, `scope_type` and `relation` are all checked in
//! the database, so a typo in a handler is a failed write rather than a record
//! nobody can find.
//!
//! ## Windows are half-open
//!
//! `from`/`to` filter `occurred_at` as `from <= occurred_at < to`. A date-only
//! `to` therefore means "up to *the start of* that day" — so `to=2026-11-01`
//! returns all of October and nothing from November. Every response that applied
//! a window says so (`"to_exclusive": true`) rather than leaving the caller to
//! guess.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// A Congress proceeding: what the troop's highest body did, in session.
pub const KIND_CONGRESS: &str = "congress_proceeding";
/// Minutes of a Troop Council or Lodge meeting.
pub const KIND_MINUTES: &str = "minutes";
/// A motion as it was proposed (before it was decided).
pub const KIND_MOTION: &str = "motion";
/// A decision a body reached about a motion.
pub const KIND_DECISION: &str = "decision";
/// A policy, including an adopted version of the Accords.
pub const KIND_POLICY: &str = "policy";
/// A mission report, filed when a mission closes.
pub const KIND_MISSION_REPORT: &str = "mission_report";
/// The impact a mission reported — the end of the SPEC's chain.
pub const KIND_IMPACT: &str = "impact";
/// Correspondence worth keeping (a letter to a landowner, a charter).
pub const KIND_CORRESPONDENCE: &str = "correspondence";
/// A note the troop chose to archive.
pub const KIND_NOTE: &str = "note";

/// Every record kind the database will accept.
pub const KINDS: [&str; 9] = [
    KIND_CONGRESS,
    KIND_MINUTES,
    KIND_MOTION,
    KIND_DECISION,
    KIND_POLICY,
    KIND_MISSION_REPORT,
    KIND_IMPACT,
    KIND_CORRESPONDENCE,
    KIND_NOTE,
];

/// The SPEC §7.8 chain as an ordered list a client can render: a decision leads
/// to a policy, the policy to missions, the missions to impact.
pub const CHAIN: [&str; 4] = [KIND_DECISION, KIND_POLICY, KIND_MISSION_REPORT, KIND_IMPACT];

/// Congress.
pub const BODY_CONGRESS: &str = "congress";
/// Troop Council.
pub const BODY_TC: &str = "tc";
/// A Lodge meeting.
pub const BODY_LODGE: &str = "lodge";
/// A committee.
pub const BODY_COMMITTEE: &str = "committee";

/// The governing bodies a record can belong to. Governance's stable codes
/// (SPEC §7.4), reused verbatim so a client can correlate an archived Congress
/// proceeding with the motions decided in it. `""` = not a governing body.
pub const BODY_CODES: [&str; 4] = [BODY_CONGRESS, BODY_TC, BODY_LODGE, BODY_COMMITTEE];

/// The outcome of a decision that passed.
pub const OUTCOME_PASSED: &str = "passed";
/// The outcome of a decision that failed. It is still a decision, and the
/// archive keeps it: a troop's record of what it rejected is part of its history.
pub const OUTCOME_FAILED: &str = "failed";
/// Withdrawn before a vote.
pub const OUTCOME_WITHDRAWN: &str = "withdrawn";
/// Adopted (an Accords version).
pub const OUTCOME_ADOPTED: &str = "adopted";

pub const OUTCOMES: [&str; 4] = [
    OUTCOME_PASSED,
    OUTCOME_FAILED,
    OUTCOME_WITHDRAWN,
    OUTCOME_ADOPTED,
];

/// Troop-wide record: everyone with the read permission at some scope sees it.
pub const SCOPE_TROOP: &str = "troop";
/// Lodge-scoped record: visible to a grant covering that Lodge.
pub const SCOPE_LODGE: &str = "lodge";

pub const SCOPE_TYPES: [&str; 2] = [SCOPE_TROOP, SCOPE_LODGE];

/// A decision's effect on a policy (SPEC §7.8's first link).
pub const RELATION_DECIDES: &str = "decides";
/// A policy's authority over a mission.
pub const RELATION_AUTHORIZES: &str = "authorizes";
/// A mission's product: its impact.
pub const RELATION_PRODUCES: &str = "produces";
/// A motion's recorded outcome.
pub const RELATION_OUTCOME: &str = "outcome";
/// One record amends an earlier one (an Accords version, a revised policy).
pub const RELATION_AMENDS: &str = "amends";
/// Minutes document the proceeding they record.
pub const RELATION_DOCUMENTS: &str = "documents";
/// The edge a troop draws by hand.
pub const RELATION_RELATES_TO: &str = "relates_to";

pub const RELATIONS: [&str; 7] = [
    RELATION_DECIDES,
    RELATION_AUTHORIZES,
    RELATION_PRODUCES,
    RELATION_OUTCOME,
    RELATION_AMENDS,
    RELATION_DOCUMENTS,
    RELATION_RELATES_TO,
];

/// A record filed by a person through the API.
pub const SOURCE_MANUAL: &str = "manual";
/// A record accrued from a `missions` event.
pub const SOURCE_MISSIONS: &str = "missions";
/// A record accrued from a `governance` event.
pub const SOURCE_GOVERNANCE: &str = "governance";
/// A record that corrects an earlier one.
pub const SOURCE_CORRECTION: &str = "correction";

pub const SOURCES: [&str; 4] = [
    SOURCE_MANUAL,
    SOURCE_MISSIONS,
    SOURCE_GOVERNANCE,
    SOURCE_CORRECTION,
];

/// `filed_by` on a record the plugin accrued for itself. Deliberately not a user
/// id: it is a stable marker a report can filter on, and no member is called
/// this.
pub const INGEST_ACTOR: &str = "archive:ingest";

/// `websearch_to_tsquery`: quoted phrases, OR, -exclude.
pub const SEARCH_MODE_WEB: &str = "websearch";
/// `plainto_tsquery`: every word is ANDed, no operator syntax.
pub const SEARCH_MODE_ALL_WORDS: &str = "all_words";

/// Default page size for the list-shaped routes.
pub const DEFAULT_LIMIT: i64 = 50;
/// Hard ceiling on a page, so one request cannot read the whole archive.
pub const MAX_LIMIT: i64 = 200;
/// Default depth for a lineage walk (the four-link chain fits in three steps).
pub const DEFAULT_DEPTH: i64 = 3;
/// Ceiling on a lineage walk — a bound, not a graph algorithm.
pub const MAX_DEPTH: i64 = 6;
/// Ceiling on a correction chain walk.
pub const MAX_CHAIN: i64 = 50;
/// The separator inside an opaque timeline cursor: `<rfc3339>|<id>`.
pub const CURSOR_SEPARATOR: char = '|';

// ---------------------------------------------------------------------------
// Labels — one place where a code becomes something a human reads
// ---------------------------------------------------------------------------

/// A record kind as a heading.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        KIND_CONGRESS => "Congress proceedings",
        KIND_MINUTES => "Minutes",
        KIND_MOTION => "Motion",
        KIND_DECISION => "Decision",
        KIND_POLICY => "Policy",
        KIND_MISSION_REPORT => "Mission report",
        KIND_IMPACT => "Impact",
        KIND_CORRESPONDENCE => "Correspondence",
        KIND_NOTE => "Note",
        _ => "Record",
    }
}

/// A governing-body code as a body.
pub fn body_label(body: &str) -> &'static str {
    match body {
        BODY_CONGRESS => "the Congress",
        BODY_TC => "the Troop Council",
        BODY_LODGE => "the Lodge",
        BODY_COMMITTEE => "a committee",
        _ => "the troop",
    }
}

/// The verb an edge's relation contributes to a sentence, chosen so
/// `"{from} {phrase} {to}"` reads as English in the direction the edge points.
pub fn relation_phrase(relation: &str) -> &'static str {
    match relation {
        RELATION_DECIDES => "decided",
        RELATION_AUTHORIZES => "authorized",
        RELATION_PRODUCES => "produced",
        RELATION_OUTCOME => "was decided by",
        RELATION_AMENDS => "amends",
        RELATION_DOCUMENTS => "documents",
        RELATION_RELATES_TO => "relates to",
        _ => "links to",
    }
}

/// What a relation means, for the vocabulary route and the link route's advice.
pub fn relation_meaning(relation: &str) -> &'static str {
    match relation {
        RELATION_DECIDES => "the decision established this policy",
        RELATION_AUTHORIZES => "the policy authorises this mission",
        RELATION_PRODUCES => "the mission produced this impact",
        RELATION_OUTCOME => "the decision is the recorded outcome of the motion",
        RELATION_AMENDS => "this record amends the one it points at",
        RELATION_DOCUMENTS => "these minutes document the record they point at",
        RELATION_RELATES_TO => "an edge the troop drew by hand",
        _ => "an unrecognised relationship",
    }
}

/// The kinds a relation expects on each end — `(from, to)`.
///
/// Empty slices mean "anything". Used for **advice**, never for a refusal: a
/// troop that records its Congress as a `congress_proceeding` and links it
/// `decides` a policy is describing the same fact in its own vocabulary, and the
/// archive should say "did you mean a decision here?" rather than refuse to keep
/// the record.
pub fn relation_kinds(relation: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match relation {
        RELATION_DECIDES => (&[KIND_DECISION], &[KIND_POLICY]),
        RELATION_AUTHORIZES => (&[KIND_POLICY], &[KIND_MISSION_REPORT]),
        RELATION_PRODUCES => (&[KIND_MISSION_REPORT], &[KIND_IMPACT]),
        RELATION_OUTCOME => (&[KIND_MOTION], &[KIND_DECISION]),
        RELATION_DOCUMENTS => (&[KIND_MINUTES, KIND_CONGRESS], &[]),
        _ => (&[], &[]),
    }
}

/// One node of a graph, rendered for a sentence.
fn node_label(node: &Value) -> String {
    let kind = node["kind"].as_str().unwrap_or("record");
    let id = node["id"]
        .as_i64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
    match node["title"].as_str().filter(|t| !t.is_empty()) {
        Some(title) => format!("{kind} #{id} \u{201c}{title}\u{201d}"),
        None => format!("{kind} #{id}"),
    }
}

/// The plain-language sentence for one edge — the answer to "why does this
/// policy exist?" without the caller having to know the vocabulary.
///
/// Pure, so the wording is pinned by a test rather than by a running database.
pub fn describe_edge(relation: &str, from: &Value, to: &Value) -> String {
    format!(
        "{} {} {}",
        node_label(from),
        relation_phrase(relation),
        node_label(to)
    )
}

/// Is this record the current version (nothing supersedes it)?
pub fn is_current(record: &Value) -> bool {
    record["superseded_by"].is_null()
}

// ---------------------------------------------------------------------------
// Pure helpers — parsing and validation, no context, no database
// ---------------------------------------------------------------------------

/// Parse an instant from the forms a person or a client actually sends: RFC 3339
/// (`2026-09-14T18:00:00Z`, `2026-09-14T18:00:00-04:00`), a bare wall clock
/// (`2026-09-14T18:00[:SS]`, `2026-09-14 18:00[:SS]`, read as UTC), or a date
/// (`2026-09-14`, read as midnight UTC).
///
/// `occurred_at` is an absolute instant and the archive has no timezone column
/// of its own: an archive records *when* something happened, and a timestamp
/// with an offset says that unambiguously.
pub fn parse_instant(raw: &str) -> Result<DateTime<Utc>, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("timestamp is empty".into());
    }
    if let Ok(absolute) = DateTime::parse_from_rfc3339(value) {
        return Ok(absolute.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(naive.and_utc());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc());
    }
    Err(format!(
        "{value:?} is not a timestamp — use RFC 3339 (2026-09-14T18:00:00Z) or a date (2026-09-14)"
    ))
}

/// Render an instant the way this plugin binds and compares it: RFC 3339 in UTC
/// with whole seconds. PostgreSQL parses this form natively as `timestamptz`.
pub fn render_instant(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The opaque timeline cursor for a record: `<rfc3339>|<id>`.
///
/// Ordering is `(occurred_at DESC, id DESC)`, so a cursor needs both halves —
/// two records filed in the same second still page deterministically.
pub fn cursor_of(record: &Value) -> Option<String> {
    let at = record["occurred_at_rfc3339"].as_str()?;
    let id = record["id"].as_i64()?;
    Some(format!("{at}{CURSOR_SEPARATOR}{id}"))
}

/// Parse a cursor produced by [`cursor_of`].
pub fn parse_cursor(raw: &str) -> Result<(DateTime<Utc>, i64), String> {
    let (at, id) = raw
        .split_once(CURSOR_SEPARATOR)
        .ok_or_else(|| format!("cursor {raw:?} is not <timestamp>{}<id>", CURSOR_SEPARATOR))?;
    let at = DateTime::parse_from_rfc3339(at.trim())
        .map_err(|e| format!("cursor timestamp {at:?}: {e}"))?
        .with_timezone(&Utc);
    let id = id
        .trim()
        .parse::<i64>()
        .map_err(|e| format!("cursor id {id:?}: {e}"))?;
    Ok((at, id))
}

/// Trim a body field, treating blank as absent.
fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Membership in a closed vocabulary, with a message that names the vocabulary.
fn normalize_from(list: &[&str], value: Option<String>, field: &str) -> Result<String, String> {
    match value {
        Some(v) if list.contains(&v.as_str()) => Ok(v),
        Some(v) => Err(format!("{field} {v:?} is not one of {}", list.join(", "))),
        None => Ok(String::new()),
    }
}

fn lower(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_ascii_lowercase())
}

fn normalize_kind(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(format!("kind is required, one of {}", KINDS.join(", ")));
    }
    normalize_from(&KINDS, lower(Some(value.to_string())), "kind")
}

fn normalize_body_code(value: &Option<String>) -> Result<String, String> {
    normalize_from(&BODY_CODES, lower(trimmed(value)), "body_code")
}

fn normalize_outcome(value: &Option<String>) -> Result<String, String> {
    normalize_from(&OUTCOMES, lower(trimmed(value)), "outcome")
}

fn normalize_relation(value: &str) -> Result<String, String> {
    normalize_from(&RELATIONS, lower(Some(value.to_string())), "relation")
}

fn normalize_scope_type(value: &Option<String>) -> Result<String, String> {
    let scope = lower(trimmed(value)).unwrap_or_else(|| SCOPE_TROOP.to_string());
    normalize_from(&SCOPE_TYPES, Some(scope), "scope_type")
}

/// A source slug: lowercase letters, digits, `_`, `-`. Anything else is a caller
/// mistake, not a value to store verbatim.
fn normalize_source(value: &Option<String>) -> Result<String, String> {
    let Some(raw) = trimmed(value) else {
        return Ok(SOURCE_MANUAL.to_string());
    };
    let slug = raw.to_ascii_lowercase();
    if slug.len() > 32
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "source {raw:?} must be a short slug ([a-z0-9_-]{{1,32}})"
        ));
    }
    Ok(slug)
}

/// The scope an authority check runs at. A Lodge record without a Lodge id is
/// troop-wide, not "lodge with no id": the database refuses that shape on write
/// and a read that meets one must not guess a wider or narrower check.
pub fn record_scope(scope_type: &str, scope_id: Option<&str>) -> Scope {
    match (scope_type, scope_id) {
        (SCOPE_LODGE, Some(id)) if !id.trim().is_empty() => Scope::lodge(id),
        _ => Scope::troop(),
    }
}

/// The scope a fetched record claims.
pub fn scope_of(record: &Value) -> Scope {
    record_scope(
        record["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        record["scope_id"].as_str(),
    )
}

/// Validate a `(scope_type, scope_id)` pair from a request body.
fn validate_scope(
    scope_type: &Option<String>,
    scope_id: &Option<String>,
) -> Result<(String, Option<String>), String> {
    let scope_type = normalize_scope_type(scope_type)?;
    let scope_id = trimmed(scope_id);
    if scope_type == SCOPE_LODGE && scope_id.is_none() {
        return Err("a Lodge record needs scope_id (the Lodge it belongs to)".into());
    }
    if scope_type == SCOPE_TROOP && scope_id.is_some() {
        return Err(
            "a troop-wide record carries no scope_id (scope_id is for Lodge records)".into(),
        );
    }
    Ok((scope_type, scope_id))
}

/// Validate a source reference. It is an idempotency key under a unique index, so
/// its shape is constrained rather than trusted: `<plugin>:<entity>:<id>`.
fn validate_source_ref(value: &Option<String>) -> Result<Option<String>, String> {
    let Some(raw) = trimmed(value) else {
        return Ok(None);
    };
    if raw.len() > 200 {
        return Err("source_ref is longer than 200 characters".into());
    }
    if raw.split(':').count() < 3 {
        return Err(format!(
            "source_ref {raw:?} must be <plugin>:<entity>:<id> (e.g. governance:motion:7)"
        ));
    }
    Ok(Some(raw))
}

/// Which PostgreSQL function turns the query text into a `tsquery`.
///
/// The name is returned as a `&'static str` so it can be interpolated into SQL
/// while the *caller's* value only ever picks from a closed set — a mode string
/// never reaches the statement.
fn search_function(mode: &str) -> Result<&'static str, String> {
    let value = mode.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | SEARCH_MODE_WEB => Ok("websearch_to_tsquery"),
        SEARCH_MODE_ALL_WORDS => Ok("plainto_tsquery"),
        other => Err(format!(
            "mode {other:?} is not one of {SEARCH_MODE_WEB}, {SEARCH_MODE_ALL_WORDS}"
        )),
    }
}

fn limit_of(req: &PluginRequest, default: i64) -> i64 {
    req.query_int("limit")
        .unwrap_or(default)
        .clamp(1, MAX_LIMIT)
}

/// A comma-separated `?kind=` filter, validated against the vocabulary.
fn kinds_of(req: &PluginRequest) -> Result<Option<Vec<String>>, SdkError> {
    let Some(raw) = req.query_param("kind") else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for part in raw.split(',') {
        if part.trim().is_empty() {
            continue;
        }
        out.push(normalize_kind(part).map_err(SdkError::BadRequest)?);
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// Predicate building
// ---------------------------------------------------------------------------

/// The visibility half of the scoped-permission rule (SPEC §9.2), as SQL.
///
/// `$1` — the caller holds the read permission troop-wide (sees everything);
/// `$2` — the Lodge ids their grants cover; `$3` — the caller, who always sees
/// what they filed.
const VISIBILITY_CLAUSE: &str =
    "(r.filed_by = $3 OR $1::bool OR r.scope_type = 'troop' OR r.scope_id = ANY($2))";

/// The columns of a record, without the derived ones. Safe in a `RETURNING` list
/// as well as a `SELECT` (the target table is always aliased `r`).
const RECORD_FIELDS: &str = "\
    r.id, r.kind, r.title, r.summary, r.body, r.body_code, r.outcome, \
    r.scope_type, r.scope_id, \
    r.occurred_at::text AS occurred_at, \
    to_char(r.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS occurred_at_rfc3339, \
    r.source, r.source_ref, r.source_url, r.filed_by, r.filed_at::text AS filed_at, \
    r.supersedes_id, r.correction_reason";

/// The derived read-only columns: whether something supersedes this record, and
/// how many edges touch it.
///
/// `superseded_by` is a subquery rather than a column on purpose — the archive
/// never updates a record, so the pointer has to be derived to exist at all.
fn read_fields(c: &PluginContext) -> String {
    format!(
        "{RECORD_FIELDS}, \
         (SELECT c.id FROM {records} c WHERE c.supersedes_id = r.id \
          ORDER BY c.filed_at, c.id LIMIT 1) AS superseded_by, \
         (SELECT count(*) FROM {links} l WHERE l.from_id = r.id) AS links_out, \
         (SELECT count(*) FROM {links} l WHERE l.to_id = r.id) AS links_in",
        records = c.db.table("records"),
        links = c.db.table("links")
    )
}

/// The `superseded_by` expression on its own, for the node list a lineage walk
/// returns (which joins no records and so cannot use [`read_fields`]).
fn superseded_by_expr(c: &PluginContext) -> String {
    format!(
        "(SELECT c.id FROM {records} c WHERE c.supersedes_id = r.id \
         ORDER BY c.filed_at, c.id LIMIT 1) AS superseded_by",
        records = c.db.table("records")
    )
}

/// The `(from, to)` window a list-shaped route applied, as parsed — reported
/// back to the caller so a half-open `to` is never a surprise.
type Window = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// A `WHERE` clause assembled in one place, so the placeholder number and the
/// bind order can never drift apart (each `push_*` appends to both).
struct Predicates {
    clauses: Vec<String>,
    params: Vec<SqlValue>,
}

impl Predicates {
    fn new(troop_wide: bool, lodges: Vec<String>, caller: &str) -> Self {
        Self {
            clauses: vec![VISIBILITY_CLAUSE.to_string()],
            params: vec![
                SqlValue::Bool(troop_wide),
                SqlValue::TextArray(lodges),
                SqlValue::Text(caller.to_string()),
            ],
        }
    }

    fn text_eq(&mut self, column: &str, value: Option<String>) {
        if let Some(value) = value {
            self.params.push(SqlValue::Text(value));
            let placeholder = self.params.len();
            self.clauses.push(format!("{column} = ${placeholder}"));
        }
    }

    fn kind_in(&mut self, kinds: Option<Vec<String>>) {
        if let Some(kinds) = kinds.filter(|k| !k.is_empty()) {
            self.params.push(SqlValue::TextArray(kinds));
            let placeholder = self.params.len();
            self.clauses.push(format!("r.kind = ANY(${placeholder})"));
        }
    }

    fn since(&mut self, instant: Option<DateTime<Utc>>) {
        if let Some(instant) = instant {
            self.params.push(SqlValue::Text(render_instant(instant)));
            let placeholder = self.params.len();
            self.clauses
                .push(format!("r.occurred_at >= ${placeholder}::timestamptz"));
        }
    }

    /// Half-open upper bound: `occurred_at < to`. See the module docs.
    fn before(&mut self, instant: Option<DateTime<Utc>>) {
        if let Some(instant) = instant {
            self.params.push(SqlValue::Text(render_instant(instant)));
            let placeholder = self.params.len();
            self.clauses
                .push(format!("r.occurred_at < ${placeholder}::timestamptz"));
        }
    }

    /// Keyset pagination: strictly older than `(occurred_at, id)`.
    fn older_than(&mut self, cursor: Option<(DateTime<Utc>, i64)>) {
        if let Some((at, id)) = cursor {
            self.params.push(SqlValue::Text(render_instant(at)));
            let at_placeholder = self.params.len();
            self.params.push(SqlValue::Int(id));
            let id_placeholder = self.params.len();
            self.clauses.push(format!(
                "((r.occurred_at, r.id) < (${at_placeholder}::timestamptz, ${id_placeholder}::bigint))"
            ));
        }
    }

    /// Only the live version of each record: nothing supersedes it.
    fn current_only(&mut self, table_records: &str) {
        self.clauses.push(format!(
            "NOT EXISTS (SELECT 1 FROM {table_records} c WHERE c.supersedes_id = r.id)"
        ));
    }

    fn where_sql(&self) -> String {
        format!("WHERE {}", self.clauses.join(" AND "))
    }

    fn next_placeholder(&self) -> usize {
        self.params.len() + 1
    }
}

/// The filters every list-shaped route shares, so `/records`, `/timeline` and
/// `/search` cannot drift into disagreeing about what `?body=` means.
fn apply_common_filters(preds: &mut Predicates, req: &PluginRequest) -> Result<Window, SdkError> {
    preds.kind_in(kinds_of(req)?);
    preds.text_eq(
        "r.body_code",
        match req.query_param("body") {
            Some(raw) => {
                Some(normalize_body_code(&Some(raw.to_string())).map_err(SdkError::BadRequest)?)
            }
            None => None,
        },
    );
    preds.text_eq(
        "r.outcome",
        match req.query_param("outcome") {
            Some(raw) => {
                Some(normalize_outcome(&Some(raw.to_string())).map_err(SdkError::BadRequest)?)
            }
            None => None,
        },
    );
    preds.text_eq(
        "r.scope_type",
        match req.query_param("scope_type") {
            Some(raw) => {
                Some(normalize_scope_type(&Some(raw.to_string())).map_err(SdkError::BadRequest)?)
            }
            None => None,
        },
    );
    preds.text_eq("r.scope_id", req.query_param("scope_id").map(String::from));
    let from = match req.query_param("from") {
        Some(raw) => Some(parse_instant(raw).map_err(SdkError::BadRequest)?),
        None => None,
    };
    let to = match req.query_param("to") {
        Some(raw) => Some(parse_instant(raw).map_err(SdkError::BadRequest)?),
        None => None,
    };
    preds.since(from);
    preds.before(to);
    Ok((from, to))
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RecordBody {
    kind: String,
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_code: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    scope_type: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    /// When the archived thing happened. Defaults to now.
    #[serde(default)]
    occurred_at: Option<String>,
    #[serde(default)]
    source: Option<String>,
    /// The idempotency key, `<plugin>:<entity>:<id>`.
    #[serde(default)]
    source_ref: Option<String>,
    /// Where the source lives (a route in another plugin, a document reference).
    #[serde(default)]
    source_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CorrectionsBody {
    /// Why the original is being corrected. Required: an unexplained correction
    /// is indistinguishable from an edit.
    reason: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_code: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    occurred_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LinkBody {
    to_id: i64,
    relation: String,
    #[serde(default)]
    note: Option<String>,
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct ArchivePlugin {
    ctx: OnceLock<PluginContext>,
}

impl ArchivePlugin {
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

impl Default for ArchivePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for ArchivePlugin {
    fn id(&self) -> &str {
        "archive"
    }

    fn name(&self) -> &str {
        "Archive"
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
                "archive:read",
                "Read archived records, the timeline, correction chains and lineage",
            ),
            Permission::new(
                "archive:search",
                "Search the archive (full-text; scoped to the grants held)",
            ),
            Permission::new(
                "archive:write",
                "File a record into the archive, or file a correction that supersedes one",
            ),
            Permission::new(
                "archive:link",
                "Draw a relationship between two records (decision → policy → mission → impact)",
            ),
            Permission::new(
                "archive:manage",
                "Administer the archive: remove a mis-filed relationship",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![
            Migration::new(1, "archive_schema", MIGRATION_SCHEMA),
            Migration::new(2, "archive_append_only", MIGRATION_APPEND_ONLY),
        ]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- filing ----------------------------------------------------------
        // Object-shaped: the caller needs `archive:write` at *some* scope and the
        // handler checks the record's own scope, so a Lodge secretary can file
        // Lodge minutes without being able to file Congress proceedings.
        let c = ctx.clone();
        let file = RouteDefinition::post_protected_any_scope(
            "/api/archive/record",
            "archive:write",
            route_handler(move |req| {
                let c = c.clone();
                async move { file_record(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let supersede = RouteDefinition::post_protected_any_scope(
            "/api/archive/record/{id}/supersede",
            "archive:write",
            route_handler(move |req| {
                let c = c.clone();
                async move { supersede_record(&c, req).await }
            }),
        );

        // --- reading ---------------------------------------------------------
        let c = ctx.clone();
        let list = RouteDefinition::get_protected_any_scope(
            "/api/archive/records",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { list_records(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let detail = RouteDefinition::get_protected_any_scope(
            "/api/archive/record/{id}",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { record_detail(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let chain = RouteDefinition::get_protected_any_scope(
            "/api/archive/record/{id}/chain",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { record_chain(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let timeline = RouteDefinition::get_protected_any_scope(
            "/api/archive/timeline",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { timeline(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let stats = RouteDefinition::get_protected_any_scope(
            "/api/archive/stats",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { archive_stats(&c, req).await }
            }),
        );

        // Reference data: no database, but it is still the archive's, so it is
        // still gated — an open vocabulary is a free map of the troop.
        let c = ctx.clone();
        let vocabulary = RouteDefinition::get_protected_any_scope(
            "/api/archive/vocabulary",
            "archive:read",
            route_handler(move |_req| {
                let _c = c.clone();
                async move { vocabulary_response() }
            }),
        );

        // --- search ----------------------------------------------------------
        // Its own permission: a troop can open search to every member and keep
        // the chronological view to the archive keeper, or the other way round.
        let c = ctx.clone();
        let search = RouteDefinition::get_protected_any_scope(
            "/api/archive/search",
            "archive:search",
            route_handler(move |req| {
                let c = c.clone();
                async move { search_records(&c, req).await }
            }),
        );

        // --- relationships ---------------------------------------------------
        let c = ctx.clone();
        let link = RouteDefinition::post_protected_any_scope(
            "/api/archive/record/{id}/link",
            "archive:link",
            route_handler(move |req| {
                let c = c.clone();
                async move { create_link(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let links = RouteDefinition::get_protected_any_scope(
            "/api/archive/record/{id}/links",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { list_links(&c, req).await }
            }),
        );

        let c = ctx.clone();
        let lineage = RouteDefinition::get_protected_any_scope(
            "/api/archive/record/{id}/lineage",
            "archive:read",
            route_handler(move |req| {
                let c = c.clone();
                async move { lineage(&c, req).await }
            }),
        );

        // Unlinking is destructive and belongs to `archive:manage`; the SDK's
        // delete constructor makes it troop-covering even though the route is
        // declared `any_scope` (SPEC §8 #3), so a Lodge grant cannot erase a
        // relationship the troop drew.
        let c = ctx.clone();
        let unlink = RouteDefinition::delete_protected_any_scope(
            "/api/archive/link/{id}",
            "archive:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move { delete_link(&c, req).await }
            }),
        );

        vec![
            file, supersede, list, detail, chain, timeline, stats, vocabulary, search, link, links,
            lineage, unlink,
        ]
    }

    fn subscriptions(&self) -> Vec<EventSubscription> {
        let ctx = self.ctx().clone();

        // `motion.` rather than the three exact types: one subscription covers
        // proposed/passed/failed, and a future `motion.amended` lands in a
        // handler that already knows how to file a motion.
        let c = ctx.clone();
        let motions = EventSubscription::new(
            "motion.",
            event_handler(move |ev| {
                let c = c.clone();
                async move { ingest_motion(&c, &ev).await }
            }),
        );

        let c = ctx.clone();
        let accords = EventSubscription::new(
            "accords.",
            event_handler(move |ev| {
                let c = c.clone();
                async move { ingest_accords(&c, &ev).await }
            }),
        );

        let c = ctx.clone();
        let missions = EventSubscription::new(
            "mission.",
            event_handler(move |ev| {
                let c = c.clone();
                async move { ingest_mission(&c, &ev).await }
            }),
        );

        vec![motions, accords, missions]
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// The tables. `records` is SPEC §7.8's; `links` is the relationship graph
/// underneath its `decisions → policies → missions → impact`.
///
/// The `search` column is **generated and weighted**: a title match outranks a
/// summary match, which outranks a body match (`ts_rank_cd` reads the weights).
/// `to_tsvector('english', …)` is the two-argument form on purpose — it is
/// IMMUTABLE, which a generated column requires, while the one-argument form is
/// merely STABLE and PostgreSQL refuses the column.
const MIGRATION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS records (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    body_code TEXT NOT NULL DEFAULT '',
    outcome TEXT NOT NULL DEFAULT '',
    scope_type TEXT NOT NULL DEFAULT 'troop',
    scope_id TEXT,
    occurred_at TIMESTAMPTZ NOT NULL,
    source TEXT NOT NULL DEFAULT 'manual',
    source_ref TEXT,
    source_url TEXT NOT NULL DEFAULT '',
    filed_by TEXT NOT NULL,
    filed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    supersedes_id BIGINT REFERENCES records(id) ON DELETE SET NULL,
    correction_reason TEXT NOT NULL DEFAULT '',
    search TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', title), 'A') ||
        setweight(to_tsvector('english', summary), 'B') ||
        setweight(to_tsvector('english', body), 'D')
    ) STORED,
    CONSTRAINT records_kind_valid CHECK (kind IN (
        'congress_proceeding', 'minutes', 'motion', 'decision', 'policy',
        'mission_report', 'impact', 'correspondence', 'note')),
    CONSTRAINT records_body_code_valid CHECK (
        body_code IN ('', 'congress', 'tc', 'lodge', 'committee')),
    CONSTRAINT records_outcome_valid CHECK (
        outcome IN ('', 'passed', 'failed', 'withdrawn', 'adopted')),
    CONSTRAINT records_scope_type_valid CHECK (scope_type IN ('troop', 'lodge')),
    CONSTRAINT records_scope_id_present CHECK (
        (scope_type = 'troop' AND scope_id IS NULL) OR
        (scope_type = 'lodge' AND scope_id IS NOT NULL AND scope_id <> '')
    ),
    CONSTRAINT records_title_present CHECK (btrim(title) <> ''),
    CONSTRAINT records_no_self_supersede CHECK (supersedes_id IS NULL OR supersedes_id <> id)
);
CREATE INDEX IF NOT EXISTS idx_records_search ON records USING GIN (search);
CREATE INDEX IF NOT EXISTS idx_records_timeline ON records (occurred_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_records_kind ON records (kind, occurred_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_records_scope ON records (scope_type, scope_id);
CREATE INDEX IF NOT EXISTS idx_records_body_code ON records (body_code);
CREATE INDEX IF NOT EXISTS idx_records_supersedes ON records (supersedes_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_records_source_ref ON records (source_ref);
CREATE TABLE IF NOT EXISTS links (
    id BIGSERIAL PRIMARY KEY,
    from_id BIGINT NOT NULL REFERENCES records(id) ON DELETE CASCADE,
    to_id BIGINT NOT NULL REFERENCES records(id) ON DELETE CASCADE,
    relation TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT links_relation_valid CHECK (relation IN (
        'decides', 'authorizes', 'produces', 'outcome', 'amends', 'documents', 'relates_to')),
    CONSTRAINT links_not_self CHECK (from_id <> to_id),
    CONSTRAINT links_unique UNIQUE (from_id, to_id, relation)
);
CREATE INDEX IF NOT EXISTS idx_links_from ON links (from_id);
CREATE INDEX IF NOT EXISTS idx_links_to ON links (to_id);
CREATE INDEX IF NOT EXISTS idx_links_relation ON links (relation);
"#;

/// The append-only guarantee, at the layer that cannot be argued with.
///
/// The trigger refuses every UPDATE and every DELETE on `records`. A correction
/// is an INSERT with `supersedes_id` set, so nothing legitimate needs either verb
/// — which is exactly why this can be unconditional instead of a list of
/// protected columns that a later migration has to remember to extend.
const MIGRATION_APPEND_ONLY: &str = r#"
CREATE OR REPLACE FUNCTION archive_records_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'archive.records is append-only (SPEC 7.8): record % cannot be deleted. Retire it with a correction that supersedes it (POST /api/archive/record/%/supersede).', OLD.id, OLD.id;
    END IF;
    RAISE EXCEPTION 'archive.records is append-only (SPEC 7.8): record % cannot be modified. File a correction that supersedes it (POST /api/archive/record/%/supersede).', OLD.id, OLD.id;
END;
$$;
CREATE OR REPLACE TRIGGER records_append_only
    BEFORE UPDATE OR DELETE ON records
    FOR EACH ROW EXECUTE FUNCTION archive_records_append_only();
"#;

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// The `(troop-wide, lodge ids)` the caller covers — the visibility half of the
/// scoped-permission rule. Management is checked separately, per object.
async fn visibility(
    c: &PluginContext,
    identity: Option<&Identity>,
    permission: &str,
) -> (bool, Vec<String>) {
    let troop_wide = c
        .permissions
        .has_in_scope(identity, permission, &Scope::troop())
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

/// May this caller read this record?
///
/// A troop-wide grant sees everything. A troop-wide *record* is the troop's, so
/// anyone who reached the route (which already required the permission at some
/// scope) sees it. A Lodge record needs a grant covering that Lodge. And the
/// author always sees what they filed — including a grant that has since been
/// narrowed.
async fn can_read(
    c: &PluginContext,
    identity: Option<&Identity>,
    permission: &str,
    record: &Value,
) -> bool {
    let Some(identity) = identity else {
        return false;
    };
    if c.permissions
        .has_in_scope(Some(identity), permission, &Scope::troop())
        .await
    {
        return true;
    }
    if record["scope_type"].as_str().unwrap_or(SCOPE_TROOP) == SCOPE_TROOP {
        return true;
    }
    if c.permissions
        .has_in_scope(Some(identity), permission, &scope_of(record))
        .await
    {
        return true;
    }
    record["filed_by"].as_str() == Some(identity.user_id.as_str())
}

/// Fetch the record a route is about, or the answer that leaks nothing.
///
/// Absent and forbidden are the same 403: a caller who cannot see a record must
/// not learn from the status code that it exists.
async fn fetch_visible(
    c: &PluginContext,
    identity: Option<&Identity>,
    permission: &str,
    id: i64,
) -> Result<Value, SdkError> {
    let gone = || SdkError::Forbidden(format!("no record {id}, or you cannot see it"));
    let Some(record) = fetch_record(c, id).await? else {
        return Err(gone());
    };
    if !can_read(c, identity, permission, &record).await {
        return Err(gone());
    }
    Ok(record)
}

async fn fetch_record(c: &PluginContext, id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {} FROM {} r WHERE r.id = $1",
            read_fields(c),
            c.db.table("records")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// The id of the record that supersedes `id` first, if any.
async fn find_superseder(c: &PluginContext, id: i64) -> Result<Option<i64>, SdkError> {
    let row =
        c.db.query_one(
            format!(
                "SELECT c.id FROM {records} c WHERE c.supersedes_id = $1 \
                 ORDER BY c.filed_at, c.id LIMIT 1",
                records = c.db.table("records")
            ),
            vec![SqlValue::Int(id)],
        )
        .await?;
    Ok(row.and_then(|r| r["id"].as_i64()))
}

// ---------------------------------------------------------------------------
// Filing
// ---------------------------------------------------------------------------

/// The shared write: one INSERT whose conflict target is the idempotency key.
///
/// Returns `Ok(None)` when the row was already there — the caller decides whether
/// that is a quiet no-op (ingestion) or a 409 (a human filing the same reference
/// twice).
struct NewRecord {
    kind: String,
    title: String,
    summary: String,
    body: String,
    body_code: String,
    outcome: String,
    scope_type: String,
    scope_id: Option<String>,
    occurred_at: DateTime<Utc>,
    source: String,
    source_ref: Option<String>,
    source_url: String,
    filed_by: String,
    supersedes_id: Option<i64>,
    correction_reason: String,
}

async fn insert_record(c: &PluginContext, new: &NewRecord) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "INSERT INTO {records} AS r \
               (kind, title, summary, body, body_code, outcome, scope_type, scope_id, \
                occurred_at, source, source_ref, source_url, filed_by, supersedes_id, \
                correction_reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz, $10, $11, $12, $13, \
                     $14, $15) \
             ON CONFLICT (source_ref) DO NOTHING \
             RETURNING {fields}",
            records = c.db.table("records"),
            fields = RECORD_FIELDS
        ),
        vec![
            SqlValue::Text(new.kind.clone()),
            SqlValue::Text(new.title.clone()),
            SqlValue::Text(new.summary.clone()),
            SqlValue::Text(new.body.clone()),
            SqlValue::Text(new.body_code.clone()),
            SqlValue::Text(new.outcome.clone()),
            SqlValue::Text(new.scope_type.clone()),
            new.scope_id.clone().into(),
            SqlValue::Text(render_instant(new.occurred_at)),
            SqlValue::Text(new.source.clone()),
            new.source_ref.clone().into(),
            SqlValue::Text(new.source_url.clone()),
            SqlValue::Text(new.filed_by.clone()),
            new.supersedes_id
                .map(SqlValue::Int)
                .unwrap_or(SqlValue::NullInt),
            SqlValue::Text(new.correction_reason.clone()),
        ],
    )
    .await
}

/// `POST /api/archive/record` — file a record.
async fn file_record(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let body: RecordBody = req.json()?;
    let kind = normalize_kind(&body.kind).map_err(SdkError::BadRequest)?;
    if body.title.trim().is_empty() {
        return Err(SdkError::BadRequest("title is required".into()));
    }
    let (scope_type, scope_id) =
        validate_scope(&body.scope_type, &body.scope_id).map_err(SdkError::BadRequest)?;
    let body_code = normalize_body_code(&body.body_code).map_err(SdkError::BadRequest)?;
    let outcome = normalize_outcome(&body.outcome).map_err(SdkError::BadRequest)?;
    let source = normalize_source(&body.source).map_err(SdkError::BadRequest)?;
    let source_ref = validate_source_ref(&body.source_ref).map_err(SdkError::BadRequest)?;
    let occurred_at = match trimmed(&body.occurred_at) {
        Some(raw) => parse_instant(&raw).map_err(SdkError::BadRequest)?,
        None => Utc::now(),
    };
    // A body code is a governing body, so it is troop-wide: a Congress, a Troop
    // Council or a committee is the troop's, never one Lodge's (SPEC §7.4's
    // bodies, and calendar's rule for the events of the same meetings).
    if !body_code.is_empty() && scope_type != SCOPE_TROOP {
        return Err(SdkError::BadRequest(format!(
            "a {body_code} record is troop-wide: it cannot be scoped to a Lodge"
        )));
    }
    let scope = record_scope(&scope_type, scope_id.as_deref());
    c.permissions
        .reach(req.identity.as_ref(), "archive:write", &scope)
        .await?;

    let filer = req
        .identity
        .as_ref()
        .map(|i| i.user_id.clone())
        .unwrap_or_default();
    let new = NewRecord {
        kind: kind.clone(),
        title: body.title.trim().to_string(),
        summary: trimmed(&body.summary).unwrap_or_default(),
        body: body.body.clone().unwrap_or_default(),
        body_code: body_code.clone(),
        outcome: outcome.clone(),
        scope_type: scope_type.clone(),
        scope_id: scope_id.clone(),
        occurred_at,
        source: source.clone(),
        source_ref: source_ref.clone(),
        source_url: trimmed(&body.source_url).unwrap_or_default(),
        filed_by: filer.clone(),
        supersedes_id: None,
        correction_reason: String::new(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        return Err(SdkError::Conflict(format!(
            "a record with source_ref {source_ref:?} is already filed — the reference is an \
             idempotency key (GET /api/archive/records?source_ref=… to find it)"
        )));
    };
    let id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            req.identity.as_ref(),
            "record.file",
            "record",
            &id.to_string(),
            json!({
                "kind": kind,
                "title": new.title,
                "body_code": body_code,
                "scope_type": scope_type,
                "scope_id": scope_id,
                "source": source,
                "source_ref": source_ref,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.filed",
            json!({
                "record_id": id,
                "kind": kind,
                "title": row["title"],
                "body_code": row["body_code"],
                "outcome": row["outcome"],
                "scope_type": row["scope_type"],
                "scope_id": row["scope_id"],
                "occurred_at": row["occurred_at_rfc3339"],
                "source": source,
                "filed_by": filer,
            }),
        )
        .await?;
    PluginResponse::created(
        &format!("/api/archive/record/{id}"),
        &json!({
            "record": row,
            "immutable": "a filed record is never edited — POST /api/archive/record/{id}/supersede \
                          to correct it",
            "next": format!("POST /api/archive/record/{id}/link to tie it into the chain"),
        }),
    )
}

/// `POST /api/archive/record/{id}/supersede` — file a correction.
///
/// The original is never touched (the database would refuse); the correction is a
/// new record carrying `supersedes_id` and the reason. Correcting an already
/// corrected record is refused with a pointer at the current version, so the
/// chain stays a chain and "which version is current?" always has one answer.
async fn supersede_record(
    c: &PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: CorrectionsBody = req.json()?;
    if body.reason.trim().is_empty() {
        return Err(SdkError::BadRequest(
            "reason is required — a correction without a stated reason is an edit".into(),
        ));
    }
    let original = fetch_visible(c, req.identity.as_ref(), "archive:write", id).await?;
    c.permissions
        .reach(req.identity.as_ref(), "archive:write", &scope_of(&original))
        .await?;
    if let Some(current) = find_superseder(c, id).await? {
        return Err(SdkError::Conflict(format!(
            "record {id} was already corrected by record {current} — correct that one instead \
             (POST /api/archive/record/{current}/supersede)"
        )));
    }

    let kind = match trimmed(&body.kind) {
        Some(raw) => normalize_kind(&raw).map_err(SdkError::BadRequest)?,
        None => original["kind"].as_str().unwrap_or_default().to_string(),
    };
    let body_code = match &body.body_code {
        Some(_) => normalize_body_code(&body.body_code).map_err(SdkError::BadRequest)?,
        None => original["body_code"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    };
    let outcome = match &body.outcome {
        Some(_) => normalize_outcome(&body.outcome).map_err(SdkError::BadRequest)?,
        None => original["outcome"].as_str().unwrap_or_default().to_string(),
    };
    // The correction keeps the original's place in the troop's history unless the
    // caller moves it: a minutes entry corrected for a typo still happened on the
    // day it happened.
    let occurred_at = match trimmed(&body.occurred_at) {
        Some(raw) => parse_instant(&raw).map_err(SdkError::BadRequest)?,
        None => match original["occurred_at_rfc3339"].as_str() {
            Some(raw) => parse_instant(raw).map_err(SdkError::Internal)?,
            None => Utc::now(),
        },
    };
    let title = trimmed(&body.title)
        .unwrap_or_else(|| original["title"].as_str().unwrap_or_default().to_string());

    let filer = req
        .identity
        .as_ref()
        .map(|i| i.user_id.clone())
        .unwrap_or_default();
    let reason = body.reason.trim().to_string();
    let new = NewRecord {
        kind: kind.clone(),
        title: title.clone(),
        summary: trimmed(&body.summary).unwrap_or_default(),
        body: body.body.clone().unwrap_or_default(),
        body_code,
        outcome,
        scope_type: original["scope_type"]
            .as_str()
            .unwrap_or(SCOPE_TROOP)
            .to_string(),
        scope_id: original["scope_id"].as_str().map(String::from),
        occurred_at,
        source: SOURCE_CORRECTION.to_string(),
        // A correction has no idempotency key: two corrections of the same
        // original are two different acts, and the second is refused above.
        source_ref: None,
        source_url: original["source_url"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        filed_by: filer.clone(),
        supersedes_id: Some(id),
        correction_reason: reason.clone(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        // Unreachable while `source_ref` is NULL (a unique index does not
        // constrain NULLs), but an INSERT that returns no row must not be
        // reported as a success.
        return Err(SdkError::Internal(
            "the correction insert returned no row".into(),
        ));
    };
    let correction_id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            req.identity.as_ref(),
            "record.supersede",
            "record",
            &id.to_string(),
            json!({
                "correction_id": correction_id,
                "reason": reason,
                "kind": kind,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.superseded",
            json!({
                "record_id": id,
                "superseded_by": correction_id,
                "reason": reason,
                "original_title": original["title"],
                "correction_title": title,
                "filed_by": filer,
            }),
        )
        .await?;
    PluginResponse::created(
        &format!("/api/archive/record/{correction_id}"),
        &json!({
            "record": row,
            "supersedes": original,
            "original_preserved": true,
            "next": format!(
                "GET /api/archive/record/{id}/chain to read the whole correction history"
            ),
        }),
    )
}

/// Draw one edge. Idempotent: a repeated link is a no-op, not a duplicate.
async fn link_records(
    c: &PluginContext,
    from_id: i64,
    to_id: i64,
    relation: &str,
    note: &str,
    actor: &str,
) -> Result<u64, SdkError> {
    c.db.execute(
        format!(
            "INSERT INTO {links} (from_id, to_id, relation, note, created_by) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (from_id, to_id, relation) DO NOTHING",
            links = c.db.table("links")
        ),
        vec![
            SqlValue::Int(from_id),
            SqlValue::Int(to_id),
            SqlValue::Text(relation.to_string()),
            SqlValue::Text(note.to_string()),
            SqlValue::Text(actor.to_string()),
        ],
    )
    .await
}

/// Find a record by its idempotency key.
async fn find_by_source_ref(c: &PluginContext, source_ref: &str) -> Result<Option<i64>, SdkError> {
    let row =
        c.db.query_one(
            format!(
                "SELECT r.id FROM {records} r WHERE r.source_ref = $1",
                records = c.db.table("records")
            ),
            vec![SqlValue::Text(source_ref.to_string())],
        )
        .await?;
    Ok(row.and_then(|r| r["id"].as_i64()))
}

/// Link `to_id` from the record an ingested event referred to, if that record is
/// in the archive.
///
/// The parent usually arrives first (a motion is proposed, then decided), but
/// replay and late installation mean it may not be — a missing parent is a
/// missing edge, never an error, and never a reason to drop the outcome.
async fn link_by_source_ref(
    c: &PluginContext,
    from_source_ref: &str,
    to_id: i64,
    relation: &str,
    note: &str,
) -> Result<bool, SdkError> {
    let Some(from_id) = find_by_source_ref(c, from_source_ref).await? else {
        return Ok(false);
    };
    link_records(c, from_id, to_id, relation, note, INGEST_ACTOR).await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Event ingestion
// ---------------------------------------------------------------------------

/// `motion.` — a proposal becomes a `motion` record, its outcome a `decision`.
async fn ingest_motion(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    if ev.event_type == event_type::MOTION_PASSED {
        return ingest_motion_outcome(c, ev, OUTCOME_PASSED).await;
    }
    if ev.event_type == event_type::MOTION_FAILED {
        return ingest_motion_outcome(c, ev, OUTCOME_FAILED).await;
    }
    if ev.event_type == event_type::MOTION_PROPOSED {
        return ingest_motion_proposed(c, ev).await;
    }
    // `motion.` is a prefix filter, so anything else governance publishes under
    // it lands here. Nothing to file yet — a silent no-op is the correct answer,
    // not a BadRequest.
    Ok(())
}

async fn ingest_motion_proposed(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    let Some(motion_id) = ev.payload["motion_id"].as_i64() else {
        return Err(SdkError::BadRequest(
            "motion.proposed payload has no motion_id".into(),
        ));
    };
    let title = ev.payload["title"].as_str().unwrap_or_default().to_string();
    let body_code = ev.payload["body"].as_str().unwrap_or_default().to_string();
    let meeting_id = ev.payload["meeting_id"].as_i64();
    let proposed_by = ev.payload["proposed_by"].as_str().unwrap_or_default();
    let new = NewRecord {
        kind: KIND_MOTION.to_string(),
        title: if title.is_empty() {
            format!("Motion {motion_id}")
        } else {
            title
        },
        summary: format!("Proposed in {}", body_label(&body_code)),
        body: format!(
            "Motion {motion_id} was proposed in {}. The motion's own text lives with the motion: \
             /api/governance/motion/{motion_id}.\n\nProposed by: {proposed_by}\nMeeting: {}",
            body_label(&body_code),
            meeting_id
                .map(|m| m.to_string())
                .unwrap_or_else(|| "none recorded".to_string())
        ),
        body_code: body_code.clone(),
        outcome: String::new(),
        scope_type: SCOPE_TROOP.to_string(),
        scope_id: None,
        occurred_at: ev.timestamp,
        source: SOURCE_GOVERNANCE.to_string(),
        source_ref: Some(format!("governance:motion:{motion_id}")),
        source_url: format!("/api/governance/motion/{motion_id}"),
        filed_by: INGEST_ACTOR.to_string(),
        supersedes_id: None,
        correction_reason: String::new(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        return Ok(()); // already filed — a replayed event is a no-op
    };
    let id = row["id"].as_i64().unwrap_or_default();
    c.audit
        .log(
            None,
            "record.ingest.motion",
            "record",
            &id.to_string(),
            json!({
                "motion_id": motion_id,
                "source": ev.source,
                "event_id": ev.id,
                "body_code": body_code,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.filed",
            json!({
                "record_id": id,
                "kind": KIND_MOTION,
                "title": row["title"],
                "body_code": row["body_code"],
                "occurred_at": row["occurred_at_rfc3339"],
                "source": SOURCE_GOVERNANCE,
                "source_ref": row["source_ref"],
            }),
        )
        .await?;
    Ok(())
}

async fn ingest_motion_outcome(
    c: &PluginContext,
    ev: &Event,
    outcome: &str,
) -> Result<(), SdkError> {
    // The two typed payloads carry the same tally under different field names
    // (`passed_at` / `failed_at`) — which is the point of having two types, so a
    // consumer never reads a `passed_at` on a motion that failed.
    let (motion_id, title, body_code, meeting_id, yes, no, abstain, threshold, at) =
        if outcome == OUTCOME_PASSED {
            let parsed: MotionPassed = serde_json::from_value(ev.payload.clone()).map_err(|e| {
                SdkError::BadRequest(format!("motion.passed does not match MotionPassed: {e}"))
            })?;
            (
                parsed.motion_id,
                parsed.title,
                parsed.body,
                parsed.meeting_id,
                parsed.votes_yes,
                parsed.votes_no,
                parsed.votes_abstain,
                parsed.threshold,
                parsed.passed_at,
            )
        } else {
            let parsed: MotionFailed = serde_json::from_value(ev.payload.clone()).map_err(|e| {
                SdkError::BadRequest(format!("motion.failed does not match MotionFailed: {e}"))
            })?;
            (
                parsed.motion_id,
                parsed.title,
                parsed.body,
                parsed.meeting_id,
                parsed.votes_yes,
                parsed.votes_no,
                parsed.votes_abstain,
                parsed.threshold,
                parsed.failed_at,
            )
        };
    let verb = if outcome == OUTCOME_PASSED {
        "Passed"
    } else {
        "Failed"
    };
    let body = format!(
        "{verb} by a vote of {yes} yes, {no} no, {abstain} abstaining, against a threshold of \
         {threshold}, in {}.\n\nThe motion and its votes: /api/governance/motion/{motion_id}\n\
         Meeting: {}",
        body_label(&body_code),
        meeting_id
            .map(|m| m.to_string())
            .unwrap_or_else(|| "none recorded".to_string())
    );
    let new = NewRecord {
        kind: KIND_DECISION.to_string(),
        title: if title.is_empty() {
            format!("Decision on motion {motion_id}")
        } else {
            title
        },
        summary: format!(
            "{verb} {yes}-{no} ({abstain} abstaining, {threshold}) in {}",
            body_label(&body_code)
        ),
        body,
        body_code: body_code.clone(),
        outcome: outcome.to_string(),
        scope_type: SCOPE_TROOP.to_string(),
        scope_id: None,
        occurred_at: at,
        source: SOURCE_GOVERNANCE.to_string(),
        source_ref: Some(format!("governance:motion:{motion_id}:outcome")),
        source_url: format!("/api/governance/motion/{motion_id}"),
        filed_by: INGEST_ACTOR.to_string(),
        supersedes_id: None,
        correction_reason: String::new(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        return Ok(());
    };
    let id = row["id"].as_i64().unwrap_or_default();
    // A decision is not an edit of the motion it decides. This is the whole
    // reason the chain exists: the motion record says what was proposed, and this
    // record says what the troop did about it.
    let linked = link_by_source_ref(
        c,
        &format!("governance:motion:{motion_id}"),
        id,
        RELATION_OUTCOME,
        "",
    )
    .await?;
    c.audit
        .log(
            None,
            "record.ingest.decision",
            "record",
            &id.to_string(),
            json!({
                "motion_id": motion_id,
                "outcome": outcome,
                "linked_to_motion": linked,
                "source": ev.source,
                "event_id": ev.id,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.filed",
            json!({
                "record_id": id,
                "kind": KIND_DECISION,
                "title": row["title"],
                "outcome": outcome,
                "body_code": row["body_code"],
                "occurred_at": row["occurred_at_rfc3339"],
                "source": SOURCE_GOVERNANCE,
                "source_ref": row["source_ref"],
                "linked_to_motion": linked,
            }),
        )
        .await?;
    Ok(())
}

/// `accords.adopted` — each adopted version is a `policy` record, and the chain
/// of versions is an `amends` edge.
async fn ingest_accords(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    if ev.event_type != "accords.adopted" {
        return Ok(());
    }
    let Some(version) = ev.payload["version"].as_i64() else {
        return Err(SdkError::BadRequest(
            "accords.adopted payload has no version".into(),
        ));
    };
    let title = ev.payload["title"]
        .as_str()
        .filter(|t| !t.is_empty())
        .unwrap_or("The Accords")
        .to_string();
    let motion_id = ev.payload["motion_id"].as_i64();
    let adopted_on = ev.payload["adopted_on"].as_str().unwrap_or_default();
    let adopted_by = ev.payload["adopted_by"].as_str().unwrap_or_default();
    let new = NewRecord {
        kind: KIND_POLICY.to_string(),
        title: format!("{title} — version {version}"),
        summary: format!("Adopted {adopted_on}"),
        body: format!(
            "Version {version} of {title} was adopted on {adopted_on} by {adopted_by}, from \
             motion {}.\n\nThe version's text: /api/governance/accords/{version}\n\nThe Accords \
             are the troop's constitution: every policy below them is read in their light.",
            motion_id
                .map(|m| m.to_string())
                .unwrap_or_else(|| "not recorded".to_string())
        ),
        body_code: BODY_CONGRESS.to_string(),
        outcome: OUTCOME_ADOPTED.to_string(),
        scope_type: SCOPE_TROOP.to_string(),
        scope_id: None,
        occurred_at: match parse_instant(adopted_on) {
            Ok(instant) => instant,
            // A date-less payload is a publisher bug. The record's own timestamp
            // is the honest fallback — losing the record over it is not.
            Err(_) => ev.timestamp,
        },
        source: SOURCE_GOVERNANCE.to_string(),
        source_ref: Some(format!("governance:accords:{version}")),
        source_url: format!("/api/governance/accords/{version}"),
        filed_by: INGEST_ACTOR.to_string(),
        supersedes_id: None,
        correction_reason: String::new(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        return Ok(());
    };
    let id = row["id"].as_i64().unwrap_or_default();
    // Version N amends version N-1. The previous version may never have been
    // archived — the plugin can be installed long after the Accords were written
    // — and a missing parent is a missing edge, not an error.
    let mut amended = false;
    if version > 1 {
        amended = link_by_source_ref(
            c,
            &format!("governance:accords:{}", version - 1),
            id,
            RELATION_AMENDS,
            "",
        )
        .await?;
    }
    c.audit
        .log(
            None,
            "record.ingest.accords",
            "record",
            &id.to_string(),
            json!({
                "version": version,
                "motion_id": motion_id,
                "amends_previous": amended,
                "source": ev.source,
                "event_id": ev.id,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.filed",
            json!({
                "record_id": id,
                "kind": KIND_POLICY,
                "title": row["title"],
                "outcome": OUTCOME_ADOPTED,
                "occurred_at": row["occurred_at_rfc3339"],
                "source": SOURCE_GOVERNANCE,
                "source_ref": row["source_ref"],
                "amends_previous": amended,
            }),
        )
        .await?;
    Ok(())
}

/// `mission.completed` — the mission report, and its impact as the end of the
/// chain.
async fn ingest_mission(c: &PluginContext, ev: &Event) -> Result<(), SdkError> {
    if ev.event_type != event_type::MISSION_COMPLETED {
        return Ok(());
    }
    let mission: MissionCompleted = serde_json::from_value(ev.payload.clone()).map_err(|e| {
        SdkError::BadRequest(format!(
            "mission.completed payload does not match MissionCompleted: {e}"
        ))
    })?;
    let lodge = mission
        .lodge_id
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let (scope_type, scope_id) = match lodge {
        Some(lodge) => (SCOPE_LODGE, Some(lodge.to_string())),
        None => (SCOPE_TROOP, None),
    };
    let hours = mission.impact["service_hours"].as_f64();
    let participants = mission.impact["participant_count"].as_i64();
    let summary_text = mission.impact["summary"].as_str().unwrap_or_default();
    let new = NewRecord {
        kind: KIND_MISSION_REPORT.to_string(),
        title: mission.title.clone(),
        summary: summary_text.to_string(),
        body: mission_report_body(&mission, hours, participants),
        body_code: String::new(),
        outcome: String::new(),
        scope_type: scope_type.to_string(),
        scope_id: scope_id.clone(),
        occurred_at: mission.completed_at,
        source: SOURCE_MISSIONS.to_string(),
        source_ref: Some(format!("missions:mission:{}", mission.mission_id)),
        source_url: format!("/api/missions/mission/{}", mission.mission_id),
        filed_by: INGEST_ACTOR.to_string(),
        supersedes_id: None,
        correction_reason: String::new(),
    };
    let Some(row) = insert_record(c, &new).await? else {
        return Ok(());
    };
    let report_id = row["id"].as_i64().unwrap_or_default();

    // The impact is its own record, linked `produces` from the report, because
    // the SPEC's question is "what did this decision actually produce?" — an
    // answer that has to be dug out of a JSON blob inside a report is not an
    // answer. An empty impact object is not worth a record.
    let mut impact_id: Option<i64> = None;
    let has_impact = mission
        .impact
        .as_object()
        .is_some_and(|fields| !fields.is_empty());
    if has_impact {
        let impact = NewRecord {
            kind: KIND_IMPACT.to_string(),
            title: format!("Impact — {}", mission.title),
            summary: impact_line(hours, participants),
            body: serde_json::to_string_pretty(&mission.impact).unwrap_or_else(|_| "{}".into()),
            body_code: String::new(),
            outcome: String::new(),
            scope_type: scope_type.to_string(),
            scope_id: scope_id.clone(),
            occurred_at: mission.completed_at,
            source: SOURCE_MISSIONS.to_string(),
            source_ref: Some(format!("missions:mission:{}:impact", mission.mission_id)),
            source_url: format!("/api/missions/mission/{}", mission.mission_id),
            filed_by: INGEST_ACTOR.to_string(),
            supersedes_id: None,
            correction_reason: String::new(),
        };
        if let Some(impact_row) = insert_record(c, &impact).await? {
            let id = impact_row["id"].as_i64().unwrap_or_default();
            impact_id = Some(id);
            link_records(c, report_id, id, RELATION_PRODUCES, "", INGEST_ACTOR).await?;
        } else if let Some(existing) = find_by_source_ref(
            c,
            &format!("missions:mission:{}:impact", mission.mission_id),
        )
        .await?
        {
            // The impact was filed by an earlier delivery but the report is new (a
            // partial replay): draw the edge anyway, so the chain is complete.
            impact_id = Some(existing);
            link_records(c, report_id, existing, RELATION_PRODUCES, "", INGEST_ACTOR).await?;
        }
    }

    c.audit
        .log(
            None,
            "record.ingest.mission",
            "record",
            &report_id.to_string(),
            json!({
                "mission_id": mission.mission_id,
                "impact_record_id": impact_id,
                "scope_type": scope_type,
                "scope_id": scope_id,
                "source": ev.source,
                "event_id": ev.id,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.record.filed",
            json!({
                "record_id": report_id,
                "kind": KIND_MISSION_REPORT,
                "title": row["title"],
                "scope_type": row["scope_type"],
                "scope_id": row["scope_id"],
                "occurred_at": row["occurred_at_rfc3339"],
                "source": SOURCE_MISSIONS,
                "source_ref": row["source_ref"],
                "impact_record_id": impact_id,
            }),
        )
        .await?;
    Ok(())
}

/// The mission report's prose, built from the payload rather than stored
/// verbatim: the archive is readable, and a JSON blob is not a record.
fn mission_report_body(
    mission: &MissionCompleted,
    hours: Option<f64>,
    participants: Option<i64>,
) -> String {
    let mut body = format!(
        "{} closed in the {} stage on {}.\n\n",
        mission.title,
        mission.stage,
        render_instant(mission.completed_at)
    );
    if let Some(summary) = mission.impact["summary"].as_str() {
        if !summary.is_empty() {
            body.push_str(summary);
            body.push_str("\n\n");
        }
    }
    body.push_str(&format!(
        "Reported impact: {}\n\nThe mission's own record: /api/missions/mission/{}",
        impact_line(hours, participants),
        mission.mission_id
    ));
    body
}

/// One line of impact, in the terms missions reports them.
pub fn impact_line(hours: Option<f64>, participants: Option<i64>) -> String {
    match (hours, participants) {
        (Some(h), Some(p)) => format!("{h} service hours, {p} participants"),
        (Some(h), None) => format!("{h} service hours"),
        (None, Some(p)) => format!("{p} participants"),
        (None, None) => "no metrics were reported".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// `GET /api/archive/records` — the archive's catalogue.
async fn list_records(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let identity = req.identity.as_ref();
    let (troop_wide, lodges) = visibility(c, identity, "archive:read").await;
    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
    let mut preds = Predicates::new(troop_wide, lodges, &caller);
    let (from, to) = apply_common_filters(&mut preds, &req)?;
    preds.text_eq("r.source", req.query_param("source").map(String::from));
    preds.text_eq(
        "r.source_ref",
        req.query_param("source_ref").map(String::from),
    );
    let include_superseded = req.query_bool("include_superseded");
    if !include_superseded {
        preds.current_only(&c.db.table("records"));
    }
    // `cursor` wins if both are given: it is the value the previous page handed
    // back, and mixing the two is how a client silently skips a record.
    let cursor = match req.query_param("cursor") {
        Some(raw) => Some(parse_cursor(raw).map_err(SdkError::BadRequest)?),
        None => None,
    };
    preds.older_than(cursor);

    let limit = limit_of(&req, DEFAULT_LIMIT);
    let limit_placeholder = preds.next_placeholder();
    let where_sql = preds.where_sql();
    let mut params = preds.params;
    params.push(SqlValue::Int(limit));
    let rows =
        c.db.query(
            format!(
                "SELECT {fields} FROM {records} r {where_sql} \
                 ORDER BY r.occurred_at DESC, r.id DESC LIMIT ${limit_placeholder}",
                fields = read_fields(c),
                records = c.db.table("records"),
            ),
            params,
        )
        .await?;
    let next = rows.last().and_then(cursor_of);
    PluginResponse::json(
        200,
        &json!({
            "records": rows,
            "count": rows.len(),
            "limit": limit,
            "cursor": next,
            "next_cursor": next,
            "include_superseded": include_superseded,
            "window": {
                "from": from.map(render_instant),
                "to": to.map(render_instant),
                "to_exclusive": to.is_some(),
            },
        }),
    )
}

/// `GET /api/archive/record/{id}` — one record, its links, and its place in the
/// correction history.
async fn record_detail(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let record = fetch_visible(c, req.identity.as_ref(), "archive:read", id).await?;
    let edges = fetch_links(c, id).await?;
    let steps = describe_edges_any(&edges);
    let superseded_by = record["superseded_by"].clone();
    PluginResponse::json(
        200,
        &json!({
            "record": record,
            "current": is_current(&record),
            "superseded_by": superseded_by,
            "links": edges,
            "steps": steps,
            "why_it_exists": describe_edges("incoming", &edges),
            "what_it_produced": describe_edges("outgoing", &edges),
            "immutable": true,
            "next": "GET /api/archive/record/{id}/lineage for the whole chain, /chain for corrections",
        }),
    )
}

/// `GET /api/archive/record/{id}/links` — the direct edges, both directions.
async fn list_links(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let record = fetch_visible(c, req.identity.as_ref(), "archive:read", id).await?;
    let edges = fetch_links(c, id).await?;
    PluginResponse::json(
        200,
        &json!({
            "record_id": id,
            "kind": record["kind"],
            "links": edges,
            "steps": describe_edges_any(&edges),
            "downstream": describe_edges("outgoing", &edges).len(),
            "upstream": describe_edges("incoming", &edges).len(),
            "why_it_exists": describe_edges("incoming", &edges),
            "what_it_produced": describe_edges("outgoing", &edges),
        }),
    )
}

/// Every edge touching one record, from both ends, with the far node's kind and
/// title already joined on — a relationship nobody can read is a foreign key.
async fn fetch_links(c: &PluginContext, id: i64) -> Result<Vec<Value>, SdkError> {
    c.db.query(
        format!(
            "SELECT l.id, l.relation, l.note, l.created_by, l.created_at::text AS created_at, \
                    l.from_id, l.to_id, \
                    f.kind AS from_kind, f.title AS from_title, \
                    f.occurred_at::text AS from_occurred_at, \
                    t.kind AS to_kind, t.title AS to_title, \
                    t.occurred_at::text AS to_occurred_at, \
                    CASE WHEN l.from_id = $1 THEN 'outgoing' ELSE 'incoming' END AS direction \
             FROM {links} l \
             JOIN {records} f ON f.id = l.from_id \
             JOIN {records} t ON t.id = l.to_id \
             WHERE l.from_id = $1 OR l.to_id = $1 \
             ORDER BY l.relation, l.id",
            links = c.db.table("links"),
            records = c.db.table("records")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// The sentence for one edge, built from the joined far-node columns.
fn edge_sentence(edge: &Value) -> String {
    describe_edge(
        edge["relation"].as_str().unwrap_or_default(),
        &json!({
            "id": edge["from_id"],
            "kind": edge["from_kind"],
            "title": edge["from_title"],
        }),
        &json!({
            "id": edge["to_id"],
            "kind": edge["to_kind"],
            "title": edge["to_title"],
        }),
    )
}

/// Every edge's sentence, in the order the query returned them.
fn describe_edges_any(edges: &[Value]) -> Vec<String> {
    edges.iter().map(edge_sentence).collect()
}

/// The sentences for the edges in one direction (`incoming`/`outgoing`).
fn describe_edges(direction: &str, edges: &[Value]) -> Vec<String> {
    edges
        .iter()
        .filter(|edge| edge["direction"] == json!(direction))
        .map(edge_sentence)
        .collect()
}

/// `GET /api/archive/record/{id}/chain` — the correction history.
///
/// The chain is walked *both* ways in one statement: backwards through
/// `supersedes_id` to the original, forwards to the current version. Two
/// recursive CTEs rather than one with two self-references, because a recursive
/// term with multiple recursive references has sharp edges in SQL and this walk
/// is linear in each direction anyway.
async fn record_chain(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    fetch_visible(c, req.identity.as_ref(), "archive:read", id).await?;
    let leg_select = "id, supersedes_id, title, kind, correction_reason, filed_by, \
                      filed_at::text AS filed_at, depth";
    let rows =
        c.db.query(
            format!(
                "WITH RECURSIVE \
                   ancestors AS ( \
                     SELECT r.id, r.supersedes_id, r.title, r.kind, r.correction_reason, \
                            r.filed_at, r.filed_by, 0 AS depth \
                     FROM {records} r WHERE r.id = $1 \
                     UNION ALL \
                     SELECT r.id, r.supersedes_id, r.title, r.kind, r.correction_reason, \
                            r.filed_at, r.filed_by, a.depth + 1 \
                     FROM {records} r JOIN ancestors a ON r.id = a.supersedes_id \
                     WHERE a.depth < $2 \
                   ), \
                   descendants AS ( \
                     SELECT r.id, r.supersedes_id, r.title, r.kind, r.correction_reason, \
                            r.filed_at, r.filed_by, 0 AS depth \
                     FROM {records} r WHERE r.id = $1 \
                     UNION ALL \
                     SELECT r.id, r.supersedes_id, r.title, r.kind, r.correction_reason, \
                            r.filed_at, r.filed_by, d.depth + 1 \
                     FROM {records} r JOIN descendants d ON r.supersedes_id = d.id \
                     WHERE d.depth < $2 \
                   ) \
                 SELECT 'ancestor' AS leg, {leg_select} FROM ancestors WHERE depth > 0 \
                 UNION ALL \
                 SELECT 'descendant', {leg_select} FROM descendants WHERE depth > 0 \
                 ORDER BY leg, depth, id",
                records = c.db.table("records"),
                leg_select = leg_select
            ),
            vec![SqlValue::Int(id), SqlValue::Int(MAX_CHAIN)],
        )
        .await?;
    let ancestors: Vec<&Value> = rows
        .iter()
        .filter(|r| r["leg"] == json!("ancestor"))
        .collect();
    let descendants: Vec<&Value> = rows
        .iter()
        .filter(|r| r["leg"] == json!("descendant"))
        .collect();
    // Deepest ancestor = the original; deepest descendant = the current version.
    // Read from `depth` rather than from row order: the answer to "which version
    // is current?" must not depend on how the database happened to return rows.
    let deepest = |leg: &[&Value]| -> Option<i64> {
        leg.iter()
            .max_by_key(|r| r["depth"].as_i64().unwrap_or(0))
            .and_then(|r| r["id"].as_i64())
    };
    let original = deepest(&ancestors).unwrap_or(id);
    let current = deepest(&descendants).unwrap_or(id);
    PluginResponse::json(
        200,
        &json!({
            "record_id": id,
            "original_id": original,
            "current_id": current,
            "corrected": !descendants.is_empty(),
            "corrections": descendants.len(),
            "ancestors": ancestors,
            "descendants": descendants,
        }),
    )
}

/// `GET /api/archive/timeline` — the troop's history, in order, across every kind
/// of record.
async fn timeline(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let identity = req.identity.as_ref();
    let (troop_wide, lodges) = visibility(c, identity, "archive:read").await;
    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
    let mut preds = Predicates::new(troop_wide, lodges, &caller);
    let (from, to) = apply_common_filters(&mut preds, &req)?;
    let include_superseded = req.query_bool("include_superseded");
    if !include_superseded {
        preds.current_only(&c.db.table("records"));
    }
    let cursor = match req.query_param("cursor") {
        Some(raw) => Some(parse_cursor(raw).map_err(SdkError::BadRequest)?),
        None => None,
    };
    preds.older_than(cursor);

    let limit = limit_of(&req, DEFAULT_LIMIT);
    let limit_placeholder = preds.next_placeholder();
    let where_sql = preds.where_sql();
    let mut params = preds.params;
    params.push(SqlValue::Int(limit));
    let rows =
        c.db.query(
            format!(
                "SELECT {fields} FROM {records} r {where_sql} \
                 ORDER BY r.occurred_at DESC, r.id DESC LIMIT ${limit_placeholder}",
                fields = read_fields(c),
                records = c.db.table("records"),
            ),
            params,
        )
        .await?;

    // The page's own shape, computed here rather than with a second query: a
    // client drawing a timeline wants to know what it is looking at, and the
    // numbers must agree with the items it was handed.
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut entries: Vec<Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        let kind = row["kind"].as_str().unwrap_or_default();
        *counts.entry(kind.to_string()).or_insert(0) += 1;
        entries.push(json!({
            "record_id": row["id"],
            "kind": kind,
            "kind_label": kind_label(kind),
            "title": row["title"],
            "summary": row["summary"],
            "body_code": row["body_code"],
            "outcome": row["outcome"],
            "scope_type": row["scope_type"],
            "scope_id": row["scope_id"],
            "occurred_at": row["occurred_at_rfc3339"],
            "source": row["source"],
            "current": row["superseded_by"].is_null(),
            "links_in": row["links_in"],
            "links_out": row["links_out"],
        }));
    }
    let next = rows.last().and_then(cursor_of);
    PluginResponse::json(
        200,
        &json!({
            "items": entries,
            "count": entries.len(),
            "limit": limit,
            "cursor": next,
            "next_cursor": next,
            "counts_by_kind": counts,
            "window": {
                "from": from.map(render_instant),
                "to": to.map(render_instant),
                "to_exclusive": to.is_some(),
            },
            "order": "occurred_at DESC, id DESC",
            "scope": if troop_wide { "troop" } else { "scoped" },
        }),
    )
}

/// `GET /api/archive/search` — full-text search over the archive.
///
/// The predicate is `search @@ q`, which the GIN index serves; the ranking is
/// `ts_rank_cd`, which reads the weights migration 1 gave the three text fields.
/// The snippet is PostgreSQL's own `ts_headline`, so the caller sees *why* a
/// record matched rather than re-scanning the body for the term.
async fn search_records(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let query = req.query_required("q")?.to_string();
    let mode = req.query_param("mode").unwrap_or_default();
    let function = search_function(mode).map_err(SdkError::BadRequest)?;
    let mode_label = if function == "plainto_tsquery" {
        SEARCH_MODE_ALL_WORDS
    } else {
        SEARCH_MODE_WEB
    };

    let identity = req.identity.as_ref();
    let (troop_wide, lodges) = visibility(c, identity, "archive:search").await;
    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();

    // What did the database make of the query? A search for "the and of" parses
    // to an empty tsquery and matches nothing — the caller deserves to be told
    // that, rather than left thinking the archive is empty.
    let parsed =
        c.db.query_one(
            format!("SELECT {function}('english', $1)::text AS parsed"),
            vec![SqlValue::Text(query.clone())],
        )
        .await?
        .and_then(|row| row["parsed"].as_str().map(String::from))
        .unwrap_or_default();
    if parsed.trim().is_empty() {
        return PluginResponse::json(
            200,
            &json!({
                "query": query,
                "mode": mode_label,
                "parsed": parsed,
                "matches": 0,
                "results": [],
                "note": "the query parsed to no searchable terms (stopwords only) — websearch \
                         syntax supports \"exact phrases\", OR and -exclude",
            }),
        );
    }

    let mut preds = Predicates::new(troop_wide, lodges, &caller);
    let (from, to) = apply_common_filters(&mut preds, &req)?;
    let include_superseded = req.query_bool("include_superseded");
    if !include_superseded {
        preds.current_only(&c.db.table("records"));
    }
    // The tsquery is built once, in the FROM list, and reused by the predicate,
    // the rank and the headline — one parse, three uses.
    let limit = limit_of(&req, DEFAULT_LIMIT);
    let query_placeholder = preds.next_placeholder();
    let limit_placeholder = query_placeholder + 1;
    let where_sql = format!("{} AND r.search @@ q.query", preds.where_sql());
    let mut params = preds.params;
    params.push(SqlValue::Text(query.clone()));
    params.push(SqlValue::Int(limit));
    let rows =
        c.db.query(
            format!(
                "SELECT {fields}, \
                        ts_rank_cd(r.search, q.query) AS rank, \
                        ts_headline('english', \
                                    CASE WHEN r.body <> '' THEN r.body ELSE r.summary END, \
                                    q.query, \
                                    'MaxFragments=2,MaxWords=25,MinWords=8,\
                                     StartSel=<mark>,StopSel=</mark>') AS snippet \
                 FROM {records} r, {function}('english', ${query_placeholder}) AS q(query) \
                 {where_sql} \
                 ORDER BY rank DESC, r.occurred_at DESC, r.id DESC LIMIT ${limit_placeholder}",
                fields = read_fields(c),
                records = c.db.table("records"),
                where_sql = where_sql,
            ),
            params,
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "query": query,
            "mode": mode_label,
            "parsed": parsed,
            "matches": rows.len(),
            "results": rows,
            "limit": limit,
            "window": {
                "from": from.map(render_instant),
                "to": to.map(render_instant),
                "to_exclusive": to.is_some(),
            },
            "scope": if troop_wide { "troop" } else { "scoped" },
            "note": "ranked by ts_rank_cd over a weighted tsvector (title > summary > body), \
                     served by the GIN index on archive.records.search",
        }),
    )
}

/// `GET /api/archive/stats` — what the archive holds, for the troop's annual
/// report and for the archive keeper's conscience about gaps.
async fn archive_stats(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let identity = req.identity.as_ref();
    let (troop_wide, lodges) = visibility(c, identity, "archive:read").await;
    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
    let mut preds = Predicates::new(troop_wide, lodges, &caller);
    preds.text_eq(
        "r.scope_type",
        match req.query_param("scope_type") {
            Some(raw) => {
                Some(normalize_scope_type(&Some(raw.to_string())).map_err(SdkError::BadRequest)?)
            }
            None => None,
        },
    );
    preds.text_eq("r.scope_id", req.query_param("scope_id").map(String::from));
    let where_sql = preds.where_sql();
    let records = c.db.table("records");

    let by_kind =
        c.db.query(
            format!(
                "SELECT r.kind, count(*) AS n FROM {records} r {where_sql} \
                 GROUP BY r.kind ORDER BY n DESC, r.kind"
            ),
            preds.params.clone(),
        )
        .await?;
    let by_year =
        c.db.query(
            format!(
                "SELECT to_char(r.occurred_at, 'YYYY') AS year, count(*) AS n \
                 FROM {records} r {where_sql} GROUP BY 1 ORDER BY 1 DESC"
            ),
            preds.params.clone(),
        )
        .await?;
    let by_body =
        c.db.query(
            format!(
                "SELECT r.body_code, count(*) AS n FROM {records} r {where_sql} \
                 GROUP BY r.body_code ORDER BY n DESC, r.body_code"
            ),
            preds.params.clone(),
        )
        .await?;
    let totals = c
        .db
        .query_one(
            format!(
                "SELECT count(*) AS total, \
                        count(*) FILTER (WHERE r.supersedes_id IS NOT NULL) AS corrections, \
                        count(*) FILTER (WHERE EXISTS ( \
                            SELECT 1 FROM {records} c WHERE c.supersedes_id = r.id)) AS superseded, \
                        count(*) FILTER (WHERE NOT EXISTS ( \
                            SELECT 1 FROM {links} l WHERE l.from_id = r.id)) AS without_links_out, \
                        count(DISTINCT r.source) AS sources, \
                        min(r.occurred_at)::text AS earliest, \
                        max(r.occurred_at)::text AS latest \
                 FROM {records} r {where_sql}",
                links = c.db.table("links")
            ),
            preds.params.clone(),
        )
        .await?
        .unwrap_or_else(|| {
            json!({
                "total": 0, "corrections": 0, "superseded": 0, "without_links_out": 0,
                "sources": 0, "earliest": Value::Null, "latest": Value::Null
            })
        });
    let kinds_present: Vec<&str> = by_kind.iter().filter_map(|r| r["kind"].as_str()).collect();
    let missing: Vec<&str> = KINDS
        .iter()
        .copied()
        .filter(|k| !kinds_present.contains(k))
        .collect();
    PluginResponse::json(
        200,
        &json!({
            "totals": totals,
            "by_kind": by_kind,
            "by_body": by_body,
            "by_year": by_year,
            "kinds_without_records": missing,
            "locked": "records are append-only; corrections supersede",
            "scope": if troop_wide { "troop" } else { "scoped" },
        }),
    )
}

/// `GET /api/archive/vocabulary` — the codes, their meanings, and the chain, so a
/// client can build a filing form without hard-coding the archive's words.
fn vocabulary_response() -> Result<PluginResponse, SdkError> {
    let kinds: Vec<Value> = KINDS
        .iter()
        .map(|kind| json!({ "kind": kind, "label": kind_label(kind) }))
        .collect();
    let relations: Vec<Value> = RELATIONS
        .iter()
        .map(|relation| {
            let (from_kinds, to_kinds) = relation_kinds(relation);
            json!({
                "relation": relation,
                "phrase": relation_phrase(relation),
                "meaning": relation_meaning(relation),
                "from_kinds": from_kinds,
                "to_kinds": to_kinds,
            })
        })
        .collect();
    PluginResponse::json(
        200,
        &json!({
            "kinds": kinds,
            "body_codes": BODY_CODES,
            "outcomes": OUTCOMES,
            "scope_types": SCOPE_TYPES,
            "sources": SOURCES,
            "relations": relations,
            "chain": CHAIN,
            "search_modes": [SEARCH_MODE_WEB, SEARCH_MODE_ALL_WORDS],
            "immutable": true,
            "correction": "a filed record is never edited; POST /api/archive/record/{id}/supersede \
                           files a new one that supersedes it",
            "timeline_order": "occurred_at DESC, id DESC (cursor: <rfc3339>|<id>)",
            "window": "from <= occurred_at < to (half-open)",
        }),
    )
}

// ---------------------------------------------------------------------------
// Relationships
// ---------------------------------------------------------------------------

/// Advice when a link's endpoints do not match what the relation usually joins.
///
/// Advice, never a refusal: a troop that files its Congress as a
/// `congress_proceeding` and links it `decides` a policy is describing the same
/// fact in its own words.
fn link_advice(from_kind: &str, to_kind: &str, relation: &str) -> Option<String> {
    let (expected_from, expected_to) = relation_kinds(relation);
    let from_ok = expected_from.is_empty() || expected_from.contains(&from_kind);
    let to_ok = expected_to.is_empty() || expected_to.contains(&to_kind);
    if from_ok && to_ok {
        return None;
    }
    let expected = |kinds: &[&str]| {
        if kinds.is_empty() {
            "any".to_string()
        } else {
            kinds.join("|")
        }
    };
    Some(format!(
        "{relation:?} normally links {} → {}; this links {from_kind} → {to_kind}. Stored as \
         given — the troop's vocabulary is the troop's.",
        expected(expected_from),
        expected(expected_to)
    ))
}

/// `POST /api/archive/record/{id}/link` — draw a relationship.
///
/// The caller must hold `archive:link` covering **both** records' scopes: an edge
/// is a statement about two records, and a Lodge-scoped grant has no business
/// asserting what a troop-wide decision authorised.
async fn create_link(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let from_id = req.int_param("id")?;
    let body: LinkBody = req.json()?;
    let relation = normalize_relation(&body.relation).map_err(SdkError::BadRequest)?;
    if body.to_id == from_id {
        return Err(SdkError::BadRequest(
            "a record cannot be linked to itself".into(),
        ));
    }
    let from = fetch_visible(c, req.identity.as_ref(), "archive:link", from_id).await?;
    let to = fetch_visible(c, req.identity.as_ref(), "archive:link", body.to_id).await?;
    c.permissions
        .reach(req.identity.as_ref(), "archive:link", &scope_of(&from))
        .await?;
    c.permissions
        .reach(req.identity.as_ref(), "archive:link", &scope_of(&to))
        .await?;

    let note = trimmed(&body.note).unwrap_or_default();
    let from_kind = from["kind"].as_str().unwrap_or_default().to_string();
    let to_kind = to["kind"].as_str().unwrap_or_default().to_string();
    let advice = link_advice(&from_kind, &to_kind, &relation);
    let actor = req
        .identity
        .as_ref()
        .map(|i| i.user_id.clone())
        .unwrap_or_default();
    let link =
        c.db.query_one(
            format!(
                "INSERT INTO {links} AS l (from_id, to_id, relation, note, created_by) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (from_id, to_id, relation) DO UPDATE SET note = EXCLUDED.note \
                 RETURNING l.id, l.from_id, l.to_id, l.relation, l.note, l.created_by, \
                           l.created_at::text AS created_at",
                links = c.db.table("links")
            ),
            vec![
                SqlValue::Int(from_id),
                SqlValue::Int(body.to_id),
                SqlValue::Text(relation.clone()),
                SqlValue::Text(note.clone()),
                SqlValue::Text(actor.clone()),
            ],
        )
        .await?
        .ok_or_else(|| SdkError::Internal("the link upsert returned no row".into()))?;
    c.audit
        .log(
            req.identity.as_ref(),
            "link.create",
            "record",
            &from_id.to_string(),
            json!({
                "to_id": body.to_id,
                "relation": relation,
                "note": note,
            }),
        )
        .await?;
    c.events
        .publish(
            "archive.link.created",
            json!({
                "from_id": from_id,
                "to_id": body.to_id,
                "relation": relation,
                "from_kind": from_kind,
                "to_kind": to_kind,
                "created_by": actor,
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "link": link,
            "step": describe_edge(&relation, &from, &to),
            "advice": advice,
            "idempotent": "re-linking the same pair updates the note and keeps the edge",
        }),
    )
}

/// `GET /api/archive/record/{id}/lineage` — the whole chain around a record.
///
/// Two recursive walks (upstream = the causes that explain it, downstream = what
/// it produced), each with a visited-array cycle guard and a depth bound. A cycle
/// is hard to create through the API's vocabulary in ordinary use, but
/// `relates_to` can be drawn by hand in both directions, so the guard is a
/// correctness requirement rather than a precaution.
async fn lineage(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    // The caller's own query string is validated before the record is read: a
    // typo in `direction` should not be answered with "no such record".
    let depth = req
        .query_int("depth")
        .unwrap_or(DEFAULT_DEPTH)
        .clamp(1, MAX_DEPTH);
    let direction = req
        .query_param("direction")
        .unwrap_or("both")
        .trim()
        .to_ascii_lowercase();
    let (want_up, want_down) = match direction.as_str() {
        "" | "both" => (true, true),
        "up" | "upstream" => (true, false),
        "down" | "downstream" => (false, true),
        other => {
            return Err(SdkError::BadRequest(format!(
                "direction {other:?} is not one of both, upstream, downstream"
            )))
        }
    };
    let record = fetch_visible(c, req.identity.as_ref(), "archive:read", id).await?;

    let links = c.db.table("links");
    let rows =
        c.db.query(
            format!(
                "WITH RECURSIVE \
                   downstream AS ( \
                     SELECT l.id AS link_id, l.from_id, l.to_id, l.relation, 1 AS depth, \
                            ARRAY[$1, l.to_id] AS visited \
                     FROM {links} l WHERE l.from_id = $1 \
                     UNION ALL \
                     SELECT l.id, l.from_id, l.to_id, l.relation, d.depth + 1, \
                            d.visited || l.to_id \
                     FROM downstream d JOIN {links} l ON l.from_id = d.to_id \
                     WHERE d.depth < $2 AND NOT (l.to_id = ANY(d.visited)) \
                   ), \
                   upstream AS ( \
                     SELECT l.id AS link_id, l.from_id, l.to_id, l.relation, 1 AS depth, \
                            ARRAY[$1, l.from_id] AS visited \
                     FROM {links} l WHERE l.to_id = $1 \
                     UNION ALL \
                     SELECT l.id, l.from_id, l.to_id, l.relation, u.depth + 1, \
                            u.visited || l.from_id \
                     FROM upstream u JOIN {links} l ON l.to_id = u.from_id \
                     WHERE u.depth < $2 AND NOT (l.from_id = ANY(u.visited)) \
                   ) \
                 SELECT 'downstream' AS direction, link_id, from_id, to_id, relation, depth \
                 FROM downstream \
                 UNION ALL \
                 SELECT 'upstream', link_id, from_id, to_id, relation, depth FROM upstream \
                 ORDER BY direction, depth, link_id"
            ),
            vec![SqlValue::Int(id), SqlValue::Int(depth)],
        )
        .await?;

    let downstream: Vec<Value> = rows
        .iter()
        .filter(|r| want_down && r["direction"] == json!("downstream"))
        .cloned()
        .collect();
    let upstream: Vec<Value> = rows
        .iter()
        .filter(|r| want_up && r["direction"] == json!("upstream"))
        .cloned()
        .collect();

    // One query for the titles of every node involved, rather than a join per
    // edge: the client gets a graph it can draw, not a list of id pairs.
    let mut node_ids: Vec<i64> = vec![id];
    for edge in downstream.iter().chain(upstream.iter()) {
        for key in ["from_id", "to_id"] {
            if let Some(node) = edge[key].as_i64() {
                if !node_ids.contains(&node) {
                    node_ids.push(node);
                }
            }
        }
    }
    let nodes =
        c.db.query(
            format!(
                "SELECT r.id, r.kind, r.title, r.outcome, {superseded_by} \
                 FROM {records} r WHERE r.id = ANY($1) ORDER BY r.id",
                superseded_by = superseded_by_expr(c),
                records = c.db.table("records")
            ),
            vec![SqlValue::IntArray(node_ids)],
        )
        .await?;

    let node_of = |edge: &Value, key: &str| -> Value {
        nodes
            .iter()
            .find(|n| n["id"] == edge[key])
            .cloned()
            .unwrap_or_else(|| json!({ "id": edge[key], "kind": "record" }))
    };
    let explanation: Vec<String> = downstream
        .iter()
        .chain(upstream.iter())
        .map(|edge| {
            describe_edge(
                edge["relation"].as_str().unwrap_or_default(),
                &node_of(edge, "from_id"),
                &node_of(edge, "to_id"),
            )
        })
        .collect();

    PluginResponse::json(
        200,
        &json!({
            "record": record,
            "depth": depth,
            "direction": direction,
            "nodes": nodes,
            "downstream": downstream,
            "upstream": upstream,
            "explanation": explanation,
            "why_it_exists": upstream.len(),
            "what_it_produced": downstream.len(),
            "questions": {
                "why_does_this_exist": "read `upstream` — the edges pointing at this record",
                "what_did_it_produce": "read `downstream` — the edges leaving this record",
            },
        }),
    )
}

/// `DELETE /api/archive/link/{id}` — remove a relationship drawn in error.
///
/// Unlinking is administration, not editing: an edge is the troop's assertion
/// about two records, and a wrong assertion should be removable. It is audited
/// with what it said, so removing a link is itself part of the record.
async fn delete_link(c: &PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(link) =
        c.db.query_one(
            format!(
                "SELECT l.id, l.from_id, l.to_id, l.relation, l.note, l.created_by, \
                        l.created_at::text AS created_at \
                 FROM {links} l WHERE l.id = $1",
                links = c.db.table("links")
            ),
            vec![SqlValue::Int(id)],
        )
        .await?
    else {
        return Err(SdkError::NotFound(format!("no link {id}")));
    };
    c.audit
        .log(
            req.identity.as_ref(),
            "link.delete",
            "record",
            &link["from_id"].as_i64().unwrap_or_default().to_string(),
            json!({
                "link_id": id,
                "to_id": link["to_id"],
                "relation": link["relation"],
                "note": link["note"],
                "created_by": link["created_by"],
            }),
        )
        .await?;
    c.db.execute(
        format!("DELETE FROM {} WHERE id = $1", c.db.table("links")),
        vec![SqlValue::Int(id)],
    )
    .await?;
    c.events
        .publish(
            "archive.link.deleted",
            json!({
                "link_id": id,
                "from_id": link["from_id"],
                "to_id": link["to_id"],
                "relation": link["relation"],
            }),
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "deleted": id,
            "from_id": link["from_id"],
            "to_id": link["to_id"],
            "relation": link["relation"],
            "note": "the records themselves are untouched — an edge is not a record",
        }),
    )
}

export_plugin!(ArchivePlugin);
