//! MCP plugin tests: the filtered tool catalogue, the per-invocation permission
//! check, the downstream call, and the audit trail (SPEC §7.10).
//!
//! Handlers are driven through `adjutant_sdk::testing` — no database, no server.
//! The mock replays rows in call order, so each test names the **query order**
//! it arranges around. The catalogue checks one permission per distinct
//! (permission, scope) pair, in catalogue order, which is what
//! [`permission_check_order`] documents; a troop-wide identity also needs a row
//! for each check, a scoped one only for the checks it can reach (`has_in_scope`
//! answers `false` without a query when no grant covers the scope).

use adjutant_mcp::{
    builtin_tools, hash_token, McpPlugin, ToolScope, DEFAULT_BASE_URL, MCP_PROTOCOL_VERSION,
    PERM_AUDIT, PERM_CONNECT, PERM_INVOKE,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use adjutant_sdk::RouteHandler;
use serde_json::{json, Value};

// --- harness ----------------------------------------------------------------

async fn plugin_routes_with(config: Value) -> (TestHost, Vec<RouteDefinition>) {
    let host = TestHost::new().with_config(config);
    let mut plugin = McpPlugin::new();
    plugin.init(host.context("mcp")).await.unwrap();
    let routes = plugin.routes();
    (host, routes)
}

async fn plugin_routes() -> (TestHost, Vec<RouteDefinition>) {
    plugin_routes_with(json!({ "base_url": "http://adjutant.test:9000" })).await
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

/// Drive a handler and report `(status, body)`.
///
/// A handler may refuse two ways — an `SdkError` (mapped through
/// `SdkError::status()`) or a `PluginResponse::error` — and from a client they
/// are the same answer. This collapses both.
async fn call(handler: &RouteHandler, req: PluginRequest) -> (u16, Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// The order the catalogue checks permissions in: one check per distinct
/// (permission, scope) pair, in catalogue order.
fn permission_check_order() -> Vec<(String, ToolScope)> {
    let mut seen: Vec<(String, ToolScope)> = Vec::new();
    for tool in builtin_tools() {
        let key = (tool.permission.clone(), tool.scope);
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen
}

/// Queue one permission row per check a **troop-wide** identity makes, answering
/// `n = 1` for the granted permissions (the shape the routes' gate uses).
fn queue_troop_checks(host: &TestHost, granted: &[&str]) {
    for (permission, _scope) in permission_check_order() {
        let n = i64::from(granted.contains(&permission.as_str()));
        host.db.push_rows(vec![json!({ "n": n })]);
    }
}

/// Queue one permission row per check a **lodge-scoped** identity makes: a troop-scope
/// check is answered `false` by the SDK without a query, so only the `any`-scope
/// checks reach the database — and they are checked at the caller's own scope.
fn queue_scoped_checks(host: &TestHost, granted: &[&str]) {
    for (permission, scope) in permission_check_order() {
        if scope != ToolScope::Any {
            continue;
        }
        let n = i64::from(granted.contains(&permission.as_str()));
        host.db.push_rows(vec![json!({ "n": n })]);
    }
}

/// Queue the single check an invocation of one tool makes (the tool's own
/// permission is checked once, not the whole catalogue's).
fn queue_permission(host: &TestHost, granted: bool) {
    host.db.push_rows(vec![json!({ "n": i64::from(granted) })]);
}

fn scoped_identity(role: &str, scope: Scope) -> Identity {
    Identity::from_grants("bea", vec![RoleGrant { role_id: role.into(), scope }])
}

fn troop_identity() -> Identity {
    Identity::new("bea", vec!["chief".to_string()])
}

// `SqlValue` crosses the plugin boundary and is deliberately not `PartialEq`, so
// bound parameters are matched structurally rather than compared.
fn bound_text(params: &[SqlValue], needle: &str) -> bool {
    params.iter().any(|p| matches!(p, SqlValue::Text(v) if v == needle))
}

fn bound_json(params: &[SqlValue], needle: &str) -> bool {
    params.iter().any(|p| matches!(p, SqlValue::Json(v) if v == needle))
}

/// Does any bound JSON value contain `needle`? (Used for the audit `details`
/// payload, which is where a non-UUID actor is recorded.)
fn bound_json_contains(params: &[SqlValue], needle: &str) -> bool {
    params.iter().any(|p| matches!(p, SqlValue::Json(v) if v.contains(needle)))
}

fn bound_int(params: &[SqlValue], needle: i64) -> bool {
    params.iter().any(|p| matches!(p, SqlValue::Int(v) if *v == needle))
}

fn bound_bool_at(params: &[SqlValue], index: usize, needle: bool) -> bool {
    matches!(params.get(index), Some(SqlValue::Bool(v)) if *v == needle)
}

fn bound_int_at(params: &[SqlValue], index: usize, needle: i64) -> bool {
    matches!(params.get(index), Some(SqlValue::Int(v)) if *v == needle)
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

const CONNECT: &str = "/api/mcp/connect";
const TOOLS: &str = "/api/mcp/tools";
const INVOKE: &str = "/api/mcp/invoke";
const INVOCATIONS: &str = "/api/mcp/invocations";

// --- the declaration the loader validates ----------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, routes) = plugin_routes().await;
    let plugin = McpPlugin::new();

    assert_eq!(plugin.id(), "mcp");
    assert_eq!(plugin.name(), "Hermes MCP");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        permissions,
        vec![PERM_CONNECT.to_string(), PERM_INVOKE.to_string(), PERM_AUDIT.to_string()]
    );
    for permission in &permissions {
        assert!(
            permission.starts_with("mcp:"),
            "permission {permission} must be namespaced by the plugin id"
        );
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    assert!(versions.iter().all(|v| *v >= 1), "versions start at 1");

    // SPEC §7.10's tables and the columns the audit trail depends on.
    let ddl = &migrations[0].sql;
    for table in ["connections", "invocations"] {
        assert!(ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")), "{table}");
    }
    for column in [
        "token_hash",
        "expires_at",
        "revoked_at",
        "invocation_count",
        "connection_id",
        "user_id",
        "arguments JSONB",
        "downstream JSONB",
        "status TEXT",
        "http_status INTEGER",
        "result JSONB",
        "duration_ms",
        "created_at TIMESTAMPTZ",
    ] {
        assert!(ddl.contains(column), "schema is missing {column}");
    }
    assert!(
        ddl.contains("REFERENCES connections(id)"),
        "an invocation must point at its connection"
    );

    // Routes: the plugin's own namespace, each gated by a permission it declares.
    for r in &routes {
        assert!(
            r.path.starts_with("/api/mcp/"),
            "route {} escapes the plugin namespace",
            r.path
        );
        let permission = r.required_permission.clone().unwrap_or_default();
        assert!(
            permissions.contains(&permission),
            "route {} requires undeclared permission {permission}",
            r.path
        );
        assert!(
            r.required_scope.is_none(),
            "route {} is an object route: the handler checks the scope it targets",
            r.path
        );
    }

    assert_eq!(route(&routes, "POST", CONNECT).required_permission.as_deref(), Some(PERM_CONNECT));
    assert_eq!(route(&routes, "GET", TOOLS).required_permission.as_deref(), Some(PERM_CONNECT));
    assert_eq!(route(&routes, "POST", INVOKE).required_permission.as_deref(), Some(PERM_INVOKE));
    assert_eq!(
        route(&routes, "GET", INVOCATIONS).required_permission.as_deref(),
        Some(PERM_CONNECT)
    );
    assert_eq!(routes.len(), 4);
}

// --- connect ----------------------------------------------------------------

#[tokio::test]
async fn connect_mints_a_connection_and_stores_only_the_token_hash() {
    let (host, routes) = plugin_routes().await;
    host.db.push_rows(vec![json!({
        "id": 7,
        "created_at": "2026-01-01T00:00:00+00:00",
        "expires_at": "2026-01-02T00:00:00+00:00",
    })]);

    let identity = troop_identity();
    let req = TestRequest::post(CONNECT)
        .json(&json!({
            "client": "Hermes",
            "client_version": "1.4",
            "protocolVersion": "2025-06-18",
            "capabilities": { "roots": {} }
        }))
        .build();
    let mut req = req;
    req.identity = Some(identity);

    let (status, body) = call(&route(&routes, "POST", CONNECT).handler, req).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["connection_id"], 7);
    assert_eq!(body["protocolVersion"], "2025-06-18");
    assert_eq!(body["server"]["name"], "adjutant-mcp");
    assert_eq!(body["tools_endpoint"], TOOLS);

    let token = body["token"].as_str().expect("a token");
    assert_eq!(token.len(), 64, "256 bits of hex");

    // The row was written to the plugin's own schema, and the stored value is
    // the hash — the raw token never reaches the database.
    let params = host.db.last_query_params("connections").expect("the insert");
    assert!(
        bound_text(&params, &hash_token(token)),
        "the token hash is bound: {params:?}"
    );
    assert!(
        !bound_text(&params, token),
        "the raw token must never be bound: {params:?}"
    );
    assert!(
        host.db.queried_sql()[0].contains("\"mcp\".\"connections\""),
        "the insert is schema-qualified: {}",
        host.db.queried_sql()[0]
    );
    assert!(host
        .db
        .queried_sql()
        .iter()
        .any(|sql| sql.contains("expires_at") && sql.contains("interval")));

    // Opening a connection is audited: the action, resource and actor are bound
    // parameters, so the assertion reads them back rather than the SQL text.
    host.db.assert_executed(&["core.audit_log"]);
    let audit = host.db.last_execute_params("core.audit_log").unwrap();
    assert!(bound_text(&audit, "mcp.connect"), "{audit:?}");
    assert!(bound_text(&audit, "mcp_connection"), "{audit:?}");
    assert!(bound_text(&audit, "7"), "the new connection id: {audit:?}");
}

#[tokio::test]
async fn connect_needs_a_session() {
    let (_host, routes) = plugin_routes().await;
    let req = TestRequest::post(CONNECT).json(&json!({})).build();
    let (status, body) = call(&route(&routes, "POST", CONNECT).handler, req).await;
    assert_eq!(status, 401, "{body}");
}

#[tokio::test]
async fn connect_uses_the_default_ttl_and_base_url_when_unconfigured() {
    let (host, routes) = plugin_routes_with(Value::Null).await;
    host.db.push_rows(vec![json!({ "id": 1, "created_at": "now", "expires_at": "soon" })]);

    let mut req = TestRequest::post(CONNECT).json(&json!({ "client": "Hermes" })).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "POST", CONNECT).handler, req).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["protocolVersion"], MCP_PROTOCOL_VERSION);

    // The TTL is bound as text and turned into an interval by PostgreSQL (the
    // typed-null trap's neighbour: never cast a bare parameter).
    let params = host.db.last_query_params("connections").unwrap();
    assert!(bound_text(&params, "24"), "{params:?}");

    // With no config the plugin calls the core's own default bind.
    assert_eq!(DEFAULT_BASE_URL, "http://127.0.0.1:8787");
}

// --- tools ------------------------------------------------------------------

#[tokio::test]
async fn tools_are_filtered_by_the_callers_permissions() {
    let (host, routes) = plugin_routes().await;
    queue_troop_checks(&host, &["missions:read"]);

    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["filtered"], true);
    assert_eq!(body["count"], 2);
    assert_eq!(
        tool_names(&body),
        vec!["missions_list_missions", "missions_get_mission"],
        "only what the caller may invoke is advertised"
    );

    // The listing carries the schema and the permission an agent needs to plan.
    let listed = &body["tools"][1];
    assert_eq!(listed["requiredPermission"], "missions:read");
    assert_eq!(listed["scope"], "any");
    assert_eq!(listed["inputSchema"]["properties"]["id"]["type"], "number");

    // One permission check per distinct permission, not per tool: `missions:read`
    // backs two tools and was checked once.
    let checks: Vec<String> = host
        .db
        .queried_sql()
        .into_iter()
        .filter(|sql| sql.contains("core.role_permissions"))
        .collect();
    assert_eq!(checks.len(), permission_check_order().len());
}

#[tokio::test]
async fn a_lodge_scoped_grant_sees_only_the_tools_it_can_reach() {
    let (host, routes) = plugin_routes().await;
    queue_scoped_checks(&host, &["missions:read"]);

    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(scoped_identity("lodge_commander", Scope::lodge("3")));
    let (status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        tool_names(&body),
        vec!["missions_list_missions", "missions_get_mission"],
        "missions:read is an any-scope tool; governance:read is troop-wide and is not granted"
    );
}

#[tokio::test]
async fn an_unheld_permission_hides_its_tool_entirely() {
    let (host, routes) = plugin_routes().await;
    // Nothing granted: every check answers 0.
    queue_troop_checks(&host, &[]);

    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;

    assert_eq!(status, 200);
    assert_eq!(body["count"], 0);
    assert!(tool_names(&body).is_empty());
}

#[tokio::test]
async fn the_calendar_tools_are_invisible_until_their_plugin_is_installed() {
    let (host, routes) = plugin_routes().await;
    // The calendar plugin is not installed: no role holds `calendar:*`, so those
    // tools cannot appear.
    queue_troop_checks(&host, &["membership:read_all", "governance:read"]);

    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;

    assert_eq!(status, 200);
    let names = tool_names(&body);
    assert!(names.contains(&"membership_list_members".to_string()));
    assert!(names.contains(&"governance_list_motions".to_string()));
    assert!(
        !names.iter().any(|n| n.starts_with("calendar_")),
        "calendar tools need calendar:* grants: {names:?}"
    );
}

#[tokio::test]
async fn an_operator_override_retargets_a_tool() {
    let (host, routes) = plugin_routes_with(json!({
        "base_url": "http://adjutant.test:9000",
        "tools": {
            "override": {
                "missions_list_missions": {
                    "path": "/api/missions/mission",
                    "description": "Missions, as this troop exposes them"
                },
                "calendar_create_event": { "enabled": false }
            }
        }
    }))
    .await;
    queue_permission(&host, true);
    host.http.push_json(200, &json!({ "missions": [{ "id": 5 }] }));
    host.db.push_rows(vec![json!({ "id": 31 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_list_missions", "arguments": { "limit": 5 } }))
        .build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 200, "{body}");

    let (method, url) = host.http.request_urls().remove(0);
    assert_eq!(method, "GET");
    assert_eq!(
        url, "http://adjutant.test:9000/api/missions/mission?limit=5",
        "the override's path replaces the built-in one"
    );

    // `enabled: false` removes a tool, and the catalogue reports it.
    queue_troop_checks(&host, &[]);
    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(troop_identity());
    let (_status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;
    assert!(!tool_names(&body).iter().any(|n| n == "calendar_create_event"));
    assert!(
        body["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("calendar_create_event")),
        "the operator is told what config did: {body}"
    );
}

#[tokio::test]
async fn an_override_that_contradicts_its_arguments_is_dropped_not_served() {
    // The override retargets `missions_get_mission` to a path with no `{id}`
    // capture: the tool could never be invoked, so it is dropped and reported
    // rather than answering 500 for every call.
    let (host, routes) = plugin_routes_with(json!({
        "base_url": "http://adjutant.test:9000",
        "tools": {
            "override": { "missions_get_mission": { "path": "/api/missions/mission" } }
        }
    }))
    .await;
    queue_troop_checks(&host, &[]);

    let mut req = TestRequest::get(TOOLS).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", TOOLS).handler, req).await;
    assert_eq!(status, 200);
    assert!(!tool_names(&body).iter().any(|n| n == "missions_get_mission"));
    assert!(
        body["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("dropped")),
        "{body}"
    );
}

// --- invoke -----------------------------------------------------------------

#[tokio::test]
async fn invoke_runs_the_tool_through_the_api_and_logs_it() {
    let (host, routes) = plugin_routes().await;
    queue_permission(&host, true);
    host.http
        .push_json(200, &json!({ "missions": [{ "id": 5, "title": "Coyote survey" }] }));
    host.db.push_rows(vec![json!({ "id": 99 })]); // the invocation row

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_get_mission",
            "arguments": { "id": 5 },
            "connection_id": null
        }))
        .header("authorization", "Bearer session-token")
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["tool"], "missions_get_mission");
    assert_eq!(body["http_status"], 200);
    assert_eq!(body["invocation_id"], 99);
    assert_eq!(body["result"]["missions"][0]["title"], "Coyote survey");
    assert!(body["duration_ms"].is_number());

    // It really called the API, at the route the tool declares.
    let (method, url) = host.http.request_urls().remove(0);
    assert_eq!(method, "GET");
    assert_eq!(url, "http://adjutant.test:9000/api/missions/mission/5");

    // The invocation is in the plugin's own audit table, with the parameters.
    let params = host.db.last_query_params("invocations").expect("the invocation row");
    assert!(bound_text(&params, "missions_get_mission"));
    assert!(bound_text(&params, "ok"));
    assert!(bound_json(&params, "{\"id\":5}"), "{params:?}");
    assert!(
        matches!(params.first(), Some(SqlValue::NullInt)),
        "no connection named: a typed null, not a text one: {params:?}"
    );
    assert!(bound_int(&params, 200), "the downstream status is recorded: {params:?}");

    // …and in the hash-chained core audit log.
    host.db.assert_executed(&["core.audit_log"]);
    let audit = host.db.last_execute_params("core.audit_log").unwrap();
    assert!(bound_text(&audit, "mcp.invoke"), "{audit:?}");
    assert!(bound_text(&audit, "mcp_tool"), "{audit:?}");
    assert!(bound_text(&audit, "missions_get_mission"), "{audit:?}");
}

#[tokio::test]
async fn invoke_refuses_a_tool_the_caller_was_not_granted_and_logs_the_refusal() {
    let (host, routes) = plugin_routes().await;
    queue_permission(&host, false); // the caller holds nothing
    host.db.push_rows(vec![json!({ "id": 12 })]); // the refusal row

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_create_mission", "arguments": { "title": "x" } }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["status"], "denied");
    assert_eq!(body["invocation_id"], 12);
    assert!(
        body["error"].as_str().unwrap().contains("missions:create"),
        "the refusal names the permission: {body}"
    );

    assert!(
        host.http.request_urls().is_empty(),
        "a refusal must not reach the API"
    );

    let params = host.db.last_query_params("invocations").unwrap();
    assert!(bound_text(&params, "denied"), "{params:?}");
    assert!(bound_text(&params, "bea"), "the actor is recorded");
    host.db.assert_executed(&["core.audit_log"]);
    let audit = host.db.last_execute_params("core.audit_log").unwrap();
    assert!(bound_text(&audit, "mcp.invoke.denied"), "{audit:?}");
    assert!(
        bound_json_contains(&audit, "bea"),
        "the actor is named in details (no core.users row for a stub id): {audit:?}"
    );
}

#[tokio::test]
async fn invoke_refuses_an_unknown_tool_and_lists_what_the_caller_can_use() {
    let (host, routes) = plugin_routes().await;
    queue_troop_checks(&host, &["missions:read"]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "auth_become_chief", "arguments": {} }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        body["available_tools"],
        json!(["missions_list_missions", "missions_get_mission"])
    );
    assert!(host.http.request_urls().is_empty());
    assert!(
        host.db
            .queried_sql()
            .iter()
            .all(|sql| !sql.contains("invocations")),
        "nothing was invoked, so nothing is logged as an invocation"
    );
}

#[tokio::test]
async fn invoke_refuses_arguments_the_tool_does_not_declare() {
    let (host, routes) = plugin_routes().await;
    queue_permission(&host, true);
    host.db.push_rows(vec![json!({ "id": 13 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_get_mission",
            "arguments": { "id": 5, "created_by": "someone-else" }
        }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["status"], "denied");
    assert!(body["error"].as_str().unwrap().contains("created_by"));
    assert!(host.http.request_urls().is_empty());
}

#[tokio::test]
async fn invoke_passes_a_downstream_error_through_and_logs_the_status() {
    let (host, routes) = plugin_routes().await;
    queue_permission(&host, true);
    host.http.push_json(404, &json!({ "error": "no such mission" }));
    host.db.push_rows(vec![json!({ "id": 14 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_get_mission", "arguments": { "id": 4044 } }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 404, "the API's answer is the caller's answer: {body}");
    assert_eq!(body["status"], "error");
    assert_eq!(body["http_status"], 404);
    assert_eq!(body["error"], "no such mission");

    let params = host.db.last_query_params("invocations").unwrap();
    assert!(bound_int(&params, 404), "{params:?}");
    host.db.assert_executed(&["core.audit_log"]);
    let audit = host.db.last_execute_params("core.audit_log").unwrap();
    assert!(bound_text(&audit, "mcp.invoke.failed"), "{audit:?}");
}

#[tokio::test]
async fn invoke_reports_an_unreachable_api_as_a_gateway_error() {
    let (host, routes) = plugin_routes().await;
    queue_permission(&host, true);
    // No queued HTTP response: MockHttp fails the call, which is exactly the
    // unreachable-API case.
    host.db.push_rows(vec![json!({ "id": 15 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_get_mission", "arguments": { "id": 1 } }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["status"], "error");
    assert!(body["error"].as_str().unwrap().contains("could not reach"));
    assert!(body["result"].is_null());
}

#[tokio::test]
async fn invoke_requires_an_authenticated_caller() {
    let (_host, routes) = plugin_routes().await;
    let req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_get_mission", "arguments": { "id": 1 } }))
        .build();
    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 401, "{body}");
}

// --- connections ------------------------------------------------------------

#[tokio::test]
async fn invoke_uses_a_connection_that_belongs_to_the_caller() {
    let (host, routes) = plugin_routes().await;
    host.db.push_rows(vec![json!({ "id": 3, "user_id": "bea" })]); // the connection
    queue_permission(&host, true);
    host.http.push_json(200, &json!({ "missions": [] }));
    host.db.push_rows(vec![json!({ "id": 21 })]); // the invocation row

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_list_missions",
            "arguments": {},
            "connection_token": "deadbeefdeadbeef"
        }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");

    // The connection lookup hashed the token and refused nothing.
    let lookup = host.db.last_query_params("token_hash").expect("the lookup");
    assert!(bound_text(&lookup, &hash_token("deadbeefdeadbeef")));
    assert!(host.db.queried_sql().iter().any(|sql| sql.contains("expires_at > now()")));

    // The connection is stamped with the use.
    host.db.assert_executed(&["last_used_at", "invocation_count + 1"]);
}

#[tokio::test]
async fn invoke_refuses_a_connection_that_belongs_to_someone_else() {
    let (host, routes) = plugin_routes().await;
    host.db.push_rows(vec![json!({ "id": 3, "user_id": "mallory" })]);
    host.db.push_rows(vec![json!({ "id": 22 })]); // the refusal row

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_list_missions",
            "arguments": {},
            "connection_id": 3
        }))
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["status"], "denied");
    assert!(
        body["error"].as_str().unwrap().contains("another user"),
        "{body}"
    );
    assert!(
        host.http.request_urls().is_empty(),
        "a connection is not a way to borrow someone else's authority"
    );
    assert!(
        !host
            .db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("core.role_permissions")),
        "the refusal happens before any permission check"
    );
}

#[tokio::test]
async fn invoke_refuses_an_expired_connection_and_a_double_identity() {
    let (host, routes) = plugin_routes().await;
    // No row: expired or revoked.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![json!({ "id": 23 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_list_missions",
            "arguments": {},
            "connection_token": "gone"
        }))
        .build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no live MCP connection"));

    // Naming both a connection id and a token is a client bug, and is audited.
    host.db.push_rows(vec![json!({ "id": 24 })]);
    let mut req = TestRequest::post(INVOKE)
        .json(&json!({
            "tool": "missions_list_missions",
            "arguments": {},
            "connection_id": 1,
            "connection_token": "tok"
        }))
        .build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("not both"));
}

#[tokio::test]
async fn the_mcp_session_header_carries_the_connection_token() {
    let (host, routes) = plugin_routes().await;
    host.db.push_rows(vec![json!({ "id": 4, "user_id": "bea" })]);
    queue_permission(&host, true);
    host.http.push_json(200, &json!({ "missions": [] }));
    host.db.push_rows(vec![json!({ "id": 25 })]);

    let mut req = TestRequest::post(INVOKE)
        .json(&json!({ "tool": "missions_list_missions", "arguments": {} }))
        .header("mcp-session-id", "session-token-1")
        .build();
    req.identity = Some(troop_identity());

    let (status, body) = call(&route(&routes, "POST", INVOKE).handler, req).await;
    assert_eq!(status, 200, "{body}");
    let lookup = host.db.last_query_params("token_hash").unwrap();
    assert!(
        bound_text(&lookup, &hash_token("session-token-1")),
        "the header token was used: {lookup:?}"
    );
}

// --- the audit trail --------------------------------------------------------

#[tokio::test]
async fn the_invocation_trail_is_own_by_default_and_troop_wide_with_mcp_audit() {
    let (host, routes) = plugin_routes().await;
    host.db.push_rows(vec![json!({ "n": 0 })]); // no mcp:audit
    host.db.push_rows(vec![json!({
        "id": 31, "connection_id": 3, "user_id": "bea", "tool": "missions_list_missions",
        "status": "ok", "http_status": 200, "error": null, "duration_ms": 4,
        "created_at": "2026-01-01T00:00:00+00:00",
    })]);

    let mut req = TestRequest::get(INVOCATIONS).query_param("status", "ok").build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", INVOCATIONS).handler, req).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["scope"], "own");
    assert_eq!(body["invocations"][0]["tool"], "missions_list_missions");

    let params = host.db.last_query_params("invocations").unwrap();
    assert!(bound_bool_at(&params, 0, false), "the caller has no mcp:audit");
    assert!(bound_text(&params, "bea"), "their own rows only");
    assert!(
        host.db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("user_id = $2")),
        "the filter is in SQL, not applied after the fact"
    );

    // With `mcp:audit` the same route answers troop-wide.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![]);
    let mut req = TestRequest::get(INVOCATIONS).build();
    req.identity = Some(troop_identity());
    let (status, body) = call(&route(&routes, "GET", INVOCATIONS).handler, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["scope"], "troop");
    let params = host.db.last_query_params("invocations").unwrap();
    assert!(bound_bool_at(&params, 0, true));
    assert!(bound_int_at(&params, 4, 50), "the default page size");
}
