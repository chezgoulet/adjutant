//! Missions plugin tests: the lifecycle guards, mentor matching, and the routes
//! that carry the Accords' Mission System (SPEC §7.3, Accords Art 8).
//!
//! Handlers are driven through `adjutant_sdk::testing` — no database, no server.
//! The mock replays rows in call order, so each test names the **query order**
//! it arranges around: the first `push_rows` answers the first query.

use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use adjutant_missions::{
    is_iso_date, rank_mentors, require_stage, require_stage_in, MissionsPlugin, Proposal,
    CATEGORIES, STAGE_APPROVAL, STAGE_DEBRIEF, STAGE_EXECUTION, STAGE_REPORT, STAGE_REQUEST,
    STAGE_REVIEW, STAGES, STATE_COMPLETED, STATE_OPEN, STATE_REJECTED,
};

/// Initialise the plugin against a mock host and hand back its routes.
async fn plugin_routes() -> (TestHost, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = MissionsPlugin::new();
    plugin.init(host.context("missions")).await.unwrap();
    let routes = plugin.routes();
    (host, routes)
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

/// A troop-wide identity: every check of a troop scope is a query answering
/// `n = 1`, so callers push one permission row per in-handler check.
fn troop_identity() -> Identity {
    Identity::new("bea", vec!["chief".to_string()])
}

fn scoped_identity(role: &str, scope: Scope) -> Identity {
    Identity::from_grants("bea", vec![RoleGrant { role_id: role.into(), scope }])
}

fn mission_row(id: i64, stage: &str, created_by: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "title": "Coyote survey",
        "stage": stage,
        "state": STATE_OPEN,
        "lodge_id": "3",
        "created_by": created_by,
        "service_hours": 12.5,
        "participant_count": 6,
        "impact_metrics": {},
        "report_summary": "done",
        "tags": ["wildlife", "tracking"],
    })
}

fn rejected_mission_row(id: i64) -> serde_json::Value {
    let mut row = mission_row(id, STAGE_APPROVAL, "bea");
    row["state"] = serde_json::json!(STATE_REJECTED);
    row
}

const MAYBE_LODGE: &str = "/api/missions/mission/{id}";

fn req_with_identity(mut req: PluginRequest, identity: Identity) -> PluginRequest {
    req.identity = Some(identity);
    req
}

/// Drive a handler and report `(status, body)`.
///
/// A handler may refuse two ways — an `SdkError` (which the core maps through
/// `SdkError::status()`) or a `PluginResponse::error` — and from a client they
/// are the same answer. This collapses both so a test asserts the status the
/// caller actually sees.
async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, serde_json::Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), serde_json::json!({ "error": e.to_string() })),
    }
}

// --- the declaration the loader validates ----------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, routes) = plugin_routes().await;
    let plugin = MissionsPlugin::new();
    assert_eq!(plugin.id(), "missions");

    let permissions: Vec<String> =
        plugin.permissions_granted().into_iter().map(|p| p.id).collect();
    for perm in &permissions {
        assert!(
            perm.starts_with("missions:"),
            "permission {perm} must be namespaced by the plugin id"
        );
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    assert!(versions.iter().all(|v| *v >= 1), "versions start at 1");

    // SPEC §7.3's tables, plus the supporting ones the lifecycle needs.
    let ddl = &migrations[0].sql;
    for table in [
        "missions",
        "milestones",
        "mentorships",
        "mentor_profiles",
        "progress_notes",
        "mission_appeals",
        "mission_stage_log",
    ] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "migration is missing {table}"
        );
    }

    // Every route is inside the namespace, gated on a permission the plugin
    // declares, and unique after capture normalisation — the four things
    // `plugin_runtime::validate_declaration` rejects at load.
    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(r.path.starts_with("/api/missions"), "{} escapes the namespace", r.path);
        let perm = r.required_permission.as_deref().expect("every missions route is gated");
        assert!(permissions.iter().any(|p| p == perm), "{perm} is required but not declared");
        for segment in r.path.split('/') {
            assert!(
                !segment.contains('{') || (segment.starts_with('{') && segment.ends_with('}')),
                "{} has a partial capture",
                r.path
            );
        }
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
    assert!(routes.len() >= 20, "the lifecycle needs its whole surface");
}

#[test]
fn entry_symbol_is_exported() {
    // The core resolves these by name via libloading, so a typo would only
    // surface at load time — pin them from outside the crate.
    let raw = adjutant_missions::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "missions");
    assert_eq!(adjutant_missions::adjutant_sdk_abi(), adjutant_sdk::SDK_ABI_VERSION);
}

// --- the six-stage lifecycle -----------------------------------------------

#[test]
fn the_six_stages_are_the_accords_order() {
    assert_eq!(
        STAGES,
        [
            STAGE_REQUEST,
            STAGE_REVIEW,
            STAGE_APPROVAL,
            STAGE_EXECUTION,
            STAGE_DEBRIEF,
            STAGE_REPORT
        ]
    );
    assert_eq!(adjutant_missions::stage_index(STAGE_REQUEST), Some(0));
    assert_eq!(adjutant_missions::stage_index(STAGE_REPORT), Some(5));
    assert_eq!(adjutant_missions::stage_index("nonsense"), None);
}

#[test]
fn stage_guards_name_both_stages() {
    assert!(require_stage(STAGE_REQUEST, STAGE_REQUEST, "submit").is_ok());
    let err = require_stage(STAGE_APPROVAL, STAGE_REQUEST, "submit").unwrap_err();
    assert_eq!(err.status(), 409);
    let message = err.to_string();
    assert!(
        message.contains("approval") && message.contains("request"),
        "a 409 must name where the mission is and where it must be: {message}"
    );

    assert!(require_stage_in(STAGE_EXECUTION, &[STAGE_APPROVAL, STAGE_EXECUTION], "plan").is_ok());
    assert!(require_stage_in(STAGE_DEBRIEF, &[STAGE_EXECUTION], "record progress").is_err());
}

#[test]
fn iso_dates_only() {
    assert!(is_iso_date("2026-12-13"));
    assert!(!is_iso_date("13/12/2026"));
    assert!(!is_iso_date("2026-12-13T00:00:00Z"));
    assert!(!is_iso_date("2026-13-01"));
    assert!(!is_iso_date("2026-12-32"));
    assert!(!is_iso_date(""));
}

// --- the proposal form ------------------------------------------------------

#[test]
fn proposal_validation_names_the_field_at_fault() {
    let mut proposal = Proposal {
        title: "Coyote survey".into(),
        purpose: "count".into(),
        objectives: "walk transects".into(),
        expected_impact: "data for the town".into(),
        category: None,
        lodge_id: None,
        lodge_name: None,
        tags: vec![],
        location: None,
        starts_on: None,
        ends_on: None,
        resources_needed: None,
        risk_notes: None,
        youth_safety_notes: None,
        participant_count: None,
    };
    assert!(proposal.validate().is_ok());
    assert_eq!(proposal.category_code(), "service", "the default is a stable code");
    assert!(CATEGORIES.contains(&proposal.category_code().as_str()));

    proposal.purpose = "   ".into();
    let err = proposal.validate().unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.to_string().contains("purpose"), "got: {err}");
    proposal.purpose = "count".into();

    proposal.category = Some("Conservation".into());
    assert!(proposal.validate().is_ok(), "categories match case-insensitively");
    assert_eq!(proposal.category_code(), "conservation");
    proposal.category = Some("birdwatching".into());
    assert!(proposal.validate().is_err(), "an unknown category is refused, not stored");
    proposal.category = None;

    proposal.starts_on = Some("12/13/2026".into());
    assert!(proposal.validate().is_err());
    proposal.starts_on = Some("2026-12-20".into());
    proposal.ends_on = Some("2026-12-13".into());
    let err = proposal.validate().unwrap_err();
    assert!(err.to_string().contains("starts_on"), "got: {err}");
    proposal.ends_on = Some("2026-12-27".into());
    assert!(proposal.validate().is_ok());

    proposal.participant_count = Some(-1);
    assert!(proposal.validate().is_err());
}

// --- mentor matching --------------------------------------------------------

#[test]
fn rank_mentors_orders_by_overlap_then_spare_capacity() {
    let candidates = vec![
        serde_json::json!({
            "member_id": "ar", "display_name": "Arielle", "capacity": 3, "is_active": true,
            "expertise": ["tracking", "wildlife", "first aid"], "load": 1
        }),
        serde_json::json!({
            "member_id": "ir", "display_name": "Ira", "capacity": 2, "is_active": true,
            "expertise": ["tracking"], "load": 0
        }),
        serde_json::json!({
            "member_id": "sa", "display_name": "Sara", "capacity": 1, "is_active": true,
            "expertise": [], "load": 0
        }),
        serde_json::json!({
            "member_id": "full", "display_name": "Full", "capacity": 1, "is_active": true,
            "expertise": ["tracking", "wildlife"], "load": 1
        }),
        serde_json::json!({
            "member_id": "gone", "display_name": "Gone", "capacity": 5, "is_active": false,
            "expertise": ["tracking"], "load": 0
        }),
    ];
    let tags = vec!["Wildlife".to_string(), "tracking".to_string()];
    let ranked = rank_mentors(&candidates, &tags, &["ir".to_string()]);

    let ids: Vec<&str> = ranked.iter().map(|m| m.member_id.as_str()).collect();
    // Arielle shares both tags (case-insensitively); Sara shares none but has
    // spare capacity; Ira is excluded (already matching this mission); Full has
    // no spare capacity; Gone is inactive.
    assert_eq!(ids, vec!["ar", "sa"]);
    assert_eq!(ranked[0].tag_overlap, 2);
    assert_eq!(ranked[1].tag_overlap, 0);

    // With no mission tags, spare capacity decides.
    let ranked = rank_mentors(&candidates, &[], &[]);
    assert_eq!(
        ranked.iter().map(|m| m.member_id.as_str()).collect::<Vec<_>>(),
        vec!["ar", "ir", "sa"]
    );
}

// --- routes -----------------------------------------------------------------

/// The submit transition refuses a mission that is not in Request.
#[tokio::test]
async fn submit_requires_the_request_stage() {
    let (host, routes) = plugin_routes().await;
    let submit = route(&routes, "POST", "/api/missions/mission/{id}/submit");

    host.db.push_rows(vec![mission_row(7, STAGE_APPROVAL, "bea")]); // 1: fetch mission
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // 2: has(update, lodge 3)

    let req = TestRequest::post("/api/missions/mission/7/submit")
        .param("id", "7")
        .build();
    let (status, body) = call(&submit.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("approval"));
    host.events.assert_none();
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("UPDATE")),
        "a refused transition writes nothing"
    );
}

/// request → review: the UPDATE, the stage trail, and the audit entry.
#[tokio::test]
async fn submit_moves_request_to_review_and_trails_it() {
    let (host, routes) = plugin_routes().await;
    let submit = route(&routes, "POST", "/api/missions/mission/{id}/submit");

    host.db.push_rows(vec![mission_row(7, STAGE_REQUEST, "bea")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(update, lodge 3)

    let req = TestRequest::post("/api/missions/mission/7/submit").param("id", "7").build();
    let resp = (submit.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 200);
    assert_eq!(response_json(&resp)["stage"], serde_json::json!(STAGE_REVIEW));

    host.db.assert_executed(&["UPDATE", "missions", "stage = $2", "state = $3"]);
    host.db.assert_executed(&["INSERT INTO", "mission_stage_log", "to_stage"]);
    host.db.assert_executed(&["INSERT INTO", "core.audit_log"]);
    let params = host.db.last_execute_params("mission_stage_log").expect("stage-log params");
    assert!(matches!(params[1], SqlValue::Text(ref s) if s == STAGE_REQUEST));
    assert!(matches!(params[2], SqlValue::Text(ref s) if s == STAGE_REVIEW));
    host.events.assert_none();
}

/// A proposal is validated before anything is written; a good one publishes
/// `mission.created` and answers 201 with a `location` header.
#[tokio::test]
async fn propose_validates_then_publishes_mission_created() {
    let (host, routes) = plugin_routes().await;
    let propose = route(&routes, "POST", "/api/missions/mission");

    let bad = TestRequest::post("/api/missions/mission")
        .json(&serde_json::json!({
            "title": "Coyote survey", "purpose": "", "objectives": "walk",
            "expected_impact": "data"
        }))
        .identity("bea", &["chief"])
        .build();
    let (status, body) = call(&propose.handler, bad).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("purpose"));
    assert_eq!(host.db.query_count(), 0, "a rejected proposal touches no table");
    assert!(host.db.executed_sql().is_empty());

    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // reach(create, lodge 3)
    host.db.push_rows(vec![serde_json::json!({
        "id": 12, "stage": STAGE_REQUEST, "state": STATE_OPEN, "created_at": "2026-09-25T00:00:00Z"
    })]);
    let good = TestRequest::post("/api/missions/mission")
        .json(&serde_json::json!({
            "title": "Coyote survey",
            "purpose": "count the local coyote population",
            "objectives": "walk transects with the game warden",
            "expected_impact": "a baseline the town can plan against",
            "category": "conservation",
            "lodge_id": "3",
            "tags": ["wildlife", "tracking"],
            "starts_on": "2026-12-13",
            "ends_on": "2026-12-27"
        }))
        .identity("bea", &["chief"])
        .build();
    let resp = (propose.handler)(good).await.expect("handler");
    assert_eq!(resp.status, 201);
    assert!(
        resp.headers.iter().any(|(k, v)| k == "location" && v.contains("/api/missions/mission/12")),
        "a created mission is addressable"
    );
    assert_eq!(response_json(&resp)["stage"], serde_json::json!(STAGE_REQUEST));

    let published = host.events.payloads(event_type::MISSION_CREATED);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["mission_id"], serde_json::json!(12));
    // `INSERT … RETURNING` is a query on the host, so the bindings are read from
    // the query side.
    let insert = host.db.last_query_params("COALESCE($15, 0)").expect("insert params");
    assert!(matches!(insert[4], SqlValue::Text(ref s) if s == "conservation"));
    assert!(matches!(insert[5], SqlValue::TextArray(_)), "tags bind as text[]");
    assert!(matches!(insert[9], SqlValue::Text(ref s) if s == "2026-12-13"));
    // Dates are cast in SQL: a bare text parameter against a date column is a
    // runtime type error, and a text null is the wrong null for a date.
    let sql = host.db.queried_sql();
    assert!(sql[1].contains("$10::date"), "dates arrive as text and are cast: {}", sql[1]);
}

/// A mission outside the caller's scope is a 403 — the same answer for absent
/// and for forbidden, so existence is not leaked.
#[tokio::test]
async fn detail_hides_another_mission_from_a_scoped_caller() {
    let (host, routes) = plugin_routes().await;
    let detail = route(&routes, "GET", MAYBE_LODGE);

    host.db.push_rows(vec![mission_row(9, STAGE_EXECUTION, "someone_else")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has(read, troop)
    host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has(read, lodge 3)

    let req = TestRequest::get("/api/missions/mission/9").param("id", "9").build();
    let scoped = scoped_identity("scout", Scope::troop());
    let (status, body) = call(&detail.handler, req_with_identity(req, scoped)).await;
    assert_eq!(status, 403);
    assert!(body["error"].as_str().unwrap().contains("missions:read"));
    assert_eq!(host.db.query_count(), 3, "no child rows are fetched for a denied caller");

    // The same answer for a mission that does not exist.
    let (host, routes) = plugin_routes().await;
    let detail = route(&routes, "GET", MAYBE_LODGE);
    host.db.push_rows(vec![]);
    let req = TestRequest::get("/api/missions/mission/404").param("id", "404").build();
    let (status, _) = call(&detail.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 403);
}

/// A troop-wide reader gets the whole record: the mission, its milestones, the
/// mentorships, the progress notes, the stage trail, and the appeals.
#[tokio::test]
async fn detail_returns_the_children_for_a_troop_reader() {
    let (host, routes) = plugin_routes().await;
    let detail = route(&routes, "GET", MAYBE_LODGE);

    host.db.push_rows(vec![mission_row(9, STAGE_EXECUTION, "bea")]);
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(read, troop)
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "title": "Transect 1", "status": "done", "progress_pct": 100 })]);
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "mentor_member": "ar", "status": "active" })]);
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "note": "started", "progress_pct": 10 })]);
    host.db.push_rows(vec![serde_json::json!({ "from_stage": null, "to_stage": "request" })]);
    host.db.push_rows(vec![]);

    let req = TestRequest::get("/api/missions/mission/9").param("id", "9").build();
    let resp = (detail.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    assert_eq!(body["mission"]["id"], serde_json::json!(9));
    assert_eq!(body["milestones"][0]["status"], serde_json::json!("done"));
    assert_eq!(body["mentorships"][0]["mentor_member"], serde_json::json!("ar"));
    assert_eq!(body["progress"][0]["note"], serde_json::json!("started"));
    assert_eq!(body["stage_log"][0]["to_stage"], serde_json::json!("request"));
    assert_eq!(body["appeals"], serde_json::json!([]));
    assert_eq!(host.db.query_count(), 7);
}

/// Report → completed publishes `mission.completed` with the SDK payload — the
/// M4 exit criterion.
#[tokio::test]
async fn complete_publishes_mission_completed() {
    let (host, routes) = plugin_routes().await;
    let complete = route(&routes, "POST", "/api/missions/mission/{id}/complete");

    host.db.push_rows(vec![mission_row(7, STAGE_REPORT, "bea")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // reach(approve, lodge 3)

    let req = TestRequest::post("/api/missions/mission/7/complete").param("id", "7").build();
    let resp = (complete.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 200);
    assert_eq!(response_json(&resp)["state"], serde_json::json!(STATE_COMPLETED));

    host.events.assert_published(event_type::MISSION_COMPLETED);
    let payload = &host.events.payloads(event_type::MISSION_COMPLETED)[0];
    assert_eq!(payload["mission_id"], serde_json::json!(7));
    assert_eq!(payload["stage"], serde_json::json!(STAGE_REPORT));
    assert_eq!(payload["lodge_id"], serde_json::json!("3"));
    assert_eq!(payload["impact"]["service_hours"], serde_json::json!(12.5));
    // The payload round-trips into the SDK's typed struct — the contract that
    // archive/finance/the client subscribe to.
    let typed: MissionCompleted = serde_json::from_value(payload.clone()).expect("typed payload");
    assert_eq!(typed.title, "Coyote survey");
    assert_eq!(typed.stage, STAGE_REPORT);

    host.db.assert_executed(&["completed_at = now()"]);
    host.db.assert_executed(&["INSERT INTO", "core.audit_log"]);
}

/// A mission that has not reached Report cannot be closed out.
#[tokio::test]
async fn complete_requires_the_report_stage() {
    let (host, routes) = plugin_routes().await;
    let complete = route(&routes, "POST", "/api/missions/mission/{id}/complete");

    host.db.push_rows(vec![mission_row(7, STAGE_EXECUTION, "bea")]);
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]);

    let req = TestRequest::post("/api/missions/mission/7/complete").param("id", "7").build();
    let (status, body) = call(&complete.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("report"));
    host.events.assert_none();
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("completed_at")),
        "nothing was closed"
    );
}

/// The proposal edit route validates what it will cast in SQL, so a client's
/// typo is a 400 rather than a database error surfacing as a 500.
#[tokio::test]
async fn update_validates_dates_before_the_cast() {
    let (host, routes) = plugin_routes().await;
    let update = route(&routes, "PATCH", "/api/missions/mission/{id}");

    host.db.push_rows(vec![mission_row(7, STAGE_REQUEST, "bea")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(update, lodge 3)
    let req = TestRequest::patch("/api/missions/mission/7")
        .param("id", "7")
        .json(&serde_json::json!({ "starts_on": "13/12/2026" }))
        .build();
    let (status, body) = call(&update.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("starts_on"));
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("UPDATE")),
        "nothing is written for a malformed date"
    );

    // A reversed range is refused too, and a valid edit goes through.
    host.db.push_rows(vec![mission_row(7, STAGE_REQUEST, "bea")]);
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]);
    let req = TestRequest::patch("/api/missions/mission/7")
        .param("id", "7")
        .json(&serde_json::json!({ "starts_on": "2026-12-20", "ends_on": "2026-12-13" }))
        .build();
    let (status, _) = call(&update.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 400);
}

/// Progress belongs to the Execution stage.
#[tokio::test]
async fn progress_is_recorded_only_during_execution() {
    let (host, routes) = plugin_routes().await;
    let progress = route(&routes, "POST", "/api/missions/mission/{id}/progress");

    host.db.push_rows(vec![mission_row(7, STAGE_DEBRIEF, "bea")]);
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]);
    let req = TestRequest::post("/api/missions/mission/7/progress")
        .param("id", "7")
        .json(&serde_json::json!({ "note": "late entry" }))
        .build();
    let (status, body) = call(&progress.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("execution"));

    let (host, routes) = plugin_routes().await;
    let progress = route(&routes, "POST", "/api/missions/mission/{id}/progress");
    host.db.push_rows(vec![mission_row(7, STAGE_EXECUTION, "bea")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(update, lodge 3)
    host.db.push_rows(vec![serde_json::json!({ "id": 3, "created_at": "2026-09-25T10:00:00Z" })]);
    let req = TestRequest::post("/api/missions/mission/7/progress")
        .param("id", "7")
        .json(&serde_json::json!({
            "note": "transect 2 done", "progress_pct": 50, "service_hours": 4.5, "participant_count": 6
        }))
        .build();
    let resp = (progress.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 201);
    assert_eq!(response_json(&resp)["progress"]["id"], serde_json::json!(3));
    // The note inserts with RETURNING, so it is a *query* on the host; the
    // running totals are a plain execute.
    assert!(
        host.db.queried_sql().iter().any(|s| s.contains("INSERT INTO") && s.contains("progress_notes")),
        "the note is stored: {:?}",
        host.db.queried_sql()
    );
    host.db.assert_executed(&["service_hours = COALESCE($2, service_hours)"]);
    host.db.assert_executed(&["INSERT INTO", "core.audit_log"]);
}

/// The appeal pathway (Accords Art 8): only a rejected mission can be appealed,
/// and the decision needs a seconding Council member.
#[tokio::test]
async fn appeal_requires_a_rejected_mission_and_a_seconder() {
    let (host, routes) = plugin_routes().await;
    let appeal = route(&routes, "POST", "/api/missions/mission/{id}/appeal");

    // An open mission cannot be appealed.
    host.db.push_rows(vec![mission_row(7, STAGE_APPROVAL, "bea")]);
    let req = TestRequest::post("/api/missions/mission/7/appeal")
        .param("id", "7")
        .json(&serde_json::json!({ "reason": "we disagree" }))
        .build();
    let (status, body) = call(&appeal.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(
        body["error"].as_str().unwrap().contains("rejected"),
        "the refusal names the condition"
    );

    // A rejected one can, by its lead.
    let (host, routes) = plugin_routes().await;
    let appeal = route(&routes, "POST", "/api/missions/mission/{id}/appeal");
    host.db.push_rows(vec![rejected_mission_row(7)]);
    host.db.push_rows(vec![serde_json::json!({ "id": 2, "status": "pending" })]);
    let req = TestRequest::post("/api/missions/mission/7/appeal")
        .param("id", "7")
        .json(&serde_json::json!({ "reason": "the guidance is impossible to meet" }))
        .build();
    let resp = (appeal.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 201);
    assert_eq!(response_json(&resp)["status"], serde_json::json!("pending"));

    // An appeal belonging to someone else needs a grant reaching the mission.
    let (host, routes) = plugin_routes().await;
    let appeal = route(&routes, "POST", "/api/missions/mission/{id}/appeal");
    host.db.push_rows(vec![rejected_mission_row(7)]);
    host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // reach(appeal, lodge 3) → denied
    let req = TestRequest::post("/api/missions/mission/7/appeal")
        .param("id", "7")
        .json(&serde_json::json!({ "reason": "not mine" }))
        .build();
    let other = Identity::from_grants(
        "mallory",
        vec![RoleGrant { role_id: "scout".into(), scope: Scope::troop() }],
    );
    let (status, _) = call(&appeal.handler, req_with_identity(req, other)).await;
    assert_eq!(status, 403);

    // Deciding without a seconder is refused before anything is written.
    let (host, routes) = plugin_routes().await;
    let decide = route(&routes, "POST", "/api/missions/appeal/{id}/decide");
    let req = TestRequest::post("/api/missions/appeal/2/decide")
        .param("id", "2")
        .json(&serde_json::json!({ "seconded_by": "", "outcome": "overturned" }))
        .build();
    let (status, _) = call(&decide.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 400);
    assert_eq!(host.db.query_count(), 0);

    // With a seconder, overturning puts the mission into Execution.
    let (host, routes) = plugin_routes().await;
    let decide = route(&routes, "POST", "/api/missions/appeal/{id}/decide");
    host.db.push_rows(vec![serde_json::json!({
        "id": 2, "mission_id": 7, "status": "pending", "reason": "impossible guidance"
    })]);
    host.db.push_rows(vec![rejected_mission_row(7)]);
    let req = TestRequest::post("/api/missions/appeal/2/decide")
        .param("id", "2")
        .json(&serde_json::json!({
            "seconded_by": "sara", "outcome": "overturned", "votes_for": 5, "votes_against": 1
        }))
        .build();
    let resp = (decide.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    assert_eq!(body["outcome"], serde_json::json!("overturned"));
    assert_eq!(body["mission_stage"], serde_json::json!(STAGE_EXECUTION));
    host.db.assert_executed(&["UPDATE", "mission_appeals", "seconded_by"]);
    let params = host.db.last_execute_params("approved_by = $4").expect("approval params");
    assert!(
        matches!(params[3], SqlValue::Text(ref s) if s.starts_with("troop_council")),
        "the approval records that the Council, not the Lodge Commander, approved it: {params:?}"
    );
}

/// A decided appeal is not decided twice.
#[tokio::test]
async fn appeal_decision_refuses_a_second_decision() {
    let (host, routes) = plugin_routes().await;
    let decide = route(&routes, "POST", "/api/missions/appeal/{id}/decide");

    host.db.push_rows(vec![serde_json::json!({
        "id": 2, "mission_id": 7, "status": "upheld", "reason": "settled"
    })]);
    let req = TestRequest::post("/api/missions/appeal/2/decide")
        .param("id", "2")
        .json(&serde_json::json!({ "seconded_by": "sara", "outcome": "overturned" }))
        .build();
    let (status, _) = call(&decide.handler, req_with_identity(req, troop_identity())).await;
    assert_eq!(status, 409);
    assert!(host.db.executed_sql().is_empty());
}

/// The Impact Report rolls completed missions up by totals, category, year, and
/// lodge (Accords Art 12: progress reviewed, impact published).
#[tokio::test]
async fn impact_report_aggregates_completed_missions() {
    let (host, routes) = plugin_routes().await;
    let impact = route(&routes, "GET", "/api/missions/impact");

    host.db.push_rows(vec![serde_json::json!({
        "completed": 4, "open": 2, "rejected": 1, "total": 7,
        "service_hours": 96.5, "participant_count": 88, "lodges": 2
    })]);
    host.db.push_rows(vec![serde_json::json!({ "category": "conservation", "completed": 3, "service_hours": 60.0 })]);
    host.db.push_rows(vec![serde_json::json!({ "year": 2026, "completed": 4, "service_hours": 96.5, "participant_count": 88 })]);
    host.db.push_rows(vec![serde_json::json!({ "lodge_id": "3", "lodge_name": "Windsor", "completed": 4, "service_hours": 96.5 })]);

    let req = TestRequest::get("/api/missions/impact").query_param("lodge", "3").build();
    let resp = (impact.handler)(req).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    assert_eq!(body["totals"]["completed"], serde_json::json!(4));
    assert_eq!(body["by_category"][0]["category"], serde_json::json!("conservation"));
    assert_eq!(body["by_year"][0]["year"], serde_json::json!(2026));
    assert_eq!(body["by_lodge"][0]["lodge_name"], serde_json::json!("Windsor"));
    let sql = host.db.queried_sql();
    assert!(sql[0].contains("FILTER (WHERE state = 'completed')"));
    assert!(sql[0].contains("$1::text IS NULL OR lodge_id = $1"));
}

/// Mentor suggestions rank the registry and never re-propose someone who is
/// already mentoring this mission.
#[tokio::test]
async fn mentor_suggestions_rank_registry_candidates() {
    let (host, routes) = plugin_routes().await;
    let suggestions = route(&routes, "GET", "/api/missions/mission/{id}/mentor/suggestions");

    host.db.push_rows(vec![mission_row(7, STAGE_REVIEW, "bea")]); // fetch
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // reach(approve, lodge 3)
    host.db.push_rows(vec![
        serde_json::json!({
            "member_id": "ar", "display_name": "Arielle", "expertise": ["wildlife"],
            "capacity": 2, "is_active": true, "load": 0
        }),
        serde_json::json!({
            "member_id": "ir", "display_name": "Ira", "expertise": ["wildlife", "tracking"],
            "capacity": 1, "is_active": true, "load": 0
        }),
    ]);
    host.db.push_rows(vec![serde_json::json!({ "mentor_member": "ar" })]); // already matched

    let req = TestRequest::get("/api/missions/mission/7/mentor/suggestions")
        .param("id", "7")
        .build();
    let resp = (suggestions.handler)(req_with_identity(req, troop_identity())).await.expect("handler");
    assert_eq!(resp.status, 200);
    let body = response_json(&resp);
    let candidates = body["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "a current mentor is not suggested again");
    assert_eq!(candidates[0]["member_id"], serde_json::json!("ir"));
    assert_eq!(candidates[0]["tag_overlap"], serde_json::json!(2));
    assert_eq!(candidates[0]["spare"], serde_json::json!(1));
}

/// A scoped caller lists only the mentorships they are part of.
#[tokio::test]
async fn mentorships_are_scoped_to_the_caller() {
    let (host, routes) = plugin_routes().await;
    let list = route(&routes, "GET", "/api/missions/mentorships");

    // A lodge-scoped grant has no troop-covering role, so the troop check makes
    // no query at all — the rows query is the first call.
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "mentor_member": "bea" })]);

    let req = TestRequest::get("/api/missions/mentorships")
        .identity_grants(
            "bea",
            vec![RoleGrant { role_id: "scout".into(), scope: Scope::lodge("3") }],
        )
        .build();
    let resp = (list.handler)(req).await.expect("handler");
    assert_eq!(resp.status, 200);
    assert_eq!(response_json(&resp)["mentorships"][0]["mentor_member"], serde_json::json!("bea"));
    let sql = host.db.queried_sql();
    assert!(
        sql[0].contains("ms.mentor_member = $2 OR ms.mentee_member = $2"),
        "a scoped caller is narrowed to their own mentorships: {}",
        sql[0]
    );
}

/// The list narrows rows by the lodges the caller's grants cover, and honours
/// the stage and limit filters.
#[tokio::test]
async fn list_passes_the_callers_lodge_scopes_as_an_array() {
    let (host, routes) = plugin_routes().await;
    let list = route(&routes, "GET", "/api/missions/missions");

    host.db.push_rows(vec![serde_json::json!({ "id": 1, "title": "Survey" })]);

    let req = TestRequest::get("/api/missions/missions")
        .query_param("stage", "execution")
        .query_param("limit", "10")
        .identity_grants(
            "bea",
            vec![RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("3") }],
        )
        .build();
    let resp = (list.handler)(req).await.expect("handler");
    assert_eq!(resp.status, 200);
    assert_eq!(response_json(&resp)["scope"], serde_json::json!("scoped"));

    let call = host.db.queried.lock().unwrap()[0].clone();
    match &call.params[1] {
        SqlValue::TextArray(lodges) => assert_eq!(lodges, &vec!["3".to_string()]),
        other => panic!("expected the caller's lodge ids, got {other:?}"),
    }
    assert!(matches!(&call.params[3], SqlValue::Text(s) if s == "execution"));
    assert!(matches!(&call.params[7], SqlValue::Int(10)));
}
