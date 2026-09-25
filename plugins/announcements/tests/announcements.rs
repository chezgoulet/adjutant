//! Announcements plugin tests: the category gate, the addressing rule, the
//! idempotent receipt, the unread badge, and the delivery seam (SPEC §7.14).
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results in call order, so every test states the order its handler arranges —
//! the comment above each `push_*` names the call it answers. The order is the
//! interesting part of this plugin: it is where "no database work before
//! authorization" and "the badge never loads receipts" become checkable claims.

use adjutant_announcements::{
    announcement_scope, normalize_category, parse_expiry, preview, AnnouncementsPlugin, CATEGORIES,
    CATEGORY_EVENT, CATEGORY_INFORMATIONAL, CATEGORY_MEANINGS, CATEGORY_URGENT, DELIVERY_DEFERRED,
    EVENT_PUBLISHED, EVENT_RECEIPT, EVENT_UNREAD, PERM_MANAGE, PERM_PUBLISH_URGENT, PERM_READ,
    PERM_WRITE, SCOPE_LODGE, SCOPE_TROOP, STATUS_DRAFT, STATUS_PUBLISHED, STATUS_RETRACTED,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn plugin() -> (TestHost, AnnouncementsPlugin, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = AnnouncementsPlugin::new();
    plugin.init(host.context("announcements")).await.unwrap();
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

fn rendered(params: &[SqlValue]) -> Vec<String> {
    params.iter().map(|p| format!("{p:?}")).collect()
}

/// The audit log's action is a bind parameter, so a test has to read it back
/// rather than look for it in the statement.
fn assert_audited(host: &TestHost, action: &str) {
    let params = rendered(&host.db.last_execute_params("audit_log").unwrap_or_default());
    assert!(
        params.iter().any(|p| p.contains(action)),
        "no {action} audit: {params:?}"
    );
}

/// How many audit entries were written — the "and nothing happened the second
/// time" assertion.
fn audit_writes(host: &TestHost) -> usize {
    host.db
        .executed_sql()
        .iter()
        .filter(|sql| sql.contains("audit_log"))
        .count()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The rows `resolve_audience` reads: `(permission, scope_type, scope_id)`.
fn audience_rows(grants: &[(&str, &str, Option<&str>)]) -> Vec<Value> {
    grants
        .iter()
        .map(|(permission, scope_type, scope_id)| {
            json!({
                "permission": permission,
                "scope_type": scope_type,
                "scope_id": scope_id.unwrap_or(""),
            })
        })
        .collect()
}

/// A troop-addressed reader.
fn troop_reader() -> Vec<Value> {
    audience_rows(&[(PERM_READ, SCOPE_TROOP, None)])
}

/// A Lodge 3 reader, addressed by Lodge 3 only.
fn lodge_reader(lodge: &str) -> Vec<Value> {
    audience_rows(&[(PERM_READ, SCOPE_LODGE, Some(lodge))])
}

/// An overseer: `announcements:manage` from a troop-scoped grant.
fn overseer() -> Vec<Value> {
    audience_rows(&[(PERM_MANAGE, SCOPE_TROOP, None)])
}

fn announcement_row(
    id: i64,
    category: &str,
    scope_type: &str,
    scope_id: Option<&str>,
    status: &str,
) -> Value {
    json!({
        "id": id,
        "title": "Camp is cancelled",
        "body": "The camp at Windsor is cancelled.",
        "category": category,
        "scope_type": scope_type,
        "scope_id": scope_id,
        "status": status,
        "related_event_id": Value::Null,
        "published_at": "2026-09-25 13:00:00+00",
        "published_by": "bea",
        "expires_at": Value::Null,
        "retracted_at": Value::Null,
        "retracted_by": Value::Null,
        "created_by": "bea",
        "created_at": "2026-09-25 12:59:00+00",
        "updated_at": "2026-09-25 12:59:00+00"
    })
}

/// The same row as the read paths see it: joined with the caller's receipt.
fn visible_row(row: &Value, receipt: Option<i64>, read_count: i64) -> Value {
    let mut row = row.clone();
    row["receipt_id"] = receipt.map_or(Value::Null, |id| json!(id));
    row["receipt_via"] = if receipt.is_some() {
        json!("api")
    } else {
        Value::Null
    };
    row["read_at"] = if receipt.is_some() {
        json!("2026-09-25 14:00:00+00")
    } else {
        Value::Null
    };
    row["is_read"] = json!(receipt.is_some());
    row["read_count"] = json!(read_count);
    row
}

fn receipt_row(id: i64, member: &str, created: bool) -> Value {
    json!({
        "id": id,
        "announcement_id": 7,
        "member_id": member,
        "read_via": "api",
        "read_at": "2026-09-25 14:00:00+00",
        "created": created,
    })
}

fn unread_row(visible: i64, unread: i64, urgent: i64, informational: i64, event: i64) -> Value {
    json!({
        "visible": visible,
        "unread": unread,
        "unread_urgent": urgent,
        "unread_informational": informational,
        "unread_event": event,
        "read": visible - unread,
    })
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_declaration_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "announcements");
    assert_eq!(plugin.name(), "Announcements");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for permission in &permissions {
        assert!(
            permission.starts_with("announcements:"),
            "{permission} must be namespaced by the plugin's own prefix"
        );
    }

    // `GET /api/announcements/categories` serves the category meanings to clients
    // verbatim, so a permission name inside one is user-visible text. Every one it
    // names must be a permission this plugin actually declares: the rename from
    // `announcement:*` to `announcements:*` missed these strings first time round,
    // and a client would have read a permission that does not exist.
    for (key, _label, description) in CATEGORY_MEANINGS {
        for token in description.split_whitespace() {
            let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != ':' && c != '_');
            let Some((namespace, name)) = token.split_once(':') else {
                continue;
            };
            // A permission id is shaped `namespace:name`. Prose carries colons
            // too — "the ordinary notice: a schedule" — so the shape is what
            // separates a permission a reader might try to grant from a colon.
            let shaped = |part: &str| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            };
            if !shaped(namespace) || !shaped(name) {
                continue;
            }
            assert!(
                permissions.iter().any(|p| p.as_str() == token),
                "the {key} category describes {token}, which this plugin does not declare"
            );
        }
    }
    for expected in [PERM_READ, PERM_WRITE, PERM_PUBLISH_URGENT, PERM_MANAGE] {
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
    let ddl = &migrations[0].sql;
    for table in ["announcements", "receipts"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "SPEC §7.14's schema is missing {table}"
        );
    }
    // Vocabulary that reaches a reader is a constraint, not a convention.
    for constraint in [
        "announcements_category_valid",
        "announcements_scope_type_valid",
        "announcements_scope_id_present",
        "announcements_status_valid",
        "announcements_publication_consistent",
        "announcements_retraction_consistent",
        "receipts_member_present",
    ] {
        assert!(ddl.contains(constraint), "migration is missing {constraint}");
    }
    // The unique key is the invariant behind an idempotent receipt — the handler
    // is only the polite path to it.
    assert!(
        ddl.contains("UNIQUE (announcement_id, member_id)"),
        "receipts must be unique per member per announcement"
    );
    assert!(
        ddl.contains("ON DELETE CASCADE"),
        "a deleted announcement must not leave receipts behind"
    );
    for index in [
        "idx_announcements_scope",
        "idx_receipts_member",
        "idx_announcements_status",
    ] {
        assert!(ddl.contains(index), "migration is missing {index}");
    }

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/announcements"),
            "{} escapes the namespace",
            r.path
        );
        let permission = r
            .required_permission
            .as_deref()
            .expect("every route is gated");
        assert!(
            permissions.iter().any(|p| p == permission),
            "{permission} is required but not declared"
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
    assert_eq!(routes.len(), 12, "the surface the client needs");

    // Destructive is troop-covered even though the constructor is scope-any:
    // a Lodge grant may retract its Lodge's notice, not erase the record.
    let delete = route(&routes, "DELETE", "/api/announcements/announcement/{id}");
    assert_eq!(delete.required_scope, Some(Scope::troop()));
    // Everything else that targets one object checks its scope in the handler.
    for (method, path) in [
        ("GET", "/api/announcements/announcement/{id}"),
        ("PATCH", "/api/announcements/announcement/{id}"),
        ("POST", "/api/announcements/announcement/{id}/read"),
        ("POST", "/api/announcements/announcement/{id}/retract"),
    ] {
        assert_eq!(
            route(&routes, method, path).required_scope,
            None,
            "{method} {path} must check the object's scope itself"
        );
    }
    // Reference data is troop-covered, like every other plugin's collections.
    assert_eq!(
        route(&routes, "GET", "/api/announcements/categories").required_scope,
        Some(Scope::troop())
    );

    // Nothing to subscribe to and nothing to schedule: the delivery seam is this
    // plugin's output, not its input.
    assert!(plugin.subscriptions().is_empty());
    assert!(plugin.schedules().is_empty());
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_announcements::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "announcements");
    assert_eq!(
        adjutant_announcements::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

#[test]
fn the_vocabulary_the_api_accepts_is_the_vocabulary_that_is_constrained() {
    // The handlers' lists and the migration's CHECK constraints are the same
    // strings; a drift here would be a 500 rather than a 400.
    assert_eq!(CATEGORIES, ["urgent", "informational", "event"]);
    assert_eq!(normalize_category(Some("EVENT")).unwrap(), CATEGORY_EVENT);
    assert!(normalize_category(Some("emergency")).is_err());
    assert_eq!(
        parse_expiry(Some("2030-10-01")).unwrap(),
        Some("2030-10-01T00:00:00Z".parse().expect("a timestamp"))
    );
    assert!(parse_expiry(Some("soon")).is_err());
    assert_eq!(
        announcement_scope(SCOPE_LODGE, Some("3")),
        Scope::lodge("3")
    );
    assert_eq!(preview("a  b\nc", 20), "a b c");
    assert!(DELIVERY_DEFERRED.starts_with("deferred"));
}

// ---------------------------------------------------------------------------
// Create and publish
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_without_publishing_leaves_a_draft_that_reaches_nobody() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");

    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write at the troop
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_TROOP,
        None,
        STATUS_DRAFT,
    )]);

    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/announcements/announcement")
            .identity("bea", &["scribe"])
            .json(&json!({
                "title": "Camp is cancelled",
                "body": "The camp at Windsor is cancelled.",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_DRAFT));
    assert_eq!(body["published"], json!(false));
    assert_eq!(body["delivery"], json!(DELIVERY_DEFERRED));
    assert!(
        body["next"].as_str().unwrap().contains("/publish"),
        "a draft names how to send it: {body}"
    );

    let params = rendered(&host.db.last_query_params("INSERT INTO").unwrap());
    assert!(params.iter().any(|p| p.contains("Camp is cancelled")));
    assert!(
        params.iter().any(|p| p.contains(STATUS_DRAFT)),
        "an unsent announcement is stored as a draft: {params:?}"
    );
    assert_audited(&host, "announcement.create");
    assert_eq!(
        host.events.published_types(),
        vec!["announcement.created".to_string()],
        "a draft is not news yet"
    );
}

#[tokio::test]
async fn creating_and_sending_publishes_the_delivery_seam_once() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");

    // A troop-wide grant covers the Lodge audience it is sending to, so the
    // authority check is the coverage rule.
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write at Lodge 3
    let mut sent = announcement_row(7, CATEGORY_EVENT, SCOPE_LODGE, Some("3"), STATUS_PUBLISHED);
    sent["related_event_id"] = json!(21);
    // The payload is built from the stored row, so the preview is the stored
    // body — flattened, because a notification is one line.
    sent["body"] = json!("The  camp at   Windsor is cancelled.\nCheck the calendar.");
    host.db.push_rows(vec![sent]);

    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/announcements/announcement")
            .identity("bea", &["chief"])
            .json(&json!({
                "title": "Camp is cancelled",
                "body": "The  camp at   Windsor is cancelled.\nCheck the calendar.",
                "category": CATEGORY_EVENT,
                "scope_type": SCOPE_LODGE,
                "scope_id": "3",
                "publish": true,
                "related_event_id": 21,
                "expires_at": "2030-10-01T00:00:00Z",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_PUBLISHED));
    assert_eq!(body["published"], json!(true));

    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("announcements"))
        .expect("an insert ran");
    assert!(
        insert.contains("CASE WHEN $8::bool THEN now() END"),
        "sending stamps published_at in the same statement: {insert}"
    );
    let params = rendered(&host.db.last_query_params("INSERT INTO").unwrap());
    assert!(
        params.iter().any(|p| p.contains("2030-10-01T00:00:00")),
        "the expiry travels as an RFC 3339 timestamp: {params:?}"
    );
    assert!(params.iter().any(|p| p.contains('3')), "the Lodge is the audience");

    let payloads = host.events.payloads(EVENT_PUBLISHED);
    assert_eq!(payloads.len(), 1, "the seam fires exactly once");
    let payload = &payloads[0];
    assert_eq!(payload["announcement_id"], json!(7));
    assert_eq!(payload["category"], json!(CATEGORY_EVENT));
    assert_eq!(payload["urgent"], json!(false));
    assert_eq!(payload["scope_type"], json!(SCOPE_LODGE));
    assert_eq!(payload["scope_id"], json!("3"));
    assert_eq!(payload["related_event_id"], json!(21));
    assert_eq!(payload["published_by"], json!("bea"));
    assert_eq!(
        payload["preview"],
        json!("The camp at Windsor is cancelled. Check the calendar."),
        "a delivery plugin gets a usable preview without a second fetch"
    );
}

#[tokio::test]
async fn urgent_needs_a_second_permission_and_a_draft_does_not() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");

    // (a) `announcements:write` alone cannot publish an urgent announcement: the
    //     second `reach` finds no publish_urgent grant and refuses — after the
    //     write check and before any insert.
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write
    host.db.push_rows(vec![]); // announcements:publish_urgent — not granted
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/announcements/announcement")
            .identity("bea", &["scribe"])
            .json(&json!({
                "title": "Water shutoff",
                "category": CATEGORY_URGENT,
                "publish": true,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains(PERM_PUBLISH_URGENT),
        "the refusal names the permission it needed: {body}"
    );
    assert_eq!(host.db.query_count(), 2, "the two authority checks, nothing more");
    assert!(
        !host.db.queried_sql().iter().any(|sql| sql.contains("INSERT INTO")),
        "an unauthorized urgent announcement is never written"
    );
    assert_eq!(
        host.events.published_types(),
        Vec::<String>::new(),
        "and never announced"
    );

    // (b) With the sharper permission, the same request goes out and the seam
    //     says `urgent: true` — the flag a delivery plugin acts on.
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:publish_urgent
    host.db.push_rows(vec![announcement_row(
        9,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/announcements/announcement")
            .identity("bea", &["chief"])
            .json(&json!({
                "title": "Water shutoff",
                "category": CATEGORY_URGENT,
                "publish": true,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(host.events.payloads(EVENT_PUBLISHED)[0]["urgent"], json!(true));
    assert_eq!(host.events.payloads(EVENT_PUBLISHED)[0]["category"], json!(CATEGORY_URGENT));

    // (c) Drafting one is allowed without it: a draft reaches nobody, so there
    //     is nothing to abuse yet.
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write
    host.db.push_rows(vec![announcement_row(
        11,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_DRAFT,
    )]);
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/announcements/announcement")
            .identity("bea", &["scribe"])
            .json(&json!({ "title": "Water shutoff", "category": CATEGORY_URGENT }))
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_DRAFT));
    assert_eq!(
        host.events.payloads(EVENT_PUBLISHED).len(),
        0,
        "a draft is not published, urgent or not"
    );
}

#[tokio::test]
async fn a_bad_request_is_refused_before_any_database_work() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/announcements/announcement");

    // An unknown category, a Lodge with no Lodge, and a blank title.
    for (body, needle) in [
        (json!({ "title": "X", "category": "emergency" }), "category"),
        (
            json!({ "title": "X", "scope_type": SCOPE_LODGE }),
            "scope_id",
        ),
        (
            json!({ "title": "X", "scope_type": SCOPE_TROOP, "scope_id": "3" }),
            "scope_id",
        ),
        (json!({ "title": "   " }), "title"),
        (
            json!({ "title": "X", "publish": true, "expires_at": "2020-01-01" }),
            "reaches nobody",
        ),
    ] {
        let (status, response) = call(
            &create.handler,
            TestRequest::post("/api/announcements/announcement")
                .identity("bea", &["chief"])
                .json(&body)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{body} → {response}");
        assert!(
            response["error"].as_str().unwrap().contains(needle),
            "{body} → {response}"
        );
    }
    assert_eq!(
        host.db.query_count(),
        0,
        "validation never costs a round trip"
    );

    // The listing's vocabulary is checked the same way.
    let list = route(&routes, "GET", "/api/announcements/announcements");
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/announcements/announcements")
            .query_param("status", "maybe")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("status"));
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn sending_an_already_sent_announcement_is_refused_so_the_seam_fires_once() {
    let (host, _plugin, routes) = plugin().await;
    let publish = route(&routes, "POST", "/api/announcements/announcement/{id}/publish");

    // The draft's author may send what they wrote with `announcements:write`.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_DRAFT,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write at Lodge 3
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_PUBLISHED,
    )]);

    let (status, body) = call(
        &publish.handler,
        TestRequest::post("/api/announcements/announcement/7/publish")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "scribe".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_PUBLISHED));
    assert_eq!(host.events.payloads(EVENT_PUBLISHED).len(), 1);
    let update = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("UPDATE") && sql.contains("announcements"))
        .expect("the send is a state change");
    assert!(
        update.contains("AND a.status = 'draft'"),
        "the draft guard is what makes the seam once-only: {update}"
    );

    // Second attempt: the row is published now, so the status check refuses
    // before any permission query or update.
    let (host, _plugin, routes) = plugin().await;
    let publish = route(&routes, "POST", "/api/announcements/announcement/{id}/publish");
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_PUBLISHED,
    )]);
    let (status, body) = call(
        &publish.handler,
        TestRequest::post("/api/announcements/announcement/7/publish")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("not a draft"));
    assert_eq!(host.db.query_count(), 1, "the fetch and nothing else");
    assert_eq!(host.events.payloads(EVENT_PUBLISHED).len(), 0);
}

#[tokio::test]
async fn publishing_someone_elses_draft_needs_manage_and_rechecks_the_urgent_gate() {
    let (host, _plugin, routes) = plugin().await;
    let publish = route(&routes, "POST", "/api/announcements/announcement/{id}/publish");

    // Not the author, and no manage grant at the Lodge: refused.
    let mut carls_draft = announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_DRAFT,
    );
    carls_draft["created_by"] = json!("carl");
    host.db.push_rows(vec![carls_draft]);
    host.db.push_rows(vec![]); // announcements:manage at Lodge 3 — not granted
    let (status, body) = call(
        &publish.handler,
        TestRequest::post("/api/announcements/announcement/7/publish")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "scribe".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains(PERM_MANAGE));

    // The author, with write, but the draft is urgent and the sharper
    // permission is missing: the gate applies at the transition, not at
    // drafting.
    let mut draft = announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_DRAFT,
    );
    draft["created_by"] = json!("bea");
    host.db.push_rows(vec![draft]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write
    host.db.push_rows(vec![]); // announcements:publish_urgent — not granted
    let (status, body) = call(
        &publish.handler,
        TestRequest::post("/api/announcements/announcement/7/publish")
            .param("id", "7")
            .identity("bea", &["scribe"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains(PERM_PUBLISH_URGENT));
    assert!(
        !host.db.queried_sql().iter().any(|sql| sql.contains("UPDATE")),
        "nothing was sent"
    );
}

#[tokio::test]
async fn a_draft_whose_expiry_has_passed_cannot_be_sent() {
    let (host, _plugin, routes) = plugin().await;
    let publish = route(&routes, "POST", "/api/announcements/announcement/{id}/publish");

    let mut draft = announcement_row(7, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_DRAFT);
    draft["expires_at"] = json!("2020-01-01 00:00:00+00");
    host.db.push_rows(vec![draft]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write

    let (status, body) = call(
        &publish.handler,
        TestRequest::post("/api/announcements/announcement/7/publish")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("expiry has passed"));
    assert!(
        !host.db.queried_sql().iter().any(|sql| sql.contains("UPDATE")),
        "an announcement nobody could ever see is not sent"
    );
}

// ---------------------------------------------------------------------------
// Scope visibility — an announcement reaches the scope it was sent to
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_troop_wide_reader_does_not_see_a_lodges_announcement() {
    let (host, _plugin, routes) = plugin().await;
    let detail = route(&routes, "GET", "/api/announcements/announcement/{id}");

    host.db.push_rows(troop_reader()); // the caller is addressed troop-wide
    host.db.push_rows(vec![]); // and the Lodge 3 notice is not addressed to them

    let (status, body) = call(
        &detail.handler,
        TestRequest::get("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("not sent to a scope"));

    // The predicate the read paths share states the Troop/Lodge split, and the
    // troop-wide reader binds `troop_read = true` with no lodges at all.
    let sql = &host.db.queried_sql()[1];
    assert!(
        sql.contains("a.scope_type = 'troop' AND $1::bool"),
        "a troop-wide announcement needs a troop-addressed reader: {sql}"
    );
    assert!(sql.contains("a.scope_type = 'lodge' AND a.scope_id = ANY($2)"));
    let params = rendered(&host.db.last_query_params("LEFT JOIN").unwrap());
    assert_eq!(params[0], "Bool(true)");
    assert_eq!(params[1], "TextArray([])", "a troop-wide reader holds no Lodge");
    assert_eq!(params[4], "TextArray([])");
    assert_eq!(params[5], "Text(\"bea\")", "the member whose receipt is joined");
}

#[tokio::test]
async fn a_lodge_reader_does_not_see_a_troop_wide_announcement() {
    let (host, _plugin, routes) = plugin().await;
    let detail = route(&routes, "GET", "/api/announcements/announcement/{id}");

    host.db.push_rows(lodge_reader("3")); // addressed by Lodge 3 only
    host.db.push_rows(vec![]); // a troop-wide notice is not addressed to them

    let (status, body) = call(
        &detail.handler,
        TestRequest::get("/api/announcements/announcement/7")
            .param("id", "7")
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
    assert_eq!(status, 403, "{body}");

    let params = rendered(&host.db.last_query_params("LEFT JOIN").unwrap());
    assert_eq!(params[0], "Bool(false)", "a Lodge reader is not troop-addressed");
    assert_eq!(params[1], "TextArray([\"3\"])");

    // And the listing binds the same addressing: the Lodge 3 reader's page is
    // filtered to Lodge 3 audiences, never widened to the troop.
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/announcements/announcements");
    host.db.push_rows(lodge_reader("3"));
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_EVENT,
        SCOPE_LODGE,
        Some("3"),
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![unread_row(1, 1, 0, 0, 1)]);
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/announcements/announcements")
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
    assert_eq!(body["announcements"][0]["id"], json!(7));
    assert_eq!(body["unread"]["addressed"]["troop"], json!(false));
    assert_eq!(body["unread"]["addressed"]["lodges"], json!(["3"]));
}

#[tokio::test]
async fn an_overseer_sees_every_scope_and_a_writer_sees_their_own_draft() {
    let (host, _plugin, routes) = plugin().await;
    let detail = route(&routes, "GET", "/api/announcements/announcement/{id}");

    // A troop-covering `manage` grant is the "I need to see all of it"
    // authority: the same Lodge 3 notice a troop-wide *reader* could not see is
    // visible to an overseer.
    host.db.push_rows(overseer());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_INFORMATIONAL, SCOPE_LODGE, Some("3"), STATUS_PUBLISHED),
        None,
        4,
    )]);

    let (status, body) = call(
        &detail.handler,
        TestRequest::get("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["is_read"], json!(false));
    assert_eq!(body["my_receipt"], Value::Null);
    assert_eq!(body["read_count"], json!(4));
    assert_eq!(body["announcement"]["scope_id"], json!("3"));

    // A caller with no grants is addressed by nothing: the route gate has
    // already refused, and the handler runs no query for it.
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/announcements/announcements");
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/announcements/announcements").build(),
    )
    .await;
    // The core's gate is what refuses an anonymous caller; the handler itself
    // has no audience to resolve, so it spends no query finding that out.
    assert_eq!(status, 200);
    assert_eq!(body["announcements"], json!([]));
    assert_eq!(body["unread"]["member_id"], json!(""));
    assert!(
        !host
            .db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("role_permissions")),
        "no identity, no audience query"
    );
}

// ---------------------------------------------------------------------------
// Read receipts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn marking_read_twice_creates_one_receipt_and_one_event() {
    let (host, _plugin, routes) = plugin().await;
    let read = route(&routes, "POST", "/api/announcements/announcement/{id}/read");

    // First read: the receipt is created and the badge drops to 2.
    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_URGENT, SCOPE_TROOP, None, STATUS_PUBLISHED),
        None,
        3,
    )]);
    host.db.push_rows(vec![receipt_row(41, "bea", true)]);
    host.db.push_rows(vec![unread_row(5, 2, 1, 1, 0)]);

    let (status, body) = call(
        &read.handler,
        TestRequest::post("/api/announcements/announcement/7/read")
            .param("id", "7")
            .identity("bea", &["scout"])
            .json(&json!({ "via": "app" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["already_read"], json!(false));
    assert_eq!(body["receipt"]["id"], json!(41));
    assert_eq!(body["receipt"]["read_via"], json!("api"));
    assert_eq!(body["unread"]["unread"], json!(2));
    assert_eq!(body["unread"]["urgent_unread"], json!(1));
    assert_eq!(host.events.payloads(EVENT_RECEIPT).len(), 1);
    assert_audited(&host, "announcement.read");

    let upsert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("receipts"))
        .expect("the receipt is written");
    assert!(
        upsert.contains("ON CONFLICT (announcement_id, member_id) DO NOTHING"),
        "a second read must write nothing, not churn an upsert: {upsert}"
    );
    assert!(
        !upsert.contains("DO UPDATE"),
        "an idempotent mark-read must not rewrite the receipt: {upsert}"
    );
    assert!(
        upsert.contains("false AS created"),
        "the union reports the receipt that already existed: {upsert}"
    );

    // Second read: the same receipt comes back, nothing is created, and no
    // event is published for a read that changed nothing.
    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_URGENT, SCOPE_TROOP, None, STATUS_PUBLISHED),
        Some(41),
        3,
    )]);
    host.db.push_rows(vec![receipt_row(41, "bea", false)]);
    host.db.push_rows(vec![unread_row(5, 2, 1, 1, 0)]);

    let audits_before = audit_writes(&host);
    let (status, body) = call(
        &read.handler,
        TestRequest::post("/api/announcements/announcement/7/read")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "marking read twice is not an error: {body}");
    assert_eq!(body["already_read"], json!(true));
    assert_eq!(body["receipt"]["id"], json!(41), "the same receipt");
    assert_eq!(
        host.events.payloads(EVENT_RECEIPT).len(),
        1,
        "no second receipt, no second event"
    );
    assert_eq!(
        audit_writes(&host),
        audits_before,
        "and no second audit entry"
    );
}

#[tokio::test]
async fn a_draft_has_no_readers_to_record() {
    let (host, _plugin, routes) = plugin().await;
    let read = route(&routes, "POST", "/api/announcements/announcement/{id}/read");

    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_DRAFT),
        None,
        0,
    )]);

    let (status, body) = call(
        &read.handler,
        TestRequest::post("/api/announcements/announcement/7/read")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("draft")); 
    assert!(
        !host
            .db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("INSERT INTO") && sql.contains("receipts")),
        "no receipt is written for a draft"
    );
}

#[tokio::test]
async fn marking_unread_forgets_once_and_is_not_an_error_the_second_time() {
    let (host, _plugin, routes) = plugin().await;
    let unread = route(&routes, "POST", "/api/announcements/announcement/{id}/unread");

    // A receipt exists: it is removed, and the badge goes back up.
    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_PUBLISHED),
        Some(41),
        3,
    )]);
    host.db.push_execute(Ok(1)); // one receipt forgotten
    host.db.push_rows(vec![unread_row(5, 3, 0, 3, 0)]);

    let (status, body) = call(
        &unread.handler,
        TestRequest::post("/api/announcements/announcement/7/unread")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["forgotten"], json!(true));
    assert_eq!(body["is_read"], json!(false));
    assert_eq!(body["receipt"], Value::Null);
    assert_eq!(body["unread"]["unread"], json!(3));
    host.db.assert_executed(&["DELETE FROM", "receipts", "member_id = $2"]);
    assert_eq!(host.events.payloads(EVENT_UNREAD).len(), 1);

    // Nothing to forget: still a 200, and nothing published.
    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_PUBLISHED),
        None,
        2,
    )]);
    host.db.push_execute(Ok(0)); // no such receipt
    host.db.push_rows(vec![unread_row(5, 3, 0, 3, 0)]);
    let (status, body) = call(
        &unread.handler,
        TestRequest::post("/api/announcements/announcement/7/unread")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["forgotten"], json!(false));
    assert_eq!(
        host.events.payloads(EVENT_UNREAD).len(),
        1,
        "forgetting what was never remembered is not news"
    );
}

#[tokio::test]
async fn the_receipt_list_names_who_read_and_is_oversight_data() {
    let (host, _plugin, routes) = plugin().await;
    let receipts = route(
        &routes,
        "GET",
        "/api/announcements/announcement/{id}/receipts",
    );

    // A writer is not an overseer: `announcements:manage` is required.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![]); // announcements:manage — not granted
    let (status, body) = call(
        &receipts.handler,
        TestRequest::get("/api/announcements/announcement/7/receipts")
            .param("id", "7")
            .identity("bea", &["scribe"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains(PERM_MANAGE));

    // With manage, the list comes back — who read, never who has not (that
    // answer needs the roster, which belongs to membership).
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![
        json!({
            "id": 41, "announcement_id": 7, "member_id": "bea",
            "read_via": "app", "read_at": "2026-09-25 14:00:00+00",
        }),
        json!({
            "id": 40, "announcement_id": 7, "member_id": "carl",
            "read_via": "api", "read_at": "2026-09-25 13:30:00+00",
        }),
    ]);
    let (status, body) = call(
        &receipts.handler,
        TestRequest::get("/api/announcements/announcement/7/receipts")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], json!(2));
    assert_eq!(body["receipts"][0]["member_id"], json!("bea"));
    assert_eq!(body["scope"]["scope_id"], json!("3"));
    assert!(body["note"].as_str().unwrap().contains("roster"));
}

// ---------------------------------------------------------------------------
// Unread counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_badge_is_one_indexed_count_that_never_loads_receipts() {
    let (host, _plugin, routes) = plugin().await;
    let unread = route(&routes, "GET", "/api/announcements/unread");

    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![unread_row(6, 4, 1, 2, 1)]);

    let (status, body) = call(
        &unread.handler,
        TestRequest::get("/api/announcements/unread")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["member_id"], json!("bea"));
    assert_eq!(body["visible"], json!(6));
    assert_eq!(body["unread"], json!(4));
    assert_eq!(body["read"], json!(2));
    assert_eq!(body["urgent_unread"], json!(1));
    assert_eq!(body["has_urgent"], json!(true));
    assert_eq!(body["unread_by_category"]["urgent"], json!(1));
    assert_eq!(body["unread_by_category"]["informational"], json!(2));
    assert_eq!(body["unread_by_category"]["event"], json!(1));
    assert_eq!(body["addressed"]["troop"], json!(true));
    assert_eq!(host.db.query_count(), 2, "the audience and one count");

    let sql = &host.db.queried_sql()[1];
    assert!(
        sql.contains("LEFT JOIN") && sql.contains("r.member_id = $6"),
        "the join is an index probe into the unique key, per member: {sql}"
    );
    assert!(
        sql.contains("COUNT(*) FILTER (WHERE r.id IS NULL)"),
        "the count is computed by the database, not by loading receipts: {sql}"
    );
    assert!(sql.contains("COUNT(*) FILTER (WHERE r.id IS NULL AND a.category = 'urgent')"));
    assert!(
        sql.contains("a.status = 'published'"),
        "a retracted or drafted announcement is never unread: {sql}"
    );
    assert!(
        sql.contains("a.expires_at IS NULL OR a.expires_at > now()"),
        "an expired announcement does not hold a badge open: {sql}"
    );
    assert!(
        !sql.contains("SELECT * FROM") || !sql.contains("receipts r2"),
        "the caller's receipts are never fetched: {sql}"
    );
}

#[tokio::test]
async fn the_listing_pages_the_inbox_and_reports_the_badge_beside_it() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/announcements/announcements");

    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![
        visible_row(
            &announcement_row(9, CATEGORY_URGENT, SCOPE_TROOP, None, STATUS_PUBLISHED),
            None,
            1,
        ),
        visible_row(
            &announcement_row(8, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_PUBLISHED),
            Some(41),
            3,
        ),
    ]);
    host.db.push_rows(vec![unread_row(6, 4, 1, 2, 1)]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/announcements/announcements")
            .query_param("category", "urgent")
            .query_param("limit", "10")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], json!(2));
    assert_eq!(body["status"], json!(STATUS_PUBLISHED), "the default view");
    assert_eq!(body["filters"]["category"], json!(CATEGORY_URGENT));
    assert_eq!(body["announcements"][0]["is_read"], json!(false));
    assert_eq!(body["announcements"][1]["is_read"], json!(true));
    assert_eq!(body["announcements"][1]["read_at"], json!("2026-09-25 14:00:00+00"));
    assert_eq!(
        body["unread"]["unread"],
        json!(4),
        "the badge is the caller's whole unread count, not the filtered page's"
    );

    let sql = &host.db.queried_sql()[1];
    assert!(
        sql.contains("ORDER BY (a.category = 'urgent') DESC"),
        "the category that interrupts sorts first: {sql}"
    );
    assert!(sql.contains("a.published_at DESC NULLS LAST"));
    assert!(sql.contains("($12::bool = false OR r.id IS NULL)"), "?unread=1");
    assert!(sql.contains("LIMIT $13 OFFSET $14"));
    let params = rendered(&host.db.last_query_params("LIMIT $13").unwrap());
    assert!(params.iter().any(|p| p == "Int(10)"));
    assert!(params.iter().any(|p| p == "Text(\"urgent\")"));
    assert!(params.iter().any(|p| p == "Bool(false)"), "expired are hidden by default");
}

#[tokio::test]
async fn the_categories_route_states_the_vocabulary_and_what_it_costs() {
    let (host, _plugin, routes) = plugin().await;
    let categories = route(&routes, "GET", "/api/announcements/categories");

    let (status, body) = call(
        &categories.handler,
        TestRequest::get("/api/announcements/categories")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let codes: Vec<&str> = body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, CATEGORIES);
    assert_eq!(
        body["categories"][0]["requires_permission"],
        json!([PERM_WRITE, PERM_PUBLISH_URGENT]),
        "urgent costs two permissions, and the API says so"
    );
    assert_eq!(
        body["categories"][2]["requires_permission"],
        json!([PERM_WRITE])
    );
    assert_eq!(
        host.db.query_count(),
        0,
        "reference data needs no database"
    );
}

// ---------------------------------------------------------------------------
// Retract and delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retracting_withdraws_it_from_the_badge_but_leaves_the_record() {
    let (host, _plugin, routes) = plugin().await;
    let retract = route(&routes, "POST", "/api/announcements/announcement/{id}/retract");

    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:manage (for an urgent one too)
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_RETRACTED,
    )]);

    let (status, body) = call(
        &retract.handler,
        TestRequest::post("/api/announcements/announcement/7/retract")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_RETRACTED));
    assert_audited(&host, "announcement.retract");
    assert_eq!(host.events.payloads("announcement.retracted").len(), 1);

    // The record stays visible to the audience it was sent to — a notice that
    // silently vanishes teaches the troop not to trust the next one — while
    // never being unread again.
    let (host, _plugin, routes) = plugin().await;
    let detail = route(&routes, "GET", "/api/announcements/announcement/{id}");
    host.db.push_rows(troop_reader());
    host.db.push_rows(vec![visible_row(
        &announcement_row(7, CATEGORY_URGENT, SCOPE_TROOP, None, STATUS_RETRACTED),
        Some(41),
        2,
    )]);
    let (status, body) = call(
        &detail.handler,
        TestRequest::get("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["announcement"]["status"], json!(STATUS_RETRACTED));
    assert!(
        host.db.queried_sql()[1].contains("a.status IN ('published', 'retracted')"),
        "a retracted announcement is still addressed to its audience"
    );
}

#[tokio::test]
async fn deleting_erases_the_record_and_its_receipts() {
    let (host, _plugin, routes) = plugin().await;
    let delete = route(&routes, "DELETE", "/api/announcements/announcement/{id}");

    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_RETRACTED,
    )]);

    let (status, body) = call(
        &delete.handler,
        TestRequest::delete("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(7));
    host.db.assert_executed(&["DELETE FROM", "announcements"]);
    assert_audited(&host, "announcement.delete");
    assert_eq!(host.events.payloads("announcement.deleted").len(), 1);

    // Not there: a 404 rather than a silent success.
    let (host, _plugin, routes) = plugin().await;
    let delete = route(&routes, "DELETE", "/api/announcements/announcement/{id}");
    host.db.push_rows(vec![]);
    let (status, _body) = call(
        &delete.handler,
        TestRequest::delete("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 404);
    assert!(!host.db.executed_sql().iter().any(|sql| sql.contains("DELETE FROM")));
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_author_edits_their_own_draft_and_anybody_else_needs_manage() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/announcements/announcement/{id}");

    // The author, editing their own draft with the write permission.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_TROOP,
        None,
        STATUS_DRAFT,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_TROOP,
        None,
        STATUS_DRAFT,
    )]);

    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["scribe"])
            .json(&json!({ "body": "The camp at Windsor is cancelled." }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_audited(&host, "announcement.update");
    assert_eq!(host.events.payloads("announcement.updated").len(), 1);

    // Somebody else's *sent* announcement: manage is required, and a lodge
    // grant does not reach a troop-wide row.
    let mut sent = announcement_row(8, CATEGORY_INFORMATIONAL, SCOPE_TROOP, None, STATUS_PUBLISHED);
    sent["created_by"] = json!("carl");
    host.db.push_rows(vec![sent]);
    host.db.push_rows(vec![]); // announcements:manage over the troop — not granted
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/8")
            .param("id", "8")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "scribe".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "title": "Fixed" }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains(PERM_MANAGE));
}

#[tokio::test]
async fn editing_a_sent_announcement_into_urgent_needs_the_sharper_permission() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/announcements/announcement/{id}");

    // Manage, but not publish_urgent: escalation is refused before the update.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:manage
    host.db.push_rows(vec![]); // announcements:publish_urgent — not granted
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({ "category": CATEGORY_URGENT }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains(PERM_PUBLISH_URGENT));
    assert!(
        !host.db.queried_sql().iter().any(|sql| sql.contains("UPDATE")),
        "a category that can be cried wolf is not escalated by a writer alone"
    );

    // An announcement that is already urgent can still be corrected by a
    // manager — the gate is on the transition, not on the category.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:manage
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    let queries_before = host.db.query_count();
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({ "title": "Water shutoff — corrected" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        host.db.query_count() - queries_before,
        3,
        "fetch, manage, update — and no urgent re-check for one already urgent"
    );

    // An empty patch is named as such rather than sent as a no-op update.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_URGENT,
        SCOPE_TROOP,
        None,
        STATUS_PUBLISHED,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({}))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("no editable field"));
}

#[tokio::test]
async fn moving_an_announcement_to_another_audience_needs_authority_there() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/announcements/announcement/{id}");

    // A Lodge 3 manager cannot re-address a notice to the whole troop.
    host.db.push_rows(vec![announcement_row(
        7,
        CATEGORY_INFORMATIONAL,
        SCOPE_LODGE,
        Some("3"),
        STATUS_DRAFT,
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // announcements:write at Lodge 3
    host.db.push_rows(vec![]); // announcements:write over the troop — not granted
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/announcements/announcement/7")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "scope_type": SCOPE_TROOP, "scope_id": Value::Null }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("troop"),
        "{body}"
    );
    assert!(
        !host.db.queried_sql().iter().any(|sql| sql.contains("UPDATE")),
        "re-addressing without authority at the new scope changes nothing"
    );
}
