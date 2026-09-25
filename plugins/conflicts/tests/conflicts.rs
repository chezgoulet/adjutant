//! Conflicts plugin tests (SPEC §7.9): the pathway arithmetic, the privacy
//! boundary, the ledger, the resolution path, and the anti-dropout mechanism.
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results **in call order**, so each test states the order its handler arranges
//! — the comment above each `push_rows` says which call it answers. The route
//! gate (the core's permission check) is not exercised here — it is the core's —
//! so these tests pin the half the plugin owns: the **object-level** check that
//! decides whether this caller may see this case at all.

use adjutant_conflicts::{
    advance_targets, is_later_stage, is_stalled, may_read, may_record_agreement, may_withdraw,
    next_stage, nudge_cooldown_hours, parse_timestamp, perms, should_nudge, stage_age_hours,
    stage_index,
    stall_report, stall_threshold_hours, standing_of, ConflictsPlugin, Standing,
    DEFAULT_NUDGE_COOLDOWN_HOURS, DEFAULT_STALL_HOURS, EVENT_CONFLICT_FILED,
    EVENT_CONFLICT_PARTY_ADDED, EVENT_CONFLICT_RESOLVED, EVENT_CONFLICT_STAFFING,
    EVENT_CONFLICT_STAGE_STALLED, EVENT_CONFLICT_WITHDRAWN, KIND_AGREEMENT,
    KIND_FACILITATOR_ASSIGNED, KIND_FACILITATOR_RELEASED, KIND_FILED, KIND_NUDGE, KIND_PARTY_ADDED,
    KIND_RESOLUTION, KIND_TRANSITION, KIND_WITHDRAWN, LOG_KINDS, STAGES, STAGE_ARBITRATION,
    STAGE_COUNCIL, STAGE_DIRECT, STAGE_FACILITATION, STATUS_OPEN, STATUS_RESOLVED,
    STATUS_WITHDRAWN,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use chrono::{Duration, TimeZone, Utc};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn plugin() -> (TestHost, ConflictsPlugin, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = ConflictsPlugin::new();
    plugin.init(host.context("conflicts")).await.unwrap();
    let routes = plugin.routes();
    (host, plugin, routes)
}

async fn plugin_with_config(config: Value) -> (TestHost, ConflictsPlugin, Vec<RouteDefinition>) {
    let host = TestHost::new().with_config(config);
    let mut plugin = ConflictsPlugin::new();
    plugin.init(host.context("conflicts")).await.unwrap();
    let routes = plugin.routes();
    (host, plugin, routes)
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

/// Drive a handler and report `(status, body)` — an `SdkError` and a
/// `PluginResponse::error` are the same answer to a client.
async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// A case row as `CASE_FIELDS` renders one.
fn case_row(id: i64, stage: &str, parties: &[&str], facilitators: &[&str]) -> Value {
    json!({
        "id": id,
        "title": "A private matter",
        "summary": "the filer's own account, which nobody outside the case may read",
        "stage": stage,
        "status": STATUS_OPEN,
        "filed_by": parties.first().copied().unwrap_or("bea"),
        "party_ids": parties,
        "facilitator_ids": facilitators,
        "outcome": "",
        "agreement": "",
        "nudge_count": 0,
        "stage_since": "2026-09-20 12:00:00+00",
        "nudged_at": Value::Null,
        "resolved_at": Value::Null,
        "resolved_by": Value::Null,
        "withdrawn_at": Value::Null,
        "withdrawn_by": Value::Null,
        "created_at": "2026-09-20 12:00:00+00",
        "updated_at": "2026-09-20 12:00:00+00",
        "stage_age_hours": 100
    })
}

/// A row for the anti-dropout probe (`STALL_PROBE_FIELDS`), with a stage age the
/// nudge's own clock can act on. The handler calls `Utc::now()`, so the fixture
/// must be relative to real time, not to a fixed date.
fn stalled_row(
    id: i64,
    stage: &str,
    facilitators: &[&str],
    nudges: i64,
    nudged_hours_ago: Option<i64>,
) -> Value {
    let now = Utc::now();
    let since = now - Duration::days(9);
    json!({
        "id": id,
        "stage": stage,
        "facilitator_ids": facilitators,
        "nudge_count": nudges,
        "stage_since": render_pg(since),
        "nudged_at": nudged_hours_ago
            .map(|hours| json!(render_pg(now - Duration::hours(hours))))
            .unwrap_or(Value::Null),
    })
}

/// A timestamp as the host renders a `timestamptz::text`.
fn render_pg(at: chrono::DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S%.6f%:z").to_string()
}

/// The rendered bind parameters of the last call whose SQL contains `needle`.
fn param_text(host: &TestHost, needle: &str) -> String {
    let params = host
        .db
        .last_query_params(needle)
        .or_else(|| host.db.last_execute_params(needle))
        .unwrap_or_else(|| panic!("no query or execute whose SQL contains {needle:?}"));
    params
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The audit log's action is a bind parameter, so a test has to read it back
/// rather than look for it in the statement.
fn assert_audited(host: &TestHost, action: &str) {
    let rendered = param_text(host, "audit_log");
    assert!(rendered.contains(action), "no {action} audit: {rendered}");
}

/// Assert a payload/body carries no party name, no facilitator name and not the
/// case's title — the privacy rule, stated as a test rather than a comment.
fn assert_opaque(rendered: &str, what: &str) {
    for secret in [
        "bea",
        "carl",
        "dana",
        "gwen",
        "A private matter",
        "the filer's own account",
    ] {
        assert!(
            !rendered.contains(secret),
            "{what} leaked {secret:?}: {rendered}"
        );
    }
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "conflicts");
    assert_eq!(plugin.name(), "Conflicts");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for perm in &permissions {
        assert!(perm.starts_with("conflicts:"), "{perm} must be namespaced");
    }
    // The whole vocabulary, and nothing wider. There is deliberately no
    // `conflicts:read`: a permission that reads *anybody's* case is the failure
    // mode this plugin exists to prevent.
    assert_eq!(
        permissions,
        vec![
            "conflicts:file".to_string(),
            "conflicts:read_own".to_string(),
            "conflicts:facilitate".to_string(),
            "conflicts:manage".to_string(),
        ],
        "the permission surface is deliberately narrow"
    );
    assert!(
        !permissions.contains(&"conflicts:read".to_string()),
        "a bare read permission must not exist"
    );

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    assert_eq!(versions, vec![1, 2]);

    let ddl = migrations
        .iter()
        .map(|m| m.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for table in ["cases", "stage_log"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "SPEC §7.9's schema is missing {table}"
        );
    }
    // Vocabulary that reaches a decision is a constraint, not a convention.
    for constraint in [
        "cases_stage_valid",
        "cases_status_valid",
        "cases_filer_is_a_party",
        "cases_resolution_recorded",
        "stage_log_kind_valid",
        "stage_log_reason_present",
    ] {
        assert!(
            ddl.contains(constraint),
            "migration is missing {constraint}"
        );
    }
    // The parties and the facilitators are the visibility list; both are
    // indexed because "can this caller read this case" is the hot question.
    assert!(ddl.contains("idx_cases_parties"));
    assert!(ddl.contains("idx_cases_facilitators"));
    // Append-only by construction: the database refuses to rewrite the history.
    assert!(ddl.contains("CREATE TRIGGER stage_log_append_only"));
    assert!(ddl.contains("BEFORE UPDATE OR DELETE ON stage_log"));
    assert!(
        ddl.contains("ON DELETE RESTRICT"),
        "a case cannot be erased"
    );

    // Every stage, status and log kind this crate knows is in the CHECK.
    for stage in STAGES {
        assert!(ddl.contains(stage), "the stage {stage} is not constrained");
    }
    for kind in LOG_KINDS {
        assert!(ddl.contains(kind), "the log kind {kind} is not constrained");
    }

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/conflicts"),
            "{} escapes the namespace",
            r.path
        );
        let perm = r
            .required_permission
            .as_deref()
            .expect("every route is gated");
        assert!(
            permissions.iter().any(|p| p == perm),
            "{perm} is required but not declared"
        );
        let normalized = r
            .path
            .split('/')
            .map(|s| {
                if s.starts_with('{') {
                    "{}".to_string()
                } else {
                    s.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let key = (r.method.as_str().to_string(), normalized);
        assert!(!seen.contains(&key), "duplicate route {key:?}");
        seen.push(key);
    }
    // The same check the core's loader makes, as a test: every gate names a
    // permission this crate declares (`perms`, generated by `permissions!`).
    perms::assert_routes_gate_declared(&routes);
    assert_eq!(routes.len(), 11, "the pathway's surface is these eleven");

    // Nothing is destroyed. A case is resolved or withdrawn, and the log's
    // trigger would refuse the erasure anyway.
    assert!(routes.iter().all(|r| r.method != Method::Delete));

    // Every case route is object-shaped: the core's gate only asks that the
    // caller holds the permission *somewhere*, and the handler decides per case.
    for path in [
        "/api/conflicts/case/{id}",
        "/api/conflicts/case/{id}/log",
        "/api/conflicts/case/{id}/stage",
        "/api/conflicts/case/{id}/resolution",
        "/api/conflicts/case/{id}/agreement",
        "/api/conflicts/case/{id}/withdraw",
    ] {
        let method = match path.rsplit_once('/').map(|(_, last)| last) {
            Some("stage") | Some("resolution") | Some("agreement") | Some("withdraw") => "POST",
            _ => "GET",
        };
        assert_eq!(
            route(&routes, method, path).required_scope,
            None,
            "{method} {path} must check its object in the handler"
        );
    }

    // The narrow gates, stated one by one.
    assert_eq!(
        route(&routes, "POST", "/api/conflicts/case")
            .required_permission
            .as_deref(),
        Some("conflicts:file")
    );
    for path in [
        "/api/conflicts/case/{id}",
        "/api/conflicts/case/{id}/log",
        "/api/conflicts/cases",
    ] {
        assert_eq!(
            route(&routes, "GET", path).required_permission.as_deref(),
            Some("conflicts:read_own"),
            "{path} is a standing read"
        );
    }
    for path in [
        "/api/conflicts/case/{id}/stage",
        "/api/conflicts/case/{id}/resolution",
        "/api/conflicts/stalled",
    ] {
        let method = if path.ends_with("/stalled") {
            "GET"
        } else {
            "POST"
        };
        assert_eq!(
            route(&routes, method, path).required_permission.as_deref(),
            Some("conflicts:facilitate"),
            "{path} is the facilitator's"
        );
    }
    assert_eq!(
        route(&routes, "POST", "/api/conflicts/case/{id}/facilitator")
            .required_permission
            .as_deref(),
        Some("conflicts:manage")
    );

    // No cross-plugin event may move a case: the pathway is moved by people.
    assert!(
        plugin.subscriptions().is_empty(),
        "an event that advanced a case would be an unaudited actor on a private record"
    );

    // The anti-dropout mechanism is a schedule.
    let schedules = plugin.schedules();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].name, "stage_nudge");
}

/// The declarations the plugin makes are the *only* place the ids and the
/// migration identity are written, and this test pins the values that reach the
/// database: `core.permissions`, `core.role_permissions` (by id), and
/// `core.schema_migrations` (by version and name).
#[tokio::test]
async fn the_declaration_is_the_single_source() {
    let (_host, plugin, routes) = plugin().await;

    // `permissions_granted()` *is* the declaration — the ids below are asserted
    // against the documented contract in `declared_manifest_satisfies_the_load_
    // rules`, so this test only has to prove the wiring.
    let pairs = |perms: Vec<Permission>| -> Vec<(String, String)> {
        perms.into_iter().map(|p| (p.id, p.description)).collect()
    };
    assert_eq!(
        pairs(perms::granted()),
        pairs(plugin.permissions_granted()),
        "permissions_granted() returns the declaration verbatim, ids and descriptions"
    );
    assert_eq!(perms::FILE.id, "conflicts:file");
    assert_eq!(perms::READ_OWN.id, "conflicts:read_own");
    assert_eq!(perms::FACILITATE.id, "conflicts:facilitate");
    assert_eq!(perms::MANAGE.id, "conflicts:manage");
    assert_eq!(
        perms::SET.ids(),
        vec![
            "conflicts:file",
            "conflicts:read_own",
            "conflicts:facilitate",
            "conflicts:manage",
        ]
    );
    assert!(
        !perms::SET.has("conflicts:read"),
        "a bare read permission must not exist"
    );

    // Every route gate is one of those declarations, read from it rather than
    // repeated — so the consts below are what the routes actually gate on.
    let gated: Vec<&str> = routes
        .iter()
        .filter_map(|r| r.required_permission.as_deref())
        .collect();
    assert_eq!(gated.len(), 11, "every route is gated");
    assert!(gated.iter().all(|p| perms::SET.has(p)), "{gated:?}");
    assert!(routes.iter().any(|r| r.required_permission.as_deref() == Some(perms::MANAGE.id)));
}

/// Migration identity — the `(version, name)` a deployed database has recorded in
/// `core.schema_migrations` — as a test. Both halves matter: the version decides
/// whether a migration runs at all (the table is keyed by `(schema, version)`,
/// and an applied version is skipped), and the name is what the record says the
/// migration did.
#[tokio::test]
async fn migration_identity_is_pinned_to_the_files() {
    let (_host, plugin, _routes) = plugin().await;
    let migrations = plugin.migrations();
    let identity: Vec<(i64, &str)> = migrations
        .iter()
        .map(|m| (m.version, m.name.as_str()))
        .collect();
    assert_eq!(
        identity,
        vec![(1, "conflicts_schema"), (2, "stage_log_append_only")],
        "renumbering or renaming a migration changes what a deployed database has \
         recorded; these two are the shipped identity"
    );

    // The same two, through the generated declaration helpers.
    assert_eq!(adjutant_conflicts::migrations::MIGRATIONS.len(), 2);
    assert_eq!(
        adjutant_conflicts::migrations::find(1).map(|m| m.name),
        Some("conflicts_schema")
    );
    assert_eq!(
        adjutant_conflicts::migrations::find(2).map(|m| m.name),
        Some("stage_log_append_only")
    );
    assert!(adjutant_conflicts::migrations::find(3).is_none());

    // Each version carries its own file's SQL, embedded at compile time.
    assert!(migrations[0]
        .sql
        .contains("CREATE TABLE IF NOT EXISTS cases"));
    assert!(migrations[0].sql.contains("CREATE INDEX IF NOT EXISTS idx_cases_parties"));
    assert!(!migrations[0].sql.contains("CREATE OR REPLACE FUNCTION"));
    assert!(migrations[1].sql.contains("CREATE TRIGGER stage_log_append_only"));
    assert!(!migrations[1].sql.contains("CREATE TABLE IF NOT EXISTS cases"));
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_conflicts::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "conflicts");
    assert_eq!(
        adjutant_conflicts::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

// ---------------------------------------------------------------------------
// The pathway itself
// ---------------------------------------------------------------------------

#[test]
fn the_pathway_runs_from_direct_conversation_to_the_troop_council() {
    assert_eq!(
        STAGES,
        [
            STAGE_DIRECT,
            STAGE_FACILITATION,
            STAGE_ARBITRATION,
            STAGE_COUNCIL
        ]
    );
    assert_eq!(stage_index(STAGE_DIRECT), Some(0));
    assert_eq!(stage_index(STAGE_COUNCIL), Some(3));
    assert_eq!(stage_index("mediation"), None);

    assert_eq!(next_stage(STAGE_DIRECT), Some(STAGE_FACILITATION));
    assert_eq!(next_stage(STAGE_COUNCIL), None);
    assert_eq!(
        advance_targets(STAGE_DIRECT),
        vec![STAGE_FACILITATION, STAGE_ARBITRATION, STAGE_COUNCIL],
        "a stage may be skipped, which the reason recorded with the move accounts for"
    );
    assert!(advance_targets(STAGE_COUNCIL).is_empty());

    assert!(is_later_stage(STAGE_DIRECT, STAGE_COUNCIL));
    assert!(is_later_stage(STAGE_FACILITATION, STAGE_ARBITRATION));
    assert!(
        !is_later_stage(STAGE_ARBITRATION, STAGE_DIRECT),
        "a case never walks backwards: that would undo a party's participation"
    );
    assert!(!is_later_stage(STAGE_ARBITRATION, STAGE_ARBITRATION));
    assert!(!is_later_stage("mediation", STAGE_COUNCIL));
}

// ---------------------------------------------------------------------------
// Filing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filing_a_case_makes_the_filer_a_party_and_opens_the_ledger() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/conflicts/case");
    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea", "carl"], &[])]); // the INSERT … RETURNING

    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/conflicts/case")
            .json(&json!({
                "title": "A private matter",
                "summary": "carl took my gear and will not say where it is",
                "parties": ["carl"],
            }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["standing"], json!("party"));
    assert_eq!(body["advance_targets"][0], json!(STAGE_FACILITATION));
    assert!(body["privacy"].as_str().unwrap().contains("private"));

    // The insert binds the filer first, then the named parties — the visibility
    // list is exactly who may read this case.
    let insert = param_text(&host, "INSERT INTO");
    assert!(insert.contains("bea"), "{insert}");
    assert!(insert.contains("carl"), "{insert}");

    // The ledger's opening entry: who, which stage, and why.
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_FILED), "{log}");
    assert!(log.contains(STAGE_DIRECT), "{log}");
    assert!(log.contains("bea"), "{log}");
    assert!(log.contains("carl took my gear"), "{log}");

    assert_audited(&host, "case.filed");
    host.events.assert_published(EVENT_CONFLICT_FILED);

    // The event is a reference, not a record: an opaque id and a stage.
    let published = host.events.payloads(EVENT_CONFLICT_FILED);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["case_id"], json!(11));
    assert_opaque(&published[0].to_string(), "conflict.filed payload");
}

#[tokio::test]
async fn filing_refuses_an_incomplete_record() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/conflicts/case");
    let _ = &host;

    // A blank title is the caller's mistake, and nothing is written.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/conflicts/case")
            .json(&json!({ "title": "   ", "summary": "something happened" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("title"));

    // The summary is the "why" the ledger opens with: without it there is no
    // account of the filing at all.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/conflicts/case")
            .json(&json!({ "title": "A private matter", "summary": "" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("summary"));
    assert_eq!(host.db.query_count(), 0, "nothing reached the database");
}

// ---------------------------------------------------------------------------
// The privacy boundary — the point of this plugin
// ---------------------------------------------------------------------------

#[test]
fn standing_is_a_property_of_the_case_not_of_the_caller() {
    let case = case_row(11, STAGE_FACILITATION, &["bea", "carl"], &["dana"]);
    assert_eq!(standing_of(&case, "bea"), Some(Standing::Party));
    assert_eq!(standing_of(&case, "dana"), Some(Standing::Facilitator));
    assert_eq!(
        standing_of(&case, "erin"),
        None,
        "a stranger has no standing"
    );
    assert_eq!(standing_of(&case, "ivy"), None, "nor has the administrator");
    assert_eq!(
        standing_of(&case, ""),
        None,
        "an anonymous id matches nobody"
    );
    assert_eq!(standing_of(&case, "  "), None);

    assert!(may_read(&case, "carl"));
    assert!(!may_read(&case, "ivy"));
    assert!(may_withdraw(&case, "bea"));
    assert!(
        !may_withdraw(&case, "dana"),
        "a facilitator cannot withdraw somebody else's case"
    );

    // The case is theirs before it is a job: a facilitator who is also a party
    // answers as a party.
    let both = case_row(11, STAGE_DIRECT, &["bea", "carl"], &["bea"]);
    assert_eq!(standing_of(&both, "bea"), Some(Standing::Party));

    // The parties' own agreement is the entry stage's, and a party's.
    let entry = case_row(11, STAGE_DIRECT, &["bea", "carl"], &["dana"]);
    assert!(may_record_agreement(&entry, "carl"));
    assert!(!may_record_agreement(&entry, "dana"), "not a party");
    assert!(
        !may_record_agreement(&case, "carl"),
        "past the entry stage the record is the facilitator's"
    );

    // A malformed row gives nobody standing rather than everybody.
    for broken in [
        json!({ "id": 11 }),
        json!({ "id": 11, "party_ids": Value::Null, "facilitator_ids": Value::Null }),
        json!({ "id": 11, "party_ids": "bea", "facilitator_ids": 3 }),
    ] {
        assert!(!may_read(&broken, "bea"), "{broken}");
    }
}

#[tokio::test]
async fn a_party_reads_their_own_case() {
    let (host, _plugin, routes) = plugin().await;
    let get = route(&routes, "GET", "/api/conflicts/case/{id}");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_FACILITATION,
        &["bea", "carl"],
        &["dana"],
    )]);

    let (status, body) = call(
        &get.handler,
        TestRequest::get("/api/conflicts/case/11")
            .param("id", "11")
            .identity("carl", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["standing"], json!("party"));
    assert_eq!(body["case"]["party_ids"], json!(["bea", "carl"]));
    assert_eq!(
        body["case"]["summary"],
        json!("the filer's own account, which nobody outside the case may read")
    );
    // A party reads; a party does not carry the case.
    assert_eq!(body["may"]["advance"], json!(false));
    assert_eq!(body["may"]["record_resolution"], json!(false));
    assert_eq!(body["may"]["withdraw"], json!(true));
    assert_eq!(body["may"]["record_agreement"], json!(false));
    assert_eq!(
        body["advance_targets"],
        json!([STAGE_ARBITRATION, STAGE_COUNCIL])
    );
    // One read, no permission lookup: the object list *is* the check.
    assert_eq!(host.db.query_count(), 1);
}

#[tokio::test]
async fn a_non_party_is_refused_and_learns_nothing() {
    let (host, _plugin, routes) = plugin().await;
    let get = route(&routes, "GET", "/api/conflicts/case/{id}");
    let log = route(&routes, "GET", "/api/conflicts/case/{id}/log");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_FACILITATION,
        &["bea", "carl"],
        &["dana"],
    )]);

    // Erin is a member of the troop — and the handler does not consult a role at
    // all. That is the design: no troop-wide or lodge-wide grant reaches a case.
    let (status, body) = call(
        &get.handler,
        TestRequest::get("/api/conflicts/case/11")
            .param("id", "11")
            .identity("erin", &["scout", "chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], json!("no such case, or you cannot see it"));
    assert_opaque(&body.to_string(), "the refusal");
    assert_eq!(
        host.db.query_count(),
        1,
        "refused without a permission lookup"
    );

    // The pathway's administrator is refused too: `conflicts:manage` staffs
    // cases, it does not read them. (Self-appointment as facilitator is refused
    // as well — see the staffing test — so there is no route around this.)
    let (status, body) = call(
        &get.handler,
        TestRequest::get("/api/conflicts/case/11")
            .param("id", "11")
            .identity("ivy", &["troop_council_chair", "conflict_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "the administrator's refusal");

    // The history is the dispute: the log answers the same way.
    let (status, body) = call(
        &log.handler,
        TestRequest::get("/api/conflicts/case/11/log")
            .param("id", "11")
            .identity("erin", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "the log refusal");
    // The ledger of a case the caller cannot see is never read: the route fetched
    // the case (three reads so far) and stopped.
    assert_eq!(host.db.query_count(), 3);
    assert!(
        host.db
            .queried_sql()
            .iter()
            .all(|sql| !sql.contains("stage_log")),
        "a refused log read reached the ledger: {:?}",
        host.db.queried_sql()
    );
}

#[tokio::test]
async fn an_assigned_facilitator_reads_their_case_and_nobody_elses() {
    let (host, _plugin, routes) = plugin().await;
    let get = route(&routes, "GET", "/api/conflicts/case/{id}");
    let case = case_row(11, STAGE_FACILITATION, &["bea", "carl"], &["dana"]);

    // Dana carries this case.
    host.db.push_rows(vec![case.clone()]);
    let (status, body) = call(
        &get.handler,
        TestRequest::get("/api/conflicts/case/11")
            .param("id", "11")
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["standing"], json!("facilitator"));
    // A facilitator carries the case; they are not a party to it.
    assert_eq!(body["may"]["advance"], json!(true));
    assert_eq!(body["may"]["record_resolution"], json!(true));
    assert_eq!(body["may"]["record_agreement"], json!(false));
    assert_eq!(body["may"]["withdraw"], json!(false));

    // Frank is a facilitator too — of other cases. Standing is per case.
    let (status, body) = call(
        &get.handler,
        TestRequest::get("/api/conflicts/case/11")
            .param("id", "11")
            .identity("frank", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "another facilitator's refusal");
}

#[tokio::test]
async fn the_listing_has_no_troop_wide_branch() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/conflicts/cases");
    host.db
        .push_rows(vec![case_row(11, STAGE_FACILITATION, &["bea"], &["dana"])]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/conflicts/cases")
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["returned"], json!(1));
    assert_eq!(body["status"], json!(STATUS_OPEN), "open is the default");

    let sql = host.db.queried_sql().join("\n");
    assert!(
        sql.contains("= ANY(c.party_ids)") && sql.contains("= ANY(c.facilitator_ids)"),
        "the listing selects on the caller's standing: {sql}"
    );
    assert!(
        !sql.contains("scope"),
        "a case has no troop or lodge scope to widen: {sql}"
    );
    assert!(
        !sql.contains("permission"),
        "the listing is a standing check, not a role check: {sql}"
    );

    // A filter that cannot be read is the caller's mistake, not an empty list.
    for (key, value) in [
        ("status", "maybe"),
        ("role", "facilitator_of_everything"),
        ("stage", "mediation"),
    ] {
        let (status, _body) = call(
            &list.handler,
            TestRequest::get("/api/conflicts/cases")
                .query_param(key, value)
                .identity("dana", &["conflict_facilitator"])
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{key}={value}");
    }
}

// ---------------------------------------------------------------------------
// Stage transitions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stage_move_is_recorded_with_its_actor_and_reason() {
    let (host, _plugin, routes) = plugin().await;
    let advance = route(&routes, "POST", "/api/conflicts/case/{id}/stage");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]); // fetch
    host.db.push_rows(vec![case_row(
        11,
        STAGE_ARBITRATION,
        &["bea", "carl"],
        &["dana"],
    )]); // UPDATE … RETURNING

    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({
                "stage": STAGE_ARBITRATION,
                "reason": "facilitation failed: carl did not attend twice; both parties now want terms set",
            }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["from_stage"], json!(STAGE_DIRECT));
    assert_eq!(body["case"]["stage"], json!(STAGE_ARBITRATION));
    assert_eq!(body["advance_targets"], json!([STAGE_COUNCIL]));

    // The move restarts the dropout clock: the new stage has just begun.
    let update = param_text(&host, "UPDATE");
    assert!(update.contains("arbitration"), "{update}");
    let sql = host.db.queried_sql().join("\n");
    for clause in ["stage_since = now()", "nudged_at = NULL", "nudge_count = 0"] {
        assert!(sql.contains(clause), "the move must reset {clause}: {sql}");
    }

    // Who moved it, from where to where, and why — all four in the ledger.
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_TRANSITION), "{log}");
    assert!(log.contains(STAGE_DIRECT), "{log}");
    assert!(log.contains(STAGE_ARBITRATION), "{log}");
    assert!(log.contains("dana"), "{log}");
    assert!(log.contains("facilitation failed"), "{log}");

    assert_audited(&host, "case.stage.advance");
    host.events
        .assert_published(adjutant_sdk::event_type::CONFLICT_ESCALATED);
    let escalation = host
        .events
        .payloads(adjutant_sdk::event_type::CONFLICT_ESCALATED);
    assert_eq!(escalation[0]["from_stage"], json!(STAGE_DIRECT));
    assert_eq!(escalation[0]["to_stage"], json!(STAGE_ARBITRATION));
    assert_eq!(escalation[0]["case_id"], json!(11));
    // The opaqueness rule: the escalation names the case, never the people.
    assert_opaque(&escalation[0].to_string(), "conflict.escalated payload");
}

#[tokio::test]
async fn invalid_stage_transitions_are_refused() {
    let (host, _plugin, routes) = plugin().await;
    let advance = route(&routes, "POST", "/api/conflicts/case/{id}/stage");

    // Backwards, from arbitration to the direct conversation: 409.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_ARBITRATION,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({ "stage": STAGE_DIRECT, "reason": "let us start again" }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("only advances"),
        "{body}"
    );
    assert_eq!(host.db.query_count(), 1, "refused before any write");

    // A stage that does not exist: 400, naming the pathway.
    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea"], &["dana"])]);
    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({ "stage": "mediation", "reason": "x" }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("facilitation"),
        "{body}"
    );

    // An unexplained escalation: 400. The "why" is the accountability.
    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea"], &["dana"])]);
    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({ "stage": STAGE_FACILITATION }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("reason"));

    // A party cannot advance their own case past themselves.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({ "stage": STAGE_FACILITATION, "reason": "I want this heard" }))
            .identity("carl", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "the party's refusal");

    // A closed case has no next stage, and the failure names where the record
    // of it lives.
    let mut resolved = case_row(11, STAGE_FACILITATION, &["bea", "carl"], &["dana"]);
    resolved["status"] = json!(STATUS_RESOLVED);
    host.db.push_rows(vec![resolved]);
    let (status, body) = call(
        &advance.handler,
        TestRequest::post("/api/conflicts/case/11/stage")
            .param("id", "11")
            .json(&json!({ "stage": STAGE_COUNCIL, "reason": "not good enough" }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("log"));

    // Every refusal wrote nothing.
    assert_eq!(
        host.db.executed_sql().len(),
        0,
        "a refused transition must not touch the ledger"
    );
}

// ---------------------------------------------------------------------------
// Resolution — the outcome, not a verdict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_facilitator_records_the_outcome_and_closes_the_case() {
    let (host, _plugin, routes) = plugin().await;
    let resolve = route(&routes, "POST", "/api/conflicts/case/{id}/resolution");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_FACILITATION,
        &["bea", "carl"],
        &["dana"],
    )]);
    let mut resolved = case_row(11, STAGE_FACILITATION, &["bea", "carl"], &["dana"]);
    resolved["status"] = json!(STATUS_RESOLVED);
    resolved["outcome"] =
        json!("carl returns the gear by Friday; bea apologises for the accusation");
    resolved["agreement"] = json!("both parties agreed in facilitation to speak directly first");
    host.db.push_rows(vec![resolved]);

    let (status, body) = call(
        &resolve.handler,
        TestRequest::post("/api/conflicts/case/11/resolution")
            .param("id", "11")
            .json(&json!({
                "outcome": "carl returns the gear by Friday; bea apologises for the accusation",
                "agreement": "both parties agreed in facilitation to speak directly first",
                "reason": "agreed in facilitation on 2026-09-24",
            }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["case"]["status"], json!(STATUS_RESOLVED));
    assert_eq!(body["resolution"]["stage"], json!(STAGE_FACILITATION));
    assert_eq!(body["resolution"]["recorded_by"], json!("facilitator"));

    let update = param_text(&host, "UPDATE");
    assert!(
        update.contains("carl returns the gear by Friday"),
        "{update}"
    );
    assert!(update.contains("agreed in facilitation"), "{update}");
    assert!(update.contains("dana"), "{update}");

    // The closure is in the ledger, with its reason and the stage it closed at.
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_RESOLUTION), "{log}");
    assert!(log.contains(STAGE_FACILITATION), "{log}");
    assert!(log.contains("dana"), "{log}");
    assert!(
        log.contains("agreed in facilitation on 2026-09-24"),
        "{log}"
    );

    assert_audited(&host, "case.resolved");
    // The troop-level ledger records that a case closed — never the outcome's
    // text, which belongs to the parties.
    let audited = param_text(&host, "audit_log");
    assert!(!audited.contains("carl returns the gear"), "{audited}");

    host.events.assert_published(EVENT_CONFLICT_RESOLVED);
    let published = host.events.payloads(EVENT_CONFLICT_RESOLVED);
    assert_eq!(published[0]["resolved_by"], json!("facilitator"));
    assert_opaque(&published[0].to_string(), "conflict.resolved payload");
}

#[tokio::test]
async fn the_parties_record_the_agreement_they_reached_themselves() {
    let (host, _plugin, routes) = plugin().await;
    let agree = route(&routes, "POST", "/api/conflicts/case/{id}/agreement");
    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea", "carl"], &[])]);
    let mut resolved = case_row(11, STAGE_DIRECT, &["bea", "carl"], &[]);
    resolved["status"] = json!(STATUS_RESOLVED);
    resolved["agreement"] = json!("we each apologise and split the cost of the replacement");
    host.db.push_rows(vec![resolved]);

    let (status, body) = call(
        &agree.handler,
        TestRequest::post("/api/conflicts/case/11/agreement")
            .param("id", "11")
            .json(&json!({
                "agreement": "we each apologise and split the cost of the replacement",
                "reason": "we talked it through at the meeting and shook on it",
            }))
            .identity("carl", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["case"]["status"], json!(STATUS_RESOLVED));
    assert_eq!(body["resolution"]["recorded_by"], json!("party"));
    assert_eq!(body["resolution"]["stage"], json!(STAGE_DIRECT));

    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_AGREEMENT), "{log}");
    assert!(log.contains("carl"), "{log}");
    assert!(log.contains("we talked it through at the meeting"), "{log}");
    // The outcome defaults to the agreement when none is given separately.
    let update = param_text(&host, "UPDATE");
    assert!(
        update.contains("we each apologise and split the cost"),
        "{update}"
    );

    let published = host.events.payloads(EVENT_CONFLICT_RESOLVED);
    assert_eq!(published[0]["resolved_by"], json!("parties"));
    assert_opaque(&published[0].to_string(), "conflict.resolved payload");
}

#[tokio::test]
async fn the_parties_agreement_route_closes_at_the_entry_stage_only() {
    let (host, _plugin, routes) = plugin().await;
    let agree = route(&routes, "POST", "/api/conflicts/case/{id}/agreement");

    // A facilitator is not a party, so they cannot write the parties' agreement.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &agree.handler,
        TestRequest::post("/api/conflicts/case/11/agreement")
            .param("id", "11")
            .json(&json!({ "agreement": "settled" }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "the facilitator's refusal");

    // Past the entry stage a third party is in the room, and the record is
    // theirs to keep: the failure says so and names the right route.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_ARBITRATION,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &agree.handler,
        TestRequest::post("/api/conflicts/case/11/agreement")
            .param("id", "11")
            .json(&json!({ "agreement": "settled" }))
            .identity("carl", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains(STAGE_DIRECT), "{error}");
    assert!(error.contains("/resolution"), "{error}");
    assert_eq!(
        host.db.executed_sql().len(),
        0,
        "a refused agreement writes nothing"
    );
}

#[tokio::test]
async fn a_party_may_withdraw_a_case_it_filed() {
    let (host, _plugin, routes) = plugin().await;
    let withdraw = route(&routes, "POST", "/api/conflicts/case/{id}/withdraw");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let mut withdrawn = case_row(11, STAGE_DIRECT, &["bea", "carl"], &["dana"]);
    withdrawn["status"] = json!(STATUS_WITHDRAWN);
    host.db.push_rows(vec![withdrawn]);

    let (status, body) = call(
        &withdraw.handler,
        TestRequest::post("/api/conflicts/case/11/withdraw")
            .param("id", "11")
            .json(&json!({ "reason": "we sorted it out ourselves over the weekend" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["case"]["status"], json!(STATUS_WITHDRAWN));
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_WITHDRAWN), "{log}");
    assert!(log.contains("bea"), "{log}");
    assert!(
        log.contains("we sorted it out ourselves over the weekend"),
        "{log}"
    );
    host.events.assert_published(EVENT_CONFLICT_WITHDRAWN);
    assert_opaque(
        &host.events.payloads(EVENT_CONFLICT_WITHDRAWN)[0].to_string(),
        "conflict.withdrawn payload",
    );

    // A facilitator cannot withdraw somebody else's case.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &withdraw.handler,
        TestRequest::post("/api/conflicts/case/11/withdraw")
            .param("id", "11")
            .json(&json!({ "reason": "not worth the troop's time" }))
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_opaque(&body.to_string(), "the facilitator's refusal");

    // A withdrawal has to say why.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &withdraw.handler,
        TestRequest::post("/api/conflicts/case/11/withdraw")
            .param("id", "11")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("reason"));
}

// ---------------------------------------------------------------------------
// Staffing and the visibility list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_pathway_administrator_appoints_a_facilitator_but_never_themselves() {
    let (host, _plugin, routes) = plugin().await;
    let staff = route(&routes, "POST", "/api/conflicts/case/{id}/facilitator");

    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea", "carl"], &[])]);
    let (status, body) = call(
        &staff.handler,
        TestRequest::post("/api/conflicts/case/11/facilitator")
            .param("id", "11")
            .json(&json!({ "user_id": "dana", "reason": "nearest to both parties" }))
            .identity("ivy", &["troop_council_chair"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["action"], json!("assign"));
    assert_eq!(body["facilitator_ids"], json!(["dana"]));
    // No party is in this response: staffing is not reading.
    assert!(!body.to_string().contains("bea"), "{body}");

    let sql = host.db.executed_sql().join("\n");
    assert!(sql.contains("array_append"), "{sql}");
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_FACILITATOR_ASSIGNED), "{log}");
    assert!(log.contains("dana"), "{log}");
    assert!(log.contains("nearest to both parties"), "{log}");
    host.events.assert_published(EVENT_CONFLICT_STAFFING);
    assert_opaque(
        &host.events.payloads(EVENT_CONFLICT_STAFFING)[0].to_string(),
        "conflict.staffing.changed payload",
    );
    // The troop-level ledger records the act, not who carries the case.
    let audited = param_text(&host, "audit_log");
    assert!(!audited.contains("dana"), "{audited}");

    // Self-appointment is refused: it is the one route from "administers the
    // pathway" to "reads any case".
    host.db
        .push_rows(vec![case_row(11, STAGE_DIRECT, &["bea", "carl"], &[])]);
    let (status, body) = call(
        &staff.handler,
        TestRequest::post("/api/conflicts/case/11/facilitator")
            .param("id", "11")
            .json(&json!({ "user_id": "ivy" }))
            .identity("ivy", &["troop_council_chair"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("somebody else"));

    // Appointing the same person twice is a conflict, not a second row.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &staff.handler,
        TestRequest::post("/api/conflicts/case/11/facilitator")
            .param("id", "11")
            .json(&json!({ "user_id": "dana" }))
            .identity("ivy", &["troop_council_chair"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // Release, with its own ledger kind.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_DIRECT,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &staff.handler,
        TestRequest::post("/api/conflicts/case/11/facilitator")
            .param("id", "11")
            .json(&json!({
                "user_id": "dana",
                "action": "release",
                "reason": "dana is a party to a related matter",
            }))
            .identity("ivy", &["troop_council_chair"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["facilitator_ids"], json!([]));
    let sql = host.db.executed_sql().join("\n");
    assert!(sql.contains("array_remove"), "{sql}");
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_FACILITATOR_RELEASED), "{log}");
    assert!(log.contains("dana is a party to a related matter"), "{log}");
}

#[tokio::test]
async fn a_party_may_widen_the_visibility_list_and_the_widening_is_recorded() {
    let (host, _plugin, routes) = plugin().await;
    let add = route(&routes, "POST", "/api/conflicts/case/{id}/party");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_FACILITATION,
        &["bea", "carl"],
        &["dana"],
    )]);

    let (status, body) = call(
        &add.handler,
        TestRequest::post("/api/conflicts/case/11/party")
            .param("id", "11")
            .json(&json!({
                "user_id": "gwen",
                "reason": "gwen was there and both parties want her account on the record",
            }))
            .identity("carl", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["added"], json!("gwen"));
    assert_eq!(body["added_by"], json!("party"));
    let sql = host.db.executed_sql().join("\n");
    assert!(sql.contains("array_append"), "{sql}");
    assert!(sql.contains("party_ids"), "{sql}");
    // Adding a party hands them the record, so the reason is mandatory and lands
    // in the log.
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_PARTY_ADDED), "{log}");
    assert!(log.contains("carl"), "{log}");
    assert!(
        log.contains("gwen"),
        "the ledger names who was added: {log}"
    );
    assert!(log.contains("both parties want her account"), "{log}");
    host.events.assert_published(EVENT_CONFLICT_PARTY_ADDED);
    assert_opaque(
        &host.events.payloads(EVENT_CONFLICT_PARTY_ADDED)[0].to_string(),
        "conflict.party.added payload",
    );

    // A stranger cannot widen a case they cannot see — and neither can the
    // pathway's administrator, who is not on the case.
    for caller in ["erin", "ivy"] {
        host.db.push_rows(vec![case_row(
            11,
            STAGE_FACILITATION,
            &["bea", "carl"],
            &["dana"],
        )]);
        let (status, body) = call(
            &add.handler,
            TestRequest::post("/api/conflicts/case/11/party")
                .param("id", "11")
                .json(&json!({ "user_id": "gwen", "reason": "because I said so" }))
                .identity(caller, &["troop_council_chair"])
                .build(),
        )
        .await;
        assert_eq!(status, 403, "{caller}: {body}");
        assert_opaque(&body.to_string(), "the stranger's refusal");
    }

    // Somebody already on the case cannot be added twice.
    host.db.push_rows(vec![case_row(
        11,
        STAGE_FACILITATION,
        &["bea", "carl"],
        &["dana"],
    )]);
    let (status, body) = call(
        &add.handler,
        TestRequest::post("/api/conflicts/case/11/party")
            .param("id", "11")
            .json(&json!({ "user_id": "carl", "reason": "again" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_log_returns_the_whole_journey_in_order() {
    let (host, _plugin, routes) = plugin().await;
    let log_route = route(&routes, "GET", "/api/conflicts/case/{id}/log");
    host.db.push_rows(vec![case_row(
        11,
        STAGE_ARBITRATION,
        &["bea", "carl"],
        &["dana"],
    )]);
    host.db.push_rows(vec![
        json!({
            "id": 1, "case_id": 11, "kind": KIND_FILED, "from_stage": "",
            "to_stage": STAGE_DIRECT, "actor": "bea", "reason": "carl took my gear",
            "occurred_at": "2026-09-01 12:00:00+00"
        }),
        json!({
            "id": 2, "case_id": 11, "kind": KIND_TRANSITION, "from_stage": STAGE_DIRECT,
            "to_stage": STAGE_FACILITATION, "actor": "dana",
            "reason": "direct conversation broke down",
            "occurred_at": "2026-09-08 12:00:00+00"
        }),
        json!({
            "id": 3, "case_id": 11, "kind": KIND_NUDGE, "from_stage": STAGE_FACILITATION,
            "to_stage": STAGE_FACILITATION, "actor": "conflicts:auto",
            "reason": "no movement in facilitation for 8 day(s)",
            "occurred_at": "2026-09-16 12:00:00+00"
        }),
    ]);

    let (status, body) = call(
        &log_route.handler,
        TestRequest::get("/api/conflicts/case/11/log")
            .param("id", "11")
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["append_only"], json!(true));
    let entries = body["log"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["kind"], json!(KIND_FILED));
    assert_eq!(entries[1]["actor"], json!("dana"));
    assert_eq!(
        entries[1]["reason"],
        json!("direct conversation broke down")
    );
    assert_eq!(entries[2]["kind"], json!(KIND_NUDGE));
    // The journey, not just the destination: the stage that failed is still in
    // the record after the case moved past it.
    assert!(entries
        .iter()
        .any(|e| e["from_stage"] == json!(STAGE_DIRECT)
            && e["to_stage"] == json!(STAGE_FACILITATION)));

    let sql = host.db.queried_sql().join("\n");
    assert!(
        sql.contains("ORDER BY occurred_at, id"),
        "the history is read in the order it happened: {sql}"
    );
}

// ---------------------------------------------------------------------------
// Anti-dropout: the stall arithmetic
// ---------------------------------------------------------------------------

#[test]
fn a_stage_that_sits_beyond_the_threshold_is_stalled() {
    let now = Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 0).unwrap();

    assert_eq!(stage_age_hours(now - Duration::days(9), now), 9 * 24);
    assert!(is_stalled(
        now - Duration::hours(DEFAULT_STALL_HOURS),
        now,
        DEFAULT_STALL_HOURS
    ));
    assert!(!is_stalled(
        now - Duration::hours(DEFAULT_STALL_HOURS - 1),
        now,
        DEFAULT_STALL_HOURS
    ));
    // A clock that jumped backwards never reads as a case that is negatively
    // stalled — `0` age is not past any threshold.
    assert_eq!(stage_age_hours(now + Duration::days(2), now), 0);
    assert!(!is_stalled(
        now + Duration::days(2),
        now,
        DEFAULT_STALL_HOURS
    ));
    // A threshold of zero (a misconfiguration) is treated as one hour, so the
    // mechanism cannot be silenced into nudging nothing.
    assert!(is_stalled(now - Duration::hours(1), now, 0));

    // Nudging requires a stall *and* an expired cooldown.
    let since = now - Duration::days(30);
    assert!(should_nudge(
        since,
        None,
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    ));
    assert!(!should_nudge(
        since,
        Some(now - Duration::hours(6)),
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    ));
    assert!(should_nudge(
        since,
        Some(now - Duration::hours(DEFAULT_NUDGE_COOLDOWN_HOURS)),
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    ));
    // A case that is not stalled is never nudged, however long its cooldown.
    assert!(!should_nudge(
        now - Duration::hours(2),
        Some(now - Duration::days(30)),
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    ));
    // A previously nudged case stays eligible: anti-dropout that gave up after
    // N nudges would be the dropout it exists to prevent.
    assert!(should_nudge(
        since,
        Some(now - Duration::days(60)),
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    ));
}

#[test]
fn the_stall_threshold_is_configurable_and_defensive() {
    assert_eq!(stall_threshold_hours(&Value::Null), DEFAULT_STALL_HOURS);
    assert_eq!(stall_threshold_hours(&json!({})), DEFAULT_STALL_HOURS);
    assert_eq!(
        stall_threshold_hours(&json!({ "stage_stall_hours": 48 })),
        48
    );
    // A typo cannot silence the mechanism for a decade, or turn it into noise.
    assert_eq!(stall_threshold_hours(&json!({ "stage_stall_hours": 0 })), 1);
    assert_eq!(
        stall_threshold_hours(&json!({ "stage_stall_hours": 999_999 })),
        adjutant_conflicts::MAX_HOURS
    );
    assert_eq!(
        stall_threshold_hours(&json!({ "stage_stall_hours": "weekly" })),
        DEFAULT_STALL_HOURS,
        "an unreadable setting falls back rather than failing a schedule"
    );
    assert_eq!(
        nudge_cooldown_hours(&Value::Null),
        DEFAULT_NUDGE_COOLDOWN_HOURS
    );
    assert_eq!(
        nudge_cooldown_hours(&json!({ "nudge_cooldown_hours": 1 })),
        1
    );
}

#[test]
fn a_stall_report_names_the_case_and_never_the_people() {
    let now = Utc::now();
    let mut case = case_row(11, STAGE_FACILITATION, &["bea", "carl"], &["dana"]);
    case["stage_since"] = json!(render_pg(now - Duration::days(9)));
    case["nudged_at"] = Value::Null;

    let report = stall_report(
        &case,
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS,
    )
    .expect("a nine-day-old stage is stalled");
    assert_eq!(report["case_id"], json!(11));
    assert_eq!(report["stage"], json!(STAGE_FACILITATION));
    assert_eq!(report["days_stalled"], json!(9));
    assert_eq!(report["nudge_count"], json!(0));
    assert_eq!(report["needs_nudge"], json!(true));
    assert!(
        report.get("party_ids").is_none() && report.get("title").is_none(),
        "a stall report is built from an allowlist, not by echoing the row: {report}"
    );
    assert_opaque(&report.to_string(), "a stall report");

    // Not stalled: no report at all.
    let mut fresh = case.clone();
    fresh["stage_since"] = json!(render_pg(now - Duration::hours(2)));
    assert!(stall_report(
        &fresh,
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    )
    .is_none());

    // An unreadable stage timestamp is skipped rather than guessed at.
    let mut broken = case.clone();
    broken["stage_since"] = json!("not a timestamp");
    assert!(stall_report(
        &broken,
        now,
        DEFAULT_STALL_HOURS,
        DEFAULT_NUDGE_COOLDOWN_HOURS
    )
    .is_none());
}

#[test]
fn the_hosts_timestamp_rendering_parses() {
    let expected = Utc
        .with_ymd_and_hms(2026, 9, 1, 12, 0, 0)
        .unwrap()
        .timestamp();
    for raw in [
        "2026-09-01 12:00:00+00",
        "2026-09-01 12:00:00.123456+00",
        "2026-09-01 12:00:00+00:00",
        "2026-09-01T12:00:00Z",
    ] {
        let parsed = parse_timestamp(raw).unwrap_or_else(|| panic!("{raw} must parse"));
        assert_eq!(parsed.timestamp(), expected, "{raw}");
    }
    // A rendering with no offset is read as UTC rather than skipped: one host's
    // formatting choice must not become a silent dropout.
    assert_eq!(
        parse_timestamp("2026-09-01 12:00:00").unwrap().timestamp(),
        expected
    );
    // A real minute offset is preserved, not flattened to UTC.
    assert_eq!(
        parse_timestamp("2026-09-01 12:00:00+05:30")
            .unwrap()
            .timestamp(),
        expected - 5 * 3600 - 30 * 60
    );
    assert!(parse_timestamp("").is_none());
    assert!(parse_timestamp("soon").is_none());
}

// ---------------------------------------------------------------------------
// Anti-dropout: the scheduled nudge and the facilitator's queue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_scheduled_nudge_surfaces_a_stalled_case_exactly_once_per_cooldown() {
    let (host, plugin, _routes) =
        plugin_with_config(json!({ "stage_stall_hours": 48, "nudge_cooldown_hours": 12 })).await;

    // Case 11 stalled nine days ago and has never been nudged; case 12 stalled
    // too but was nudged two hours ago, so its cooldown has not expired.
    host.db.push_rows(vec![
        stalled_row(11, STAGE_FACILITATION, &["dana"], 0, None),
        stalled_row(12, STAGE_ARBITRATION, &["dana"], 1, Some(2)),
    ]);

    let schedules = plugin.schedules();
    let nudge = &schedules[0];
    (nudge.handler)().await.unwrap();

    // The probe reads open cases past the threshold and nothing that names a
    // party.
    let probe = host.db.queried_sql().join("\n");
    assert!(probe.contains("status = 'open'"), "{probe}");
    assert!(probe.contains("stage_since < now()"), "{probe}");
    assert!(!probe.contains("party_ids"), "{probe}");

    // One nudge written: counter bumped, ledger informed, event published.
    let updates = host
        .db
        .executed_sql()
        .into_iter()
        .filter(|sql| sql.contains("nudge_count = nudge_count + 1"))
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 1, "case 12 is inside its cooldown");
    assert!(
        !updates[0].contains("updated_at"),
        "a nudge is the clock talking, not a person: {}",
        updates[0]
    );
    let log = param_text(&host, "stage_log");
    assert!(log.contains(KIND_NUDGE), "{log}");
    assert!(log.contains("conflicts:auto"), "{log}");
    assert!(log.contains(STAGE_FACILITATION), "{log}");

    host.events.assert_published(EVENT_CONFLICT_STAGE_STALLED);
    let published = host.events.payloads(EVENT_CONFLICT_STAGE_STALLED);
    assert_eq!(published.len(), 1, "only the case whose cooldown expired");
    assert_eq!(published[0]["case_id"], json!(11));
    assert_eq!(published[0]["stage"], json!(STAGE_FACILITATION));
    assert_eq!(published[0]["days_stalled"], json!(9));
    assert_eq!(published[0]["nudge_count"], json!(1));
    assert_opaque(&published[0].to_string(), "conflict.stage.stalled payload");

    assert_audited(&host, "case.nudge");
}

#[tokio::test]
async fn the_stalled_queue_shows_a_facilitator_their_own_cases_only() {
    let (host, _plugin, routes) = plugin().await;
    let stalled = route(&routes, "GET", "/api/conflicts/stalled");
    // Dana carries 11 (stalled) and 13 (moving); Gwen carries 12 (stalled).
    let mut fresh = stalled_row(13, STAGE_DIRECT, &["dana"], 0, None);
    fresh["stage_since"] = json!(render_pg(Utc::now() - Duration::hours(3)));
    host.db.push_rows(vec![
        stalled_row(11, STAGE_FACILITATION, &["dana"], 2, Some(100)),
        stalled_row(12, STAGE_ARBITRATION, &["gwen"], 0, None),
        fresh,
    ]);

    let (status, body) = call(
        &stalled.handler,
        TestRequest::get("/api/conflicts/stalled")
            .identity("dana", &["conflict_facilitator"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["threshold_hours"], json!(DEFAULT_STALL_HOURS));
    let cases = body["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 1, "only Dana's stalled case: {cases:?}");
    assert_eq!(cases[0]["case_id"], json!(11));
    assert_eq!(cases[0]["needs_nudge"], json!(true));
    // Metadata only — the queue is a *facilitator's* view, not a reading of the
    // case.
    assert_opaque(&body.to_string(), "the stalled queue");

    let sql = host.db.queried_sql().join("\n");
    assert!(!sql.contains("party_ids"), "{sql}");
    assert!(sql.contains("facilitator_ids"), "{sql}");
}
