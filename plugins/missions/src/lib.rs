//! # adjutant-missions — the six-stage mission lifecycle (SPEC §7.3)
//!
//! The Accords' Mission System (Art 8) in software:
//!
//! > Mission lifecycle: Request → Review (mentorship, scope, success criteria) →
//! > Approval and Consent (logged with TC, Lodge Commander approves) →
//! > Execution → Debrief → Report.
//! >
//! > Lodge Commander rejection of a mission may be appealed to TC (requires
//! > seconding by one other TC voting member; TC votes by simple majority to
//! > overturn).
//!
//! Every stage transition is a route, guarded by the stage the mission is
//! actually in — a request cannot be approved before it has been reviewed, and
//! a debrief cannot be recorded before the mission ran. Transitions are written
//! to `mission_stage_log` and to the core audit log, because "logged with TC"
//! is a requirement of the pathway, not a nicety.
//!
//! ## Permissions
//!
//! | Permission | Who | Reach |
//! |---|---|---|
//! | `missions:read` | everyone | troop, else own lodge / own mission |
//! | `missions:create` | any scout | the lodge the mission is proposed for |
//! | `missions:update` | the mission's lead, its mentor, or its approver | object scope |
//! | `missions:approve` | Lodge Commander (lodge grant), TC (troop grant) | object scope |
//! | `missions:appeal` | the proposer of a rejected mission | troop |
//! | `missions:mentor` | a mentor | own profile, missions they mentor |
//!
//! `missions:mentor` is an M4 addition to SPEC §9.1's taxonomy: mentor matching
//! needs a permission that a plain scout can hold without also holding
//! `missions:update` over every mission.
//!
//! ## Schema
//!
//! `missions`, `milestones`, and `mentorships` are SPEC §7.3's tables.
//! `mentor_profiles` (the matching registry), `progress_notes` (execution
//! progress), `mission_appeals` (Art 8's appeal), and `mission_stage_log` (the
//! lifecycle trail) support the required behaviour and are documented here
//! rather than folded into the three.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// The lifecycle (Accords Art 8 / SPEC §7.3)
// ---------------------------------------------------------------------------

/// Stage 1 — the proposal.
pub const STAGE_REQUEST: &str = "request";
/// Stage 2 — mentorship, scope, and success criteria are settled.
pub const STAGE_REVIEW: &str = "review";
/// Stage 3 — the Lodge Commander approves (or rejects with guidance).
pub const STAGE_APPROVAL: &str = "approval";
/// Stage 4 — the mission runs; milestones and progress are recorded.
pub const STAGE_EXECUTION: &str = "execution";
/// Stage 5 — the debrief.
pub const STAGE_DEBRIEF: &str = "debrief";
/// Stage 6 — the report that feeds the cumulative Impact Report.
pub const STAGE_REPORT: &str = "report";

/// The six stages, in order.
pub const STAGES: [&str; 6] = [
    STAGE_REQUEST,
    STAGE_REVIEW,
    STAGE_APPROVAL,
    STAGE_EXECUTION,
    STAGE_DEBRIEF,
    STAGE_REPORT,
];

/// The mission is inside the lifecycle.
pub const STATE_OPEN: &str = "open";
/// A Lodge Commander rejected it and the rejection was not overturned.
pub const STATE_REJECTED: &str = "rejected";
/// It reached the end of stage 6 and was signed off.
pub const STATE_COMPLETED: &str = "completed";

/// Proposal categories — **stable codes**, never display strings (a stored
/// `"Conservation"` is a translation bug; SPEC-adjacent design decision in
/// `docs/design/localization.md`).
pub const CATEGORIES: [&str; 7] = [
    "service",
    "conservation",
    "expedition",
    "training",
    "community",
    "ceremonial",
    "other",
];

/// Milestone states.
pub const MILESTONE_STATUSES: [&str; 4] = ["pending", "in_progress", "done", "missed"];

/// Appeal states.
pub const APPEAL_PENDING: &str = "pending";
/// Seconded by another Troop Council voting member — the appeal is now before
/// the Council.
pub const APPEAL_SECONDED: &str = "seconded";
/// The Council refused to overturn.
pub const APPEAL_UPHELD: &str = "upheld";
/// The Council overturned the rejection.
pub const APPEAL_OVERTURNED: &str = "overturned";

/// The 403 a caller gets for a mission they may not see. The **same** message
/// covers "absent" and "forbidden", so a caller without `missions:read` at
/// troop scope cannot use the status code to learn which mission ids exist.
const MISSION_FORBIDDEN: &str =
    "reading this mission requires missions:read at troop scope, at the mission's lodge, \
     or of your own proposal";

/// Position of a stage in the lifecycle (`None` for an unknown code).
pub fn stage_index(stage: &str) -> Option<usize> {
    STAGES.iter().position(|s| *s == stage)
}

/// Guard a transition: the mission must be in `expected` for `action`.
///
/// Returns `409 Conflict` naming both stages — the caller is told what the
/// mission's actual stage is, so a client can re-render rather than guess.
pub fn require_stage(current: &str, expected: &str, action: &str) -> Result<(), SdkError> {
    if current == expected {
        return Ok(());
    }
    Err(SdkError::Conflict(format!(
        "cannot {action}: the mission is in stage {current:?}, and {action} requires it to be \
         in stage {expected:?}"
    )))
}

/// Guard a transition that is legal from any of `allowed`.
pub fn require_stage_in(current: &str, allowed: &[&str], action: &str) -> Result<(), SdkError> {
    if allowed.contains(&current) {
        return Ok(());
    }
    Err(SdkError::Conflict(format!(
        "cannot {action}: the mission is in stage {current:?}, and {action} requires one of \
         {allowed:?}"
    )))
}

/// `YYYY-MM-DD`, the only date shape this plugin accepts (PostgreSQL would
/// otherwise reject it with a less helpful message, and `starts_on <= ends_on`
/// is only meaningful when both are ISO).
pub fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

// ---------------------------------------------------------------------------
// The proposal (the Request stage's structured form)
// ---------------------------------------------------------------------------

/// The structured proposal form (Accords Art 8).
///
/// Request captures *intent* — what, why, for whom, to what end. Review (with
/// the mentor) turns it into `success_criteria` and scope; Approval decides it.
/// The required fields are the ones a Lodge Commander cannot evaluate without:
/// a proposal with no purpose or no expected impact is not a proposal.
#[derive(Debug, Clone, Deserialize)]
pub struct Proposal {
    pub title: String,
    /// Why this mission exists.
    pub purpose: String,
    /// What it will do, concretely.
    pub objectives: String,
    /// The change it expects to make — the raw material of the Impact Report.
    pub expected_impact: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub lodge_id: Option<String>,
    #[serde(default)]
    pub lodge_name: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// ISO `YYYY-MM-DD`.
    #[serde(default)]
    pub starts_on: Option<String>,
    /// ISO `YYYY-MM-DD`.
    #[serde(default)]
    pub ends_on: Option<String>,
    #[serde(default)]
    pub resources_needed: Option<String>,
    #[serde(default)]
    pub risk_notes: Option<String>,
    /// Youth-safety considerations (OSG policy is upstream; this records the
    /// mission-specific answer).
    #[serde(default)]
    pub youth_safety_notes: Option<String>,
    #[serde(default)]
    pub participant_count: Option<i64>,
}

impl Proposal {
    /// Validate the proposal, naming the field at fault.
    pub fn validate(&self) -> Result<(), SdkError> {
        for (field, value) in [
            ("title", &self.title),
            ("purpose", &self.purpose),
            ("objectives", &self.objectives),
            ("expected_impact", &self.expected_impact),
        ] {
            if value.trim().is_empty() {
                return Err(SdkError::BadRequest(format!(
                    "{field} is required — a proposal states what the mission is for \
                     (Accords Art 8)"
                )));
            }
        }
        if let Some(category) = &self.category {
            let category = category.trim().to_ascii_lowercase();
            if !CATEGORIES.contains(&category.as_str()) {
                return Err(SdkError::BadRequest(format!(
                    "category {category:?} is not one of {}",
                    CATEGORIES.join(", ")
                )));
            }
        }
        for (field, value) in [("starts_on", &self.starts_on), ("ends_on", &self.ends_on)] {
            if let Some(v) = value {
                if !v.trim().is_empty() && !is_iso_date(v.trim()) {
                    return Err(SdkError::BadRequest(format!(
                        "{field} must be an ISO date (YYYY-MM-DD), got {v:?}"
                    )));
                }
            }
        }
        if let (Some(start), Some(end)) = (&self.starts_on, &self.ends_on) {
            if !start.trim().is_empty() && !end.trim().is_empty() && start.trim() > end.trim() {
                return Err(SdkError::BadRequest(
                    "starts_on must not be after ends_on".into(),
                ));
            }
        }
        if let Some(n) = self.participant_count {
            if n < 0 {
                return Err(SdkError::BadRequest(
                    "participant_count must not be negative".into(),
                ));
            }
        }
        Ok(())
    }

    /// The category code, defaulted to `service` and normalized.
    pub fn category_code(&self) -> String {
        self.category
            .as_deref()
            .map(|c| c.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "service".to_string())
    }
}

// ---------------------------------------------------------------------------
// Mentor matching
// ---------------------------------------------------------------------------

/// One ranked mentor candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentorScore {
    pub member_id: String,
    pub display_name: String,
    /// Shared expertise tags with the mission.
    pub tag_overlap: i64,
    /// Open mentorships the mentor is already carrying.
    pub load: i64,
    pub capacity: i64,
}

/// Rank mentor candidates for a mission.
///
/// Pure: the route supplies `mentor_profiles` rows joined with each mentor's
/// current mentorship load, and this decides the order. Rules, in order:
///
/// 1. an inactive profile or a candidate already mentoring this mission is out;
/// 2. a mentor at or over capacity is out — proposing a match that cannot be
///    honoured is worse than proposing none;
/// 3. more shared expertise tags first (case-insensitive);
/// 4. then more spare capacity;
/// 5. then the member id, so the order is stable and testable.
pub fn rank_mentors(
    candidates: &[serde_json::Value],
    mission_tags: &[String],
    exclude: &[String],
) -> Vec<MentorScore> {
    let wanted: Vec<String> = mission_tags.iter().map(|t| t.trim().to_lowercase()).collect();
    let mut scored: Vec<MentorScore> = Vec::new();
    for row in candidates {
        let member_id = row["member_id"].as_str().unwrap_or_default().to_string();
        if member_id.is_empty()
            || exclude.iter().any(|e| e == &member_id)
            || row["is_active"].as_bool() == Some(false)
        {
            continue;
        }
        let capacity = row["capacity"].as_i64().unwrap_or(1).max(0);
        let load = row["load"].as_i64().unwrap_or(0);
        if load >= capacity {
            continue;
        }
        let expertise: Vec<String> = row["expertise"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        let tag_overlap = wanted.iter().filter(|t| expertise.contains(t)).count() as i64;
        scored.push(MentorScore {
            member_id,
            display_name: row["display_name"].as_str().unwrap_or_default().to_string(),
            tag_overlap,
            load,
            capacity,
        });
    }
    scored.sort_by(|a, b| {
        b.tag_overlap
            .cmp(&a.tag_overlap)
            .then((b.capacity - b.load).cmp(&(a.capacity - a.load)))
            .then(a.member_id.cmp(&b.member_id))
    });
    scored
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct MissionsPlugin {
    ctx: OnceLock<PluginContext>,
}

impl MissionsPlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }
}

impl Default for MissionsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The mission row every object route starts from. `lodge_id` matters: the
/// object scope is the lodge's.
async fn fetch_mission(c: &PluginContext, id: i64) -> Result<Option<serde_json::Value>, SdkError> {
    c.db.query_one(
        format!("SELECT * FROM {} WHERE id = $1", c.db.table("missions")),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// The scope a mission's authority lives at: its lodge, or troop-wide.
fn mission_scope(mission: &serde_json::Value) -> Scope {
    match mission["lodge_id"].as_str() {
        Some(lodge) if !lodge.is_empty() => Scope::lodge(lodge),
        _ => Scope::troop(),
    }
}

/// The scope of a mission referenced by id (for creation-time checks).
fn proposed_scope(lodge_id: Option<&str>) -> Scope {
    match lodge_id {
        Some(lodge) if !lodge.trim().is_empty() => Scope::lodge(lodge),
        _ => Scope::troop(),
    }
}

/// May this caller see this mission? Troop-wide `missions:read`, a grant
/// covering the mission's lodge, or their own proposal.
async fn can_read_mission(
    c: &PluginContext,
    identity: Option<&Identity>,
    mission: &serde_json::Value,
) -> bool {
    let Some(identity) = identity else { return false };
    if c.permissions
        .has_in_scope(Some(identity), "missions:read", &Scope::troop())
        .await
    {
        return true;
    }
    if mission["created_by"].as_str() == Some(identity.user_id.as_str()) {
        return true;
    }
    c.permissions
        .has_in_scope(Some(identity), "missions:read", &mission_scope(mission))
        .await
}

/// The Lodge Commander's authority over one mission: `missions:approve` at the
/// mission's scope (a troop grant covers every lodge).
async fn require_approve(
    c: &PluginContext,
    identity: Option<&Identity>,
    mission: &serde_json::Value,
) -> Result<(), SdkError> {
    c.permissions
        .reach(identity, "missions:approve", &mission_scope(mission))
        .await
}

/// Who may change a mission's work: its lead (the proposer), an approver whose
/// grant reaches it, or an **active mentor** on it.
///
/// The mentor branch additionally requires `missions:mentor`, so a mentor's
/// reach is narrow: they may touch the missions they actually mentor.
async fn require_update(
    c: &PluginContext,
    identity: Option<&Identity>,
    mission: &serde_json::Value,
) -> Result<(), SdkError> {
    let Some(identity) = identity else {
        return Err(SdkError::Unauthorized("sign in to update a mission".into()));
    };
    let scope = mission_scope(mission);
    if mission["created_by"].as_str() == Some(identity.user_id.as_str())
        && c.permissions
            .has_in_scope(Some(identity), "missions:update", &scope)
            .await
    {
        return Ok(());
    }
    if c.permissions
        .has_in_scope(Some(identity), "missions:update", &scope)
        .await
    {
        return Ok(());
    }
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    let mentoring = c
        .db
        .exists(
            format!(
                "SELECT 1 FROM {} WHERE mission_id = $1 AND mentor_member = $2 \
                 AND status IN ('proposed', 'active')",
                c.db.table("mentorships")
            ),
            vec![SqlValue::Int(mission_id), SqlValue::Text(identity.user_id.clone())],
        )
        .await?;
    if mentoring
        && c.permissions
            .has_in_scope(Some(identity), "missions:mentor", &scope)
            .await
    {
        return Ok(());
    }
    Err(SdkError::Forbidden(format!(
        "requires missions:update at {scope:?} (or an active mentorship on mission {mission_id})"
    )))
}

/// Append a lifecycle transition to the plugin's own trail. Best-effort: the
/// transition itself must not fail because the trail insert did, and the core
/// audit log (written by the caller) is the durable record either way.
async fn log_stage(
    c: &PluginContext,
    mission_id: i64,
    from: Option<&str>,
    to: &str,
    actor: &str,
    note: &str,
) {
    let result = c
        .db
        .execute(
            format!(
                "INSERT INTO {} (mission_id, from_stage, to_stage, actor, note) \
                 VALUES ($1, $2, $3, $4, $5)",
                c.db.table("mission_stage_log")
            ),
            vec![
                SqlValue::Int(mission_id),
                from.map(|s| s.to_string()).into(),
                SqlValue::Text(to.to_string()),
                SqlValue::Text(actor.to_string()),
                SqlValue::Text(note.to_string()),
            ],
        )
        .await;
    if let Err(e) = result {
        eprintln!("[adjutant-missions] stage log insert failed for mission {mission_id}: {e}");
    }
}

/// Move a mission to `to`, stamping `updated_at`, and trail the transition.
#[allow(clippy::too_many_arguments)]
async fn transition(
    c: &PluginContext,
    identity: Option<&Identity>,
    mission: &serde_json::Value,
    to: &str,
    state: &str,
    audit_action: &str,
) -> Result<(), SdkError> {
    let id = mission["id"].as_i64().unwrap_or_default();
    let from = mission["stage"].as_str().unwrap_or_default();
    c.db.execute(
        format!(
            "UPDATE {} SET stage = $2, state = $3, updated_at = now() WHERE id = $1",
            c.db.table("missions")
        ),
        vec![
            SqlValue::Int(id),
            SqlValue::Text(to.to_string()),
            SqlValue::Text(state.to_string()),
        ],
    )
    .await?;
    let actor = identity.map(|i| i.user_id.clone()).unwrap_or_default();
    log_stage(c, id, Some(from), to, &actor, "").await;
    c.audit
        .log(
            identity,
            audit_action,
            "mission",
            &id.to_string(),
            serde_json::json!({ "from_stage": from, "to_stage": to, "state": state }),
        )
        .await?;
    Ok(())
}

// --- request bodies ---------------------------------------------------------

/// The editable half of a proposal. Every field is optional: `PATCH` means
/// "change what you name", and an unnamed field keeps its stored value.
#[derive(Debug, Deserialize)]
struct ProposalEdit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    objectives: Option<String>,
    #[serde(default)]
    expected_impact: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    location: Option<String>,
    /// ISO `YYYY-MM-DD`.
    #[serde(default)]
    starts_on: Option<String>,
    /// ISO `YYYY-MM-DD`.
    #[serde(default)]
    ends_on: Option<String>,
    #[serde(default)]
    resources_needed: Option<String>,
    #[serde(default)]
    risk_notes: Option<String>,
    #[serde(default)]
    youth_safety_notes: Option<String>,
    #[serde(default)]
    participant_count: Option<i64>,
}

/// Review (stage 2): the mentor, the scope, and the success criteria.
#[derive(Debug, Deserialize)]
struct ReviewBody {
    /// The success criteria the mission will be judged against. Required: the
    /// Accords put "scope, success criteria" in the Review stage.
    success_criteria: String,
    /// How far the mission reaches (dates, place, participants) — free text,
    /// because it is a judgement, not a number.
    #[serde(default)]
    scope_notes: Option<String>,
    /// The member id (or trail name) of the mentor taking this mission on.
    #[serde(default)]
    mentor: Option<String>,
    /// `advance` (default) sends it to Approval; `return` sends it back to the
    /// proposer with guidance.
    #[serde(default)]
    recommendation: Option<String>,
    #[serde(default)]
    guidance: Option<String>,
}

/// Approval (stage 3).
#[derive(Debug, Deserialize)]
struct DecisionBody {
    /// `approved` or `rejected`.
    decision: String,
    /// Required when rejecting: "reject with guidance" (SPEC §7.3) is not a
    /// silent no.
    #[serde(default)]
    guidance: Option<String>,
}

/// A milestone.
#[derive(Debug, Deserialize)]
struct MilestoneBody {
    title: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    due_on: Option<String>,
    #[serde(default)]
    position: Option<i64>,
}

/// A milestone update.
#[derive(Debug, Deserialize)]
struct MilestoneUpdateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    progress_pct: Option<i64>,
    #[serde(default)]
    due_on: Option<String>,
}

/// An execution progress note, with optional running totals.
#[derive(Debug, Deserialize)]
struct ProgressBody {
    note: String,
    #[serde(default)]
    progress_pct: Option<i64>,
    #[serde(default)]
    service_hours: Option<f64>,
    #[serde(default)]
    participant_count: Option<i64>,
}

/// Debrief (stage 5).
#[derive(Debug, Deserialize)]
struct DebriefBody {
    notes: String,
    #[serde(default)]
    goals_met: Option<String>,
    #[serde(default)]
    lessons: Option<String>,
    #[serde(default)]
    service_hours: Option<f64>,
    #[serde(default)]
    participant_count: Option<i64>,
}

/// Report (stage 6).
#[derive(Debug, Deserialize)]
struct ReportBody {
    summary: String,
    /// Free-form impact metrics (`{"trees_planted": 40}`), stored as jsonb and
    /// carried in the `mission.completed` payload.
    #[serde(default)]
    impact_metrics: Option<serde_json::Value>,
    #[serde(default)]
    service_hours: Option<f64>,
    #[serde(default)]
    participant_count: Option<i64>,
}

/// Mentor match.
#[derive(Debug, Deserialize)]
struct MentorBody {
    mentor_member: String,
    #[serde(default)]
    mentee_member: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MentorshipCloseBody {
    /// `completed` or `ended`.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MentorProfileBody {
    member_id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    affiliation: Option<String>,
    #[serde(default)]
    expertise: Vec<String>,
    #[serde(default)]
    capacity: Option<i64>,
    #[serde(default)]
    is_active: Option<bool>,
    #[serde(default)]
    notes: Option<String>,
}

/// Art 8's appeal.
#[derive(Debug, Deserialize)]
struct AppealBody {
    reason: String,
}

#[derive(Debug, Deserialize)]
struct AppealDecisionBody {
    /// The one other Troop Council voting member who seconds the appeal.
    /// Required by Art 8 — without a seconder the appeal does not proceed.
    seconded_by: String,
    /// `overturned` or `upheld`.
    outcome: String,
    #[serde(default)]
    votes_for: Option<i64>,
    #[serde(default)]
    votes_against: Option<i64>,
    #[serde(default)]
    note: Option<String>,
}

// ---------------------------------------------------------------------------
// Routes & migrations
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for MissionsPlugin {
    fn id(&self) -> &str {
        "missions"
    }

    fn name(&self) -> &str {
        "Missions"
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
            Permission::new("missions:read", "View missions (own lodge, own proposals, or troop)"),
            Permission::new("missions:create", "Propose new missions"),
            Permission::new("missions:update", "Update a mission you lead or mentor"),
            Permission::new(
                "missions:approve",
                "Review and approve/reject missions (Lodge Commander+, Troop Council)",
            ),
            Permission::new("missions:appeal", "Appeal a rejected mission to the Troop Council"),
            Permission::new("missions:mentor", "Offer mentorship; record mentor progress"),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "mission_lifecycle_schema",
            "CREATE TABLE IF NOT EXISTS missions (\
                 id BIGSERIAL PRIMARY KEY, \
                 title TEXT NOT NULL, \
                 purpose TEXT NOT NULL DEFAULT '', \
                 objectives TEXT NOT NULL DEFAULT '', \
                 expected_impact TEXT NOT NULL DEFAULT '', \
                 success_criteria TEXT NOT NULL DEFAULT '', \
                 category TEXT NOT NULL DEFAULT 'service', \
                 tags TEXT[] NOT NULL DEFAULT '{}', \
                 lodge_id TEXT, \
                 lodge_name TEXT, \
                 location TEXT, \
                 starts_on DATE, \
                 ends_on DATE, \
                 service_hours DOUBLE PRECISION NOT NULL DEFAULT 0, \
                 participant_count INTEGER NOT NULL DEFAULT 0, \
                 resources_needed TEXT, \
                 risk_notes TEXT, \
                 youth_safety_notes TEXT, \
                 stage TEXT NOT NULL DEFAULT 'request', \
                 state TEXT NOT NULL DEFAULT 'open', \
                 guidance TEXT, \
                 reviewed_by TEXT, \
                 reviewed_at TIMESTAMPTZ, \
                 approved_by TEXT, \
                 approved_at TIMESTAMPTZ, \
                 debrief_notes TEXT, \
                 report_summary TEXT, \
                 impact_metrics JSONB NOT NULL DEFAULT '{}', \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 completed_at TIMESTAMPTZ\
             );\
             CREATE INDEX IF NOT EXISTS idx_missions_stage ON missions(stage, state);\
             CREATE INDEX IF NOT EXISTS idx_missions_lodge ON missions(lodge_id);\
             CREATE TABLE IF NOT EXISTS milestones (\
                 id BIGSERIAL PRIMARY KEY, \
                 mission_id BIGINT NOT NULL REFERENCES missions(id) ON DELETE CASCADE, \
                 title TEXT NOT NULL, \
                 detail TEXT NOT NULL DEFAULT '', \
                 due_on DATE, \
                 status TEXT NOT NULL DEFAULT 'pending', \
                 progress_pct INTEGER NOT NULL DEFAULT 0, \
                 position INTEGER NOT NULL DEFAULT 0, \
                 completed_on DATE, \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_milestones_mission ON milestones(mission_id);\
             CREATE TABLE IF NOT EXISTS mentorships (\
                 id BIGSERIAL PRIMARY KEY, \
                 mission_id BIGINT NOT NULL REFERENCES missions(id) ON DELETE CASCADE, \
                 mentor_member TEXT NOT NULL, \
                 mentee_member TEXT, \
                 role TEXT NOT NULL DEFAULT 'mentor', \
                 status TEXT NOT NULL DEFAULT 'proposed', \
                 matched_by TEXT NOT NULL, \
                 matched_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 started_on DATE, \
                 ended_on DATE, \
                 notes TEXT NOT NULL DEFAULT ''\
             );\
             CREATE INDEX IF NOT EXISTS idx_mentorships_mentor ON mentorships(mentor_member);\
             CREATE INDEX IF NOT EXISTS idx_mentorships_mission ON mentorships(mission_id);\
             CREATE TABLE IF NOT EXISTS mentor_profiles (\
                 member_id TEXT PRIMARY KEY, \
                 display_name TEXT NOT NULL DEFAULT '', \
                 affiliation TEXT, \
                 expertise TEXT[] NOT NULL DEFAULT '{}', \
                 capacity INTEGER NOT NULL DEFAULT 1, \
                 is_active BOOLEAN NOT NULL DEFAULT true, \
                 declared_by TEXT, \
                 notes TEXT NOT NULL DEFAULT '', \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE TABLE IF NOT EXISTS progress_notes (\
                 id BIGSERIAL PRIMARY KEY, \
                 mission_id BIGINT NOT NULL REFERENCES missions(id) ON DELETE CASCADE, \
                 note TEXT NOT NULL, \
                 progress_pct INTEGER, \
                 author TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_progress_mission ON progress_notes(mission_id);\
             CREATE TABLE IF NOT EXISTS mission_appeals (\
                 id BIGSERIAL PRIMARY KEY, \
                 mission_id BIGINT NOT NULL REFERENCES missions(id) ON DELETE CASCADE, \
                 reason TEXT NOT NULL, \
                 appellant TEXT NOT NULL, \
                 status TEXT NOT NULL DEFAULT 'pending', \
                 seconded_by TEXT, \
                 seconded_at TIMESTAMPTZ, \
                 votes_for INTEGER, \
                 votes_against INTEGER, \
                 note TEXT NOT NULL DEFAULT '', \
                 decided_at TIMESTAMPTZ, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_appeals_mission ON mission_appeals(mission_id);\
             CREATE TABLE IF NOT EXISTS mission_stage_log (\
                 id BIGSERIAL PRIMARY KEY, \
                 mission_id BIGINT NOT NULL REFERENCES missions(id) ON DELETE CASCADE, \
                 from_stage TEXT, \
                 to_stage TEXT NOT NULL, \
                 actor TEXT NOT NULL, \
                 note TEXT NOT NULL DEFAULT '', \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_stage_log_mission ON mission_stage_log(mission_id);",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- list ------------------------------------------------------------
        // Object-shaped collection: the caller needs `missions:read` at *some*
        // scope and the handler narrows the rows to the scopes they cover (plus
        // their own proposals).
        let c = ctx.clone();
        let list = RouteDefinition::get_protected_any_scope(
            "/api/missions/missions",
            "missions:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let identity = req.identity.as_ref();
                    let troop_wide = c
                        .permissions
                        .has_in_scope(identity, "missions:read", &Scope::troop())
                        .await;
                    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
                    let lodge_ids: Vec<String> = if troop_wide {
                        Vec::new()
                    } else {
                        identity
                            .map(|i| {
                                i.grants
                                    .iter()
                                    .filter(|g| g.scope.scope_type == ScopeType::Lodge)
                                    .filter_map(|g| g.scope.scope_id.clone())
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT id, title, stage, state, category, lodge_id, lodge_name, \
                                        starts_on::text AS starts_on, ends_on::text AS ends_on, \
                                        service_hours, participant_count, created_by, \
                                        created_at::text AS created_at \
                                 FROM {} \
                                 WHERE ($1::bool OR lodge_id = ANY($2) OR created_by = $3) \
                                   AND ($4::text IS NULL OR stage = $4) \
                                   AND ($5::text IS NULL OR state = $5) \
                                   AND ($6::text IS NULL OR category = $6) \
                                   AND ($7::text IS NULL OR lodge_id = $7) \
                                 ORDER BY created_at DESC, id DESC \
                                 LIMIT $8",
                                c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Bool(troop_wide),
                                SqlValue::TextArray(lodge_ids),
                                SqlValue::Text(caller),
                                req.query_param("stage").map(String::from).into(),
                                req.query_param("state").map(String::from).into(),
                                req.query_param("category").map(String::from).into(),
                                req.query_param("lodge").map(String::from).into(),
                                SqlValue::Int(req.query_int("limit").unwrap_or(50).clamp(1, 200)),
                            ],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "missions": rows, "scope": if troop_wide { "troop" } else { "scoped" } }),
                    )
                }
            }),
        );

        // --- detail ----------------------------------------------------------
        let c = ctx.clone();
        let detail = RouteDefinition::get_protected_any_scope(
            "/api/missions/mission/{id}",
            "missions:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        // Absent and forbidden are the same answer for a caller
                        // who cannot see it at all.
                        return PluginResponse::error(403, MISSION_FORBIDDEN);
                    };
                    if !can_read_mission(&c, req.identity.as_ref(), &mission).await {
                        return PluginResponse::error(403, MISSION_FORBIDDEN);
                    }
                    let milestones = c
                        .db
                        .query(
                            format!(
                                "SELECT id, title, detail, status, progress_pct, position, \
                                        due_on::text AS due_on, created_at::text AS created_at \
                                 FROM {} WHERE mission_id = $1 ORDER BY position, id",
                                c.db.table("milestones")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let mentorships = c
                        .db
                        .query(
                            format!(
                                "SELECT id, mentor_member, mentee_member, role, status, \
                                        matched_by, matched_at::text AS matched_at, notes \
                                 FROM {} WHERE mission_id = $1 ORDER BY id",
                                c.db.table("mentorships")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let progress = c
                        .db
                        .query(
                            format!(
                                "SELECT id, note, progress_pct, author, created_at::text AS created_at \
                                 FROM {} WHERE mission_id = $1 ORDER BY id",
                                c.db.table("progress_notes")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let stage_log = c
                        .db
                        .query(
                            format!(
                                "SELECT from_stage, to_stage, actor, note, created_at::text AS created_at \
                                 FROM {} WHERE mission_id = $1 ORDER BY id",
                                c.db.table("mission_stage_log")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    let appeals = c
                        .db
                        .query(
                            format!(
                                "SELECT id, reason, appellant, status, seconded_by, \
                                        votes_for, votes_against, note, decided_at::text AS decided_at, \
                                        created_at::text AS created_at \
                                 FROM {} WHERE mission_id = $1 ORDER BY id",
                                c.db.table("mission_appeals")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "mission": mission,
                            "milestones": milestones,
                            "mentorships": mentorships,
                            "progress": progress,
                            "stage_log": stage_log,
                            "appeals": appeals,
                        }),
                    )
                }
            }),
        );

        // --- request: propose -------------------------------------------------
        let c = ctx.clone();
        let propose = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission",
            "missions:create",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: Proposal = req.json()?;
                    body.validate()?;
                    // The mission's lodge is the scope the proposer must hold
                    // `missions:create` at (a troop grant covers every lodge).
                    c.permissions
                        .reach(
                            req.identity.as_ref(),
                            "missions:create",
                            &proposed_scope(body.lodge_id.as_deref()),
                        )
                        .await?;
                    let caller = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} \
                                   (title, purpose, objectives, expected_impact, category, tags, \
                                    lodge_id, lodge_name, location, starts_on, ends_on, \
                                    resources_needed, risk_notes, youth_safety_notes, \
                                    participant_count, stage, state, created_by) \
                                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::date, $11::date, \
                                         $12, $13, $14, COALESCE($15, 0), 'request', 'open', $16) \
                                 RETURNING id, stage, state, created_at::text AS created_at",
                                c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Text(body.title.trim().to_string()),
                                SqlValue::Text(body.purpose.trim().to_string()),
                                SqlValue::Text(body.objectives.trim().to_string()),
                                SqlValue::Text(body.expected_impact.trim().to_string()),
                                SqlValue::Text(body.category_code()),
                                SqlValue::TextArray(body.tags.clone()),
                                body.lodge_id.clone().into(),
                                body.lodge_name.clone().into(),
                                body.location.clone().into(),
                                body.starts_on.clone().into(),
                                body.ends_on.clone().into(),
                                body.resources_needed.clone().into(),
                                body.risk_notes.clone().into(),
                                body.youth_safety_notes.clone().into(),
                                body.participant_count.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(caller.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    let id = row["id"].as_i64().unwrap_or_default();
                    log_stage(&c, id, None, STAGE_REQUEST, &caller, "proposed").await;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mission.propose",
                            "mission",
                            &id.to_string(),
                            serde_json::json!({ "title": body.title, "lodge_id": body.lodge_id }),
                        )
                        .await?;
                    c.events
                        .publish(
                            event_type::MISSION_CREATED,
                            serde_json::json!({
                                "mission_id": id,
                                "title": body.title,
                                "lodge_id": body.lodge_id,
                                "created_by": caller,
                            }),
                        )
                        .await?;
                    PluginResponse::created(
                        &format!("/api/missions/mission/{id}"),
                        &serde_json::json!({ "id": id, "stage": row["stage"], "state": row["state"] }),
                    )
                }
            }),
        );

        // --- request: edit the draft -----------------------------------------
        let c = ctx.clone();
        let update = RouteDefinition::patch_protected_any_scope(
            "/api/missions/mission/{id}",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: ProposalEdit = req.json()?;
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    // Editable while the mission is being shaped: after approval
                    // the mission is what was approved.
                    require_stage_in(
                        mission["stage"].as_str().unwrap_or_default(),
                        &[STAGE_REQUEST, STAGE_REVIEW],
                        "edit the proposal",
                    )?;
                    if let Some(category) = &body.category {
                        let code = category.trim().to_ascii_lowercase();
                        if !CATEGORIES.contains(&code.as_str()) {
                            return PluginResponse::error(
                                400,
                                format!("category {code:?} is not one of {}", CATEGORIES.join(", ")),
                            );
                        }
                    }
                    // Validate before the cast: `$9::date` on garbage is a
                    // database error (a 500 with no useful message) where the
                    // caller's mistake is a 400.
                    for (field, value) in
                        [("starts_on", &body.starts_on), ("ends_on", &body.ends_on)]
                    {
                        if let Some(v) = value {
                            if !v.trim().is_empty() && !is_iso_date(v.trim()) {
                                return PluginResponse::error(
                                    400,
                                    format!("{field} must be an ISO date (YYYY-MM-DD), got {v:?}"),
                                );
                            }
                        }
                    }
                    if let (Some(start), Some(end)) = (&body.starts_on, &body.ends_on) {
                        if !start.trim().is_empty()
                            && !end.trim().is_empty()
                            && start.trim() > end.trim()
                        {
                            return PluginResponse::error(400, "starts_on must not be after ends_on");
                        }
                    }
                    if body.participant_count.is_some_and(|n| n < 0) {
                        return PluginResponse::error(400, "participant_count must not be negative");
                    }
                    let tags = body.tags.clone().map(SqlValue::TextArray);
                    let rows = c
                        .db
                        .query(
                            format!(
                                "UPDATE {} SET \
                                   title = COALESCE($2, title), \
                                   purpose = COALESCE($3, purpose), \
                                   objectives = COALESCE($4, objectives), \
                                   expected_impact = COALESCE($5, expected_impact), \
                                   category = COALESCE($6, category), \
                                   tags = COALESCE($7::text[], tags), \
                                   location = COALESCE($8, location), \
                                   starts_on = COALESCE($9::date, starts_on), \
                                   ends_on = COALESCE($10::date, ends_on), \
                                   resources_needed = COALESCE($11, resources_needed), \
                                   risk_notes = COALESCE($12, risk_notes), \
                                   youth_safety_notes = COALESCE($13, youth_safety_notes), \
                                   participant_count = COALESCE($14, participant_count), \
                                   updated_at = now() \
                                 WHERE id = $1 \
                                 RETURNING id, title, stage, state",
                                c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Int(id),
                                body.title.clone().into(),
                                body.purpose.clone().into(),
                                body.objectives.clone().into(),
                                body.expected_impact.clone().into(),
                                body.category.clone().into(),
                                tags.unwrap_or(SqlValue::Null),
                                body.location.clone().into(),
                                body.starts_on.clone().into(),
                                body.ends_on.clone().into(),
                                body.resources_needed.clone().into(),
                                body.risk_notes.clone().into(),
                                body.youth_safety_notes.clone().into(),
                                body.participant_count.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    match rows.first() {
                        Some(row) => {
                            c.audit
                                .log(
                                    req.identity.as_ref(),
                                    "mission.update",
                                    "mission",
                                    &id.to_string(),
                                    serde_json::json!({}),
                                )
                                .await?;
                            PluginResponse::json(200, &serde_json::json!({ "mission": row }))
                        }
                        None => PluginResponse::error(404, "no such mission"),
                    }
                }
            }),
        );

        // --- submit: request → review ----------------------------------------
        let c = ctx.clone();
        let submit = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/submit",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_REQUEST,
                        "submit the proposal",
                    )?;
                    transition(
                        &c,
                        req.identity.as_ref(),
                        &mission,
                        STAGE_REVIEW,
                        STATE_OPEN,
                        "mission.submit",
                    )
                    .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": STAGE_REVIEW, "state": STATE_OPEN }),
                    )
                }
            }),
        );

        // --- review: review → approval ---------------------------------------
        let c = ctx.clone();
        let review = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/review",
            "missions:approve",
            route_handler(move |c_req| {
                let c = c.clone();
                async move {
                    let id = c_req.int_param("id")?;
                    let body: ReviewBody = c_req.json()?;
                    if body.success_criteria.trim().is_empty() {
                        return PluginResponse::error(
                            400,
                            "success_criteria is required: the Review stage settles the criteria \
                             the mission will be judged against (Accords Art 8)",
                        );
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_approve(&c, c_req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_REVIEW,
                        "record the review",
                    )?;
                    let recommendation = body
                        .recommendation
                        .as_deref()
                        .map(|r| r.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "advance".to_string());
                    if !["advance", "return"].contains(&recommendation.as_str()) {
                        return PluginResponse::error(
                            400,
                            "recommendation must be 'advance' or 'return'",
                        );
                    }
                    let reviewer = c_req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    // The review's note to the proposer: explicit guidance when
                    // given (required in practice for a `return`), else the
                    // review's scope notes, which is what the proposer reads.
                    let guidance = body.guidance.clone().or_else(|| body.scope_notes.clone());
                    c.db.execute(
                        format!(
                            "UPDATE {} SET success_criteria = $2, guidance = $3, reviewed_by = $4, \
                                    reviewed_at = now(), updated_at = now() WHERE id = $1",
                            c.db.table("missions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(body.success_criteria.trim().to_string()),
                            guidance.into(),
                            SqlValue::Text(reviewer.clone()),
                        ],
                    )
                    .await?;
                    // Matching a mentor at review time is the Accords' point of
                    // the stage, so record it when one is named.
                    if let Some(mentor) = body.mentor.as_deref().map(str::trim).filter(|m| !m.is_empty())
                    {
                        c.db.execute(
                            format!(
                                "INSERT INTO {} (mission_id, mentor_member, role, status, matched_by, notes) \
                                 VALUES ($1, $2, 'mentor', 'active', $3, $4)",
                                c.db.table("mentorships")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(mentor.to_string()),
                                SqlValue::Text(reviewer.clone()),
                                SqlValue::Text(
                                    body.scope_notes.clone().unwrap_or_default(),
                                ),
                            ],
                        )
                        .await?;
                    }
                    let next = if recommendation == "return" { STAGE_REQUEST } else { STAGE_APPROVAL };
                    transition(
                        &c,
                        c_req.identity.as_ref(),
                        &mission,
                        next,
                        STATE_OPEN,
                        "mission.review",
                    )
                    .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "id": id,
                            "stage": next,
                            "recommendation": recommendation,
                            "success_criteria": body.success_criteria,
                        }),
                    )
                }
            }),
        );

        // --- approval: the Lodge Commander's decision -------------------------
        let c = ctx.clone();
        let decision = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/decision",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: DecisionBody = req.json()?;
                    let decision = body.decision.trim().to_ascii_lowercase();
                    if !["approved", "rejected"].contains(&decision.as_str()) {
                        return PluginResponse::error(400, "decision must be 'approved' or 'rejected'");
                    }
                    if decision == "rejected"
                        && body.guidance.as_deref().map(str::trim).unwrap_or("").is_empty()
                    {
                        return PluginResponse::error(
                            400,
                            "a rejection carries guidance: say what would make the proposal \
                             approvable (SPEC §7.3, 'reject with guidance')",
                        );
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_approve(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_APPROVAL,
                        "decide the mission",
                    )?;
                    let approver = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let (stage, state) = if decision == "approved" {
                        (STAGE_EXECUTION, STATE_OPEN)
                    } else {
                        (STAGE_APPROVAL, STATE_REJECTED)
                    };
                    c.db.execute(
                        format!(
                            "UPDATE {} SET stage = $2, state = $3, guidance = COALESCE($4, guidance), \
                                    approved_by = $5, approved_at = now(), updated_at = now() \
                             WHERE id = $1",
                            c.db.table("missions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(stage.to_string()),
                            SqlValue::Text(state.to_string()),
                            body.guidance.clone().into(),
                            SqlValue::Text(approver.clone()),
                        ],
                    )
                    .await?;
                    log_stage(
                        &c,
                        id,
                        Some(STAGE_APPROVAL),
                        stage,
                        &approver,
                        &decision,
                    )
                    .await;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mission.decision",
                            "mission",
                            &id.to_string(),
                            serde_json::json!({
                                "decision": decision,
                                "guidance": body.guidance,
                                "lodge_id": mission["lodge_id"],
                            }),
                        )
                        .await?;
                    if decision == "approved" {
                        c.events
                            .publish(
                                event_type::MISSION_APPROVED,
                                serde_json::json!({
                                    "mission_id": id,
                                    "title": mission["title"],
                                    "lodge_id": mission["lodge_id"],
                                    "approved_by": approver,
                                }),
                            )
                            .await?;
                    }
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "decision": decision, "stage": stage, "state": state }),
                    )
                }
            }),
        );

        // --- appeal (Art 8) ---------------------------------------------------
        let c = ctx.clone();
        let appeal = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/appeal",
            "missions:appeal",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: AppealBody = req.json()?;
                    if body.reason.trim().is_empty() {
                        return PluginResponse::error(400, "reason is required");
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    if mission["state"].as_str() != Some(STATE_REJECTED) {
                        return PluginResponse::error(
                            409,
                            "only a rejected mission can be appealed (Accords Art 8)",
                        );
                    }
                    let appellant = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    // The appeal belongs to the mission's own lead; anyone else
                    // (a Lodge Commander escalating, the Council) needs an
                    // `missions:appeal` grant that reaches the mission's scope.
                    if mission["created_by"].as_str() != Some(appellant.as_str()) {
                        c.permissions
                            .reach(req.identity.as_ref(), "missions:appeal", &mission_scope(&mission))
                            .await?;
                    }
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (mission_id, reason, appellant) \
                                 VALUES ($1, $2, $3) RETURNING id, status",
                                c.db.table("mission_appeals")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(body.reason.trim().to_string()),
                                SqlValue::Text(appellant),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mission.appeal",
                            "mission",
                            &id.to_string(),
                            serde_json::json!({ "appeal_id": row["id"], "reason": body.reason }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "appeal_id": row["id"],
                            "status": row["status"],
                            "next": "a second Troop Council voting member seconds it \
                                     (POST /api/missions/appeal/{id}/decide)",
                        }),
                    )
                }
            }),
        );

        // --- the Troop Council decides the appeal -----------------------------
        let c = ctx.clone();
        let appeal_decision = RouteDefinition::post_protected(
            "/api/missions/appeal/{id}/decide",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let appeal_id = req.int_param("id")?;
                    let body: AppealDecisionBody = req.json()?;
                    let outcome = body.outcome.trim().to_ascii_lowercase();
                    if !["overturned", "upheld"].contains(&outcome.as_str()) {
                        return PluginResponse::error(400, "outcome must be 'overturned' or 'upheld'");
                    }
                    if body.seconded_by.trim().is_empty() {
                        return PluginResponse::error(
                            400,
                            "seconded_by is required: an appeal proceeds only when one other \
                             Troop Council voting member seconds it (Accords Art 8)",
                        );
                    }
                    let Some(app) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("mission_appeals")),
                            vec![SqlValue::Int(appeal_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such appeal");
                    };
                    if app["status"].as_str() != Some(APPEAL_PENDING) {
                        return PluginResponse::error(409, "this appeal has already been decided");
                    }
                    let mission_id = app["mission_id"].as_i64().unwrap_or_default();
                    let Some(mission) = fetch_mission(&c, mission_id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    let decider = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = $2, seconded_by = $3, seconded_at = now(), \
                                    votes_for = $4, votes_against = $5, note = $6, decided_at = now() \
                             WHERE id = $1",
                            c.db.table("mission_appeals")
                        ),
                        vec![
                            SqlValue::Int(appeal_id),
                            SqlValue::Text(outcome.clone()),
                            SqlValue::Text(body.seconded_by.trim().to_string()),
                            body.votes_for.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            body.votes_against.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            SqlValue::Text(body.note.clone().unwrap_or_default()),
                        ],
                    )
                    .await?;
                    if outcome == "overturned" {
                        // The Council overturned the rejection: the mission
                        // proceeds, approved by the Council rather than the
                        // Lodge Commander.
                        c.db.execute(
                            format!(
                                "UPDATE {} SET stage = $2, state = $3, approved_by = $4, \
                                        approved_at = now(), guidance = COALESCE($5, guidance), \
                                        updated_at = now() WHERE id = $1",
                                c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Int(mission_id),
                                SqlValue::Text(STAGE_EXECUTION.to_string()),
                                SqlValue::Text(STATE_OPEN.to_string()),
                                SqlValue::Text(format!("troop_council:{decider}")),
                                body.note.clone().into(),
                            ],
                        )
                        .await?;
                        log_stage(
                            &c,
                            mission_id,
                            Some(STAGE_APPROVAL),
                            STAGE_EXECUTION,
                            &decider,
                            "appeal overturned",
                        )
                        .await;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mission.appeal.decide",
                            "mission_appeal",
                            &appeal_id.to_string(),
                            serde_json::json!({
                                "mission_id": mission_id,
                                "outcome": outcome,
                                "seconded_by": body.seconded_by,
                                "votes_for": body.votes_for,
                                "votes_against": body.votes_against,
                            }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "appeal_id": appeal_id,
                            "mission_id": mission_id,
                            "outcome": outcome,
                            "mission_stage": if outcome == "overturned" { STAGE_EXECUTION } else { mission["stage"].as_str().unwrap_or_default() },
                        }),
                    )
                }
            }),
        );

        // --- mentor matching ---------------------------------------------------
        // A mentor profile may be declared by (or for) any member with
        // `missions:mentor` — the expertise tags and capacity are what the
        // matching ranks on.
        let c = ctx.clone();
        let mentor_profile = RouteDefinition::post_protected_any_scope(
            "/api/missions/mentor/profile",
            "missions:mentor",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let body: MentorProfileBody = req.json()?;
                    if body.member_id.trim().is_empty() {
                        return PluginResponse::error(400, "member_id is required");
                    }
                    let declared_by = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} \
                                   (member_id, display_name, affiliation, expertise, capacity, \
                                    is_active, declared_by, notes) \
                                 VALUES ($1, COALESCE($2, ''), $3, $4, COALESCE($5, 1), \
                                         COALESCE($6, true), $7, COALESCE($8, '')) \
                                 ON CONFLICT (member_id) DO UPDATE SET \
                                   display_name = COALESCE(EXCLUDED.display_name, {t}.display_name), \
                                   affiliation = COALESCE(EXCLUDED.affiliation, {t}.affiliation), \
                                   expertise = EXCLUDED.expertise, \
                                   capacity = COALESCE($5, {t}.capacity), \
                                   is_active = COALESCE($6, {t}.is_active), \
                                   notes = COALESCE($8, {t}.notes), \
                                   declared_by = EXCLUDED.declared_by, \
                                   updated_at = now() \
                                 RETURNING member_id, display_name, expertise, capacity, is_active",
                                c.db.table("mentor_profiles"),
                                t = c.db.table("mentor_profiles")
                            ),
                            vec![
                                SqlValue::Text(body.member_id.trim().to_string()),
                                body.display_name.clone().into(),
                                body.affiliation.clone().into(),
                                SqlValue::TextArray(body.expertise.clone()),
                                body.capacity.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                body.is_active.map(SqlValue::Bool).unwrap_or(SqlValue::NullBool),
                                SqlValue::Text(declared_by),
                                body.notes.clone().into(),
                            ],
                        )
                        .await?;
                    PluginResponse::json(201, &serde_json::json!({ "mentor": row }))
                }
            }),
        );

        // Ranked candidates: expertise overlap first, then spare capacity.
        let c = ctx.clone();
        let suggestions = RouteDefinition::get_protected_any_scope(
            "/api/missions/mission/{id}/mentor/suggestions",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_approve(&c, req.identity.as_ref(), &mission).await?;
                    let mission_tags: Vec<String> = mission["tags"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                        .unwrap_or_default();
                    let candidates = c
                        .db
                        .query(
                            format!(
                                "SELECT p.member_id, p.display_name, p.expertise, p.capacity, p.is_active, \
                                        COALESCE((SELECT COUNT(*) FROM {m} ms \
                                                   WHERE ms.mentor_member = p.member_id \
                                                     AND ms.status IN ('proposed', 'active')), 0) AS load \
                                 FROM {p} p ORDER BY p.member_id",
                                p = c.db.table("mentor_profiles"),
                                m = c.db.table("mentorships")
                            ),
                            vec![],
                        )
                        .await?;
                    let already: Vec<String> = c
                        .db
                        .query(
                            format!(
                                "SELECT mentor_member FROM {} WHERE mission_id = $1",
                                c.db.table("mentorships")
                            ),
                            vec![SqlValue::Int(id)],
                        )
                        .await?
                        .iter()
                        .filter_map(|r| r["mentor_member"].as_str().map(String::from))
                        .collect();
                    let ranked = rank_mentors(&candidates, &mission_tags, &already);
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "mission_id": id,
                            "tags": mission_tags,
                            "candidates": ranked.iter().map(|m| serde_json::json!({
                                "member_id": m.member_id,
                                "display_name": m.display_name,
                                "tag_overlap": m.tag_overlap,
                                "load": m.load,
                                "capacity": m.capacity,
                                "spare": m.capacity - m.load,
                            })).collect::<Vec<_>>(),
                        }),
                    )
                }
            }),
        );

        // Match a mentor (Review's "mentorship" or a mid-mission addition).
        let c = ctx.clone();
        let match_mentor = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/mentor",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: MentorBody = req.json()?;
                    if body.mentor_member.trim().is_empty() {
                        return PluginResponse::error(400, "mentor_member is required");
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_approve(&c, req.identity.as_ref(), &mission).await?;
                    let role = body
                        .role
                        .as_deref()
                        .map(|r| r.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "mentor".to_string());
                    if !["mentor", "co_mentor", "subject_expert"].contains(&role.as_str()) {
                        return PluginResponse::error(
                            400,
                            "role must be 'mentor', 'co_mentor', or 'subject_expert'",
                        );
                    }
                    let matcher = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (mission_id, mentor_member, mentee_member, role, status, matched_by, notes) \
                                 VALUES ($1, $2, $3, $4, 'active', $5, COALESCE($6, '')) \
                                 RETURNING id, mentor_member, role, status",
                                c.db.table("mentorships")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(body.mentor_member.trim().to_string()),
                                body.mentee_member.clone().into(),
                                SqlValue::Text(role.clone()),
                                SqlValue::Text(matcher),
                                body.notes.clone().into(),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mentorship.match",
                            "mission",
                            &id.to_string(),
                            serde_json::json!({ "mentor": body.mentor_member, "role": role }),
                        )
                        .await?;
                    PluginResponse::json(201, &serde_json::json!({ "mentorship": row }))
                }
            }),
        );

        // List mentorships (own, as mentor or mentee; troop/lodge callers see
        // the scoped set).
        let c = ctx.clone();
        let mentorships = RouteDefinition::get_protected_any_scope(
            "/api/missions/mentorships",
            "missions:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let identity = req.identity.as_ref();
                    let troop_wide = c
                        .permissions
                        .has_in_scope(identity, "missions:read", &Scope::troop())
                        .await;
                    let caller = identity.map(|i| i.user_id.clone()).unwrap_or_default();
                    let member = req.query_param("member").map(String::from);
                    let mission = req.query_int("mission");
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT ms.id, ms.mission_id, ms.mentor_member, ms.mentee_member, \
                                        ms.role, ms.status, ms.matched_by, \
                                        ms.matched_at::text AS matched_at, ms.notes, \
                                        m.title AS mission_title, m.stage AS mission_stage \
                                 FROM {ms} ms JOIN {m} m ON m.id = ms.mission_id \
                                 WHERE ($1::bool OR ms.mentor_member = $2 OR ms.mentee_member = $2) \
                                   AND ($3::text IS NULL OR ms.mentor_member = $3) \
                                   AND ($4::bigint IS NULL OR ms.mission_id = $4) \
                                 ORDER BY ms.id DESC LIMIT 200",
                                ms = c.db.table("mentorships"),
                                m = c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Bool(troop_wide),
                                SqlValue::Text(caller),
                                member.into(),
                                mission.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "mentorships": rows }))
                }
            }),
        );

        let c = ctx.clone();
        let close_mentorship = RouteDefinition::post_protected_any_scope(
            "/api/missions/mentorship/{id}/close",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let mentorship_id = req.int_param("id")?;
                    let body: MentorshipCloseBody = req.json()?;
                    let status = body
                        .status
                        .as_deref()
                        .map(|s| s.trim().to_ascii_lowercase())
                        .unwrap_or_else(|| "completed".to_string());
                    if !["completed", "ended"].contains(&status.as_str()) {
                        return PluginResponse::error(400, "status must be 'completed' or 'ended'");
                    }
                    let Some(m) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("mentorships")),
                            vec![SqlValue::Int(mentorship_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such mentorship");
                    };
                    let mission_id = m["mission_id"].as_i64().unwrap_or_default();
                    let Some(mission) = fetch_mission(&c, mission_id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    // The mentor, the mission's lead, or an approver may close it.
                    let caller = req.identity.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
                    let is_mentor = m["mentor_member"].as_str() == Some(caller.as_str());
                    if !is_mentor {
                        require_update(&c, req.identity.as_ref(), &mission).await?;
                    }
                    c.db.execute(
                        format!(
                            "UPDATE {} SET status = $2, ended_on = current_date, \
                                    notes = CASE WHEN $3 = '' THEN notes ELSE $3 END WHERE id = $1",
                            c.db.table("mentorships")
                        ),
                        vec![
                            SqlValue::Int(mentorship_id),
                            SqlValue::Text(status.clone()),
                            SqlValue::Text(body.notes.clone().unwrap_or_default()),
                        ],
                    )
                    .await?;
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mentorship.close",
                            "mission",
                            &mission_id.to_string(),
                            serde_json::json!({ "mentorship_id": mentorship_id, "status": status }),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "mentorship_id": mentorship_id, "status": status }),
                    )
                }
            }),
        );

        // --- execution: milestones -------------------------------------------
        let c = ctx.clone();
        let add_milestone = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/milestone",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: MilestoneBody = req.json()?;
                    if body.title.trim().is_empty() {
                        return PluginResponse::error(400, "title is required");
                    }
                    if let Some(due) = body.due_on.as_deref() {
                        if !due.trim().is_empty() && !is_iso_date(due.trim()) {
                            return PluginResponse::error(
                                400,
                                format!("due_on must be an ISO date (YYYY-MM-DD), got {due:?}"),
                            );
                        }
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage_in(
                        mission["stage"].as_str().unwrap_or_default(),
                        &[STAGE_APPROVAL, STAGE_EXECUTION],
                        "plan a milestone",
                    )?;
                    let creator = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (mission_id, title, detail, due_on, position, created_by) \
                                 VALUES ($1, $2, COALESCE($3, ''), $4::date, COALESCE($5, 0), $6) \
                                 RETURNING id, title, status, progress_pct, due_on::text AS due_on",
                                c.db.table("milestones")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(body.title.trim().to_string()),
                                body.detail.clone().into(),
                                body.due_on.clone().into(),
                                body.position.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(creator),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    PluginResponse::json(201, &serde_json::json!({ "milestone": row }))
                }
            }),
        );

        let c = ctx.clone();
        let update_milestone = RouteDefinition::patch_protected_any_scope(
            "/api/missions/milestone/{id}",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let milestone_id = req.int_param("id")?;
                    let body: MilestoneUpdateBody = req.json()?;
                    if let Some(status) = body.status.as_deref() {
                        if !MILESTONE_STATUSES.contains(&status.trim()) {
                            return PluginResponse::error(
                                400,
                                format!("status must be one of {}", MILESTONE_STATUSES.join(", ")),
                            );
                        }
                        if status.trim() == "done" && body.progress_pct.is_none() {
                            // Marking done is 100% — say so rather than store a
                            // `done` milestone at 40%.
                        }
                    }
                    if let Some(pct) = body.progress_pct {
                        if !(0..=100).contains(&pct) {
                            return PluginResponse::error(400, "progress_pct must be 0..100");
                        }
                    }
                    let Some(milestone) = c
                        .db
                        .query_one(
                            format!("SELECT * FROM {} WHERE id = $1", c.db.table("milestones")),
                            vec![SqlValue::Int(milestone_id)],
                        )
                        .await?
                    else {
                        return PluginResponse::error(404, "no such milestone");
                    };
                    let mission_id = milestone["mission_id"].as_i64().unwrap_or_default();
                    let Some(mission) = fetch_mission(&c, mission_id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage_in(
                        mission["stage"].as_str().unwrap_or_default(),
                        &[STAGE_APPROVAL, STAGE_EXECUTION],
                        "update a milestone",
                    )?;
                    let status = body.status.as_deref().map(|s| s.trim().to_string());
                    let done = status.as_deref() == Some("done");
                    let pct = body
                        .progress_pct
                        .or(if done { Some(100) } else { None });
                    let rows = c
                        .db
                        .query(
                            format!(
                                "UPDATE {} SET \
                                   title = COALESCE($2, title), \
                                   detail = COALESCE($3, detail), \
                                   status = COALESCE($4, status), \
                                   progress_pct = COALESCE($5, progress_pct), \
                                   due_on = COALESCE($6::date, due_on), \
                                   completed_on = CASE WHEN COALESCE($4, status) = 'done' \
                                                       THEN current_date ELSE completed_on END, \
                                   updated_at = now() \
                                 WHERE id = $1 \
                                 RETURNING id, mission_id, title, status, progress_pct, due_on::text AS due_on",
                                c.db.table("milestones")
                            ),
                            vec![
                                SqlValue::Int(milestone_id),
                                body.title.clone().into(),
                                body.detail.clone().into(),
                                status.into(),
                                pct.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                body.due_on.clone().into(),
                            ],
                        )
                        .await?;
                    match rows.first() {
                        Some(row) => PluginResponse::json(200, &serde_json::json!({ "milestone": row })),
                        None => PluginResponse::error(404, "no such milestone"),
                    }
                }
            }),
        );

        // Destructive: the SDK requires a troop-covering grant from every
        // constructor, so removing a milestone is an approver's action.
        let c = ctx.clone();
        let delete_milestone = RouteDefinition::delete_protected(
            "/api/missions/milestone/{id}",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let milestone_id = req.int_param("id")?;
                    let n = c
                        .db
                        .execute(
                            format!("DELETE FROM {} WHERE id = $1", c.db.table("milestones")),
                            vec![SqlValue::Int(milestone_id)],
                        )
                        .await?;
                    if n == 0 {
                        return PluginResponse::error(404, "no such milestone");
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "milestone.delete",
                            "milestone",
                            &milestone_id.to_string(),
                            serde_json::json!({}),
                        )
                        .await?;
                    Ok(PluginResponse::no_content())
                }
            }),
        );

        let c = ctx.clone();
        let progress = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/progress",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: ProgressBody = req.json()?;
                    if body.note.trim().is_empty() {
                        return PluginResponse::error(400, "note is required");
                    }
                    if let Some(pct) = body.progress_pct {
                        if !(0..=100).contains(&pct) {
                            return PluginResponse::error(400, "progress_pct must be 0..100");
                        }
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_EXECUTION,
                        "record progress",
                    )?;
                    let author = req
                        .identity
                        .as_ref()
                        .map(|i| i.user_id.clone())
                        .unwrap_or_default();
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} (mission_id, note, progress_pct, author) \
                                 VALUES ($1, $2, $3, $4) RETURNING id, created_at::text AS created_at",
                                c.db.table("progress_notes")
                            ),
                            vec![
                                SqlValue::Int(id),
                                SqlValue::Text(body.note.trim().to_string()),
                                body.progress_pct.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                SqlValue::Text(author.clone()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
                    // Running totals move here too, so the Impact Report has the
                    // numbers even when the debrief is terse.
                    if body.service_hours.is_some() || body.participant_count.is_some() {
                        c.db.execute(
                            format!(
                                "UPDATE {} SET service_hours = COALESCE($2, service_hours), \
                                        participant_count = COALESCE($3, participant_count), \
                                        updated_at = now() WHERE id = $1",
                                c.db.table("missions")
                            ),
                            vec![
                                SqlValue::Int(id),
                                body.service_hours.map(SqlValue::Float).unwrap_or(SqlValue::Null),
                                body.participant_count.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "mission.progress",
                            "mission",
                            &id.to_string(),
                            serde_json::json!({ "progress_pct": body.progress_pct, "note": body.note }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({ "progress": row, "mission_id": id }),
                    )
                }
            }),
        );

        // --- debrief: execution → debrief -------------------------------------
        let c = ctx.clone();
        let debrief = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/debrief",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: DebriefBody = req.json()?;
                    if body.notes.trim().is_empty() {
                        return PluginResponse::error(400, "notes are required for a debrief");
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_EXECUTION,
                        "debrief the mission",
                    )?;
                    let text = match (&body.goals_met, &body.lessons) {
                        (Some(goals), Some(lessons)) => format!("{goals}\n\nLessons: {lessons}"),
                        (Some(goals), None) => goals.clone(),
                        (None, Some(lessons)) => format!("Lessons: {lessons}"),
                        (None, None) => String::new(),
                    };
                    let notes = if text.is_empty() {
                        body.notes.trim().to_string()
                    } else {
                        format!("{}\n\n{}", body.notes.trim(), text)
                    };
                    c.db.execute(
                        format!(
                            "UPDATE {} SET debrief_notes = $2, \
                                    service_hours = COALESCE($3, service_hours), \
                                    participant_count = COALESCE($4, participant_count), \
                                    updated_at = now() WHERE id = $1",
                            c.db.table("missions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(notes),
                            body.service_hours.map(SqlValue::Float).unwrap_or(SqlValue::Null),
                            body.participant_count.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                        ],
                    )
                    .await?;
                    transition(
                        &c,
                        req.identity.as_ref(),
                        &mission,
                        STAGE_DEBRIEF,
                        STATE_OPEN,
                        "mission.debrief",
                    )
                    .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": STAGE_DEBRIEF, "state": STATE_OPEN }),
                    )
                }
            }),
        );

        // --- report: debrief → report -----------------------------------------
        let c = ctx.clone();
        let report = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/report",
            "missions:update",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let body: ReportBody = req.json()?;
                    if body.summary.trim().is_empty() {
                        return PluginResponse::error(400, "summary is required for a report");
                    }
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_update(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_DEBRIEF,
                        "write the report",
                    )?;
                    let metrics = body
                        .impact_metrics
                        .clone()
                        .map(|m| serde_json::to_string(&m).unwrap_or_else(|_| "{}".into()));
                    c.db.execute(
                        format!(
                            "UPDATE {} SET report_summary = $2, \
                                    impact_metrics = COALESCE($3::jsonb, impact_metrics), \
                                    service_hours = COALESCE($4, service_hours), \
                                    participant_count = COALESCE($5, participant_count), \
                                    updated_at = now() WHERE id = $1",
                            c.db.table("missions")
                        ),
                        vec![
                            SqlValue::Int(id),
                            SqlValue::Text(body.summary.trim().to_string()),
                            metrics.map(SqlValue::Json).unwrap_or(SqlValue::Null),
                            body.service_hours.map(SqlValue::Float).unwrap_or(SqlValue::Null),
                            body.participant_count.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                        ],
                    )
                    .await?;
                    transition(
                        &c,
                        req.identity.as_ref(),
                        &mission,
                        STAGE_REPORT,
                        STATE_OPEN,
                        "mission.report",
                    )
                    .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({ "id": id, "stage": STAGE_REPORT, "state": STATE_OPEN }),
                    )
                }
            }),
        );

        // --- complete: the approver signs the report off ----------------------
        // This is the transition that publishes `mission.completed`
        // (SPEC §5.4 / M4 exit criteria).
        let c = ctx.clone();
        let complete = RouteDefinition::post_protected_any_scope(
            "/api/missions/mission/{id}/complete",
            "missions:approve",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let id = req.int_param("id")?;
                    let Some(mission) = fetch_mission(&c, id).await? else {
                        return PluginResponse::error(404, "no such mission");
                    };
                    require_approve(&c, req.identity.as_ref(), &mission).await?;
                    require_stage(
                        mission["stage"].as_str().unwrap_or_default(),
                        STAGE_REPORT,
                        "close out the mission",
                    )?;
                    transition(
                        &c,
                        req.identity.as_ref(),
                        &mission,
                        STAGE_REPORT,
                        STATE_COMPLETED,
                        "mission.complete",
                    )
                    .await?;
                    c.db.execute(
                        format!(
                            "UPDATE {} SET completed_at = now(), updated_at = now() WHERE id = $1",
                            c.db.table("missions")
                        ),
                        vec![SqlValue::Int(id)],
                    )
                    .await?;
                    let completed_at = chrono::Utc::now();
                    c.events
                        .publish_mission_completed(&MissionCompleted {
                            mission_id: id,
                            title: mission["title"].as_str().unwrap_or_default().to_string(),
                            lodge_id: mission["lodge_id"].as_str().map(String::from),
                            stage: STAGE_REPORT.to_string(),
                            completed_at,
                            impact: serde_json::json!({
                                "service_hours": mission["service_hours"],
                                "participant_count": mission["participant_count"],
                                "metrics": mission["impact_metrics"],
                                "summary": mission["report_summary"],
                            }),
                        })
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "id": id,
                            "stage": STAGE_REPORT,
                            "state": STATE_COMPLETED,
                            "completed_at": completed_at.to_rfc3339(),
                        }),
                    )
                }
            }),
        );

        // --- the cumulative Impact Report -------------------------------------
        let c = ctx.clone();
        let impact = RouteDefinition::get_protected(
            "/api/missions/impact",
            "missions:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let lodge = req.query_param("lodge").map(String::from);
                    let totals = c
                        .db
                        .query_one(
                            format!(
                                "SELECT COUNT(*) FILTER (WHERE state = 'completed') AS completed, \
                                        COUNT(*) FILTER (WHERE state = 'open') AS open, \
                                        COUNT(*) FILTER (WHERE state = 'rejected') AS rejected, \
                                        COUNT(*) AS total, \
                                        COALESCE(SUM(service_hours) FILTER (WHERE state = 'completed'), 0) AS service_hours, \
                                        COALESCE(SUM(participant_count) FILTER (WHERE state = 'completed'), 0) AS participant_count, \
                                        COUNT(DISTINCT lodge_id) AS lodges \
                                 FROM {} WHERE ($1::text IS NULL OR lodge_id = $1)",
                                c.db.table("missions")
                            ),
                            vec![lodge.clone().into()],
                        )
                        .await?
                        .unwrap_or(serde_json::Value::Null);
                    let by_category = c
                        .db
                        .query(
                            format!(
                                "SELECT category, \
                                        COUNT(*) FILTER (WHERE state = 'completed') AS completed, \
                                        COALESCE(SUM(service_hours) FILTER (WHERE state = 'completed'), 0) AS service_hours \
                                 FROM {} WHERE ($1::text IS NULL OR lodge_id = $1) \
                                 GROUP BY category ORDER BY category",
                                c.db.table("missions")
                            ),
                            vec![lodge.clone().into()],
                        )
                        .await?;
                    let by_year = c
                        .db
                        .query(
                            format!(
                                "SELECT EXTRACT(YEAR FROM completed_at)::int AS year, \
                                        COUNT(*) AS completed, \
                                        COALESCE(SUM(service_hours), 0) AS service_hours, \
                                        COALESCE(SUM(participant_count), 0) AS participant_count \
                                 FROM {} WHERE state = 'completed' \
                                   AND ($1::text IS NULL OR lodge_id = $1) \
                                 GROUP BY 1 ORDER BY 1",
                                c.db.table("missions")
                            ),
                            vec![lodge.clone().into()],
                        )
                        .await?;
                    let by_lodge = c
                        .db
                        .query(
                            format!(
                                "SELECT COALESCE(lodge_id, '') AS lodge_id, \
                                        COALESCE(lodge_name, '') AS lodge_name, \
                                        COUNT(*) FILTER (WHERE state = 'completed') AS completed, \
                                        COALESCE(SUM(service_hours) FILTER (WHERE state = 'completed'), 0) AS service_hours \
                                 FROM {} WHERE ($1::text IS NULL OR lodge_id = $1) \
                                 GROUP BY 1, 2 ORDER BY completed DESC, lodge_id",
                                c.db.table("missions")
                            ),
                            vec![lodge.into()],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "totals": totals,
                            "by_category": by_category,
                            "by_year": by_year,
                            "by_lodge": by_lodge,
                        }),
                    )
                }
            }),
        );

        vec![
            list,
            detail,
            propose,
            update,
            submit,
            review,
            decision,
            appeal,
            appeal_decision,
            mentor_profile,
            suggestions,
            match_mentor,
            mentorships,
            close_mentorship,
            add_milestone,
            update_milestone,
            delete_milestone,
            progress,
            debrief,
            report,
            complete,
            impact,
        ]
    }
}

export_plugin!(MissionsPlugin);
