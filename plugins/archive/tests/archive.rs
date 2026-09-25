//! Archive plugin tests: filing, immutability and corrections, full-text search,
//! the cross-kind timeline, the relationship graph, and event ingestion
//! (SPEC §7.8).
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results in call order, so every test states the order its handler arranges —
//! the comment above each `push_*` says which call it answers. The order is not
//! incidental: it is the handler's authorization sequence, and a test that gets
//! it wrong would silently pass while the handler asked the wrong question.

use adjutant_archive::{
    body_label, cursor_of, describe_edge, impact_line, is_current, kind_label, parse_cursor,
    parse_instant, record_scope, relation_kinds, relation_phrase, render_instant, scope_of,
    ArchivePlugin, BODY_CODES, CHAIN, INGEST_ACTOR, KINDS, KIND_CONGRESS, KIND_CORRESPONDENCE,
    KIND_DECISION, KIND_IMPACT, KIND_MINUTES, KIND_MISSION_REPORT, KIND_MOTION, KIND_NOTE,
    KIND_POLICY, OUTCOME_ADOPTED, OUTCOME_FAILED, OUTCOME_PASSED, RELATIONS, RELATION_AMENDS,
    RELATION_AUTHORIZES, RELATION_DECIDES, RELATION_OUTCOME, RELATION_PRODUCES,
    RELATION_RELATES_TO, SEARCH_MODE_ALL_WORDS, SOURCE_CORRECTION, SOURCE_MISSIONS,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use chrono::Utc;
use serde_json::json;

/// The audit log's action is a bind parameter, so a test has to read it back
/// rather than look for it in the statement.
fn assert_audited(host: &TestHost, action: &str) {
    let params = host
        .db
        .last_execute_params("audit_log")
        .unwrap_or_else(|| panic!("no audit write ran (wanted {action})"));
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered.iter().any(|p| p.contains(action)),
        "no {action} audit: {rendered:?}"
    );
}

/// `SqlValue` crosses a boundary rather than being compared, so a test reads a
/// binding back through its debug rendering.
fn binding(value: &SqlValue) -> String {
    format!("{value:?}")
}

/// Every SQL string the host saw, queries and executes alike.
fn all_sql(host: &TestHost) -> String {
    let mut out = host.db.queried_sql().join("\n");
    out.push('\n');
    out.push_str(&host.db.executed_sql().join("\n"));
    out
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn plugin() -> (TestHost, ArchivePlugin, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
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
async fn call(
    handler: &adjutant_sdk::RouteHandler,
    req: PluginRequest,
) -> (u16, serde_json::Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// A record row as `RECORD_FIELDS` + the derived columns render one.
fn record_row(
    id: i64,
    kind: &str,
    title: &str,
    occurred_at: &str,
    scope_type: &str,
    scope_id: Option<&str>,
) -> serde_json::Value {
    json!({
        "id": id,
        "kind": kind,
        "title": title,
        "summary": "",
        "body": "",
        "body_code": "",
        "outcome": "",
        "scope_type": scope_type,
        "scope_id": scope_id,
        "occurred_at": "2026-03-02 18:00:00+00",
        "occurred_at_rfc3339": occurred_at,
        "source": "manual",
        "source_ref": serde_json::Value::Null,
        "source_url": "",
        "filed_by": "bea",
        "filed_at": "2026-03-02 18:05:00+00",
        "supersedes_id": serde_json::Value::Null,
        "correction_reason": "",
        "superseded_by": serde_json::Value::Null,
        "links_out": 0,
        "links_in": 0,
    })
}

/// A search result row: a record plus the two columns the search adds.
fn search_row(id: i64, rank: f64, snippet: &str) -> serde_json::Value {
    let mut row = record_row(
        id,
        KIND_MISSION_REPORT,
        "Coyote survey",
        "2026-03-02T18:00:00Z",
        "lodge",
        Some("3"),
    );
    row["rank"] = json!(rank);
    row["snippet"] = json!(snippet);
    row
}

fn link_row(id: i64, from: i64, to: i64, relation: &str) -> serde_json::Value {
    json!({
        "id": id,
        "from_id": from,
        "to_id": to,
        "relation": relation,
        "note": "",
        "created_by": "bea",
        "created_at": "2026-03-03 09:00:00+00",
    })
}

fn edge_row(
    id: i64,
    direction: &str,
    from: i64,
    to: i64,
    relation: &str,
    depth: i64,
) -> serde_json::Value {
    json!({
        "id": id,
        "link_id": id,
        "relation": relation,
        "note": "",
        "created_by": "bea",
        "created_at": "2026-03-03 09:00:00+00",
        "from_id": from,
        "to_id": to,
        "from_kind": "decision",
        "from_title": "Ban lead shot",
        "from_occurred_at": "2026-03-01 18:00:00+00",
        "to_kind": "policy",
        "to_title": "Lead-free ammunition policy",
        "to_occurred_at": "2026-03-02 18:00:00+00",
        "direction": direction,
        "depth": depth,
    })
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "archive");
    assert_eq!(plugin.name(), "Archive");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for perm in &permissions {
        assert!(perm.starts_with("archive:"), "{perm} must be namespaced");
    }
    for expected in [
        "archive:read",
        "archive:search",
        "archive:write",
        "archive:link",
        "archive:manage",
    ] {
        assert!(
            permissions.iter().any(|p| p == expected),
            "missing {expected}"
        );
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/archive"),
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
    assert!(
        routes.len() >= 13,
        "the surface the SPEC asks for is bigger than this: {}",
        routes.len()
    );

    // Search has its own permission, and removing a relationship is
    // administration, not filing (SPEC §9.2, §8 #3).
    let search = route(&routes, "GET", "/api/archive/search");
    assert_eq!(
        search.required_permission.as_deref(),
        Some("archive:search")
    );
    let unlink = route(&routes, "DELETE", "/api/archive/link/{id}");
    assert_eq!(
        unlink.required_permission.as_deref(),
        Some("archive:manage")
    );
    assert_eq!(unlink.required_scope, Some(Scope::troop()));
    let file = route(&routes, "POST", "/api/archive/record");
    assert_eq!(file.required_permission.as_deref(), Some("archive:write"));
    assert_eq!(file.required_scope, None, "the handler checks the scope");
}

#[tokio::test]
async fn a_record_cannot_be_edited_or_deleted_anywhere_in_the_surface() {
    let (_host, plugin, routes) = plugin().await;

    // 1. No route mutates or removes a record. `DELETE /api/archive/link/{id}`
    //    removes an edge, which is not a record.
    for r in &routes {
        let touches_a_record = r.path.contains("/record/") || r.path == "/api/archive/record";
        if !touches_a_record {
            continue;
        }
        assert!(
            r.method == Method::Post || r.method == Method::Get,
            "{} {} is a mutating verb on the record surface",
            r.method.as_str(),
            r.path
        );
    }

    // 2. The database refuses it even when a route is not the caller.
    let ddl = &plugin.migrations()[1].sql;
    assert!(
        ddl.contains("BEFORE UPDATE OR DELETE ON records"),
        "the append-only trigger is missing: {ddl}"
    );
    assert!(ddl.contains("archive_records_append_only"));
    assert_eq!(
        ddl.matches("RAISE EXCEPTION").count(),
        2,
        "both UPDATE and DELETE must raise, not silently accept: {ddl}"
    );
    assert!(
        ddl.contains("record % cannot be modified") && ddl.contains("record % cannot be deleted"),
        "each verb gets its own message: {ddl}"
    );
    assert!(
        ddl.contains("append-only"),
        "the failure has to say what the archive is: {ddl}"
    );
    assert!(
        ddl.contains("/api/archive/record/%/supersede"),
        "the failure has to say what to do instead: {ddl}"
    );
    // And the schema offers exactly one way to correct a record.
    let schema = &plugin.migrations()[0].sql;
    assert!(schema.contains("supersedes_id BIGINT REFERENCES records(id)"));
    assert!(schema.contains("correction_reason TEXT NOT NULL DEFAULT ''"));
    assert!(schema.contains("records_no_self_supersede"));
}

#[tokio::test]
async fn the_search_index_is_a_real_postgres_full_text_index() {
    let (_host, plugin, _routes) = plugin().await;
    let schema = &plugin.migrations()[0].sql;
    assert!(
        schema.contains("search TSVECTOR GENERATED ALWAYS AS ("),
        "search must be a generated tsvector, not a LIKE target: {schema}"
    );
    assert!(
        schema.contains("to_tsvector('english', title)"),
        "the two-argument to_tsvector is IMMUTABLE, which a generated column requires"
    );
    for weighted in ["title), 'A'", "summary), 'B'", "body), 'D'"] {
        assert!(
            schema.contains(&format!("setweight(to_tsvector('english', {weighted})")),
            "the tsvector is weighted so a title match outranks a body match: {weighted}"
        );
    }
    assert!(
        schema.contains(
            "CREATE INDEX IF NOT EXISTS idx_records_search ON records USING GIN (search)"
        ),
        "without the GIN index the predicate is a sequential scan: {schema}"
    );
    assert!(schema.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_records_source_ref"));
    assert!(schema.contains("CONSTRAINT links_unique UNIQUE (from_id, to_id, relation)"));
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_archive::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "archive");
    assert_eq!(
        adjutant_archive::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

// ---------------------------------------------------------------------------
// Filing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filing_a_record_stores_it_and_answers_with_it() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/archive/record");

    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:write at the troop
    host.db.push_rows(vec![record_row(
        41,
        KIND_DECISION,
        "Ban lead shot in the Coventry swamp",
        "2026-03-02T18:00:00Z",
        "troop",
        None,
    )]);

    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({
                "kind": KIND_DECISION,
                "title": "Ban lead shot in the Coventry swamp",
                "summary": "Congress vote, 12-4",
                "body": "The Congress resolved that lead shot is banned on the Coventry swamp.",
                "body_code": "congress",
                "outcome": OUTCOME_PASSED,
                "occurred_at": "2026-03-02T18:00:00Z",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["record"]["id"], json!(41));
    assert_eq!(body["record"]["kind"], json!(KIND_DECISION));

    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("records"))
        .expect("an insert ran");
    assert!(
        insert.contains("ON CONFLICT (source_ref) DO NOTHING"),
        "the idempotency key is the conflict target: {insert}"
    );
    assert!(insert.contains("RETURNING"));
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains("congress")));
    assert!(rendered.iter().any(|p| p.contains(OUTCOME_PASSED)));
    assert!(
        rendered.iter().any(|p| p.contains("2026-03-02T18:00:00Z")),
        "the instant is bound as RFC 3339 for `::timestamptz`: {rendered:?}"
    );
    host.events.assert_published("archive.record.filed");
    assert_audited(&host, "record.file");
}

#[tokio::test]
async fn filing_refuses_a_bad_kind_a_missing_title_and_an_impossible_scope() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/archive/record");

    // The vocabulary is checked before any query.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({ "kind": "scrapbook", "title": "x" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("scrapbook"));

    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({ "kind": KIND_MINUTES, "title": "   " }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("title"));

    // A Lodge record with no Lodge: the database would refuse the shape, so the
    // handler must too.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(
                &json!({ "kind": KIND_MINUTES, "title": "Lodge 3 minutes", "scope_type": "lodge" }),
            )
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("scope_id"));

    // Congress proceedings are the troop's, never one Lodge's.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({
                "kind": KIND_CONGRESS,
                "title": "Congress 4",
                "body_code": "congress",
                "scope_type": "lodge",
                "scope_id": "3",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("troop-wide"));

    assert_eq!(
        host.db.query_count(),
        0,
        "no database work before the body is valid"
    );
}

#[tokio::test]
async fn writes_are_held_to_the_scope_they_cover() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/archive/record");

    // A Lodge-only grant cannot file a troop-wide record: `reach` refuses with no
    // query at all, because no grant covers the troop.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_secretary".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "kind": KIND_CONGRESS, "title": "Congress 4" }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(host.db.query_count(), 0);

    // The same grant may file its own Lodge's minutes.
    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:write at Lodge 3
    host.db.push_rows(vec![record_row(
        52,
        KIND_MINUTES,
        "Lodge 3 minutes, March",
        "2026-03-05T19:00:00Z",
        "lodge",
        Some("3"),
    )]);
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_secretary".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({
                "kind": KIND_MINUTES,
                "title": "Lodge 3 minutes, March",
                "scope_type": "lodge",
                "scope_id": "3",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

#[tokio::test]
async fn an_explicit_source_ref_is_an_idempotency_key_and_a_409_when_taken() {
    let (host, _plugin, routes) = plugin().await;
    let file = route(&routes, "POST", "/api/archive/record");

    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![]); // ON CONFLICT DO NOTHING: the row is already there

    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({
                "kind": KIND_CORRESPONDENCE,
                "title": "Letter to the landowner",
                "source": "import",
                "source_ref": "import:letter:2009-04-01",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("already filed"));

    // A malformed reference is the caller's mistake, before any query.
    let (status, body) = call(
        &file.handler,
        TestRequest::post("/api/archive/record")
            .identity("bea", &["chief"])
            .json(&json!({
                "kind": KIND_NOTE,
                "title": "Note",
                "source_ref": "letters",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("<plugin>:<entity>:<id>"));
}

// ---------------------------------------------------------------------------
// Correction: a new record supersedes the old one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_correction_is_a_new_record_that_supersedes_the_original() {
    let (host, _plugin, routes) = plugin().await;
    let supersede = route(&routes, "POST", "/api/archive/record/{id}/supersede");

    host.db.push_rows(vec![record_row(
        41,
        KIND_MINUTES,
        "Lodge 3 minutes, March",
        "2026-03-05T19:00:00Z",
        "troop",
        None,
    )]); // fetch the original
    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:write is readable at the troop
    host.db.push_rows(vec![json!({ "n": 1 })]); // ...and writeable at the record's scope
    host.db.push_rows(vec![]); // nothing supersedes the original yet
    host.db.push_rows(vec![record_row(
        99,
        KIND_MINUTES,
        "Lodge 3 minutes, March",
        "2026-03-05T19:00:00Z",
        "troop",
        None,
    )]); // the correction

    let (status, body) = call(
        &supersede.handler,
        TestRequest::post("/api/archive/record/41/supersede")
            .param("id", "41")
            .identity("bea", &["chief"])
            .json(&json!({
                "body": "Corrected: the vote was 7-2, not 8-1.",
                "reason": "the tally was transcribed from the wrong column",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["record"]["id"], json!(99));
    assert_eq!(body["supersedes"]["id"], json!(41));
    assert_eq!(body["original_preserved"], json!(true));

    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("records"))
        .expect("the correction was inserted");
    assert!(
        insert.contains("supersedes_id"),
        "the correction points at the original: {insert}"
    );
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("transcribed from the wrong column")),
        "the reason is stored with the correction: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|p| p.contains("Int(41)")),
        "supersedes_id is 41: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|p| p.contains(SOURCE_CORRECTION)),
        "a correction is marked as one: {rendered:?}"
    );
    // Nothing anywhere UPDATEs or DELETEs a record.
    assert!(
        !all_sql(&host).contains("UPDATE"),
        "a correction must not touch the original: {}",
        all_sql(&host)
    );
    host.events.assert_published("archive.record.superseded");
    assert_audited(&host, "record.supersede");
}

#[tokio::test]
async fn correcting_an_already_corrected_record_is_refused_with_a_pointer() {
    let (host, _plugin, routes) = plugin().await;
    let supersede = route(&routes, "POST", "/api/archive/record/{id}/supersede");

    host.db.push_rows(vec![record_row(
        41,
        KIND_MINUTES,
        "Lodge 3 minutes, March",
        "2026-03-05T19:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "id": 99 })]); // already superseded by 99

    let (status, body) = call(
        &supersede.handler,
        TestRequest::post("/api/archive/record/41/supersede")
            .param("id", "41")
            .identity("bea", &["chief"])
            .json(&json!({ "body": "again", "reason": "a second go" }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("99"));
    assert_eq!(
        host.db
            .queried_sql()
            .iter()
            .filter(|s| s.contains("INSERT INTO") && s.contains("records"))
            .count(),
        0,
        "the second correction was refused, so nothing was filed"
    );

    // A correction with no stated reason is an edit wearing a hat.
    let (status, body) = call(
        &supersede.handler,
        TestRequest::post("/api/archive/record/41/supersede")
            .param("id", "41")
            .identity("bea", &["chief"])
            .json(&json!({ "body": "changed" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("reason"));
}

#[tokio::test]
async fn the_correction_chain_walks_both_ways() {
    let (host, _plugin, routes) = plugin().await;
    let chain = route(&routes, "GET", "/api/archive/record/{id}/chain");

    host.db.push_rows(vec![record_row(
        42,
        KIND_MINUTES,
        "Lodge 3 minutes, March",
        "2026-03-05T19:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:read at the troop
    host.db.push_rows(vec![
        json!({ "leg": "ancestor", "id": 40, "supersedes_id": null, "title": "Lodge 3 minutes",
                "kind": KIND_MINUTES, "correction_reason": "", "filed_by": "bea",
                "filed_at": "2026-03-05 19:30:00+00", "depth": 2 }),
        json!({ "leg": "ancestor", "id": 41, "supersedes_id": 40, "title": "Lodge 3 minutes",
                "kind": KIND_MINUTES, "correction_reason": "wrong month", "filed_by": "bea",
                "filed_at": "2026-03-06 09:00:00+00", "depth": 1 }),
        json!({ "leg": "descendant", "id": 43, "supersedes_id": 42, "title": "Lodge 3 minutes",
                "kind": KIND_MINUTES, "correction_reason": "the tally was wrong",
                "filed_by": "carl", "filed_at": "2026-03-08 09:00:00+00", "depth": 1 }),
    ]);

    let (status, body) = call(
        &chain.handler,
        TestRequest::get("/api/archive/record/42/chain")
            .param("id", "42")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["original_id"], json!(40), "the deepest ancestor");
    assert_eq!(body["current_id"], json!(43), "the deepest descendant");
    assert_eq!(body["corrected"], json!(true));
    assert_eq!(body["corrections"], json!(1));
    assert_eq!(body["ancestors"].as_array().unwrap().len(), 2);
    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("WITH RECURSIVE"))
        .expect("a recursive walk ran");
    assert!(sql.contains("JOIN ancestors a ON r.id = a.supersedes_id"));
    assert!(sql.contains("JOIN descendants d ON r.supersedes_id = d.id"));
}

// ---------------------------------------------------------------------------
// Full-text search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_finds_a_record_by_its_content_through_the_tsvector_index() {
    let (host, _plugin, routes) = plugin().await;
    let search = route(&routes, "GET", "/api/archive/search");

    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:search at the troop
    host.db
        .push_rows(vec![json!({ "parsed": "'coyote' & 'survey'" })]); // what the query parsed to
    host.db.push_rows(vec![search_row(
        7,
        0.75,
        "The troop walked <mark>coyote</mark> transects along the Coventry swamp...",
    )]);

    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "coyote survey")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["matches"], json!(1));
    assert_eq!(body["parsed"], json!("'coyote' & 'survey'"));
    assert!(
        body["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("<mark>coyote</mark>"),
        "the snippet shows why it matched: {body}"
    );
    assert_eq!(body["results"][0]["rank"], json!(0.75));

    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("ts_rank_cd"))
        .expect("the search query ran");
    assert!(
        sql.contains("websearch_to_tsquery('english', $4)"),
        "the tsquery is parsed by PostgreSQL, and reused by predicate/rank/headline: {sql}"
    );
    assert!(sql.contains("r.search @@ q.query"), "{sql}");
    assert!(sql.contains("ts_rank_cd(r.search, q.query)"), "{sql}");
    assert!(sql.contains("ts_headline('english'"), "{sql}");
    assert!(
        sql.contains("ORDER BY rank DESC"),
        "ranking, not insertion order: {sql}"
    );
    // The visibility predicate still applies: a search that leaked a Lodge's
    // minutes would undo the point of scoping them.
    assert!(sql.contains("r.scope_id = ANY($2)"), "{sql}");
    assert!(sql.contains("r.filed_by = $3"), "{sql}");
}

#[tokio::test]
async fn search_offers_all_words_mode_and_says_when_a_query_is_all_stopwords() {
    let (host, _plugin, routes) = plugin().await;
    let search = route(&routes, "GET", "/api/archive/search");

    // mode=all_words: every word ANDed, no operator syntax.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db
        .push_rows(vec![json!({ "parsed": "'coyote' & 'survey'" })]);
    host.db.push_rows(vec![search_row(7, 0.4, "…")]);
    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "coyote survey")
            .query_param("mode", SEARCH_MODE_ALL_WORDS)
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["mode"], json!(SEARCH_MODE_ALL_WORDS));
    assert!(
        host.db
            .queried_sql()
            .iter()
            .any(|s| s.contains("plainto_tsquery('english', $4)")),
        "{:?}",
        host.db.queried_sql()
    );

    // An unknown mode cannot reach SQL: the function name is chosen from a
    // closed set.
    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "x")
            .query_param("mode", "regex")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // "the and of" parses to an empty tsquery: the honest answer is zero
    // matches and a reason, not an empty archive.
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
    let routes = plugin.routes();
    let search = route(&routes, "GET", "/api/archive/search");
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "parsed": "" })]);
    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "the and of")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["matches"], json!(0));
    assert_eq!(body["results"], json!([]));
    assert!(body["note"].as_str().unwrap().contains("stopwords"));
    assert_eq!(host.db.query_count(), 2, "no scan ran");

    // A query with no text is the caller's mistake.
    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "   ")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("q"));
}

#[tokio::test]
async fn a_scoped_caller_searches_only_what_they_can_read() {
    let (host, _plugin, routes) = plugin().await;
    let search = route(&routes, "GET", "/api/archive/search");

    // A Lodge-3 grant: `has_in_scope(troop)` refuses without querying, so the
    // first thing the host sees is the parse probe.
    host.db.push_rows(vec![json!({ "parsed": "'coyote'" })]);
    host.db.push_rows(vec![]);
    let (status, body) = call(
        &search.handler,
        TestRequest::get("/api/archive/search")
            .query_param("q", "coyote")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "scout".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["scope"], json!("scoped"));
    let params = host
        .db
        .last_query_params("ts_rank_cd")
        .expect("the search query ran");
    assert_eq!(binding(&params[0]), "Bool(false)");
    assert_eq!(binding(&params[1]), "TextArray([\"3\"])");
    assert_eq!(binding(&params[2]), "Text(\"bea\")");
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_timeline_is_one_chronological_stream_across_every_kind() {
    let (host, _plugin, routes) = plugin().await;
    let timeline = route(&routes, "GET", "/api/archive/timeline");

    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:read at the troop
    host.db.push_rows(vec![
        record_row(
            91,
            KIND_DECISION,
            "Lead shot banned",
            "2026-03-02T18:00:00Z",
            "troop",
            None,
        ),
        record_row(
            92,
            KIND_MINUTES,
            "Troop Council, March",
            "2026-03-02T18:00:00Z",
            "troop",
            None,
        ),
        record_row(
            90,
            KIND_POLICY,
            "Lead-free ammunition policy",
            "2026-01-15T00:00:00Z",
            "troop",
            None,
        ),
        record_row(
            88,
            KIND_MISSION_REPORT,
            "Coyote survey",
            "2025-11-01T20:00:00Z",
            "lodge",
            Some("3"),
        ),
    ]);

    let (status, body) = call(
        &timeline.handler,
        TestRequest::get("/api/archive/timeline")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    // One stream, newest first — a decision, then the minutes of the same
    // evening, then a policy from January, then last autumn's mission report.
    assert_eq!(items[0]["kind"], json!(KIND_DECISION));
    assert_eq!(items[1]["kind"], json!(KIND_MINUTES));
    assert_eq!(items[2]["kind"], json!(KIND_POLICY));
    assert_eq!(items[3]["kind"], json!(KIND_MISSION_REPORT));
    assert_eq!(items[0]["kind_label"], json!(kind_label(KIND_DECISION)));
    assert_eq!(body["counts_by_kind"][KIND_DECISION], json!(1));
    assert_eq!(body["counts_by_kind"][KIND_MISSION_REPORT], json!(1));
    assert_eq!(body["order"], json!("occurred_at DESC, id DESC"));
    assert_eq!(body["scope"], json!("troop"));

    // The cursor is the last item's keyset position, in the documented shape.
    assert_eq!(
        body["cursor"],
        json!(format!("2025-11-01T20:00:00Z{}88", '|'))
    );

    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("ORDER BY r.occurred_at DESC"))
        .expect("the timeline query ran");
    assert!(
        sql.contains("NOT EXISTS (SELECT 1 FROM") && sql.contains("c.supersedes_id = r.id"),
        "superseded originals stay out of the timeline by default: {sql}"
    );
    assert!(sql.contains("r.scope_id = ANY($2)"), "{sql}");
}

#[tokio::test]
async fn a_cursor_pages_by_keyset_and_a_window_is_half_open() {
    let (host, _plugin, routes) = plugin().await;
    let timeline = route(&routes, "GET", "/api/archive/timeline");

    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![record_row(
        80,
        KIND_MINUTES,
        "Troop Council, February",
        "2026-02-02T18:00:00Z",
        "troop",
        None,
    )]);

    let (status, body) = call(
        &timeline.handler,
        TestRequest::get("/api/archive/timeline")
            .query_param("cursor", "2026-03-02T18:00:00Z|91")
            .query_param("to", "2026-03-01")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("ORDER BY r.occurred_at DESC"))
        .unwrap();
    assert!(
        sql.contains("(r.occurred_at, r.id) < ($5::timestamptz, $6::bigint)"),
        "keyset pagination, not OFFSET: {sql}"
    );
    let params = host.db.last_query_params("ORDER BY r.occurred_at").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains("2026-03-02T18:00:00Z")));
    assert!(rendered.iter().any(|p| p.contains("Int(91)")));
    assert!(
        rendered.iter().any(|p| p.contains("2026-03-01T00:00:00Z")),
        "a date-only bound is midnight: {rendered:?}"
    );
    assert_eq!(body["window"]["to_exclusive"], json!(true));

    // A cursor that is not a cursor is the caller's mistake.
    let (status, body) = call(
        &timeline.handler,
        TestRequest::get("/api/archive/timeline")
            .query_param("cursor", "whenever")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("cursor"));
}

#[tokio::test]
async fn listing_can_include_superseded_versions_and_filters_by_vocabulary() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/archive/records");

    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![record_row(
        41,
        KIND_CONGRESS,
        "Congress 4",
        "2026-03-02T18:00:00Z",
        "troop",
        None,
    )]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/archive/records")
            .query_param("kind", "congress_proceeding,decision")
            .query_param("body", "congress")
            .query_param("include_superseded", "1")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["include_superseded"], json!(true));
    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("ORDER BY r.occurred_at DESC"))
        .unwrap();
    assert!(sql.contains("r.kind = ANY($4)"), "{sql}");
    assert!(sql.contains("r.body_code = $5"), "{sql}");
    assert!(
        !sql.contains("NOT EXISTS (SELECT 1 FROM"),
        "include_superseded=1 drops the current-only filter: {sql}"
    );

    // An unknown kind is the caller's mistake, not an empty list.
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/archive/records")
            .query_param("kind", "gossip")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn stats_reports_the_archives_shape_and_the_kinds_it_has_none_of() {
    let (host, _plugin, routes) = plugin().await;
    let stats = route(&routes, "GET", "/api/archive/stats");

    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db
        .push_rows(vec![json!({ "kind": KIND_POLICY, "n": 3 })]);
    host.db.push_rows(vec![json!({ "year": "2026", "n": 3 })]);
    host.db
        .push_rows(vec![json!({ "body_code": "congress", "n": 2 })]);
    host.db.push_rows(vec![json!({
        "total": 3, "corrections": 1, "superseded": 1, "without_links_out": 0,
        "sources": 2, "earliest": "2026-01-15 00:00:00+00", "latest": "2026-03-02 18:00:00+00"
    })]);

    let (status, body) = call(
        &stats.handler,
        TestRequest::get("/api/archive/stats")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["totals"]["total"], json!(3));
    assert_eq!(body["totals"]["corrections"], json!(1));
    let missing: Vec<&str> = body["kinds_without_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(missing.contains(&KIND_MINUTES), "{missing:?}");
    assert!(!missing.contains(&KIND_POLICY), "{missing:?}");
}

// ---------------------------------------------------------------------------
// Relationships
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_relationship_is_recorded_with_a_sentence_and_needs_both_scopes() {
    let (host, _plugin, routes) = plugin().await;
    let link = route(&routes, "POST", "/api/archive/record/{id}/link");

    host.db.push_rows(vec![record_row(
        91,
        KIND_DECISION,
        "Lead shot banned",
        "2026-03-02T18:00:00Z",
        "troop",
        None,
    )]); // the from record
    host.db.push_rows(vec![json!({ "n": 1 })]); // archive:link as it is read
    host.db.push_rows(vec![record_row(
        90,
        KIND_POLICY,
        "Lead-free ammunition policy",
        "2026-01-15T00:00:00Z",
        "troop",
        None,
    )]); // the to record
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // reach at the from scope
    host.db.push_rows(vec![json!({ "n": 1 })]); // reach at the to scope
    host.db
        .push_rows(vec![link_row(5, 91, 90, RELATION_DECIDES)]);

    let (status, body) = call(
        &link.handler,
        TestRequest::post("/api/archive/record/91/link")
            .param("id", "91")
            .identity("bea", &["chief"])
            .json(&json!({ "to_id": 90, "relation": RELATION_DECIDES, "note": "the policy this vote established" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["link"]["relation"], json!(RELATION_DECIDES));
    assert_eq!(
        body["step"],
        json!(format!(
            "decision #91 \u{201c}Lead shot banned\u{201d} decided policy #90 \u{201c}Lead-free ammunition policy\u{201d}"
        ))
    );
    assert!(
        body["advice"].is_null(),
        "the endpoints match the relation: {body}"
    );
    let upsert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("INSERT INTO") && s.contains("links"))
        .expect("the link was written");
    assert!(
        upsert.contains("ON CONFLICT (from_id, to_id, relation) DO UPDATE SET note"),
        "linking twice is an idempotent note update, not a duplicate edge: {upsert}"
    );
    assert_audited(&host, "link.create");
    host.events.assert_published("archive.link.created");

    // A relation whose endpoints are unusual is stored, with advice — the
    // troop's vocabulary is the troop's.
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
    let routes = plugin.routes();
    let link = route(&routes, "POST", "/api/archive/record/{id}/link");
    host.db.push_rows(vec![record_row(
        1,
        KIND_MINUTES,
        "Minutes",
        "2026-01-01T00:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![record_row(
        2,
        KIND_IMPACT,
        "Impact",
        "2026-01-02T00:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![link_row(6, 1, 2, RELATION_DECIDES)]);
    let (status, body) = call(
        &link.handler,
        TestRequest::post("/api/archive/record/1/link")
            .param("id", "1")
            .identity("bea", &["chief"])
            .json(&json!({ "to_id": 2, "relation": RELATION_DECIDES }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["advice"]
            .as_str()
            .unwrap()
            .contains("decision \u{2192} policy"),
        "the advice names the conventional shape: {body}"
    );

    // A record cannot be linked to itself.
    let (status, body) = call(
        &link.handler,
        TestRequest::post("/api/archive/record/1/link")
            .param("id", "1")
            .identity("bea", &["chief"])
            .json(&json!({ "to_id": 1, "relation": RELATION_RELATES_TO }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("itself"));
}

#[tokio::test]
async fn links_traverse_both_directions_and_the_sentence_reads_both_ways() {
    let (host, _plugin, routes) = plugin().await;
    let links = route(&routes, "GET", "/api/archive/record/{id}/links");

    host.db.push_rows(vec![record_row(
        90,
        KIND_POLICY,
        "Lead-free ammunition policy",
        "2026-01-15T00:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![
        {
            let mut edge = edge_row(3, "incoming", 91, 90, RELATION_DECIDES, 1);
            edge["from_kind"] = json!(KIND_DECISION);
            edge["from_title"] = json!("Lead shot banned");
            edge["to_kind"] = json!(KIND_POLICY);
            edge["to_title"] = json!("Lead-free ammunition policy");
            edge
        },
        {
            let mut edge = edge_row(4, "outgoing", 90, 95, RELATION_AUTHORIZES, 1);
            edge["from_kind"] = json!(KIND_POLICY);
            edge["from_title"] = json!("Lead-free ammunition policy");
            edge["to_kind"] = json!(KIND_MISSION_REPORT);
            edge["to_title"] = json!("Coyote survey");
            edge
        },
    ]);

    let (status, body) = call(
        &links.handler,
        TestRequest::get("/api/archive/record/90/links")
            .param("id", "90")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["upstream"], json!(1));
    assert_eq!(body["downstream"], json!(1));
    assert_eq!(
        body["why_it_exists"],
        json!([
            "decision #91 \u{201c}Lead shot banned\u{201d} decided policy #90 \u{201c}Lead-free ammunition policy\u{201d}"
        ])
    );
    assert_eq!(
        body["what_it_produced"],
        json!([
            "policy #90 \u{201c}Lead-free ammunition policy\u{201d} authorized mission_report #95 \u{201c}Coyote survey\u{201d}"
        ])
    );

    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("CASE WHEN l.from_id = $1"))
        .expect("the edge query ran");
    assert!(
        sql.contains("WHERE l.from_id = $1 OR l.to_id = $1"),
        "edges are read from both ends: {sql}"
    );
    assert!(sql.contains("JOIN") && sql.contains("from_title"), "{sql}");
}

#[tokio::test]
async fn lineage_walks_the_chain_both_ways_and_says_so_in_prose() {
    let (host, _plugin, routes) = plugin().await;
    let lineage = route(&routes, "GET", "/api/archive/record/{id}/lineage");

    host.db.push_rows(vec![record_row(
        90,
        KIND_POLICY,
        "Lead-free ammunition policy",
        "2026-01-15T00:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![
        json!({ "direction": "downstream", "link_id": 1, "from_id": 90, "to_id": 95,
                "relation": RELATION_AUTHORIZES, "depth": 1 }),
        json!({ "direction": "downstream", "link_id": 2, "from_id": 95, "to_id": 99,
                "relation": RELATION_PRODUCES, "depth": 2 }),
        json!({ "direction": "upstream", "link_id": 3, "from_id": 91, "to_id": 90,
                "relation": RELATION_DECIDES, "depth": 1 }),
    ]);
    host.db.push_rows(vec![
        json!({ "id": 90, "kind": KIND_POLICY, "title": "Lead-free ammunition policy",
                "outcome": "", "superseded_by": null }),
        json!({ "id": 91, "kind": KIND_DECISION, "title": "Lead shot banned",
                "outcome": OUTCOME_PASSED, "superseded_by": null }),
        json!({ "id": 95, "kind": KIND_MISSION_REPORT, "title": "Coyote survey",
                "outcome": "", "superseded_by": null }),
        json!({ "id": 99, "kind": KIND_IMPACT, "title": "18.5 service hours",
                "outcome": "", "superseded_by": null }),
    ]);

    let (status, body) = call(
        &lineage.handler,
        TestRequest::get("/api/archive/record/90/lineage")
            .param("id", "90")
            .query_param("depth", "3")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["why_it_exists"], json!(1));
    assert_eq!(body["what_it_produced"], json!(2));
    assert_eq!(body["nodes"].as_array().unwrap().len(), 4);
    let explanation: Vec<&str> = body["explanation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        explanation,
        vec![
            "policy #90 \u{201c}Lead-free ammunition policy\u{201d} authorized mission_report #95 \u{201c}Coyote survey\u{201d}",
            "mission_report #95 \u{201c}Coyote survey\u{201d} produced impact #99 \u{201c}18.5 service hours\u{201d}",
            "decision #91 \u{201c}Lead shot banned\u{201d} decided policy #90 \u{201c}Lead-free ammunition policy\u{201d}",
        ],
        "why does this policy exist / what did this decision produce, in prose: {body}"
    );

    let sql = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("WITH RECURSIVE"))
        .expect("a recursive walk ran");
    assert!(sql.contains("ARRAY[$1, l.to_id] AS visited"), "{sql}");
    assert!(
        sql.contains("NOT (l.to_id = ANY(d.visited))"),
        "a hand-drawn cycle must not run forever: {sql}"
    );
    assert!(sql.contains("d.depth < $2"), "{sql}");

    // An unknown direction is the caller's mistake, and it is the *first* thing
    // checked — so it is still a 400 with no rows queued at all.
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
    let routes = plugin.routes();
    let lineage = route(&routes, "GET", "/api/archive/record/{id}/lineage");
    let (status, body) = call(
        &lineage.handler,
        TestRequest::get("/api/archive/record/90/lineage")
            .param("id", "90")
            .query_param("direction", "sideways")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn an_edge_can_be_removed_and_the_removal_is_audited_with_what_it_said() {
    let (host, _plugin, routes) = plugin().await;
    let unlink = route(&routes, "DELETE", "/api/archive/link/{id}");

    host.db
        .push_rows(vec![link_row(5, 91, 90, RELATION_DECIDES)]);

    let (status, body) = call(
        &unlink.handler,
        TestRequest::delete("/api/archive/link/5")
            .param("id", "5")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(5));
    host.db.assert_executed(&["DELETE FROM", "links"]);
    assert_audited(&host, "link.delete");
    host.events.assert_published("archive.link.deleted");
    let delete_sql = host
        .db
        .executed_sql()
        .into_iter()
        .find(|s| s.contains("DELETE FROM"))
        .expect("a delete ran");
    assert!(
        delete_sql.contains("links") && !delete_sql.contains("records"),
        "unlinking must not touch the records: {delete_sql}"
    );
}

// ---------------------------------------------------------------------------
// Event ingestion
// ---------------------------------------------------------------------------

fn motion_proposed() -> Event {
    Event {
        id: 11,
        event_type: event_type::MOTION_PROPOSED.to_string(),
        payload: json!({
            "motion_id": 7,
            "title": "Ban lead shot",
            "body": "congress",
            "meeting_id": 12,
            "proposed_by": "bea",
        }),
        source: "governance".into(),
        timestamp: "2026-09-20T18:00:00Z".parse().expect("a timestamp"),
    }
}

fn motion_passed() -> Event {
    let payload = MotionPassed {
        motion_id: 7,
        title: "Ban lead shot".into(),
        body: "congress".into(),
        meeting_id: Some(12),
        votes_yes: 12,
        votes_no: 4,
        votes_abstain: 1,
        threshold: "two_thirds".into(),
        passed_at: "2026-09-27T18:00:00Z".parse().expect("a timestamp"),
        amends_accords: false,
    };
    Event {
        id: 12,
        event_type: event_type::MOTION_PASSED.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        source: "governance".into(),
        timestamp: Utc::now(),
    }
}

fn motion_failed() -> Event {
    let payload = MotionFailed {
        motion_id: 9,
        title: "Move the meeting to Thursdays".into(),
        body: "tc".into(),
        meeting_id: None,
        votes_yes: 2,
        votes_no: 5,
        votes_abstain: 0,
        threshold: "simple_majority".into(),
        failed_at: "2026-09-28T18:00:00Z".parse().expect("a timestamp"),
        amends_accords: false,
    };
    Event {
        id: 13,
        event_type: event_type::MOTION_FAILED.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        source: "governance".into(),
        timestamp: Utc::now(),
    }
}

fn accords_adopted(version: i64) -> Event {
    Event {
        id: 20 + version,
        event_type: "accords.adopted".to_string(),
        payload: json!({
            "version": version,
            "title": "The Catamount Accords",
            "motion_id": 3,
            "adopted_on": "2026-07-04",
            "adopted_by": "bea",
        }),
        source: "governance".into(),
        timestamp: Utc::now(),
    }
}

fn mission_completed() -> Event {
    let payload = MissionCompleted {
        mission_id: 42,
        title: "Coyote survey".into(),
        lodge_id: Some("3".into()),
        stage: "report".into(),
        completed_at: "2026-09-28T20:15:00Z".parse().expect("a timestamp"),
        impact: json!({
            "service_hours": 18.5,
            "participant_count": 6,
            "summary": "Six scouts walked transect line 4.",
        }),
    };
    Event {
        id: 31,
        event_type: event_type::MISSION_COMPLETED.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        source: "missions".into(),
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn the_prefix_subscriptions_cover_the_m6_era_plugins() {
    let (_host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();
    let filters: Vec<&str> = subs.iter().map(|s| s.filter.as_str()).collect();
    assert_eq!(filters, vec!["motion.", "accords.", "mission."]);
    // A prefix filter is what makes one subscription cover the whole family…
    assert!(subs[0].matches(event_type::MOTION_PROPOSED));
    assert!(subs[0].matches(event_type::MOTION_PASSED));
    assert!(subs[0].matches(event_type::MOTION_FAILED));
    assert!(
        subs[0].matches("motion.amended"),
        "the future is covered too"
    );
    assert!(subs[1].matches("accords.adopted"));
    assert!(subs[2].matches(event_type::MISSION_COMPLETED));
    // …and what keeps them from swallowing each other's events.
    assert!(!subs[0].matches(event_type::MISSION_COMPLETED));
    assert!(!subs[2].matches(event_type::MOTION_PASSED));
}

#[tokio::test]
async fn a_proposed_motion_is_filed_once_and_a_replay_is_a_no_op() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    host.db.push_rows(vec![record_row(
        60,
        KIND_MOTION,
        "Ban lead shot",
        "2026-09-20T18:00:00Z",
        "troop",
        None,
    )]);
    (subs[0].handler)(motion_proposed()).await.unwrap();

    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|s| s.contains("INSERT INTO") && s.contains("records"))
        .unwrap();
    assert!(
        insert.contains("ON CONFLICT (source_ref) DO NOTHING"),
        "{insert}"
    );
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered.iter().any(|p| p.contains("governance:motion:7")),
        "the idempotency key is the reason a replay is safe: {rendered:?}"
    );
    assert!(rendered.iter().any(|p| p.contains(INGEST_ACTOR)));
    assert_audited(&host, "record.ingest.motion");
    host.events.assert_published("archive.record.filed");

    // The bus is a broadcast with replay: the redelivery finds the row already
    // there, the INSERT returns nothing, and the handler stops.
    host.events.published.lock().unwrap().clear();
    host.db.push_rows(vec![]);
    let queries_before = host.db.query_count();
    (subs[0].handler)(motion_proposed()).await.unwrap();
    assert_eq!(
        host.db.query_count(),
        queries_before + 1,
        "only the insert ran on the replay"
    );
    host.events.assert_none();
}

#[tokio::test]
async fn a_passed_motion_files_a_decision_and_links_it_to_the_motion() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    host.db.push_rows(vec![record_row(
        61,
        KIND_DECISION,
        "Ban lead shot",
        "2026-09-27T18:00:00Z",
        "troop",
        None,
    )]); // the decision record
    host.db.push_rows(vec![json!({ "id": 60 })]); // the motion it decides
    (subs[0].handler)(motion_passed()).await.unwrap();

    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("governance:motion:7:outcome")),
        "the decision is its own record, not an edit of the motion: {rendered:?}"
    );
    assert!(rendered.iter().any(|p| p.contains("decision")));
    assert!(rendered.iter().any(|p| p.contains(OUTCOME_PASSED)));
    let link = host
        .db
        .executed_sql()
        .into_iter()
        .find(|s| s.contains("INSERT INTO") && s.contains("links"))
        .expect("the outcome edge was drawn");
    assert!(link.contains("ON CONFLICT (from_id, to_id, relation) DO NOTHING"));
    let link_params = host.db.last_execute_params("links").unwrap();
    let rendered: Vec<String> = link_params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains(RELATION_OUTCOME)));
    assert!(
        rendered.iter().any(|p| p.contains("Int(60)")),
        "the motion is the parent end of the edge: {rendered:?}"
    );
    assert_audited(&host, "record.ingest.decision");

    // A failed motion is a decision too — the troop's record of what it
    // rejected is part of its history.
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
    let subs = plugin.subscriptions();
    host.db.push_rows(vec![record_row(
        62,
        KIND_DECISION,
        "Move the meeting to Thursdays",
        "2026-09-28T18:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![]); // the motion was never archived: a missing parent is not an error
    (subs[0].handler)(motion_failed()).await.unwrap();
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains(OUTCOME_FAILED)));
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("governance:motion:9:outcome")),
        "{rendered:?}"
    );
}

#[tokio::test]
async fn an_adopted_accords_version_amends_the_one_before_it() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    host.db.push_rows(vec![record_row(
        70,
        KIND_POLICY,
        "The Catamount Accords — version 3",
        "2026-07-04T00:00:00Z",
        "troop",
        None,
    )]);
    host.db.push_rows(vec![json!({ "id": 69 })]); // version 2 is archived
    (subs[1].handler)(accords_adopted(3)).await.unwrap();

    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains("governance:accords:3")));
    assert!(rendered.iter().any(|p| p.contains(OUTCOME_ADOPTED)));
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("The Catamount Accords — version 3")),
        "{rendered:?}"
    );
    let link_params = host.db.last_execute_params("links").unwrap();
    let rendered: Vec<String> = link_params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains(RELATION_AMENDS)));
    assert!(rendered.iter().any(|p| p.contains("Int(69)")));

    // Version 1 has nothing to amend.
    let host = TestHost::new();
    let mut plugin = ArchivePlugin::new();
    plugin.init(host.context("archive")).await.unwrap();
    let subs = plugin.subscriptions();
    host.db.push_rows(vec![record_row(
        71,
        KIND_POLICY,
        "The Catamount Accords — version 1",
        "2025-01-01T00:00:00Z",
        "troop",
        None,
    )]);
    (subs[1].handler)(accords_adopted(1)).await.unwrap();
    assert_eq!(
        host.db.query_count(),
        1,
        "no parent probe, no edge: version 1 has nothing to amend"
    );
}

#[tokio::test]
async fn a_completed_mission_files_its_report_and_its_impact_as_separate_records() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    host.db.push_rows(vec![record_row(
        80,
        KIND_MISSION_REPORT,
        "Coyote survey",
        "2026-09-28T20:15:00Z",
        "lodge",
        Some("3"),
    )]); // the report
    host.db.push_rows(vec![record_row(
        81,
        KIND_IMPACT,
        "Impact — Coyote survey",
        "2026-09-28T20:15:00Z",
        "lodge",
        Some("3"),
    )]); // the impact
    (subs[2].handler)(mission_completed()).await.unwrap();

    let inserts: Vec<String> = host
        .db
        .queried_sql()
        .into_iter()
        .filter(|s| s.contains("INSERT INTO") && s.contains("records"))
        .collect();
    assert_eq!(inserts.len(), 2, "a report and an impact: {inserts:?}");
    // The impact is the second records insert — the source_ref it carries is a
    // bind parameter, not part of the statement.
    let params = host
        .db
        .last_query_params("INSERT INTO")
        .expect("the impact insert ran");
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains("18.5")));
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("Lodge 3 minutes") || p.contains("transect line 4")),
        "the report's prose carries the mission's own summary: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|p| p.contains("lodge")),
        "a lodge mission's records are lodge-scoped: {rendered:?}"
    );
    let link = host
        .db
        .executed_sql()
        .into_iter()
        .find(|s| s.contains("links"))
        .expect("the produces edge was drawn");
    assert!(link.contains("ON CONFLICT"));
    let link_params = host.db.last_execute_params("links").unwrap();
    let rendered: Vec<String> = link_params.iter().map(|p| format!("{p:?}")).collect();
    assert!(rendered.iter().any(|p| p.contains(RELATION_PRODUCES)));
    assert_eq!(
        binding(&link_params[0]),
        "Int(80)",
        "the report is the from end"
    );
    assert_eq!(
        binding(&link_params[1]),
        "Int(81)",
        "the impact is the to end"
    );
    assert_audited(&host, "record.ingest.mission");
    host.events.assert_published("archive.record.filed");

    // A replay files nothing twice.
    host.events.published.lock().unwrap().clear();
    host.db.push_rows(vec![]);
    let queries_before = host.db.query_count();
    (subs[2].handler)(mission_completed()).await.unwrap();
    assert_eq!(host.db.query_count(), queries_before + 1);
    host.events.assert_none();
}

#[tokio::test]
async fn a_malformed_payload_is_reported_rather_than_guessed_at() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    for (index, malformed) in [
        (
            0,
            Event {
                id: 99,
                event_type: event_type::MOTION_PASSED.to_string(),
                payload: json!({ "nope": true }),
                source: "governance".into(),
                timestamp: Utc::now(),
            },
        ),
        (
            1,
            Event {
                id: 98,
                event_type: "accords.adopted".to_string(),
                payload: json!({ "title": "The Accords" }),
                source: "governance".into(),
                timestamp: Utc::now(),
            },
        ),
        (
            2,
            Event {
                id: 97,
                event_type: event_type::MISSION_COMPLETED.to_string(),
                payload: json!({ "mission_id": 42 }),
                source: "missions".into(),
                timestamp: Utc::now(),
            },
        ),
    ] {
        let error = (subs[index].handler)(malformed)
            .await
            .expect_err("a bad payload must not be guessed at");
        assert!(matches!(error, SdkError::BadRequest(_)), "{error}");
    }
    assert_eq!(host.db.query_count(), 0, "nothing was written");

    // A `motion.*` event the archive has no opinion about is a quiet no-op.
    let other = Event {
        id: 96,
        event_type: "motion.withdrawn".to_string(),
        payload: json!({ "motion_id": 7 }),
        source: "governance".into(),
        timestamp: Utc::now(),
    };
    (subs[0].handler)(other).await.unwrap();
    assert_eq!(host.db.query_count(), 0);
}

// ---------------------------------------------------------------------------
// The pure helpers (no database, no context)
// ---------------------------------------------------------------------------

#[test]
fn instants_round_trip_through_the_bound_form() {
    let instant = parse_instant("2026-09-14T18:00:00Z").unwrap();
    assert_eq!(render_instant(instant), "2026-09-14T18:00:00Z");
    // A bare wall clock is UTC; an offset is honoured.
    assert_eq!(
        parse_instant("2026-09-14T18:00").unwrap(),
        parse_instant("2026-09-14T18:00:00Z").unwrap()
    );
    assert_eq!(
        parse_instant("2026-09-14 18:00:00").unwrap(),
        parse_instant("2026-09-14T18:00:00Z").unwrap()
    );
    assert_eq!(
        parse_instant("2026-09-14T14:00:00-04:00").unwrap(),
        parse_instant("2026-09-14T18:00:00Z").unwrap()
    );
    // A date is midnight.
    assert_eq!(
        parse_instant("2026-09-14").unwrap(),
        parse_instant("2026-09-14T00:00:00Z").unwrap()
    );
    for bad in ["next tuesday", "", "2026-13-45"] {
        assert!(parse_instant(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn a_cursor_is_the_keyset_position_and_nothing_else() {
    let row = json!({ "id": 91, "occurred_at_rfc3339": "2026-03-02T18:00:00Z" });
    let cursor = cursor_of(&row).expect("a cursor");
    assert_eq!(cursor, "2026-03-02T18:00:00Z|91");
    let (at, id) = parse_cursor(&cursor).unwrap();
    assert_eq!(id, 91);
    assert_eq!(render_instant(at), "2026-03-02T18:00:00Z");
    for bad in ["91", "not-a-date|91", "2026-03-02T18:00:00Z|later"] {
        assert!(parse_cursor(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn the_edge_vocabulary_reads_as_english_in_the_direction_it_points() {
    let decision = json!({ "id": 12, "kind": KIND_DECISION, "title": "Ban lead shot" });
    let policy = json!({ "id": 20, "kind": KIND_POLICY, "title": "Lead-free ammunition" });
    assert_eq!(
        describe_edge(RELATION_DECIDES, &decision, &policy),
        "decision #12 \u{201c}Ban lead shot\u{201d} decided policy #20 \u{201c}Lead-free ammunition\u{201d}"
    );
    let motion = json!({ "id": 7, "kind": KIND_MOTION, "title": "Ban lead shot" });
    assert_eq!(
        describe_edge(RELATION_OUTCOME, &motion, &decision),
        "motion #7 \u{201c}Ban lead shot\u{201d} was decided by decision #12 \u{201c}Ban lead shot\u{201d}"
    );
    // A node without a title is still nameable, and an unknown relation still
    // produces a sentence rather than a panic.
    let bare = json!({ "id": 3, "kind": KIND_NOTE });
    assert_eq!(
        describe_edge(RELATION_RELATES_TO, &bare, &policy),
        "note #3 relates to policy #20 \u{201c}Lead-free ammunition\u{201d}"
    );
    assert_eq!(
        describe_edge("invented", &bare, &policy),
        "note #3 links to policy #20 \u{201c}Lead-free ammunition\u{201d}"
    );

    // The conventional endpoint kinds are declared for the four chain links.
    assert_eq!(
        relation_kinds(RELATION_DECIDES),
        (&[KIND_DECISION][..], &[KIND_POLICY][..])
    );
    assert_eq!(
        relation_kinds(RELATION_AUTHORIZES),
        (&[KIND_POLICY][..], &[KIND_MISSION_REPORT][..])
    );
    assert_eq!(
        relation_kinds(RELATION_PRODUCES),
        (&[KIND_MISSION_REPORT][..], &[KIND_IMPACT][..])
    );
    assert_eq!(relation_phrase(RELATION_AMENDS), "amends");
}

#[test]
fn scopes_and_labels_default_to_the_restrictive_reading() {
    assert_eq!(record_scope("lodge", Some("3")), Scope::lodge("3"));
    assert_eq!(record_scope("lodge", None), Scope::troop());
    assert_eq!(record_scope("lodge", Some("  ")), Scope::troop());
    assert_eq!(record_scope("invented", Some("3")), Scope::troop());
    let row = record_row(
        1,
        KIND_MINUTES,
        "Minutes",
        "2026-01-01T00:00:00Z",
        "lodge",
        Some("3"),
    );
    assert_eq!(scope_of(&row), Scope::lodge("3"));
    let mut current = row.clone();
    current["superseded_by"] = json!(99);
    assert!(!is_current(&current));
    assert!(is_current(&row));

    assert_eq!(kind_label(KIND_CONGRESS), "Congress proceedings");
    assert_eq!(kind_label("scrapbook"), "Record");
    assert_eq!(body_label("congress"), "the Congress");
    assert_eq!(body_label(""), "the troop");
    assert_eq!(
        impact_line(Some(18.5), Some(6)),
        "18.5 service hours, 6 participants"
    );
    assert_eq!(impact_line(None, None), "no metrics were reported");

    assert_eq!(KINDS.len(), 9);
    assert_eq!(
        CHAIN,
        [KIND_DECISION, KIND_POLICY, KIND_MISSION_REPORT, KIND_IMPACT]
    );
    assert_eq!(BODY_CODES.len(), 4);
    assert_eq!(RELATIONS.len(), 7);
    assert_eq!(SOURCE_MISSIONS, "missions");
}

#[tokio::test]
async fn the_vocabulary_route_needs_no_database_and_declares_the_chain() {
    let (host, _plugin, routes) = plugin().await;
    let vocabulary = route(&routes, "GET", "/api/archive/vocabulary");

    let (status, body) = call(
        &vocabulary.handler,
        TestRequest::get("/api/archive/vocabulary")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(host.db.query_count(), 0, "reference data, not a query");
    assert_eq!(
        body["chain"],
        json!([KIND_DECISION, KIND_POLICY, KIND_MISSION_REPORT, KIND_IMPACT])
    );
    assert_eq!(body["immutable"], json!(true));
    let relations: Vec<&str> = body["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["relation"].as_str().unwrap())
        .collect();
    assert!(relations.contains(&RELATION_DECIDES));
    assert!(relations.contains(&RELATION_PRODUCES));
    assert!(
        body["correction"].as_str().unwrap().contains("supersede"),
        "the vocabulary says how to correct a record: {body}"
    );
}
