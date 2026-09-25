//! # adjutant-governance — motions, votes, amendments, quorum, minutes, and
//! Accords versioning (SPEC §7.4, Accords Art 5/9/12/17).
//!
//! The Accords make the Troop Council the day-to-day governing body
//! ("Decisions by majority vote"), let a Congress adopt the Accords themselves
//! ("adopted by majority vote at Catamount Congress", Art 17), and require a
//! quorum to do either (the 3rd Congress locked Congress quorum at **one-third
//! of registered scouts**). This plugin is that machinery:
//!
//! * **Motions** move through `proposed → seconded → debate → voting → decided`
//!   and then `implemented` — each step a route, each step recorded.
//! * **Votes** are recorded per voter, with the method the meeting actually used
//!   (`voice`, `show_of_hands`, `ballot`, `roll_call`).
//! * **Amendments** are friendly (accepted by the mover, no vote) or formal
//!   (voted on, then applied to the motion's text).
//! * **Quorum** is computed live from attendance, and a motion cannot be closed
//!   without it.
//! * **Minutes** are drafted from the record — a starting point for the
//!   Archivist, not a replacement for them.
//! * **Accords versions** are created by adopting a passed Congress motion, so
//!   "every Congress adoption creates a new version" is a constraint, not a
//!   convention.
//!
//! ## Permissions
//!
//! SPEC §9.1 names `governance:read`, `governance:propose`, `governance:vote`,
//! and `governance:amend`. `governance:manage` is an M4 addition: running a
//! meeting (opening it, recording attendance, closing votes, keeping minutes,
//! adopting an Accords version) is the chair's/Archivist's job and is not the
//! same authority as proposing or voting.
//!
//! **Role grants are not the plugin's to make.** The core seeds `chief` with
//! every permission; a troop maps other roles to permissions through
//! `core.role_permissions` (see `docs/api-reference.md`).
//!
//! ## Schema
//!
//! `motions`, `amendments`, `votes`, and `accords_versions` are SPEC §7.4's
//! tables. `meetings` (quorum basis and minutes) and `attendance` (who was
//! present) carry the quorum requirement the SPEC puts in this plugin's
//! responsibilities.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Vocabulary — every one of these is a stable code, never a display string
// ---------------------------------------------------------------------------

/// The Catamount Congress — the annual body that adopts the Accords.
pub const BODY_CONGRESS: &str = "congress";
/// The Troop Council.
pub const BODY_TC: &str = "tc";
/// A Lodge's own meeting.
pub const BODY_LODGE: &str = "lodge";
/// A committee (Auxiliary, Finance, …).
pub const BODY_COMMITTEE: &str = "committee";

/// Governing bodies a motion or meeting can belong to.
pub const BODIES: [&str; 4] = [BODY_CONGRESS, BODY_TC, BODY_LODGE, BODY_COMMITTEE];

/// Motion stages, in order.
pub const MOTION_STAGES: [&str; 7] = [
    "proposed",
    "seconded",
    "debate",
    "voting",
    "decided",
    "implemented",
    "withdrawn",
];

/// A motion that has not been decided.
pub const RESULT_PENDING: &str = "pending";
/// Carried.
pub const RESULT_PASSED: &str = "passed";
/// Not carried.
pub const RESULT_FAILED: &str = "failed";

/// More yes than no (abstentions are not votes against).
pub const THRESHOLD_SIMPLE_MAJORITY: &str = "simple_majority";
/// Two thirds of the votes cast.
pub const THRESHOLD_TWO_THIRDS: &str = "two_thirds";
/// Every vote cast is yes.
pub const THRESHOLD_UNANIMOUS: &str = "unanimous";

pub const THRESHOLDS: [&str; 3] =
    [THRESHOLD_SIMPLE_MAJORITY, THRESHOLD_TWO_THIRDS, THRESHOLD_UNANIMOUS];

/// How the votes were taken (SPEC §7.4 requires all four).
pub const VOTE_METHODS: [&str; 4] = ["voice", "show_of_hands", "ballot", "roll_call"];

/// A vote in favour.
pub const CHOICE_YES: &str = "yes";
/// A vote against.
pub const CHOICE_NO: &str = "no";
/// Recorded but not counted either way.
pub const CHOICE_ABSTAIN: &str = "abstain";

pub const VOTE_CHOICES: [&str; 3] = [CHOICE_YES, CHOICE_NO, CHOICE_ABSTAIN];

/// Accepted by the mover; no vote needed.
pub const AMENDMENT_FRIENDLY: &str = "friendly";
/// Voted on like a motion.
pub const AMENDMENT_FORMAL: &str = "formal";

/// Quorum: one third of the expected voters (Congress — locked by the 3rd
/// Congress).
pub const QUORUM_ONE_THIRD_REGISTERED: &str = "one_third_registered";
/// Quorum: a majority of the body's members (Troop Council default).
pub const QUORUM_MAJORITY_MEMBERS: &str = "majority_members";
/// Quorum: an explicit number the operator sets.
pub const QUORUM_FIXED: &str = "fixed";

pub const QUORUM_BASES: [&str; 3] =
    [QUORUM_ONE_THIRD_REGISTERED, QUORUM_MAJORITY_MEMBERS, QUORUM_FIXED];

/// Motion categories (stable codes).
pub const MOTION_CATEGORIES: [&str; 9] = [
    "general",
    "policy",
    "finance",
    "accords_amendment",
    "lodge_charter",
    "membership",
    "recall",
    "confidence",
    "other",
];

/// Meeting states.
pub const MEETING_SCHEDULED: &str = "scheduled";
/// In progress.
pub const MEETING_OPEN: &str = "open";
/// Finished.
pub const MEETING_CLOSED: &str = "closed";

/// Accords version states.
pub const ACCORDS_DRAFT: &str = "draft";
/// Adopted by Congress.
pub const ACCORDS_ADOPTED: &str = "adopted";
/// Replaced by a later version.
pub const ACCORDS_SUPERSEDED: &str = "superseded";

// ---------------------------------------------------------------------------
// Quorum (pure)
// ---------------------------------------------------------------------------

/// The number of present voters required for quorum.
///
/// * `one_third_registered` — `ceil(expected / 3)`, the Congress rule the 3rd
///   Catamount Congress locked ("quorum = one-third of registered scouts");
/// * `majority_members` — `floor(expected / 2) + 1`, the Troop Council default
///   ("Decisions by majority vote");
/// * `fixed` — the configured number, or `0` when none is set.
///
/// An unconfigured meeting (`expected == 0`) therefore has a required quorum of
/// `0`, and [`quorum_met`] treats that as **not met**: the room fails closed
/// until somebody says how many people were expected. A meeting that silently
/// has no quorum rule is how a body votes itself a mandate it does not have.
pub fn compute_quorum(basis: &str, expected: i64, configured: Option<i64>) -> i64 {
    let expected = expected.max(0);
    match basis {
        // Guarded on `expected > 0`: a majority of nobody is not one person.
        QUORUM_ONE_THIRD_REGISTERED if expected > 0 => (expected + 2) / 3,
        QUORUM_MAJORITY_MEMBERS if expected > 0 => expected / 2 + 1,
        QUORUM_FIXED => configured.unwrap_or(0).max(0),
        _ => 0,
    }
}

/// Is quorum met? A required count of `0` is never met (see [`compute_quorum`]).
pub fn quorum_met(present: i64, required: i64) -> bool {
    required > 0 && present >= required
}

// ---------------------------------------------------------------------------
// Tallies (pure)
// ---------------------------------------------------------------------------

/// The outcome of a vote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    pub yes: i64,
    pub no: i64,
    pub abstain: i64,
    pub passed: bool,
}

impl Tally {
    /// Votes counted for or against — abstentions are recorded, not counted.
    pub fn cast(&self) -> i64 {
        self.yes + self.no
    }
}

/// Tally vote rows against a threshold.
///
/// Rows are the `votes` table shape (`choice`); anything unrecognised is
/// ignored rather than counted as a yes. The thresholds:
///
/// * `simple_majority` — more yes than no;
/// * `two_thirds` — at least two thirds of the votes cast (`yes * 3 >= cast *
///   2`, integer-exact);
/// * `unanimous` — every vote cast is yes, and at least one was cast.
///
/// A motion with no votes at all does not pass under any threshold: an empty
/// room does not carry a motion.
pub fn tally_votes(votes: &[serde_json::Value], threshold: &str) -> Tally {
    let mut tally = Tally { yes: 0, no: 0, abstain: 0, passed: false };
    for vote in votes {
        match vote["choice"].as_str() {
            Some(CHOICE_YES) => tally.yes += 1,
            Some(CHOICE_NO) => tally.no += 1,
            Some(CHOICE_ABSTAIN) => tally.abstain += 1,
            _ => {}
        }
    }
    let cast = tally.cast();
    tally.passed = match threshold {
        THRESHOLD_TWO_THIRDS => cast > 0 && tally.yes * 3 >= cast * 2,
        THRESHOLD_UNANIMOUS => cast > 0 && tally.no == 0 && tally.yes == cast,
        // Simple majority is the default for an unknown threshold, because it
        // is the weakest: an unrecognised value cannot carry a motion more
        // easily than the ordinary rule.
        _ => cast > 0 && tally.yes > tally.no,
    };
    tally
}

/// A `scheduled_for` this plugin will hand to `$n::timestamptz`.
///
/// Deliberately loose — `2026-12-06T18:00:00Z`, `2026-12-06 18:00:00-05`, or a
/// bare `2026-12-06` all pass, and PostgreSQL does the real parsing. What it
/// rejects is input that is *not a date at all*, which would otherwise surface
/// as a 500 from the database instead of the 400 the caller earned.
pub fn is_timestamp_like(value: &str) -> bool {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    if !((1..=12).contains(&month) && (1..=31).contains(&day)) {
        return false;
    }
    // Whatever follows the date must be a separator, not more of a word.
    bytes.len() == 10 || matches!(bytes[10], b'T' | b't' | b' ')
}

/// Guard a motion's stage.
pub fn require_motion_stage(
    current: &str,
    allowed: &[&str],
    action: &str,
) -> Result<(), SdkError> {
    if allowed.contains(&current) {
        return Ok(());
    }
    Err(SdkError::Conflict(format!(
        "cannot {action}: the motion is {current:?}, and {action} requires one of {allowed:?}"
    )))
}

// ---------------------------------------------------------------------------
// Minutes (pure)
// ---------------------------------------------------------------------------

fn text_of(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn number_of(value: &serde_json::Value, key: &str) -> i64 {
    value[key].as_i64().unwrap_or_default()
}

/// Draft the minutes of a meeting from its record.
///
/// Deterministic and total: every field it reads is optional, so a partial
/// record produces a partial draft rather than an error. The draft is a
/// **starting point** for the Archivist (SPEC §7.8 owns the archive) — it says
/// so in its own text, because minutes that look authoritative without review
/// are worse than no draft.
pub fn render_minutes(
    meeting: &serde_json::Value,
    attendance: &[serde_json::Value],
    motions: &[serde_json::Value],
    amendments: &[serde_json::Value],
    votes: &[serde_json::Value],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", text_of(meeting, "title")));
    out.push_str(&format!(
        "**Body:** {} · **Status:** {} · **Scheduled:** {}\n\n",
        text_of(meeting, "body"),
        text_of(meeting, "status"),
        text_of(meeting, "scheduled_for")
    ));

    let present = attendance
        .iter()
        .filter(|a| a["present"].as_bool().unwrap_or(false))
        .count() as i64;
    let expected = number_of(meeting, "expected_voters");
    let basis = text_of(meeting, "quorum_basis");
    let required = compute_quorum(&basis, expected, meeting["quorum_required"].as_i64());
    out.push_str(&format!(
        "**Quorum:** {present} of {expected} expected — {} ({basis}, requires {required})\n\n",
        if quorum_met(present, required) { "met" } else { "NOT met" }
    ));

    out.push_str(&format!("## Attendance ({present} present)\n\n"));
    if attendance.is_empty() {
        out.push_str("_No attendance recorded._\n\n");
    } else {
        for a in attendance {
            let status = match a["present"].as_bool() {
                Some(true) => "present",
                Some(false) => "absent",
                None => "recorded",
            };
            out.push_str(&format!(
                "- {} ({status}, {})\n",
                text_of(a, "member_id"),
                text_of(a, "method")
            ));
        }
        out.push('\n');
    }

    out.push_str("## Motions\n\n");
    if motions.is_empty() {
        out.push_str("_No motions were recorded._\n\n");
    }
    for motion in motions {
        let id = number_of(motion, "id");
        let result = text_of(motion, "result");
        out.push_str(&format!(
            "### {} — {}\n\n",
            format_args!("Motion {id}"),
            text_of(motion, "title")
        ));
        out.push_str(&format!(
            "- Stage: {} · Result: {} · Threshold: {}\n",
            text_of(motion, "stage"),
            if result.is_empty() { RESULT_PENDING.to_string() } else { result },
            text_of(motion, "threshold")
        ));
        out.push_str(&format!("- Proposed by: {}", text_of(motion, "proposed_by")));
        if !text_of(motion, "seconded_by").is_empty() {
            out.push_str(&format!(" · Seconded by: {}", text_of(motion, "seconded_by")));
        }
        out.push('\n');
        if motion["amends_accords"].as_bool().unwrap_or(false) {
            out.push_str("- **Amends the Accords** — Congress adoption required (Art 17)\n");
        }
        let cast = votes
            .iter()
            .filter(|v| number_of(v, "motion_id") == id && v["amendment_id"].is_null())
            .count();
        if cast > 0 {
            let methods: Vec<String> = votes
                .iter()
                .filter(|v| number_of(v, "motion_id") == id && v["amendment_id"].is_null())
                .map(|v| text_of(v, "method"))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            out.push_str(&format!(
                "- Votes: {} yes, {} no, {} abstain ({}; {cast} recorded)\n",
                number_of(motion, "votes_yes"),
                number_of(motion, "votes_no"),
                number_of(motion, "votes_abstain"),
                methods.join(", ")
            ));
        } else {
            out.push_str("- Votes: none recorded\n");
        }
        if !text_of(motion, "text").is_empty() {
            out.push_str(&format!("\n> {}\n", text_of(motion, "text").replace('\n', "\n> ")));
        }
        for amendment in amendments
            .iter()
            .filter(|a| number_of(a, "motion_id") == id)
        {
            out.push_str(&format!(
                "\n**Amendment {} ({}, {})** — proposed by {}\n\n> {}\n",
                number_of(amendment, "id"),
                text_of(amendment, "kind"),
                text_of(amendment, "status"),
                text_of(amendment, "proposed_by"),
                text_of(amendment, "text").replace('\n', "\n> ")
            ));
        }
        if !text_of(motion, "implementation_note").is_empty() {
            out.push_str(&format!(
                "\n**Implementation:** {}\n",
                text_of(motion, "implementation_note")
            ));
        }
        out.push('\n');
    }

    out.push_str("---\n\n");
    out.push_str(
        "_Draft generated from the motion record. Review, correct, and adopt it before it \
         becomes the minutes of record._\n",
    );
    out
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct GovernancePlugin {
    ctx: OnceLock<PluginContext>,
}

impl GovernancePlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }
}

impl Default for GovernancePlugin {
    fn default() -> Self {
        Self::new()
    }
}

async fn fetch_meeting(
    c: &PluginContext,
    id: i64,
) -> Result<Option<serde_json::Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT *, expected_voters, quorum_required, quorum_basis \
             FROM {} WHERE id = $1",
            c.db.table("meetings")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

async fn fetch_motion(
    c: &PluginContext,
    id: i64,
) -> Result<Option<serde_json::Value>, SdkError> {
    c.db.query_one(
        format!("SELECT * FROM {} WHERE id = $1", c.db.table("motions")),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// Present voters at a meeting.
async fn present_count(c: &PluginContext, meeting_id: i64) -> Result<i64, SdkError> {
    Ok(c.db
        .query_one(
            format!(
                "SELECT COUNT(*) AS n FROM {} WHERE meeting_id = $1 AND present",
                c.db.table("attendance")
            ),
            vec![SqlValue::Int(meeting_id)],
        )
        .await?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
}

/// The quorum numbers for a meeting: `(required, present, met)`.
async fn quorum_state(c: &PluginContext, meeting: &serde_json::Value) -> Result<(i64, i64, bool), SdkError> {
    let basis = meeting["quorum_basis"].as_str().unwrap_or(QUORUM_MAJORITY_MEMBERS);
    let expected = meeting["expected_voters"].as_i64().unwrap_or(0);
    let configured = meeting["quorum_required"].as_i64();
    let required = compute_quorum(basis, expected, configured);
    let present = match meeting["id"].as_i64() {
        Some(id) => present_count(c, id).await?,
        None => 0,
    };
    Ok((required, present, quorum_met(present, required)))
}

/// Is this caller present at the meeting they are voting in?
///
/// A motion tied to a meeting is voted by the people in the room; a motion with
/// no meeting (an asynchronous decision) is open to anyone with
/// `governance:vote`.
async fn require_present(
    c: &PluginContext,
    meeting_id: Option<i64>,
    voter: &str,
) -> Result<(), SdkError> {
    let Some(meeting_id) = meeting_id else { return Ok(()) };
    let present = c
        .db
        .exists(
            format!(
                "SELECT 1 FROM {} WHERE meeting_id = $1 AND member_id = $2 AND present",
                c.db.table("attendance")
            ),
            vec![SqlValue::Int(meeting_id), SqlValue::Text(voter.to_string())],
        )
        .await?;
    if present {
        Ok(())
    } else {
        Err(SdkError::Forbidden(format!(
            "only members recorded present at meeting {meeting_id} may vote in it \
             (POST /api/governance/meeting/{meeting_id}/attendance)"
        )))
    }
}

/// The vote rows for a motion or one of its amendments.
async fn votes_for(
    c: &PluginContext,
    motion_id: i64,
    amendment_id: Option<i64>,
) -> Result<Vec<serde_json::Value>, SdkError> {
    c.db.query(
        format!(
            "SELECT id, motion_id, amendment_id, voter, choice, method, \
                    recorded_at::text AS recorded_at, note \
             FROM {} WHERE motion_id = $1 \
               AND amendment_id IS NOT DISTINCT FROM $2::bigint \
             ORDER BY id",
            c.db.table("votes")
        ),
        vec![
            SqlValue::Int(motion_id),
            amendment_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
        ],
    )
    .await
}

/// Append an accepted amendment to a motion's text, so the record shows what was
/// actually decided rather than what was first proposed.
fn apply_amendment(motion_text: &str, amendment_id: i64, kind: &str, text: &str) -> String {
    format!("{motion_text}\n\n--- Amendment {amendment_id} ({kind}) ---\n{text}")
}

/// Apply an accepted amendment to its motion (the store half of
/// [`apply_amendment`]).
async fn apply_amendment_to_motion(
    c: &PluginContext,
    motion_id: i64,
    motion: &serde_json::Value,
    amendment_id: i64,
    kind: &str,
    text: &str,
) -> Result<(), SdkError> {
    let updated = apply_amendment(
        motion["text"].as_str().unwrap_or_default(),
        amendment_id,
        kind,
        text,
    );
    c.db.execute(
        format!(
            "UPDATE {} SET text = $2, updated_at = now() WHERE id = $1",
            c.db.table("motions")
        ),
        vec![SqlValue::Int(motion_id), SqlValue::Text(updated)],
    )
    .await?;
    Ok(())
}

// --- request bodies ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MeetingBody {
    body: String,
    title: String,
    #[serde(default)]
    scheduled_for: Option<String>,
    #[serde(default)]
    lodge_id: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    quorum_basis: Option<String>,
    #[serde(default)]
    expected_voters: Option<i64>,
    #[serde(default)]
    quorum_required: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct MeetingEditBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    scheduled_for: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    quorum_basis: Option<String>,
    #[serde(default)]
    expected_voters: Option<i64>,
    #[serde(default)]
    quorum_required: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AttendanceBody {
    member_id: String,
    #[serde(default)]
    present: Option<bool>,
    #[serde(default)]
    method: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MinutesBody {
    #[serde(default)]
    minutes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MotionBody {
    title: String,
    text: String,
    body: String,
    #[serde(default)]
    meeting_id: Option<i64>,
    #[serde(default)]
    lodge_id: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    threshold: Option<String>,
    #[serde(default)]
    amends_accords: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DebateBody {
    /// `true` opens debate, `false` closes it (moving to a vote).
    open: bool,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VoteBody {
    choice: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NoteBody {
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AmendmentBody {
    /// `friendly` or `formal`.
    kind: String,
    text: String,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AccordsBody {
    /// The **passed Congress motion** that adopted this version.
    motion_id: i64,
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    body_md: Option<String>,
    /// ISO `YYYY-MM-DD` adopting date.
    #[serde(default)]
    adopted_on: Option<String>,
    #[serde(default)]
    congress: Option<String>,
}

// ---------------------------------------------------------------------------
// Plugin declaration: permissions, schema, and the meeting surface
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for GovernancePlugin {
    fn id(&self) -> &str {
        "governance"
    }

    fn name(&self) -> &str {
        "Governance"
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
            Permission::new("governance:read", "View motions, votes, meetings, and Accords versions"),
            Permission::new("governance:propose", "Propose motions"),
            Permission::new("governance:vote", "Vote on motions and formal amendments (when eligible)"),
            Permission::new("governance:amend", "Propose friendly or formal amendments"),
            Permission::new(
                "governance:manage",
                "Run a meeting: attendance, quorum, closing votes, minutes, Accords adoption",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "governance_schema",
            "CREATE TABLE IF NOT EXISTS meetings (\
                 id BIGSERIAL PRIMARY KEY, \
                 body TEXT NOT NULL, \
                 title TEXT NOT NULL, \
                 scheduled_for TIMESTAMPTZ, \
                 status TEXT NOT NULL DEFAULT 'scheduled', \
                 quorum_basis TEXT NOT NULL DEFAULT 'majority_members', \
                 expected_voters INTEGER NOT NULL DEFAULT 0, \
                 quorum_required INTEGER, \
                 lodge_id TEXT, \
                 location TEXT, \
                 minutes TEXT NOT NULL DEFAULT '', \
                 minutes_status TEXT NOT NULL DEFAULT 'none', \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 opened_at TIMESTAMPTZ, \
                 closed_at TIMESTAMPTZ\
             );\
             CREATE INDEX IF NOT EXISTS idx_meetings_body ON meetings(body, status);\
             CREATE TABLE IF NOT EXISTS attendance (\
                 id BIGSERIAL PRIMARY KEY, \
                 meeting_id BIGINT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE, \
                 member_id TEXT NOT NULL, \
                 present BOOLEAN NOT NULL DEFAULT true, \
                 method TEXT NOT NULL DEFAULT 'present', \
                 recorded_by TEXT NOT NULL, \
                 recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 UNIQUE (meeting_id, member_id)\
             );\
             CREATE TABLE IF NOT EXISTS motions (\
                 id BIGSERIAL PRIMARY KEY, \
                 meeting_id BIGINT REFERENCES meetings(id) ON DELETE SET NULL, \
                 title TEXT NOT NULL, \
                 text TEXT NOT NULL, \
                 body TEXT NOT NULL, \
                 lodge_id TEXT, \
                 category TEXT NOT NULL DEFAULT 'general', \
                 stage TEXT NOT NULL DEFAULT 'proposed', \
                 result TEXT NOT NULL DEFAULT 'pending', \
                 threshold TEXT NOT NULL DEFAULT 'simple_majority', \
                 amends_accords BOOLEAN NOT NULL DEFAULT false, \
                 proposed_by TEXT NOT NULL, \
                 seconded_by TEXT, \
                 seconded_at TIMESTAMPTZ, \
                 debate_opened_at TIMESTAMPTZ, \
                 debate_closed_at TIMESTAMPTZ, \
                 decided_at TIMESTAMPTZ, \
                 implemented_at TIMESTAMPTZ, \
                 implemented_by TEXT, \
                 implementation_note TEXT NOT NULL DEFAULT '', \
                 votes_yes INTEGER NOT NULL DEFAULT 0, \
                 votes_no INTEGER NOT NULL DEFAULT 0, \
                 votes_abstain INTEGER NOT NULL DEFAULT 0, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_motions_meeting ON motions(meeting_id);\
             CREATE INDEX IF NOT EXISTS idx_motions_stage ON motions(stage, result);\
             CREATE TABLE IF NOT EXISTS amendments (\
                 id BIGSERIAL PRIMARY KEY, \
                 motion_id BIGINT NOT NULL REFERENCES motions(id) ON DELETE CASCADE, \
                 kind TEXT NOT NULL, \
                 text TEXT NOT NULL, \
                 rationale TEXT NOT NULL DEFAULT '', \
                 proposed_by TEXT NOT NULL, \
                 status TEXT NOT NULL DEFAULT 'proposed', \
                 decided_by TEXT, \
                 decided_at TIMESTAMPTZ, \
                 applied_at TIMESTAMPTZ, \
                 note TEXT NOT NULL DEFAULT '', \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_amendments_motion ON amendments(motion_id);\
             CREATE TABLE IF NOT EXISTS votes (\
                 id BIGSERIAL PRIMARY KEY, \
                 motion_id BIGINT NOT NULL REFERENCES motions(id) ON DELETE CASCADE, \
                 amendment_id BIGINT REFERENCES amendments(id) ON DELETE CASCADE, \
                 voter TEXT NOT NULL, \
                 choice TEXT NOT NULL, \
                 method TEXT NOT NULL DEFAULT 'voice', \
                 recorded_by TEXT NOT NULL, \
                 recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 note TEXT NOT NULL DEFAULT ''\
             );\
             CREATE UNIQUE INDEX IF NOT EXISTS idx_votes_unique \
               ON votes (motion_id, COALESCE(amendment_id, 0), voter);\
             CREATE TABLE IF NOT EXISTS accords_versions (\
                 id BIGSERIAL PRIMARY KEY, \
                 version INTEGER NOT NULL UNIQUE, \
                 title TEXT NOT NULL, \
                 summary TEXT NOT NULL DEFAULT '', \
                 body_md TEXT NOT NULL DEFAULT '', \
                 status TEXT NOT NULL DEFAULT 'draft', \
                 adopted_on DATE, \
                 congress TEXT, \
                 source_motion_id BIGINT REFERENCES motions(id) ON DELETE SET NULL, \
                 supersedes_id BIGINT REFERENCES accords_versions(id) ON DELETE SET NULL, \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- meetings ---------------------------------------------------------
        let c = ctx.clone();
        let create_meeting = RouteDefinition::post_protected(
            "/api/governance/meeting",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: MeetingBody = req.json()?;
                    let meeting_body = body.body.trim().to_ascii_lowercase();
                    if !BODIES.contains(&meeting_body.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("body must be one of {}", BODIES.join(", ")),
                        );
                    }
                    if body.title.trim().is_empty() {
                        return PluginResponse::error(400, "title is required");
                    }
                    let basis = body
                        .quorum_basis
                        .as_deref()
                        .map(|b| b.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| QUORUM_MAJORITY_MEMBERS.to_string());
                    if !QUORUM_BASES.contains(&basis.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("quorum_basis must be one of {}", QUORUM_BASES.join(", ")),
                        );
                    }
                    if let Some(expected) = body.expected_voters {
                        if expected < 0 {
                            return PluginResponse::error(400, "expected_voters must not be negative");
                        }
                    }
                    if let Some(when) = body.scheduled_for.as_deref() {
                        if !when.trim().is_empty() && !is_timestamp_like(when) {
                            return PluginResponse::error(
                                400,
                                format!("scheduled_for must be an ISO timestamp, got {when:?}"),
                            );
                        }
                    }
                    let creator = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (body, title, scheduled_for, lodge_id, location, \
                                                 quorum_basis, expected_voters, quorum_required, created_by) \
                                 VALUES ($1, $2, $3::timestamptz, $4, $5, $6, COALESCE($7, 0), $8, $9) \
                                 RETURNING id, body, title, status, quorum_basis, expected_voters, \
                                           quorum_required, created_at::text AS created_at",
                                c.db.table("meetings")
                            ),
                            vec![
                                SqlValue::Text(meeting_body.clone()),
                                SqlValue::Text(body.title.trim().to_string()),
                                body.scheduled_for.clone().into(),
                                body.lodge_id.clone().into(),
                                body.location.clone().into(),
                                SqlValue::Text(basis),
                                body.expected_voters.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                body.quorum_required.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(creator.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "meeting.create",
                            "meeting",
                            &row["id"].as_i64().unwrap_or_default().to_string(),
                            serde_json::json!({ "body": meeting_body, "title": body.title }),
                        )
                        .await?;
                    PluginResponse::json(201, &serde_json::json!({ "meeting": row }))
                }
            }),
        );

        let c = ctx.clone();
        let list_meetings = RouteDefinition::get_protected(
            "/api/governance/meetings",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT m.id, m.body, m.title, m.status, m.location, m.lodge_id, \
                                        m.scheduled_for::text AS scheduled_for, \
                                        m.opened_at::text AS opened_at, \
                                        m.closed_at::text AS closed_at, \
                                        m.quorum_basis, m.expected_voters, m.quorum_required, \
                                        m.minutes_status, \
                                        (SELECT COUNT(*) FROM {a} a WHERE a.meeting_id = m.id AND a.present) AS present, \
                                        (SELECT COUNT(*) FROM {mo} mo WHERE mo.meeting_id = m.id) AS motions \
                                 FROM {me} m \
                                 WHERE ($1::text IS NULL OR m.body = $1) \
                                   AND ($2::text IS NULL OR m.status = $2) \
                                   AND ($3::text IS NULL OR m.lodge_id = $3) \
                                 ORDER BY m.scheduled_for DESC NULLS LAST, m.id DESC \
                                 LIMIT $4",
                                me = c.db.table("meetings"),
                                a = c.db.table("attendance"),
                                mo = c.db.table("motions")
                            ),
                            vec![
                                req.query_param("body").map(String::from).into(),
                                req.query_param("status").map(String::from).into(),
                                req.query_param("lodge").map(String::from).into(),
                                SqlValue::Int(req.query_int("limit").unwrap_or(50).clamp(1, 200)),
                            ],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "meetings": rows }))
                }
            }),
        );

        // One meeting, with its live quorum numbers.
        let c = ctx.clone();
        let get_meeting = RouteDefinition::get_protected(
            "/api/governance/meeting/{id}",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    let (required, present, met) = quorum_state(&c, &meeting).await?;
                    let motions = c
                        .db
                        .query(
                            format!(
                                "SELECT id, title, text, body, category, stage, result, threshold, \
                                        amends_accords, proposed_by, seconded_by, \
                                        votes_yes, votes_no, votes_abstain, \
                                        decided_at::text AS decided_at, \
                                        implemented_at::text AS implemented_at, \
                                        implementation_note \
                                 FROM {} WHERE meeting_id = $1 ORDER BY id",
                                c.db.table("motions")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "meeting": meeting,
                            "quorum": {
                                "required": required,
                                "present": present,
                                "expected": meeting["expected_voters"],
                                "basis": meeting["quorum_basis"],
                                "met": met,
                            },
                            "motions": motions,
                        }),
                    )
                }
            }),
        );

        // Configure a meeting before it opens (quorum basis, expected voters).
        let c = ctx.clone();
        let edit_meeting = RouteDefinition::patch_protected(
            "/api/governance/meeting/{id}",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: MeetingEditBody = req.json()?;
                    if let Some(basis) = body.quorum_basis.as_deref() {
                        if !QUORUM_BASES.contains(&basis.trim()) {
                            return PluginResponse::error(
                                400,
                                format!("quorum_basis must be one of {}", QUORUM_BASES.join(", ")),
                            );
                        }
                    }
                    if let Some(expected) = body.expected_voters {
                        if expected < 0 {
                            return PluginResponse::error(400, "expected_voters must not be negative");
                        }
                    }
                    if let Some(when) = body.scheduled_for.as_deref() {
                        if !when.trim().is_empty() && !is_timestamp_like(when) {
                            return PluginResponse::error(
                                400,
                                format!("scheduled_for must be an ISO timestamp, got {when:?}"),
                            );
                        }
                    }
                    let rows = c
                        .db
                        .query(
                            format!(
                                "UPDATE {} SET \
                                   title = COALESCE($2, title), \
                                   scheduled_for = COALESCE($3::timestamptz, scheduled_for), \
                                   location = COALESCE($4, location), \
                                   quorum_basis = COALESCE($5, quorum_basis), \
                                   expected_voters = COALESCE($6, expected_voters), \
                                   quorum_required = COALESCE($7, quorum_required) \
                                 WHERE id = $1 \
                                 RETURNING id, title, quorum_basis, expected_voters, quorum_required",
                                c.db.table("meetings")
                            ),
                            vec![
                                SqlValue::Int(id),
                                body.title.clone().into(),
                                body.scheduled_for.clone().into(),
                                body.location.clone().into(),
                                body.quorum_basis.clone().into(),
                                body.expected_voters.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                body.quorum_required.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    match rows.first() {
                        Some(row) => PluginResponse::json(200, &serde_json::json!({ "meeting": row })),
                        None => PluginResponse::error(404, "no such meeting"),
                    }
                }
            }),
        );

        let c = ctx.clone();
        let open_meeting = RouteDefinition::post_protected(
            "/api/governance/meeting/{id}/open",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    if meeting["status"].as_str() == Some(MEETING_CLOSED) {
                        return PluginResponse::error(409, "this meeting is closed");
                    }
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = 'open', opened_at = COALESCE(opened_at, now()) \
                             WHERE id = $1",
                            c.db.table("meetings")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    let (required, present, met) = quorum_state(&c, &meeting).await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "meeting.open",
                            "meeting",
                            &id.to_string(),
                            serde_json::json!({ "present": present, "required": required }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "id": id,
                            "status": MEETING_OPEN,
                            "quorum": { "required": required, "present": present, "met": met },
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let close_meeting = RouteDefinition::post_protected(
            "/api/governance/meeting/{id}/close",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    if meeting["status"].as_str() == Some(MEETING_CLOSED) {
                        return PluginResponse::error(409, "this meeting is already closed");
                    }
                    // Closing a meeting with an undecided motion is allowed, but the
                    // response says so — the record should not look tidy when it is not.
                    let undecided = c
                        .db
                        .query_one(
                            format!(
                                "SELECT COUNT(*) AS n FROM {} WHERE meeting_id = $1 \
                                 AND result = 'pending' AND stage <> 'withdrawn'",
                                c.db.table("motions")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?
                        .and_then(|row| row["n"].as_i64())
                        .unwrap_or(0);
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = 'closed', closed_at = now() WHERE id = $1",
                            c.db.table("meetings")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "meeting.close",
                            "meeting",
                            &id.to_string(),
                            serde_json::json!({ "undecided_motions": undecided }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "id": id,
                            "status": MEETING_CLOSED,
                            "undecided_motions": undecided,
                        }),
                    )
                }
            }),
        );

        // --- attendance & quorum ---------------------------------------------
        let c = ctx.clone();
        let mark_attendance = RouteDefinition::post_protected(
            "/api/governance/meeting/{id}/attendance",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: AttendanceBody = req.json()?;
                    if body.member_id.trim().is_empty() {
                        return PluginResponse::error(400, "member_id is required");
                    }
                    let method = body
                        .method
                        .as_deref()
                        .map(|m| m.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "present".to_string());
                    if !["present", "remote", "proxy", "absent"].contains(&method.as_str()) {
                        return PluginResponse::error(
                            400,
                            "method must be 'present', 'remote', 'proxy', or 'absent'",
                        );
                    }
                    let recorder = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let Some(_meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (meeting_id, member_id, present, method, recorded_by) \
                                 VALUES ($1, $2, COALESCE($3, true), $4, $5) \
                                 ON CONFLICT (meeting_id, member_id) DO UPDATE SET \
                                   present = COALESCE($3, {a}.present), \
                                   method = EXCLUDED.method, \
                                   recorded_by = EXCLUDED.recorded_by, \
                                   recorded_at = now() \
                                 RETURNING meeting_id, member_id, present, method",
                                c.db.table("attendance"),
                                a = c.db.table("attendance")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(body.member_id.trim().to_string()),
                                body.present.map(SqlValue::Bool).unwrap_or(SqlValue::NullBool),
                                SqlValue::Text(method),
                                SqlValue::Text(recorder),
                            ],
                        )
                        .await?;
                    let (required, present, met) = match fetch_meeting(&c, id).await? {
                        Some(meeting) => quorum_state(&c, &meeting).await?,
                        None => (0, 0, false),
                    };
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "attendance": row,
                            "quorum": { "required": required, "present": present, "met": met },
                        }),
                    )
                }
            }),
        );

        // The real-time quorum display for the room.
        let c = ctx.clone();
        let quorum = RouteDefinition::get_protected(
            "/api/governance/meeting/{id}/quorum",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    let (required, present, met) = quorum_state(&c, &meeting).await?;
                    let basis = meeting["quorum_basis"].as_str().unwrap_or_default();
                    let expected = meeting["expected_voters"].as_i64().unwrap_or(0);
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "meeting_id": id,
                            "status": meeting["status"],
                            "quorum": {
                                "required": required,
                                "present": present,
                                "expected": expected,
                                "basis": basis,
                                "met": met,
                                "short": (required - present).max(0),
                                "basis_configured": required > 0,
                            },
                        }),
                    )
                }
            }),
        );

        // --- minutes ----------------------------------------------------------
        let c = ctx.clone();
        let draft_minutes = RouteDefinition::post_protected(
            "/api/governance/meeting/{id}/minutes/draft",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    let attendance = c
                        .db
                        .query(
                            format!(
                                "SELECT member_id, present, method FROM {} \
                                 WHERE meeting_id = $1 ORDER BY member_id",
                                c.db.table("attendance")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let motions = c
                        .db
                        .query(
                            format!(
                                "SELECT id, title, text, stage, result, threshold, proposed_by, \
                                        seconded_by, amends_accords, votes_yes, votes_no, \
                                        votes_abstain, implementation_note \
                                 FROM {} WHERE meeting_id = $1 ORDER BY id",
                                c.db.table("motions")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let amendments = c
                        .db
                        .query(
                            format!(
                                "SELECT a.id, a.motion_id, a.kind, a.text, a.status, a.proposed_by \
                                 FROM {a} a JOIN {m} m ON m.id = a.motion_id \
                                 WHERE m.meeting_id = $1 ORDER BY a.id",
                                a = c.db.table("amendments"),
                                m = c.db.table("motions")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let votes = c
                        .db
                        .query(
                            format!(
                                "SELECT v.id, v.motion_id, v.amendment_id, v.voter, v.choice, v.method \
                                 FROM {v} v JOIN {m} m ON m.id = v.motion_id \
                                 WHERE m.meeting_id = $1 ORDER BY v.id",
                                v = c.db.table("votes"),
                                m = c.db.table("motions")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let minutes = render_minutes(&meeting, &attendance, &motions, &amendments, &votes);
                    c.db.execute(
                        format!(
                            "UPDATE {} SET minutes = $2, minutes_status = 'draft' WHERE id = $1",
                            c.db.table("meetings")
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(minutes.clone())],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "meeting.minutes.draft",
                            "meeting",
                            &id.to_string(),
                            serde_json::json!({ "motions": motions.len(), "amendments": amendments.len() }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({ "meeting_id": id, "minutes_status": "draft", "minutes": minutes }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let adopt_minutes = RouteDefinition::post_protected(
            "/api/governance/meeting/{id}/minutes/adopt",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: MinutesBody = req.json()?;
                    let Some(meeting) = fetch_meeting(&c, id).await? else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    let minutes = match body.minutes.clone() {
                        Some(m) if !m.trim().is_empty() => m,
                        _ => {
                            let existing = meeting["minutes"].as_str().unwrap_or_default().to_string();
                            if existing.trim().is_empty() {
                                return PluginResponse::error(
                                    400,
                                    "there is no minutes draft to adopt — draft or supply one \
                                     (POST /api/governance/meeting/{id}/minutes/draft)",
                                );
                            }
                            existing
                        }
                    };
                    c.db.execute(
                        format!(
                            "UPDATE {} SET minutes = $2, minutes_status = 'adopted' WHERE id = $1",
                            c.db.table("meetings")
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(minutes.clone())],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "meeting.minutes.adopt",
                            "meeting",
                            &id.to_string(),
                            serde_json::json!({ "edited": body.minutes.is_some() }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "meeting_id": id, "minutes_status": "adopted" }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let get_minutes = RouteDefinition::get_protected(
            "/api/governance/meeting/{id}/minutes",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(minutes) = c
                        .db
                        .query_one(
                            format!(
                                "SELECT id, title, minutes, minutes_status FROM {} WHERE id = $1",
                                c.db.table("meetings")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such meeting");
                    };
                    PluginResponse::json(200, &serde_json::json!({ "meeting": minutes }))
                }
            }),
        );

        // --- motions ----------------------------------------------------------
        let c = ctx.clone();
        let propose_motion = RouteDefinition::post_protected(
            "/api/governance/motion",
            "governance:propose",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: MotionBody = req.json()?;
                    if body.title.trim().is_empty() || body.text.trim().is_empty() {
                        return PluginResponse::error(400, "title and text are required");
                    }
                    let motion_body = body.body.trim().to_ascii_lowercase();
                    if !BODIES.contains(&motion_body.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("body must be one of {}", BODIES.join(", ")),
                        );
                    }
                    let category = body
                        .category
                        .as_deref()
                        .map(|c| c.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "general".to_string());
                    if !MOTION_CATEGORIES.contains(&category.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("category must be one of {}", MOTION_CATEGORIES.join(", ")),
                        );
                    }
                    let threshold = body
                        .threshold
                        .as_deref()
                        .map(|t| t.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| THRESHOLD_SIMPLE_MAJORITY.to_string());
                    if !THRESHOLDS.contains(&threshold.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("threshold must be one of {}", THRESHOLDS.join(", ")),
                        );
                    }
                    // A motion attached to a meeting must reference one that exists
                    // — a motion in a meeting that never happened is not a record.
                    if let Some(meeting_id) = body.meeting_id {
                        if fetch_meeting(&c, meeting_id).await?.is_none() {
                            return PluginResponse::error(404, "no such meeting");
                        }
                    }
                    let proposer = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (meeting_id, title, text, body, lodge_id, category, \
                                                 threshold, amends_accords, proposed_by) \
                                 VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, false), $9) \
                                 RETURNING id, title, stage, result, threshold, \
                                           created_at::text AS created_at",
                                c.db.table("motions")
                            ),
                            vec![
                                body.meeting_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(body.title.trim().to_string()),
                                SqlValue::Text(body.text.trim().to_string()),
                                SqlValue::Text(motion_body.clone()),
                                body.lodge_id.clone().into(),
                                SqlValue::Text(category),
                                SqlValue::Text(threshold),
                                body.amends_accords.map(SqlValue::Bool).unwrap_or(SqlValue::NullBool),
                                SqlValue::Text(proposer.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    let id = row["id"].as_i64().unwrap_or_default();
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.propose",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({
                                "title": body.title,
                                "body": motion_body,
                                "amends_accords": body.amends_accords,
                            }),
                        )
                        .await?;
                    c.events
                        .publish(
                            event_type::MOTION_PROPOSED,
                            serde_json::json!({
                                "motion_id": id,
                                "title": body.title,
                                "body": motion_body,
                                "meeting_id": body.meeting_id,
                                "proposed_by": proposer,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "motion": row,
                            "next": "a second is required before debate (POST /api/governance/motion/{id}/second)",
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let list_motions = RouteDefinition::get_protected(
            "/api/governance/motions",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT id, meeting_id, title, body, category, stage, result, \
                                        threshold, amends_accords, proposed_by, seconded_by, \
                                        votes_yes, votes_no, votes_abstain, \
                                        decided_at::text AS decided_at, \
                                        implemented_at::text AS implemented_at, \
                                        created_at::text AS created_at \
                                 FROM {} \
                                 WHERE ($1::bigint IS NULL OR meeting_id = $1) \
                                   AND ($2::text IS NULL OR body = $2) \
                                   AND ($3::text IS NULL OR stage = $3) \
                                   AND ($4::text IS NULL OR result = $4) \
                                   AND ($5::text IS NULL OR category = $5) \
                                 ORDER BY id DESC LIMIT $6",
                                c.db.table("motions")
                            ),
                            vec![
                                req.query_int("meeting").map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                req.query_param("body").map(String::from).into(),
                                req.query_param("stage").map(String::from).into(),
                                req.query_param("result").map(String::from).into(),
                                req.query_param("category").map(String::from).into(),
                                SqlValue::Int(req.query_int("limit").unwrap_or(50).clamp(1, 200)),
                            ],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "motions": rows }))
                }
            }),
        );

        // One motion: the record, its votes, its amendments, and the tally the
        // close would produce right now (the "where do the numbers stand?" view).
        let c = ctx.clone();
        let get_motion = RouteDefinition::get_protected(
            "/api/governance/motion/{id}",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    let votes = votes_for(&c, id, None).await?;
                    let threshold = motion["threshold"].as_str().unwrap_or(THRESHOLD_SIMPLE_MAJORITY);
                    let tally = tally_votes(&votes, threshold);
                    let amendments = c
                        .db
                        .query(
                            format!(
                                "SELECT id, kind, text, rationale, proposed_by, status, \
                                        decided_by, decided_at::text AS decided_at, \
                                        applied_at::text AS applied_at, note \
                                 FROM {} WHERE motion_id = $1 ORDER BY id",
                                c.db.table("amendments")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let quorum = match motion["meeting_id"].as_i64() {
                        Some(meeting_id) => match fetch_meeting(&c, meeting_id).await? {
                            Some(meeting) => {
                                let (required, present, met) = quorum_state(&c, &meeting).await?;
                                serde_json::json!({
                                    "meeting_id": meeting_id,
                                    "required": required,
                                    "present": present,
                                    "met": met,
                                })
                            }
                            None => serde_json::Value::Null,
                        },
                        None => serde_json::Value::Null,
                    };
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "motion": motion,
                            "votes": votes,
                            "amendments": amendments,
                            "tally": {
                                "yes": tally.yes,
                                "no": tally.no,
                                "abstain": tally.abstain,
                                "cast": tally.cast(),
                                "would_pass": tally.passed,
                                "threshold": threshold,
                            },
                            "quorum": quorum,
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let second_motion = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/second",
            "governance:vote",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["proposed"],
                        "second the motion",
                    )?;
                    let seconder = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    if motion["proposed_by"].as_str() == Some(seconder.as_str()) {
                        return PluginResponse::error(
                            409,
                            "the mover cannot second their own motion — a second is a second member's \
                             agreement that it should be debated",
                        );
                    }
                    require_present(&c, motion["meeting_id"].as_i64(), &seconder).await?;
                    c.db.execute(
                        format!(
                            "UPDATE {} SET stage = 'seconded', seconded_by = $2, seconded_at = now(), \
                                    updated_at = now() WHERE id = $1",
                            c.db.table("motions")
                        ),
                        vec![SqlValue::Int(id), SqlValue::Text(seconder.clone())],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.second",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({}),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": "seconded", "seconded_by": seconder }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let debate_motion = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/debate",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: DebateBody = req.json()?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    let stage = motion["stage"].as_str().unwrap_or_default();
                    let (expected, next) = if body.open {
                        (vec!["seconded"], "debate")
                    } else {
                        (vec!["debate"], "voting")
                    };
                    require_motion_stage(
                        stage,
                        &expected,
                        if body.open { "open debate" } else { "close debate and put it to a vote" },
                    )?;
                    let assignment = if body.open {
                        "stage = 'debate', debate_opened_at = now()"
                    } else {
                        "stage = 'voting', debate_closed_at = now()"
                    };
                    c.db.execute(
                        format!(
                            "UPDATE {} SET {assignment}, updated_at = now() WHERE id = $1",
                            c.db.table("motions")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            if body.open { "motion.debate.open" } else { "motion.debate.close" },
                            "motion",
                            &id.to_string(),
                            serde_json::json!({ "note": body.note }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": next, "note": body.note }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let record_vote = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/vote",
            "governance:vote",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: VoteBody = req.json()?;
                    let choice = body.choice.trim().to_ascii_lowercase();
                    if !VOTE_CHOICES.contains(&choice.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("choice must be one of {}", VOTE_CHOICES.join(", ")),
                        );
                    }
                    let method = body
                        .method
                        .as_deref()
                        .map(|m| m.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "voice".to_string());
                    if !VOTE_METHODS.contains(&method.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("method must be one of {}", VOTE_METHODS.join(", ")),
                        );
                    }
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["seconded", "debate", "voting"],
                        "vote on the motion",
                    )?;
                    let voter = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    require_present(&c, motion["meeting_id"].as_i64(), &voter).await?;
                    // One vote per voter. A re-record is a conflict, not an edit:
                    // silently replacing a recorded vote is how a tally stops
                    // matching the room.
                    let inserted = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (motion_id, voter, choice, method, recorded_by, note) \
                                 VALUES ($1, $2, $3, $4, $2, COALESCE($5, '')) \
                                 ON CONFLICT (motion_id, COALESCE(amendment_id, 0), voter) DO NOTHING \
                                 RETURNING id",
                                c.db.table("votes")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(voter.clone()),
                                SqlValue::Text(choice.clone()),
                                SqlValue::Text(method.clone()),
                                body.note.clone().into(),
                            ],
                        )
                        .await?;
                    let Some(row) = inserted else {
                        return PluginResponse::error(409, "you have already voted on this motion");
                    };
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.vote",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({ "choice": choice, "method": method }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "vote_id": row["id"],
                            "motion_id": id,
                            "choice": choice,
                            "method": method,
                        }),
                    )
                }
            }),
        );

        // The tally: quorum first, then the threshold (SPEC §7.4 + Art 5).
        let c = ctx.clone();
        let close_motion = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/close",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["seconded", "debate", "voting"],
                        "close the vote",
                    )?;
                    // A motion in a meeting needs a quorum; the numbers are in
                    // the refusal so the room knows how many are missing.
                    let quorum = match motion["meeting_id"].as_i64() {
                        Some(meeting_id) => match fetch_meeting(&c, meeting_id).await? {
                            Some(meeting) => {
                                let (required, present, met) = quorum_state(&c, &meeting).await?;
                                if !met {
                                    return PluginResponse::error(
                                        409,
                                        format!(
                                            "no quorum at meeting {meeting_id}: {present} present, \
                                             {required} required — the motion cannot be decided"
                                        ),
                                    );
                                }
                                Some((required, present))
                            }
                            None => None,
                        },
                        None => None,
                    };
                    let votes = votes_for(&c, id, None).await?;
                    let threshold = motion["threshold"].as_str().unwrap_or(THRESHOLD_SIMPLE_MAJORITY);
                    let tally = tally_votes(&votes, threshold);
                    let result = if tally.passed { RESULT_PASSED } else { RESULT_FAILED };
                    c.db.execute(
                        format!(
                            "UPDATE {} SET stage = 'decided', result = $2, votes_yes = $3, \
                                    votes_no = $4, votes_abstain = $5, decided_at = now(), \
                                    updated_at = now() WHERE id = $1",
                            c.db.table("motions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(result.to_string()),
                            SqlValue::Int(tally.yes),
                            SqlValue::Int(tally.no),
                            SqlValue::Int(tally.abstain),
                        ],
                    )
                    .await?;
                    let title = motion["title"].as_str().unwrap_or_default().to_string();
                    let motion_body = motion["body"].as_str().unwrap_or_default().to_string();
                    let amends_accords = motion["amends_accords"].as_bool().unwrap_or(false);
                    let meeting_id = motion["meeting_id"].as_i64();
                    let passed_at = chrono::Utc::now();
                    if tally.passed {
                        c.events
                            .publish_motion_passed(&MotionPassed {
                                motion_id: id,
                                title: title.clone(),
                                body: motion_body.clone(),
                                meeting_id,
                                votes_yes: tally.yes,
                                votes_no: tally.no,
                                votes_abstain: tally.abstain,
                                threshold: threshold.to_string(),
                                passed_at,
                                amends_accords,
                            })
                            .await?;
                    } else {
                        c.events
                            .publish_motion_failed(&MotionFailed {
                                motion_id: id,
                                title: title.clone(),
                                body: motion_body.clone(),
                                meeting_id,
                                votes_yes: tally.yes,
                                votes_no: tally.no,
                                votes_abstain: tally.abstain,
                                threshold: threshold.to_string(),
                                failed_at: passed_at,
                                amends_accords,
                            })
                            .await?;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.close",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({
                                "result": result,
                                "yes": tally.yes,
                                "no": tally.no,
                                "abstain": tally.abstain,
                                "threshold": threshold,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "id": id,
                            "result": result,
                            "stage": "decided",
                            "votes_yes": tally.yes,
                            "votes_no": tally.no,
                            "votes_abstain": tally.abstain,
                            "threshold": threshold,
                            "quorum": quorum.map(|(required, present)| serde_json::json!({
                                "required": required, "present": present, "met": true
                            })),
                            "next": if tally.passed && amends_accords {
                                Some("a passed Congress motion that amends the Accords can now \
                                      create a new version (POST /api/governance/accords/adopt)")
                            } else if tally.passed {
                                Some("implement it (POST /api/governance/motion/{id}/implement)")
                            } else {
                                None
                            },
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let implement_motion = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/implement",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: NoteBody = req.json()?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    if motion["result"].as_str() != Some(RESULT_PASSED) {
                        return PluginResponse::error(
                            409,
                            "only a motion that passed can be implemented",
                        );
                    }
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["decided"],
                        "implement the motion",
                    )?;
                    let implementer = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    c.db.execute(
                        format!(
                            "UPDATE {} SET stage = 'implemented', implemented_at = now(), \
                                    implemented_by = $2, implementation_note = COALESCE($3, ''), \
                                    updated_at = now() WHERE id = $1",
                            c.db.table("motions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(implementer),
                            body.note.clone().into(),
                        ],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.implement",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({ "note": body.note }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": "implemented", "note": body.note }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let withdraw_motion = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/withdraw",
            "governance:propose",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: NoteBody = req.json()?;
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["proposed", "seconded", "debate", "voting"],
                        "withdraw the motion",
                    )?;
                    let caller = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    if motion["proposed_by"].as_str() != Some(caller.as_str())
                        && !c
                            .permissions
                            .has_in_scope(req.identity.as_ref(), "governance:manage", &Scope::troop())
                            .await
                    {
                        return PluginResponse::error(
                            403,
                            "only the mover (or a chair with governance:manage) can withdraw a motion",
                        );
                    }
                    c.db.execute(
                        format!(
                            "UPDATE {} SET stage = 'withdrawn', updated_at = now() WHERE id = $1",
                            c.db.table("motions")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "motion.withdraw",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({ "note": body.note }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": "withdrawn", "note": body.note }),
                    )
                }
            }),
        );

        // --- amendments -------------------------------------------------------
        let c = ctx.clone();
        let propose_amendment = RouteDefinition::post_protected(
            "/api/governance/motion/{id}/amendment",
            "governance:amend",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: AmendmentBody = req.json()?;
                    let kind = body.kind.trim().to_ascii_lowercase();
                    if ![AMENDMENT_FRIENDLY, AMENDMENT_FORMAL].contains(&kind.as_str()) {
                        return PluginResponse::error(
                            400,
                            "kind must be 'friendly' (accepted by the mover) or 'formal' (voted on)",
                        );
                    }
                    if body.text.trim().is_empty() {
                        return PluginResponse::error(400, "text is required");
                    }
                    let Some(motion) = fetch_motion(&c, id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["proposed", "seconded", "debate", "voting"],
                        "amend the motion",
                    )?;
                    let proposer = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (motion_id, kind, text, rationale, proposed_by) \
                                 VALUES ($1, $2, $3, COALESCE($4, ''), $5) \
                                 RETURNING id, motion_id, kind, status, text",
                                c.db.table("amendments")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(kind.clone()),
                                SqlValue::Text(body.text.trim().to_string()),
                                body.rationale.clone().into(),
                                SqlValue::Text(proposer),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "amendment.propose",
                            "motion",
                            &id.to_string(),
                            serde_json::json!({ "kind": kind, "amendment_id": row["id"] }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "amendment": row,
                            "next": if kind == AMENDMENT_FRIENDLY {
                                "the mover accepts or declines it (POST /api/governance/amendment/{id}/accept)"
                            } else {
                                "it is voted on (POST /api/governance/amendment/{id}/vote)"
                            },
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let accept_amendment = RouteDefinition::post_protected(
            "/api/governance/amendment/{id}/accept",
            "governance:amend",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let amendment_id = req.int_param("id")?;
                    let body: NoteBody = req.json()?;
                    let Some(amendment) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("amendments")),
                            vec![SqlValue::Int(amendment_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such amendment");
                    };
                    let kind = amendment["kind"].as_str().unwrap_or_default();
                    if kind != AMENDMENT_FRIENDLY {
                        return PluginResponse::error(
                            409,
                            "a formal amendment is decided by vote, not by the mover's acceptance \
                             (POST /api/governance/amendment/{id}/close)",
                        );
                    }
                    if amendment["status"].as_str() != Some("proposed") {
                        return PluginResponse::error(409, "this amendment is already decided");
                    }
                    let motion_id = amendment["motion_id"].as_i64().unwrap_or_default();
                    let Some(motion) = fetch_motion(&c, motion_id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    // A friendly amendment is the mover's to accept — or the
                    // chair's, if the mover is not present to say.
                    let caller = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let is_mover = motion["proposed_by"].as_str() == Some(caller.as_str());
                    if !is_mover
                        && !c
                            .permissions
                            .has_in_scope(req.identity.as_ref(), "governance:manage", &Scope::troop())
                            .await
                    {
                        return PluginResponse::error(
                            403,
                            "a friendly amendment is accepted by the motion's mover, or by a chair \
                             with governance:manage",
                        );
                    }
                    let text = amendment["text"].as_str().unwrap_or_default();
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = 'accepted', decided_by = $2, decided_at = now(), \
                                    applied_at = now(), note = COALESCE($3, '') WHERE id = $1",
                            c.db.table("amendments")
                        ),
                        vec![
                            SqlValue::Int(amendment_id),
                            SqlValue::Text(caller),
                            body.note.clone().into(),
                        ],
                    )
                    .await?;
                    apply_amendment_to_motion(&c, motion_id, &motion, amendment_id, kind, text)
                        .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "amendment.accept",
                            "motion",
                            &motion_id.to_string(),
                            serde_json::json!({ "amendment_id": amendment_id, "kind": kind }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "amendment_id": amendment_id,
                            "motion_id": motion_id,
                            "status": "accepted",
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let reject_amendment = RouteDefinition::post_protected(
            "/api/governance/amendment/{id}/reject",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let amendment_id = req.int_param("id")?;
                    let body: NoteBody = req.json()?;
                    let decider = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let n = c
                        .db
                        .execute(
                            format!(
                                "UPDATE {} SET status = 'rejected', decided_by = $2, \
                                        decided_at = now(), note = COALESCE($3, '') \
                                 WHERE id = $1 AND status = 'proposed'",
                                c.db.table("amendments")
                            ),
                            vec![
                                SqlValue::Int(amendment_id),
                                SqlValue::Text(decider),
                                body.note.clone().into(),
                            ],
                        )
                        .await?;
                    if n == 0 {
                        return PluginResponse::error(
                            409,
                            "no such amendment, or it is already decided",
                        );
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "amendment.reject",
                            "amendment",
                            &amendment_id.to_string(),
                            serde_json::json!({ "note": body.note }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "amendment_id": amendment_id, "status": "rejected" }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let vote_amendment = RouteDefinition::post_protected(
            "/api/governance/amendment/{id}/vote",
            "governance:vote",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let amendment_id = req.int_param("id")?;
                    let body: VoteBody = req.json()?;
                    let choice = body.choice.trim().to_ascii_lowercase();
                    if !VOTE_CHOICES.contains(&choice.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("choice must be one of {}", VOTE_CHOICES.join(", ")),
                        );
                    }
                    let method = body
                        .method
                        .as_deref()
                        .map(|m| m.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "voice".to_string());
                    if !VOTE_METHODS.contains(&method.as_str()) {
                        return PluginResponse::error(
                            400,
                            format!("method must be one of {}", VOTE_METHODS.join(", ")),
                        );
                    }
                    let Some(amendment) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("amendments")),
                            vec![SqlValue::Int(amendment_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such amendment");
                    };
                    if amendment["kind"].as_str() != Some(AMENDMENT_FORMAL) {
                        return PluginResponse::error(
                            409,
                            "a friendly amendment is not voted on — the mover accepts it",
                        );
                    }
                    if amendment["status"].as_str() != Some("proposed") {
                        return PluginResponse::error(409, "this amendment is already decided");
                    }
                    let motion_id = amendment["motion_id"].as_i64().unwrap_or_default();
                    let Some(motion) = fetch_motion(&c, motion_id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    require_motion_stage(
                        motion["stage"].as_str().unwrap_or_default(),
                        &["seconded", "debate", "voting"],
                        "vote on the amendment",
                    )?;
                    let voter = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    require_present(&c, motion["meeting_id"].as_i64(), &voter).await?;
                    let inserted = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (motion_id, amendment_id, voter, choice, method, \
                                                 recorded_by, note) \
                                 VALUES ($1, $2, $3, $4, $5, $3, COALESCE($6, '')) \
                                 ON CONFLICT (motion_id, COALESCE(amendment_id, 0), voter) DO NOTHING \
                                 RETURNING id",
                                c.db.table("votes")
                            ),
                            vec![
                                SqlValue::Int(motion_id),
                                SqlValue::Int(amendment_id),
                                SqlValue::Text(voter),
                                SqlValue::Text(choice.clone()),
                                SqlValue::Text(method.clone()),
                                body.note.clone().into(),
                            ],
                        )
                        .await?;
                    let Some(row) = inserted else {
                        return PluginResponse::error(409, "you have already voted on this amendment");
                    };
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "amendment.vote",
                            "motion",
                            &motion_id.to_string(),
                            serde_json::json!({
                                "amendment_id": amendment_id,
                                "choice": choice,
                                "method": method,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "vote_id": row["id"],
                            "amendment_id": amendment_id,
                            "choice": choice,
                            "method": method,
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let close_amendment = RouteDefinition::post_protected(
            "/api/governance/amendment/{id}/close",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let amendment_id = req.int_param("id")?;
                    let Some(amendment) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("amendments")),
                            vec![SqlValue::Int(amendment_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such amendment");
                    };
                    if amendment["kind"].as_str() != Some(AMENDMENT_FORMAL) {
                        return PluginResponse::error(
                            409,
                            "a friendly amendment is accepted or declined, not tallied \
                             (POST /api/governance/amendment/{id}/accept)",
                        );
                    }
                    if amendment["status"].as_str() != Some("proposed") {
                        return PluginResponse::error(409, "this amendment is already decided");
                    }
                    let motion_id = amendment["motion_id"].as_i64().unwrap_or_default();
                    let Some(motion) = fetch_motion(&c, motion_id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    let votes = votes_for(&c, motion_id, Some(amendment_id)).await?;
                    let threshold = motion["threshold"].as_str().unwrap_or(THRESHOLD_SIMPLE_MAJORITY);
                    let tally = tally_votes(&votes, threshold);
                    let status = if tally.passed { "accepted" } else { "rejected" };
                    let decider = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = $2, decided_by = $3, decided_at = now() \
                             WHERE id = $1",
                            c.db.table("amendments")
                        ),
                        vec![
                            SqlValue::Int(amendment_id),
                            SqlValue::Text(status.to_string()),
                            SqlValue::Text(decider),
                        ],
                    )
                    .await?;
                    if tally.passed {
                        let text = amendment["text"].as_str().unwrap_or_default();
                        apply_amendment_to_motion(
                            &c,
                            motion_id,
                            &motion,
                            amendment_id,
                            AMENDMENT_FORMAL,
                            text,
                        )
                        .await?;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "amendment.close",
                            "motion",
                            &motion_id.to_string(),
                            serde_json::json!({
                                "amendment_id": amendment_id,
                                "status": status,
                                "yes": tally.yes,
                                "no": tally.no,
                                "abstain": tally.abstain,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "amendment_id": amendment_id,
                            "motion_id": motion_id,
                            "status": status,
                            "votes_yes": tally.yes,
                            "votes_no": tally.no,
                            "votes_abstain": tally.abstain,
                            "threshold": threshold,
                        }),
                    )
                }
            }),
        );

        // --- Accords versioning ----------------------------------------------
        let c = ctx.clone();
        let adopt_accords = RouteDefinition::post_protected(
            "/api/governance/accords/adopt",
            "governance:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: AccordsBody = req.json()?;
                    if body.title.trim().is_empty() {
                        return PluginResponse::error(400, "title is required");
                    }
                    if let Some(on) = body.adopted_on.as_deref() {
                        if !on.trim().is_empty() && !is_timestamp_like(on) {
                            return PluginResponse::error(
                                400,
                                format!("adopted_on must be an ISO date (YYYY-MM-DD), got {on:?}"),
                            );
                        }
                    }
                    let Some(motion) = fetch_motion(&c, body.motion_id).await? else {
                        return PluginResponse::error(404, "no such motion");
                    };
                    // "Accords adopted by majority vote at Catamount Congress"
                    // (Art 17) — so the adopting motion must have passed, in a
                    // Congress.
                    if motion["result"].as_str() != Some(RESULT_PASSED) {
                        return PluginResponse::error(
                            400,
                            "an Accords version is created by a motion that passed (Art 17)",
                        );
                    }
                    if motion["body"].as_str() != Some(BODY_CONGRESS) {
                        return PluginResponse::error(
                            400,
                            format!(
                                "the Accords are adopted by a Congress motion; motion {} belongs to \
                                 the {:?} body (Art 17)",
                                body.motion_id, motion["body"]
                            ),
                        );
                    }
                    if c
                        .db
                        .exists(
                            format!(
                                "SELECT 1 FROM {} WHERE source_motion_id = $1",
                                c.db.table("accords_versions")
                            ),
                            vec![SqlValue::Int(body.motion_id)],
                        )
                        .await?
                    {
                        return PluginResponse::error(
                            409,
                            "this motion has already created an Accords version",
                        );
                    }
                    let previous = c
                        .db
                        .query_one(
                            format!(
                                "SELECT id, version FROM {} WHERE status = 'adopted' \
                                 ORDER BY version DESC LIMIT 1",
                                c.db.table("accords_versions")
                            ),
                            vec![],
                        )
                        .await?;
                    let creator = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (version, title, summary, body_md, status, adopted_on, \
                                                 congress, source_motion_id, supersedes_id, created_by) \
                                 VALUES ((SELECT COALESCE(MAX(version), 0) + 1 FROM {t}), $1, \
                                         COALESCE($2, ''), COALESCE($3, ''), 'adopted', \
                                         COALESCE($4::date, current_date), $5, $6, $7, $8) \
                                 RETURNING id, version, title, status, adopted_on::text AS adopted_on",
                                c.db.table("accords_versions"),
                                t = c.db.table("accords_versions")
                            ),
                            vec![
                                SqlValue::Text(body.title.trim().to_string()),
                                body.summary.clone().into(),
                                body.body_md.clone().into(),
                                body.adopted_on.clone().into(),
                                body.congress.clone().into(),
                                SqlValue::Int(body.motion_id),
                                previous
                                    .as_ref()
                                    .and_then(|p| p["id"].as_i64())
                                    .map(SqlValue::Int)
                                    .unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(creator.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    let new_id = row["id"].as_i64().unwrap_or_default();
                    // The newly adopted version supersedes the previous one.
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = 'superseded' WHERE status = 'adopted' \
                             AND id <> $1",
                            c.db.table("accords_versions")
                        ),
                        vec![SqlValue::Int(new_id)],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "accords.adopt",
                            "accords_version",
                            &row["version"].as_i64().unwrap_or_default().to_string(),
                            serde_json::json!({
                                "motion_id": body.motion_id,
                                "title": body.title,
                                "supersedes": previous.as_ref().and_then(|p| p["version"].as_i64()),
                            }),
                        )
                        .await?;
                    c.events
                        .publish(
                            "accords.adopted",
                            serde_json::json!({
                                "version": row["version"],
                                "title": body.title,
                                "motion_id": body.motion_id,
                                "adopted_on": row["adopted_on"],
                                "adopted_by": creator,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "accords": row,
                            "supersedes": previous.and_then(|p| p["version"].as_i64()),
                        }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let list_accords = RouteDefinition::get_protected(
            "/api/governance/accords",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT id, version, title, summary, status, \
                                        adopted_on::text AS adopted_on, congress, \
                                        source_motion_id, supersedes_id, \
                                        created_at::text AS created_at \
                                 FROM {} WHERE ($1::text IS NULL OR status = $1) \
                                 ORDER BY version DESC",
                                c.db.table("accords_versions")
                            ),
                            vec![req.query_param("status").map(String::from).into()],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "accords_versions": rows }))
                }
            }),
        );

        let c = ctx.clone();
        let get_accords = RouteDefinition::get_protected(
            "/api/governance/accords/{version}",
            "governance:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let version = req.int_param("version")?;
                    let Some(row) = c
                        .db
                        .query_one(
                            format!(
                                "SELECT id, version, title, summary, body_md, status, \
                                        adopted_on::text AS adopted_on, congress, \
                                        source_motion_id, supersedes_id, \
                                        created_at::text AS created_at \
                                 FROM {} WHERE version = $1",
                                c.db.table("accords_versions")
                            ),
                            vec![SqlValue::Int(version)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such Accords version");
                    };
                    PluginResponse::json(200, &serde_json::json!({ "accords": row }))
                }
            }),
        );

        vec![
            create_meeting,
            list_meetings,
            get_meeting,
            edit_meeting,
            open_meeting,
            close_meeting,
            mark_attendance,
            quorum,
            draft_minutes,
            adopt_minutes,
            get_minutes,
            propose_motion,
            list_motions,
            get_motion,
            second_motion,
            debate_motion,
            record_vote,
            close_motion,
            implement_motion,
            withdraw_motion,
            propose_amendment,
            accept_amendment,
            reject_amendment,
            vote_amendment,
            close_amendment,
            adopt_accords,
            list_accords,
            get_accords,
        ]
    }
}

export_plugin!(GovernancePlugin);
