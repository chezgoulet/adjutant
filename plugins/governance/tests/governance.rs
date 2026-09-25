//! Governance plugin tests: quorum, tallies, minutes, and the motion lifecycle
//! (SPEC §7.4, Accords Art 5/9/12/17).
//!
//! Handlers are driven through `adjutant_sdk::testing`. The mock replays rows in
//! call order, so each test names the query order it arranges around.

use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use adjutant_governance::{
    compute_quorum, is_timestamp_like, quorum_met, render_minutes, require_motion_stage,
    tally_votes, GovernancePlugin,
    AMENDMENT_FORMAL, AMENDMENT_FRIENDLY, BODY_CONGRESS, BODY_TC, MOTION_CATEGORIES, MOTION_STAGES,
    QUORUM_FIXED, QUORUM_MAJORITY_MEMBERS, QUORUM_ONE_THIRD_REGISTERED, RESULT_FAILED,
    RESULT_PASSED, RESULT_PENDING, THRESHOLD_SIMPLE_MAJORITY, THRESHOLD_TWO_THIRDS,
    THRESHOLD_UNANIMOUS, VOTE_METHODS,
};

async fn plugin_routes() -> (TestHost, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = GovernancePlugin::new();
    plugin.init(host.context("governance")).await.unwrap();
    let routes = plugin.routes();
    (host, routes)
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

fn troop_identity() -> Identity {
    Identity::new("bea", vec!["chief".to_string()])
}

fn req_with_identity(mut req: PluginRequest, identity: Identity) -> PluginRequest {
    req.identity = Some(identity);
    req
}

/// Drive a handler and report `(status, body)` — an `SdkError` and a
/// `PluginResponse::error` are the same answer to a client.
async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, serde_json::Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), serde_json::json!({ "error": e.to_string() })),
    }
}

fn meeting_row(id: i64, basis: &str, expected: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "body": BODY_TC,
        "title": "Troop Council — September",
        "status": "open",
        "scheduled_for": "2026-09-20T18:00:00Z",
        "quorum_basis": basis,
        "expected_voters": expected,
        "quorum_required": serde_json::Value::Null,
        "location": "Windsor",
        "minutes": "",
        "minutes_status": "none",
    })
}

fn motion_row(id: i64, stage: &str, result: &str, meeting_id: Option<i64>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "meeting_id": meeting_id,
        "title": "Adopt the 3rd Catamount Accords",
        "text": "that the troop adopt the 3rd Accords",
        "body": BODY_TC,
        "category": "accords_amendment",
        "stage": stage,
        "result": result,
        "threshold": THRESHOLD_SIMPLE_MAJORITY,
        "amends_accords": false,
        "proposed_by": "bea",
        "votes_yes": 0,
        "votes_no": 0,
        "votes_abstain": 0,
    })
}

fn vote(choice: &str) -> serde_json::Value {
    serde_json::json!({ "choice": choice, "method": "roll_call", "voter": "x" })
}

// --- the declaration the loader validates ----------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, routes) = plugin_routes().await;
    let plugin = GovernancePlugin::new();
    assert_eq!(plugin.id(), "governance");

    let permissions: Vec<String> =
        plugin.permissions_granted().into_iter().map(|p| p.id).collect();
    for perm in &permissions {
        assert!(perm.starts_with("governance:"), "{perm} must be namespaced");
    }
    // SPEC §9.1's four, plus the chair's.
    for expected in [
        "governance:read",
        "governance:propose",
        "governance:vote",
        "governance:amend",
        "governance:manage",
    ] {
        assert!(permissions.iter().any(|p| p == expected), "missing {expected}");
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    let ddl = &migrations[0].sql;
    for table in ["motions", "amendments", "votes", "accords_versions", "meetings", "attendance"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "migration is missing {table}"
        );
    }
    assert!(
        ddl.contains("UNIQUE INDEX IF NOT EXISTS idx_votes_unique"),
        "one vote per voter per motion is a database constraint, not a convention"
    );

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(r.path.starts_with("/api/governance"), "{} escapes the namespace", r.path);
        let perm = r.required_permission.as_deref().expect("every route is gated");
        assert!(permissions.iter().any(|p| p == perm), "{perm} is required but not declared");
        let normalized = r
            .path
            .split('/')
            .map(|s| if s.starts_with('{') { "{}".to_string() } else { s.to_string() })
            .collect::<Vec<_>>()
            .join("/");
        let key = (r.method.as_str().to_string(), normalized);
        assert!(!seen.contains(&key), "duplicate route {key:?}");
        seen.push(key);
    }
    assert!(routes.len() >= 25, "the lifecycle needs its whole surface");
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_governance::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "governance");
    assert_eq!(adjutant_governance::adjutant_sdk_abi(), adjutant_sdk::SDK_ABI_VERSION);
}

// --- quorum -----------------------------------------------------------------

#[test]
fn quorum_bases_are_the_accords_arithmetic() {
    // Congress: one-third of registered scouts, rounded up (3rd Congress locked).
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 27, None), 9);
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 25, None), 9);
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 1, None), 1);
    // Troop Council: a majority of members.
    assert_eq!(compute_quorum(QUORUM_MAJORITY_MEMBERS, 5, None), 3);
    assert_eq!(compute_quorum(QUORUM_MAJORITY_MEMBERS, 4, None), 3);
    // Fixed, and an explicit override wins over the basis.
    assert_eq!(compute_quorum(QUORUM_FIXED, 27, Some(12)), 12);
    assert_eq!(compute_quorum(QUORUM_FIXED, 27, None), 0);
    // Unconfigured fails closed: no expected voters means no quorum rule.
    assert_eq!(compute_quorum(QUORUM_MAJORITY_MEMBERS, 0, None), 0);
    assert!(!quorum_met(0, 0), "an unconfigured meeting is not in quorum");
    assert!(!quorum_met(8, 9));
    assert!(quorum_met(9, 9));
}

// --- tallies ----------------------------------------------------------------

#[test]
fn tallies_apply_the_threshold() {
    let yes3_no1: Vec<serde_json::Value> = vec![vote("yes"), vote("yes"), vote("yes"), vote("no")];
    let simple = tally_votes(&yes3_no1, THRESHOLD_SIMPLE_MAJORITY);
    assert_eq!((simple.yes, simple.no, simple.abstain), (3, 1, 0));
    assert!(simple.passed);
    assert_eq!(simple.cast(), 4, "abstentions are not votes cast");

    // A tie does not carry (majority means more yes than no).
    let tie = vec![vote("yes"), vote("no")];
    assert!(!tally_votes(&tie, THRESHOLD_SIMPLE_MAJORITY).passed);
    // ... and abstentions do not break a majority.
    let with_abstain = vec![vote("yes"), vote("no"), vote("abstain")];
    let tally = tally_votes(&with_abstain, THRESHOLD_SIMPLE_MAJORITY);
    assert_eq!(tally.abstain, 1);
    assert!(!tally.passed, "1 yes to 1 no is not a majority");

    // Two thirds: 4 of 6 passes, 3 of 5 does not.
    let four_of_six = vec![vote("yes"), vote("yes"), vote("yes"), vote("yes"), vote("no"), vote("no")];
    assert!(tally_votes(&four_of_six, THRESHOLD_TWO_THIRDS).passed);
    let three_of_five = vec![vote("yes"), vote("yes"), vote("yes"), vote("no"), vote("no")];
    assert!(!tally_votes(&three_of_five, THRESHOLD_TWO_THIRDS).passed);

    // Unanimous: every vote cast is yes, abstentions don't spoil it.
    let unanimity = vec![vote("yes"), vote("yes"), vote("abstain")];
    assert!(tally_votes(&unanimity, THRESHOLD_UNANIMOUS).passed);
    let dissent = vec![vote("yes"), vote("no")];
    assert!(!tally_votes(&dissent, THRESHOLD_UNANIMOUS).passed);

    // No votes at all never carries a motion, and an unknown threshold is the
    // weakest rule rather than the strongest.
    assert!(!tally_votes(&[], THRESHOLD_SIMPLE_MAJORITY).passed);
    assert!(!tally_votes(&[], THRESHOLD_UNANIMOUS).passed);
    assert!(
        tally_votes(&yes3_no1, "unknown-threshold").passed,
        "an unrecognised threshold falls back to the weakest rule, not the strongest"
    );
}

#[test]
fn timestamps_are_shape_checked_before_postgres_sees_them() {
    assert!(is_timestamp_like("2026-12-13T15:00:00Z"));
    assert!(is_timestamp_like("2026-12-13 15:00:00-05"));
    assert!(is_timestamp_like("2026-12-13"));
    assert!(!is_timestamp_like("tomorrow"));
    assert!(!is_timestamp_like("13/12/2026T15:00"));
    assert!(!is_timestamp_like(""));
}

#[tokio::test]
async fn a_meeting_with_a_bad_timestamp_is_refused() {
    let (host, routes) = plugin_routes().await;
    let create = route(&routes, "POST", "/api/governance/meeting");

    let req = TestRequest::post("/api/governance/meeting")
        .json(&serde_json::json!({
            "body": "tc", "title": "Someday", "scheduled_for": "next Tuesday"
        }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&create.handler, req).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("scheduled_for"));
    assert_eq!(host.db.query_count(), 0);

    // An unknown body is refused just as early.
    let req = TestRequest::post("/api/governance/meeting")
        .json(&serde_json::json!({ "body": "cabal", "title": "x" }))
        .identity("bea", &["chief"])
        .build();
    let (status, _) = call(&create.handler, req).await;
    assert_eq!(status, 400);
    assert_eq!(host.db.query_count(), 0);
}

#[test]
fn motion_stage_guard_names_the_allowed_stages() {
    assert!(require_motion_stage("seconded", &["seconded", "debate"], "close the vote").is_ok());
    let err = require_motion_stage("proposed", &["seconded", "debate"], "close the vote").unwrap_err();
    assert_eq!(err.status(), 409);
    assert!(err.to_string().contains("proposed"), "got: {err}");
    assert!(MOTION_STAGES.contains(&"implemented"));
}

// --- minutes ----------------------------------------------------------------

#[test]
fn minutes_draft_from_the_record() {
    let meeting = serde_json::json!({
        "id": 1, "title": "Troop Council — September", "body": BODY_TC, "status": "closed",
        "scheduled_for": "2026-09-20T18:00:00Z", "quorum_basis": QUORUM_MAJORITY_MEMBERS,
        "expected_voters": 5, "quorum_required": 3, "minutes": "", "minutes_status": "none",
    });
    let attendance = vec![
        serde_json::json!({ "member_id": "bea", "present": true, "method": "present" }),
        serde_json::json!({ "member_id": "sara", "present": true, "method": "remote" }),
        serde_json::json!({ "member_id": "ira", "present": false, "method": "absent" }),
    ];
    let motions = vec![serde_json::json!({
        "id": 4, "title": "Adopt the 3rd Catamount Accords", "text": "that the troop adopt it",
        "stage": "decided", "result": RESULT_PASSED, "threshold": THRESHOLD_TWO_THIRDS,
        "proposed_by": "bea", "seconded_by": "sara", "amends_accords": true,
        "votes_yes": 2, "votes_no": 0, "votes_abstain": 0, "implementation_note": ""
    })];
    let amendments = vec![serde_json::json!({
        "id": 7, "motion_id": 4, "kind": AMENDMENT_FRIENDLY, "text": "insert 'by 2027'",
        "status": "accepted", "proposed_by": "ira"
    })];
    let votes = vec![
        serde_json::json!({ "id": 1, "motion_id": 4, "amendment_id": null, "voter": "bea", "choice": "yes", "method": "roll_call" }),
        serde_json::json!({ "id": 2, "motion_id": 4, "amendment_id": null, "voter": "sara", "choice": "yes", "method": "roll_call" }),
    ];

    let minutes = render_minutes(&meeting, &attendance, &motions, &amendments, &votes);
    assert!(minutes.contains("# Troop Council — September"));
    // Two present of five expected is short of the three a majority needs — the
    // draft says so rather than implying the meeting was quorate.
    assert!(
        minutes.contains("**Quorum:** 2 of 5 expected — NOT met (majority_members, requires 3)"),
        "got: {minutes}"
    );
    assert!(minutes.contains("## Attendance (2 present)"));
    assert!(minutes.contains("- bea (present, present)"));
    assert!(minutes.contains("### Motion 4 — Adopt the 3rd Catamount Accords"));
    assert!(minutes.contains("Result: passed"));
    assert!(minutes.contains("Seconded by: sara"));
    assert!(minutes.contains("**Amends the Accords**"), "an Art 17 motion says so");
    assert!(minutes.contains("Votes: 2 yes, 0 no, 0 abstain (roll_call; 2 recorded)"));
    assert!(minutes.contains("> that the troop adopt it"));
    assert!(minutes.contains("**Amendment 7 (friendly, accepted)** — proposed by ira"));
    assert!(
        minutes.contains("Draft generated from the motion record"),
        "a draft says it is a draft"
    );

    // A meeting with no quorum says so, and a partial record still renders.
    let mut unconfigured = meeting.clone();
    unconfigured["quorum_required"] = serde_json::json!(0);
    unconfigured["expected_voters"] = serde_json::json!(0);
    let minutes = render_minutes(&unconfigured, &[], &[], &[], &[]);
    assert!(minutes.contains("NOT met"), "an unconfigured meeting is not in quorum");
    assert!(minutes.contains("_No motions were recorded._"));
    assert!(minutes.contains("_No attendance recorded._"));
}

// --- motions ----------------------------------------------------------------

/// A proposal is validated before anything is written, and given a meeting it
/// must reference one that exists.
#[tokio::test]
async fn propose_motion_validates_then_publishes_motion_proposed() {
    let (host, routes) = plugin_routes().await;
    let propose = route(&routes, "POST", "/api/governance/motion");

    let bad = TestRequest::post("/api/governance/motion")
        .json(&serde_json::json!({ "title": "", "text": "x", "body": BODY_TC }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&propose.handler, bad).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("title"));
    assert_eq!(host.db.query_count(), 0);

    let bad_category = TestRequest::post("/api/governance/motion")
        .json(&serde_json::json!({
            "title": "x", "text": "y", "body": BODY_TC, "category": "vibes"
        }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&propose.handler, bad_category).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("category"));
    assert!(MOTION_CATEGORIES.contains(&"policy"));

    let missing_meeting = TestRequest::post("/api/governance/motion")
        .json(&serde_json::json!({ "title": "x", "text": "y", "body": BODY_TC, "meeting_id": 99 }))
        .identity("bea", &["chief"])
        .build();
    host.db.push_rows(vec![]); // meeting 99 does not exist
    let (status, body) = call(&propose.handler, missing_meeting).await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("meeting"));

    let good = TestRequest::post("/api/governance/motion")
        .json(&serde_json::json!({
            "title": "Adopt the 3rd Catamount Accords",
            "text": "that the troop adopt the 3rd Accords as amended",
            "body": BODY_CONGRESS,
            "meeting_id": 2,
            "category": "accords_amendment",
            "threshold": THRESHOLD_TWO_THIRDS,
            "amends_accords": true
        }))
        .identity("bea", &["chief"])
        .build();
    host.db.push_rows(vec![meeting_row(2, QUORUM_ONE_THIRD_REGISTERED, 27)]); // meeting
    host.db.push_rows(vec![serde_json::json!({
        "id": 4, "title": "Adopt the 3rd Catamount Accords", "stage": "proposed",
        "result": RESULT_PENDING, "threshold": THRESHOLD_TWO_THIRDS,
        "created_at": "2026-09-25T00:00:00Z"
    })]);
    let (status, body) = call(&propose.handler, good).await;
    assert_eq!(status, 201);
    assert_eq!(body["motion"]["stage"], serde_json::json!("proposed"));
    assert!(body["next"].as_str().unwrap().contains("second"));

    host.events.assert_published(event_type::MOTION_PROPOSED);
    let payload = &host.events.payloads(event_type::MOTION_PROPOSED)[0];
    assert_eq!(payload["motion_id"], serde_json::json!(4));
    assert_eq!(payload["body"], serde_json::json!(BODY_CONGRESS));
    host.db.assert_executed(&["INSERT INTO", "core.audit_log"]);
}

/// A motion is seconded by someone else; the mover cannot second their own.
#[tokio::test]
async fn second_requires_another_member_and_the_proposed_stage() {
    let (host, routes) = plugin_routes().await;
    let second = route(&routes, "POST", "/api/governance/motion/{id}/second");

    // The mover cannot second their own motion.
    host.db.push_rows(vec![motion_row(4, "proposed", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/motion/4/second").param("id", "4").build();
    let (status, body) = call(&second.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("second their own"));

    // A decided motion cannot be seconded.
    let (host, routes) = plugin_routes().await;
    let second = route(&routes, "POST", "/api/governance/motion/{id}/second");
    host.db.push_rows(vec![motion_row(4, "decided", RESULT_PASSED, None)]);
    let req = TestRequest::post("/api/governance/motion/4/second").param("id", "4").build();
    let (status, _) = call(&second.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);

    // Another member seconds it.
    let (host, routes) = plugin_routes().await;
    let second = route(&routes, "POST", "/api/governance/motion/{id}/second");
    let mut row = motion_row(4, "proposed", RESULT_PENDING, None);
    row["proposed_by"] = serde_json::json!("cara");
    host.db.push_rows(vec![row]);
    let req = TestRequest::post("/api/governance/motion/4/second").param("id", "4").build();
    let (status, body) = call(&second.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 200);
    assert_eq!(body["stage"], serde_json::json!("seconded"));
    assert_eq!(body["seconded_by"], serde_json::json!("bea"));
    host.db.assert_executed(&["UPDATE", "motions", "seconded_by = $2"]);
}

/// Debate opens after a second and closes into a vote.
#[tokio::test]
async fn debate_moves_seconded_to_debate_then_to_voting() {
    let (host, routes) = plugin_routes().await;
    let debate = route(&routes, "POST", "/api/governance/motion/{id}/debate");

    host.db.push_rows(vec![motion_row(4, "proposed", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/motion/4/debate")
        .param("id", "4")
        .json(&serde_json::json!({ "open": true }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&debate.handler, req).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("seconded"));

    let (host, routes) = plugin_routes().await;
    let debate = route(&routes, "POST", "/api/governance/motion/{id}/debate");
    host.db.push_rows(vec![motion_row(4, "seconded", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/motion/4/debate")
        .param("id", "4")
        .json(&serde_json::json!({ "open": true }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&debate.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["stage"], serde_json::json!("debate"));

    let (host, routes) = plugin_routes().await;
    let debate = route(&routes, "POST", "/api/governance/motion/{id}/debate");
    host.db.push_rows(vec![motion_row(4, "debate", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/motion/4/debate")
        .param("id", "4")
        .json(&serde_json::json!({ "open": false }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&debate.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["stage"], serde_json::json!("voting"));
}

/// A vote in a meeting is cast by someone recorded present, and recorded once.
#[tokio::test]
async fn votes_require_presence_and_are_recorded_once() {
    let (host, routes) = plugin_routes().await;
    let vote_route = route(&routes, "POST", "/api/governance/motion/{id}/vote");

    // Not present → refused, and nothing is written.
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, Some(5))]);
    host.db.push_rows(vec![]); // no attendance row
    let req = TestRequest::post("/api/governance/motion/4/vote")
        .param("id", "4")
        .json(&serde_json::json!({ "choice": "yes" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&vote_route.handler, req).await;
    assert_eq!(status, 403);
    assert!(body["error"].as_str().unwrap().contains("recorded present"));
    assert!(host.db.executed_sql().is_empty());

    // Present → recorded.
    let (host, routes) = plugin_routes().await;
    let vote_route = route(&routes, "POST", "/api/governance/motion/{id}/vote");
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, Some(5))]);
    host.db.push_rows(vec![serde_json::json!({ "member_id": "bea", "present": true })]);
    host.db.push_rows(vec![serde_json::json!({ "id": 11 })]);
    let req = TestRequest::post("/api/governance/motion/4/vote")
        .param("id", "4")
        .json(&serde_json::json!({ "choice": "YES", "method": "show_of_hands" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&vote_route.handler, req).await;
    assert_eq!(status, 201);
    assert_eq!(body["choice"], serde_json::json!("yes"));
    assert_eq!(body["method"], serde_json::json!("show_of_hands"));
    let vote_sql = host.db.last_query_params("INSERT INTO").expect("vote params");
    assert!(matches!(vote_sql[2], SqlValue::Text(ref c) if c == "yes"));
    assert!(VOTE_METHODS.contains(&"ballot"));

    // A second vote by the same member conflicts rather than replacing the first.
    let (host, routes) = plugin_routes().await;
    let vote_route = route(&routes, "POST", "/api/governance/motion/{id}/vote");
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, None)]);
    host.db.push_rows(vec![]); // ON CONFLICT DO NOTHING → no row
    let req = TestRequest::post("/api/governance/motion/4/vote")
        .param("id", "4")
        .json(&serde_json::json!({ "choice": "no" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&vote_route.handler, req).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("already voted"));

    // An invalid choice is refused before any query.
    let (host, routes) = plugin_routes().await;
    let vote_route = route(&routes, "POST", "/api/governance/motion/{id}/vote");
    let req = TestRequest::post("/api/governance/motion/4/vote")
        .param("id", "4")
        .json(&serde_json::json!({ "choice": "maybe" }))
        .identity("bea", &["chief"])
        .build();
    let (status, _) = call(&vote_route.handler, req).await;
    assert_eq!(status, 400);
    assert_eq!(host.db.query_count(), 0);
}

/// Closing needs a quorum when the motion sits in a meeting.
#[tokio::test]
async fn close_needs_a_quorum_in_a_meeting() {
    let (host, routes) = plugin_routes().await;
    let close = route(&routes, "POST", "/api/governance/motion/{id}/close");

    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, Some(5))]); // motion
    host.db.push_rows(vec![meeting_row(5, QUORUM_ONE_THIRD_REGISTERED, 27)]); // meeting
    host.db.push_rows(vec![serde_json::json!({ "n": 5 })]); // 5 present, 9 required

    let req = TestRequest::post("/api/governance/motion/4/close").param("id", "4").build();
    let (status, body) = call(&close.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("quorum"), "got: {error}");
    assert!(error.contains('5') && error.contains('9'), "the numbers are in the refusal: {error}");
    host.events.assert_none();
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("decided_at")),
        "a motion without quorum is not decided"
    );
}

/// A passed motion publishes `motion.passed` with the SDK payload (M4 exit
/// criterion), and a failed one publishes `motion.failed`.
#[tokio::test]
async fn close_tallies_and_publishes_the_outcome() {
    let (host, routes) = plugin_routes().await;
    let close = route(&routes, "POST", "/api/governance/motion/{id}/close");

    // In a meeting, quorum met (6 present of 9 expected → 5 required), 4 yes 1 no.
    let mut motion = motion_row(4, "voting", RESULT_PENDING, Some(5));
    motion["amends_accords"] = serde_json::json!(true);
    motion["title"] = serde_json::json!("Adopt the 3rd Catamount Accords");
    host.db.push_rows(vec![motion]);
    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 9)]);
    host.db.push_rows(vec![serde_json::json!({ "n": 6 })]);
    host.db.push_rows(vec![vote("yes"), vote("yes"), vote("yes"), vote("yes"), vote("no")]);

    let req = TestRequest::post("/api/governance/motion/4/close").param("id", "4").build();
    let (status, body) = call(&close.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], serde_json::json!(RESULT_PASSED));
    assert_eq!(body["votes_yes"], serde_json::json!(4));
    assert_eq!(body["votes_no"], serde_json::json!(1));
    assert_eq!(body["quorum"]["met"], serde_json::json!(true));

    host.events.assert_published(event_type::MOTION_PASSED);
    let payload = &host.events.payloads(event_type::MOTION_PASSED)[0];
    assert_eq!(payload["motion_id"], serde_json::json!(4));
    assert_eq!(payload["body"], serde_json::json!(BODY_TC));
    assert_eq!(payload["votes_yes"], serde_json::json!(4));
    assert_eq!(payload["amends_accords"], serde_json::json!(true));
    let typed: MotionPassed = serde_json::from_value(payload.clone()).expect("typed payload");
    assert_eq!(typed.votes_no, 1);
    assert!(typed.meeting_id == Some(5));
    host.db.assert_executed(&["UPDATE", "motions", "decided_at = now()"]);

    // The other way: fewer yes than no.
    let (host, routes) = plugin_routes().await;
    let close = route(&routes, "POST", "/api/governance/motion/{id}/close");
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, None)]);
    host.db.push_rows(vec![vote("yes"), vote("no"), vote("no")]);
    let req = TestRequest::post("/api/governance/motion/4/close").param("id", "4").build();
    let (status, body) = call(&close.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], serde_json::json!(RESULT_FAILED));
    host.events.assert_published(event_type::MOTION_FAILED);
    assert!(host.events.payloads(event_type::MOTION_PASSED).is_empty());
    assert!(body["quorum"].is_null(), "a motion with no meeting has no quorum block");
}

/// A motion is implemented only after it passed.
#[tokio::test]
async fn implement_requires_a_passed_motion() {
    let (host, routes) = plugin_routes().await;
    let implement = route(&routes, "POST", "/api/governance/motion/{id}/implement");

    host.db.push_rows(vec![motion_row(4, "decided", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/motion/4/implement")
        .param("id", "4")
        .json(&serde_json::json!({}))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&implement.handler, req).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("passed"));

    let (host, routes) = plugin_routes().await;
    let implement = route(&routes, "POST", "/api/governance/motion/{id}/implement");
    host.db.push_rows(vec![motion_row(4, "decided", RESULT_PASSED, None)]);
    let req = TestRequest::post("/api/governance/motion/4/implement")
        .param("id", "4")
        .json(&serde_json::json!({ "note": "policy circulated to lodges" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&implement.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["stage"], serde_json::json!("implemented"));
    host.db.assert_executed(&["implemented_at = now()"]);
}

/// The real-time quorum display: what the room looks at during a meeting.
#[tokio::test]
async fn quorum_endpoint_reports_the_live_state() {
    let (host, routes) = plugin_routes().await;
    let quorum = route(&routes, "GET", "/api/governance/meeting/{id}/quorum");

    host.db.push_rows(vec![meeting_row(5, QUORUM_ONE_THIRD_REGISTERED, 27)]);
    host.db.push_rows(vec![serde_json::json!({ "n": 6 })]);

    let req = TestRequest::get("/api/governance/meeting/5/quorum").param("id", "5").build();
    let resp = (quorum.handler)(req).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    assert_eq!(body["quorum"]["required"], serde_json::json!(9));
    assert_eq!(body["quorum"]["present"], serde_json::json!(6));
    assert_eq!(body["quorum"]["met"], serde_json::json!(false));
    assert_eq!(body["quorum"]["short"], serde_json::json!(3), "how many more are needed");
    assert_eq!(body["quorum"]["basis"], serde_json::json!(QUORUM_ONE_THIRD_REGISTERED));
    assert_eq!(body["quorum"]["basis_configured"], serde_json::json!(true));

    // An unconfigured meeting reports the fail-closed state.
    let (host, routes) = plugin_routes().await;
    let quorum = route(&routes, "GET", "/api/governance/meeting/{id}/quorum");
    let mut unconfigured = meeting_row(5, QUORUM_MAJORITY_MEMBERS, 0);
    unconfigured["quorum_basis"] = serde_json::json!(QUORUM_FIXED);
    host.db.push_rows(vec![unconfigured]);
    host.db.push_rows(vec![serde_json::json!({ "n": 3 })]);
    let req = TestRequest::get("/api/governance/meeting/5/quorum").param("id", "5").build();
    let resp = (quorum.handler)(req).await.expect("handler");
    let body = response_json(&resp);
    assert_eq!(body["quorum"]["met"], serde_json::json!(false));
    assert_eq!(body["quorum"]["basis_configured"], serde_json::json!(false));
}

// --- amendments -------------------------------------------------------------

/// A friendly amendment is the mover's to accept, and accepting it changes the
/// motion's text — the record shows what was decided, not what was first said.
#[tokio::test]
async fn friendly_amendment_is_accepted_by_the_mover_and_applied() {
    let (host, routes) = plugin_routes().await;
    let accept = route(&routes, "POST", "/api/governance/amendment/{id}/accept");

    // A formal amendment is not accepted by the mover; it is voted on.
    host.db.push_rows(vec![serde_json::json!({
        "id": 7, "motion_id": 4, "kind": AMENDMENT_FORMAL, "status": "proposed",
        "text": "replace clause 2"
    })]);
    let req = TestRequest::post("/api/governance/amendment/7/accept")
        .param("id", "7")
        .json(&serde_json::json!({}))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&accept.handler, req).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("formal"));

    // The mover accepts a friendly one: no vote, and the text is amended.
    let (host, routes) = plugin_routes().await;
    let accept = route(&routes, "POST", "/api/governance/amendment/{id}/accept");
    host.db.push_rows(vec![serde_json::json!({
        "id": 7, "motion_id": 4, "kind": AMENDMENT_FRIENDLY, "status": "proposed",
        "text": "insert 'by the 3rd Congress'"
    })]);
    host.db.push_rows(vec![motion_row(4, "debate", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/amendment/7/accept")
        .param("id", "7")
        .json(&serde_json::json!({ "note": "accepted in the room" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&accept.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], serde_json::json!("accepted"));
    host.db.assert_executed(&["UPDATE", "amendments", "status = 'accepted'"]);
    let applied = host.db.last_execute_params("UPDATE").expect("params");
    assert!(
        matches!(&applied[1], SqlValue::Text(t) if t.contains("Amendment 7 (friendly)")),
        "the amendment is appended to the motion text: {applied:?}"
    );
    assert!(
        host.db
            .executed_sql()
            .iter()
            .any(|s| s.contains("INSERT INTO") && s.contains("core.audit_log")),
        "an accepted amendment is audited"
    );

    // A formal amendment is tallied, and a passing one is applied too.
    let (host, routes) = plugin_routes().await;
    let close = route(&routes, "POST", "/api/governance/amendment/{id}/close");
    host.db.push_rows(vec![serde_json::json!({
        "id": 8, "motion_id": 4, "kind": AMENDMENT_FORMAL, "status": "proposed",
        "text": "replace clause 2"
    })]);
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, None)]);
    host.db.push_rows(vec![vote("yes"), vote("yes"), vote("no")]);
    let req = TestRequest::post("/api/governance/amendment/8/close")
        .param("id", "8")
        .json(&serde_json::json!({}))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&close.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], serde_json::json!("accepted"));
    assert_eq!(body["votes_yes"], serde_json::json!(2));
    let applied = host.db.last_execute_params("SET text = $2").expect("motion text update");
    assert!(
        matches!(&applied[1], SqlValue::Text(t) if t.contains("Amendment 8 (formal)")),
        "a formal amendment that carries is applied to the motion text: {applied:?}"
    );
}

// --- Accords versioning -----------------------------------------------------

/// "Accords adopted by majority vote at Catamount Congress" (Art 17): the
/// version comes from a **passed Congress motion**, and it supersedes the
/// previous adopted one.
#[tokio::test]
async fn accords_version_requires_a_passed_congress_motion() {
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/accords/adopt");

    let body = serde_json::json!({
        "motion_id": 4, "title": "3rd Catamount Accords", "summary": "as amended",
        "body_md": "# Accords", "adopted_on": "2026-12-13", "congress": "3rd Catamount Congress"
    });

    // A motion that has not passed cannot adopt a version.
    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, None)]);
    let req = TestRequest::post("/api/governance/accords/adopt")
        .param("id", "4")
        .json(&body)
        .identity("bea", &["chief"])
        .build();
    let (status, resp_body) = call(&adopt.handler, req).await;
    assert_eq!(status, 400);
    assert!(resp_body["error"].as_str().unwrap().contains("passed"));

    // A Troop Council motion cannot adopt the Accords.
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/accords/adopt");
    host.db.push_rows(vec![motion_row(4, "decided", RESULT_PASSED, None)]);
    let req = TestRequest::post("/api/governance/accords/adopt")
        .param("id", "4")
        .json(&body)
        .identity("bea", &["chief"])
        .build();
    let (status, resp_body) = call(&adopt.handler, req).await;
    assert_eq!(status, 400);
    assert!(resp_body["error"].as_str().unwrap().contains("Congress"));

    // A motion that already created a version cannot create another.
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/accords/adopt");
    let mut congress = motion_row(4, "decided", RESULT_PASSED, None);
    congress["body"] = serde_json::json!(BODY_CONGRESS);
    host.db.push_rows(vec![congress]);
    host.db.push_rows(vec![serde_json::json!({ "1": 1 })]); // source_motion_id already present
    let req = TestRequest::post("/api/governance/accords/adopt")
        .param("id", "4")
        .json(&body)
        .identity("bea", &["chief"])
        .build();
    let (status, resp_body) = call(&adopt.handler, req).await;
    assert_eq!(status, 409);
    assert!(resp_body["error"].as_str().unwrap().contains("already"));

    // Adoption: version 3, superseding version 2.
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/accords/adopt");
    let mut congress = motion_row(4, "decided", RESULT_PASSED, None);
    congress["body"] = serde_json::json!(BODY_CONGRESS);
    host.db.push_rows(vec![congress]); // motion
    host.db.push_rows(vec![]); // no version from this motion yet
    host.db.push_rows(vec![serde_json::json!({ "id": 9, "version": 2 })]); // currently adopted
    host.db.push_rows(vec![serde_json::json!({
        "id": 10, "version": 3, "title": "3rd Catamount Accords",
        "status": "adopted", "adopted_on": "2026-12-13"
    })]);
    let req = TestRequest::post("/api/governance/accords/adopt")
        .param("id", "4")
        .json(&body)
        .identity("bea", &["chief"])
        .build();
    let (status, resp_body) = call(&adopt.handler, req).await;
    assert_eq!(status, 201);
    assert_eq!(resp_body["accords"]["version"], serde_json::json!(3));
    assert_eq!(resp_body["supersedes"], serde_json::json!(2));
    host.db.assert_executed(&["UPDATE", "accords_versions", "status = 'superseded'"]);
    let published = host.events.payloads("accords.adopted");
    assert_eq!(published[0]["version"], serde_json::json!(3));
}

// --- minutes ----------------------------------------------------------------

/// Minutes are drafted from the record, then adopted — the Archivist's edit is
/// the adoption.
#[tokio::test]
async fn minutes_are_drafted_from_the_record_then_adopted() {
    let (host, routes) = plugin_routes().await;
    let draft = route(&routes, "POST", "/api/governance/meeting/{id}/minutes/draft");

    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]); // meeting
    host.db.push_rows(vec![serde_json::json!({ "member_id": "bea", "present": true, "method": "present" })]);
    host.db.push_rows(vec![serde_json::json!({
        "id": 4, "title": "Adopt the 3rd Catamount Accords", "text": "that it be adopted",
        "stage": "decided", "result": RESULT_PASSED, "threshold": THRESHOLD_SIMPLE_MAJORITY,
        "proposed_by": "bea", "seconded_by": "sara", "amends_accords": true,
        "votes_yes": 1, "votes_no": 0, "votes_abstain": 0, "implementation_note": ""
    })]);
    host.db.push_rows(vec![]); // amendments
    host.db.push_rows(vec![serde_json::json!({
        "id": 1, "motion_id": 4, "amendment_id": null, "voter": "bea",
        "choice": "yes", "method": "voice"
    })]);

    let req = TestRequest::post("/api/governance/meeting/5/minutes/draft")
        .param("id", "5")
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&draft.handler, req).await;
    assert_eq!(status, 201);
    assert_eq!(body["minutes_status"], serde_json::json!("draft"));
    let minutes = body["minutes"].as_str().expect("minutes");
    assert!(minutes.contains("Troop Council — September"));
    assert!(minutes.contains("Motion 4 — Adopt the 3rd Catamount Accords"));
    assert!(minutes.contains("Draft generated from the motion record"));
    host.db.assert_executed(&["UPDATE", "meetings", "minutes_status = 'draft'"]);

    // Adopting an edited draft stores the edit.
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/meeting/{id}/minutes/adopt");
    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]);
    let req = TestRequest::post("/api/governance/meeting/5/minutes/adopt")
        .param("id", "5")
        .json(&serde_json::json!({ "minutes": "# Corrected minutes" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&adopt.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["minutes_status"], serde_json::json!("adopted"));
    let params = host.db.last_execute_params("minutes = $2").expect("params");
    assert!(matches!(&params[1], SqlValue::Text(m) if m == "# Corrected minutes"));

    // Adopting with nothing drafted and nothing supplied is refused.
    let (host, routes) = plugin_routes().await;
    let adopt = route(&routes, "POST", "/api/governance/meeting/{id}/minutes/adopt");
    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]);
    let req = TestRequest::post("/api/governance/meeting/5/minutes/adopt")
        .param("id", "5")
        .json(&serde_json::json!({}))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&adopt.handler, req).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("draft"));
}

/// Attendance is recorded per member and moves the live quorum count.
#[tokio::test]
async fn attendance_updates_the_quorum() {
    let (host, routes) = plugin_routes().await;
    let attendance = route(&routes, "POST", "/api/governance/meeting/{id}/attendance");

    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]); // meeting exists
    host.db.push_rows(vec![serde_json::json!({
        "meeting_id": 5, "member_id": "bea", "present": true, "method": "remote"
    })]);
    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]); // re-read for quorum
    host.db.push_rows(vec![serde_json::json!({ "n": 3 })]); // present now

    let req = TestRequest::post("/api/governance/meeting/5/attendance")
        .param("id", "5")
        .json(&serde_json::json!({ "member_id": "bea", "present": true, "method": "remote" }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&attendance.handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["attendance"]["method"], serde_json::json!("remote"));
    assert_eq!(body["quorum"]["required"], serde_json::json!(3));
    assert_eq!(body["quorum"]["present"], serde_json::json!(3));
    assert_eq!(body["quorum"]["met"], serde_json::json!(true));

    // An unknown method is refused before any query.
    let (host, routes) = plugin_routes().await;
    let attendance = route(&routes, "POST", "/api/governance/meeting/{id}/attendance");
    let req = TestRequest::post("/api/governance/meeting/5/attendance")
        .param("id", "5")
        .json(&serde_json::json!({ "member_id": "bea", "method": "carrier pigeon" }))
        .identity("bea", &["chief"])
        .build();
    let (status, _) = call(&attendance.handler, req).await;
    assert_eq!(status, 400);
    assert_eq!(host.db.query_count(), 0);
}

/// The motion list is filterable, and a motion's detail reports the tally its
/// close would produce right now.
#[tokio::test]
async fn motion_detail_reports_the_running_tally() {
    let (host, routes) = plugin_routes().await;
    let detail = route(&routes, "GET", "/api/governance/motion/{id}");

    host.db.push_rows(vec![motion_row(4, "voting", RESULT_PENDING, Some(5))]);
    host.db.push_rows(vec![vote("yes"), vote("yes"), vote("abstain")]);
    host.db.push_rows(vec![]); // amendments
    host.db.push_rows(vec![meeting_row(5, QUORUM_MAJORITY_MEMBERS, 5)]); // meeting for quorum
    host.db.push_rows(vec![serde_json::json!({ "n": 4 })]);

    let req = TestRequest::get("/api/governance/motion/4").param("id", "4").build();
    let resp = (detail.handler)(req).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    assert_eq!(body["tally"]["yes"], serde_json::json!(2));
    assert_eq!(body["tally"]["abstain"], serde_json::json!(1));
    assert_eq!(body["tally"]["would_pass"], serde_json::json!(true));
    assert_eq!(body["quorum"]["present"], serde_json::json!(4));
    assert_eq!(body["quorum"]["met"], serde_json::json!(true));
}
