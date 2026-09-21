//! HTTP server: Axum router assembly + plugin route dispatch (SPEC §5.3).
//!
//! Middleware stack (bottom → top):
//! 1. Request ID — assigned per request, logged
//! 2. Logging — structured request/response via tracing
//! 3. Identity — `x-dev-user`/`x-dev-role` stub → request extensions
//! 4. Permission gate — per-route, inside the dispatch wrapper
//! 5. Plugin routing — dispatch to the owning plugin's handler
//!
//! CORS and rate limiting are Milestone 2 (auth plugin brings real sessions;
//! tower-http adds CORS). The prototype validates the hard part — dispatch +
//! permission enforcement through plugin-registered routes.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use adjutant_sdk::{Method, PermissionService, PluginRequest, RouteHandler};

use crate::config::Config;
use crate::db;
use crate::events::EventBus;
use crate::permissions::{authorize, extract_identity};
use crate::plugin_runtime::{load_all, PluginInfo, PluginRegistry};

/// Shared app state.
///
/// Holds the plugin registry: the `Library` handles pin the plugin `.so`s in
/// memory for the process lifetime. Dropping them while the router still holds
/// handler `Arc`s into that code = SIGSEGV on first dispatch (seen in Milestone
/// 1 bring-up). AppState lives inside every dispatch closure, so the libraries
/// outlive the router by construction.
pub struct AppState {
    pub pool: Arc<sqlx::PgPool>,
    pub permissions: PermissionService,
    pub plugins: Vec<PluginInfo>,
    pub registry: PluginRegistry,
}

/// One registered plugin route: method, path, permission gate, handler.
struct RouteEntry {
    method: Method,
    path: String,
    required_permission: Option<String>,
    handler: RouteHandler,
}

/// Build the full application: connect DB, load plugins, assemble router.
/// Returns the router (ready to serve) and the event bus (kept alive by caller).
pub async fn build_app(cfg: &Config) -> Result<(Router, EventBus), BuildError> {
    let pool = db::connect_and_migrate(cfg).await.map_err(BuildError::Db)?;
    let mut bus = EventBus::new();

    let registry = load_all(
        &cfg.plugin_dir,
        pool.clone(),
        bus.sender(),
        serde_json::Value::Object(Default::default()),
    )
    .await
    .map_err(BuildError::Plugin)?;

    // Register event subscriptions before any traffic flows.
    for lp in &registry.plugins {
        let subs = lp.plugin.subscriptions();
        for sub in subs {
            bus.subscribe(lp.plugin.id(), sub);
        }
    }

    // Bootstrap role grants: plugins registered their permissions during load;
    // now grant them. SPEC §9 — Chief holds full troop authority, so `chief`
    // gets every permission that exists after load. Placeholder until the auth
    // plugin (Milestone 2) replaces static roles with real role management.
    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) \
         SELECT 'chief', id FROM core.permissions \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool.as_ref())
    .await
    .map_err(BuildError::Db)?;

    let permissions = PermissionService::new(crate::host::CoreDb::new(pool.clone()));
    let mut routes: Vec<RouteEntry> = Vec::new();
    for lp in &registry.plugins {
        for r in &lp.routes {
            routes.push(RouteEntry {
                method: r.method,
                path: r.path.clone(),
                required_permission: r.required_permission.clone(),
                handler: r.handler.clone(),
            });
        }
    }
    let routes_len = routes.len();

    // Registry moves INTO the state — see AppState doc: this is what keeps the
    // plugin libraries mapped for as long as the router can dispatch into them.
    let state = Arc::new(AppState {
        pool: pool.clone(),
        permissions: permissions.clone(),
        plugins: registry.infos.clone(),
        registry,
    });

    let mut app = Router::new()
        .route("/", get(health))
        .route("/api/plugins", get(list_plugins))
        .route("/api/events/recent", get(recent_events))
        .with_state(state.clone());

    // Register each plugin route on the router with its own dispatch wrapper.
    for entry in routes {
        let st = state.clone();
        let required = entry.required_permission.clone();
        let handler = entry.handler.clone();
        let path = entry.path.clone();

        let wrapper = move |req: Request| {
            let st = st.clone();
            let required = required.clone();
            let handler = handler.clone();
            async move { dispatch(st, required, handler, path, req).await }
        };

        app = match entry.method {
            Method::Get => app.route(&entry.path, get(wrapper)),
            Method::Post => app.route(&entry.path, post(wrapper)),
            Method::Put => app.route(&entry.path, axum::routing::put(wrapper)),
            Method::Delete => app.route(&entry.path, axum::routing::delete(wrapper)),
        };
    }

    let route_count = 3 + routes_len;
    tracing::info!(routes = route_count, "router assembled");
    Ok((app, bus))
}

/// Plugin route dispatch: permission gate → build SDK request → call handler.
async fn dispatch(
    state: Arc<AppState>,
    required: Option<String>,
    handler: RouteHandler,
    path: String,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let body_bytes = to_bytes(body, 1024 * 1024).await.unwrap_or_default();

    // 1. Identity from dev headers (Milestone 1 stub; auth plugin replaces).
    let identity = extract_identity(&parts.headers);

    // 2. Permission gate (SPEC §9 — enforced by core, not by the plugin).
    if let Some(perm) = &required {
        if let Err(status) = authorize(identity.as_ref(), &state.permissions, perm).await {
            let msg = if status == 401 {
                "authentication required"
            } else {
                "insufficient permissions"
            };
            return (StatusCode::from_u16(status).unwrap(), Json(json!({ "error": msg }))).into_response();
        }
    }

    // 3. Convert Axum request → SDK request.

    let query = parts
        .uri
        .query()
        .map(|q| {
            q.split('&')
                .filter_map(|pair| {
                    let mut it = pair.splitn(2, '=');
                    Some((decode(it.next()?), decode(it.next().unwrap_or(""))))
                })
                .collect()
        })
        .unwrap_or_default();

    let headers: HashMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();

    let preq = PluginRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query,
        headers,
        body: body_bytes.to_vec(),
        identity,
    };

    // 4. Call the plugin.
    match handler(preq).await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut response = (status, resp.body).into_response();
            for (k, v) in resp.headers {
                if let (Ok(name), Ok(val)) = (
                    k.parse::<axum::http::HeaderName>(),
                    v.parse::<axum::http::HeaderValue>(),
                ) {
                    response.headers_mut().insert(name, val);
                }
            }
            response
        }
        Err(e) => {
            tracing::warn!(path, error = %e, "plugin handler error");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

fn decode(s: &str) -> String {
    // Minimal percent-decoding for query params (no external dep).
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(h) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(h);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- core routes ------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "adjutant" }))
}

async fn list_plugins(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "plugins": state.plugins }))
}

/// Last 50 events — proves event persistence without plugin involvement.
async fn recent_events(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let rows: Vec<(i64, String, serde_json::Value, String, String)> = sqlx::query_as(
        "SELECT id, event_type, payload, source_plugin, created_at::text \
         FROM core.events ORDER BY id DESC LIMIT 50",
    )
    .fetch_all(state.pool.as_ref())
    .await
    .unwrap_or_default();

    let events: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, event_type, payload, source, created_at)| {
            json!({
                "id": id, "event_type": event_type, "payload": payload,
                "source": source, "created_at": created_at,
            })
        })
        .collect();

    Json(json!({ "events": events }))
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("plugin runtime: {0}")]
    Plugin(#[from] crate::plugin_runtime::PluginRuntimeError),
}
