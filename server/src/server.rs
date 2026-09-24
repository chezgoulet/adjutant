//! HTTP server: Axum router assembly, dynamic plugin dispatch, admin lifecycle
//! (SPEC §5.3, §15 M2).
//!
//! Middleware stack (outer → inner):
//! 1. CORS — tower-http, config-driven (`cors.origins`)
//! 2. Request ID + structured access log — `x-request-id` in, one line out
//! 3. Rate limiting — fixed window per client IP (`rates.*`)
//! 4. Identity (`x-dev-user`/`x-dev-role` stub) + permission gate + dispatch
//!
//! **Plugin routes are NOT registered statically.** Enable/disable/reload must
//! take effect without rebuilding the router, so every non-core request falls
//! through to `dynamic_dispatch`, which resolves `METHOD path` against the live
//! registry under a read lock (the lock is released before the handler runs —
//! a slow plugin must never block a reload).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use adjutant_sdk::{AuditService, PermissionService, PluginRequest};

use crate::config::Config;
use crate::db;
use crate::events::EventBus;
use crate::middleware::{request_log, RateLimiter};
use crate::permissions::{authorize, extract_identity};
use crate::plugin_runtime::{load_all, PluginRegistry, RouteLookup};

/// Shared app state. The registry lives behind a lock: admin lifecycle ops
/// (enable/disable/uninstall/reload) mutate it while requests dispatch through
/// it. `LoadedPlugin` keeps every `Library` mapped — retired plugins too (see
/// plugin_runtime's library lifetime rule), so handlers cloned out of a read
/// lock stay valid even if a reload races them.
pub struct AppState {
    pub pool: Arc<sqlx::PgPool>,
    pub permissions: PermissionService,
    pub audit: AuditService,
    pub registry: RwLock<PluginRegistry>,
    pub bus: Arc<EventBus>,
    pub config: Arc<Config>,
    /// Plugin-registered identity providers (auth plugin replaces the dev stub).
    pub identity: Arc<crate::identity::IdentityHub>,
    /// Core-mediated HTTP shared by all plugin contexts.
    pub http: Arc<crate::host::CoreHttp>,
}

impl AppState {
    /// Resolve the caller's identity the SAME way `dispatch` does: plugin
    /// providers first (auth sessions via cookie/Bearer), dev headers only as
    /// the gated fallback. Every consumer (permission gate, audit attribution)
    /// must use this — reading dev headers directly meant admin routes 401'd
    /// for real sessions once `allow_dev_headers=false`, and audit rows lost
    /// their actor entirely.
    async fn resolve_identity(&self, headers: &axum::http::HeaderMap) -> Option<adjutant_sdk::Identity> {
        let map: HashMap<String, String> = headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut identity = self.identity.identify(&map).await;
        if identity.is_none() && self.config.allow_dev_headers {
            identity = extract_identity(headers);
        }
        identity
    }

    /// Graceful shutdown: stop event handlers, then call every plugin's
    /// documented `shutdown()` hook (live and retired generations alike) before
    /// the process exits. Without this, that hook was dead code.
    pub async fn shutdown(&self) {
        self.bus.shutdown();
        let mut reg = self.registry.write().await;
        // Destructure the guard so the two vecs borrow independently.
        let PluginRegistry { plugins, retired } = &mut *reg;
        for lp in plugins.iter_mut().chain(retired.iter_mut()) {
            if let Err(e) = lp.plugin.shutdown().await {
                tracing::warn!(plugin = %lp.info.id, error = %e, "plugin shutdown failed");
            }
        }
        tracing::info!("plugins shut down");
    }

    /// Admin gate: `core:admin` permission. Returns a ready 401/403 response
    /// when the caller isn't allowed, `None` when allowed.
    async fn require_admin(&self, headers: &axum::http::HeaderMap) -> Option<Response> {
        let identity = self.resolve_identity(headers).await;
        match authorize(identity.as_ref(), &self.permissions, "core:admin").await {
            Ok(()) => None,
            Err(status) => {
                let msg = if status == 401 {
                    "authentication required"
                } else {
                    "insufficient permissions"
                };
                Some(
                    (StatusCode::from_u16(status).unwrap(), Json(json!({ "error": msg })))
                        .into_response(),
                )
            }
        }
    }
}

/// Build the full application: connect DB, load plugins, assemble router.
pub async fn build_app(cfg: &Config) -> Result<(Router, Arc<AppState>), BuildError> {
    let pool = db::connect_and_migrate(cfg).await.map_err(BuildError::Db)?;
    let bus = EventBus::new();
    let identity = crate::identity::IdentityHub::new();
    let http = crate::host::CoreHttp::new();

    let registry = load_all(
        &cfg.plugin_dir,
        pool.clone(),
        bus.sender(),
        serde_json::Value::Object(Default::default()),
        identity.clone(),
        http.clone(),
    )
    .await
    .map_err(BuildError::Plugin)?;

    // Bind event subscriptions before any traffic flows — but only for plugins
    // that are actually enabled. A plugin disabled in core.plugins is loaded with
    // enabled=false; binding its subscriptions anyway made "disabled" mean
    // "routes off, event handlers still running".
    for lp in &registry.plugins {
        if !lp.enabled {
            tracing::info!(plugin = lp.plugin.id(), "disabled at boot; not binding subscriptions");
            continue;
        }
        for sub in lp.plugin.subscriptions() {
            bus.subscribe(lp.plugin.id(), sub);
        }
    }
    let route_count: usize = registry.plugins.iter().map(|p| p.routes.len()).sum();

    // Bootstrap role grants: plugins registered their permissions during load;
    // now grant them. SPEC §9 — Chief holds full troop authority, so `chief`
    // gets every permission that exists after load. Placeholder until the auth
    // plugin replaces static roles.
    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) \
         SELECT 'chief', id FROM core.permissions \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool.as_ref())
    .await
    .map_err(BuildError::Db)?;

    let permissions = PermissionService::new(crate::host::CoreDb::new(pool.clone()));
    let audit = AuditService::new(crate::host::CoreDb::new(pool.clone()), "core".into());

    let state = Arc::new(AppState {
        pool: pool.clone(),
        permissions,
        audit,
        registry: RwLock::new(registry),
        bus,
        config: Arc::new(cfg.clone()),
        identity,
        http,
    });

    let cors = cfg.cors_origins.first().map(|_| {
        let layer = CorsLayer::new();
        if cfg.cors_origins.iter().any(|o| o == "*") {
            layer.allow_origin(Any).allow_methods(Any).allow_headers(Any)
        } else {
            let origins: Vec<_> = cfg
                .cors_origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect();
            layer
                .allow_origin(origins)
                .allow_methods(Any)
                .allow_headers(Any)
        }
    });

    let app = Router::new()
        .route("/", get(health))
        .route("/api/plugins", get(list_plugins))
        .route("/api/plugins/{name}/enable", post(enable_plugin))
        .route("/api/plugins/{name}/disable", post(disable_plugin))
        .route("/api/plugins/{name}", delete(uninstall_plugin))
        .route("/api/plugins/reload", post(reload_plugins))
        .route("/api/events/recent", get(recent_events))
        .route("/api/audit/verify", get(audit_verify))
        // Every other METHOD path resolves against the live plugin registry.
        .fallback(dynamic_dispatch)
        .with_state(state.clone());

    // Layer order: later call = outermost. CORS outermost (headers on 429s and
    // errors), then access log (sees every response incl. rate-limited), then
    // the limiter (cheap rejection before any handler work).
    let limiter = RateLimiter::new(cfg.rate.clone(), cfg.trusted_proxies.clone());
    let app = app.layer(axum::middleware::from_fn_with_state(
        limiter,
        rate_limit_layer,
    ));
    let app = app.layer(axum::middleware::from_fn(request_log));
    let app = match cors {
        Some(c) => app.layer(c),
        None => app,
    };

    tracing::info!(routes = route_count, "router assembled (dynamic plugin dispatch)");
    Ok((app, state))
}

/// from_fn wrapper over the testable `rate_limit_inner`.
async fn rate_limit_layer(
    State(limiter): State<RateLimiter>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    crate::middleware::rate_limit_inner(limiter, request, next).await
}

// ---------------------------------------------------------------------------
// Dynamic plugin dispatch
// ---------------------------------------------------------------------------

async fn dynamic_dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Resolve under the read lock, clone what we need, release BEFORE awaiting
    // the handler (a slow plugin must not block reload/uninstall writers).
    let lookup = {
        let reg = state.registry.read().await;
        match reg.find(&method, &path) {
            RouteLookup::Found {
                required_permission,
                handler,
                plugin_id,
                params,
            } => Ok((plugin_id, required_permission, handler, params)),
            RouteLookup::Disabled { plugin_id } => Err((
                StatusCode::NOT_FOUND,
                format!("plugin {plugin_id} is disabled"),
            )),
            RouteLookup::NotFound => Err((StatusCode::NOT_FOUND, "route not found".to_string())),
        }
    };

    let (plugin_id, required, handler, params) = match lookup {
        Ok(x) => x,
        Err((status, msg)) => {
            return (status, Json(json!({ "error": msg }))).into_response();
        }
    };

    dispatch(state, plugin_id, required, handler, params, req).await
}

/// Permission gate → build SDK request → call handler.
async fn dispatch(
    state: Arc<AppState>,
    plugin_id: String,
    required: Option<String>,
    handler: adjutant_sdk::RouteHandler,
    params: HashMap<String, String>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let body_bytes = to_bytes(body, state.config.max_body_bytes)
        .await
        .unwrap_or_default();

    // 1. Identity: plugin providers first (the auth plugin owns real
    // sessions), dev headers only as a gated fallback (SPEC §7.1 — spoofable,
    // off unless auth.allow_dev_headers is explicitly enabled).
    let headers: HashMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let identity = state.resolve_identity(&parts.headers).await;

    // 2. Permission gate — enforced by core, never by the plugin (SPEC §9).
    if let Some(perm) = &required {
        if let Err(status) = authorize(identity.as_ref(), &state.permissions, perm).await {
            let msg = if status == 401 {
                "authentication required"
            } else {
                "insufficient permissions"
            };
            return (StatusCode::from_u16(status).unwrap(), Json(json!({ "error": msg })))
                .into_response();
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

    // Captures are raw path segments; decode them once so a plugin receives the
    // value the client meant (query parameters are already decoded — this makes
    // the two consistent).
    let params: HashMap<String, String> = params
        .into_iter()
        .map(|(k, v)| (k, decode_path(&v)))
        .collect();

    let preq = PluginRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        params,
        query,
        headers,
        body: body_bytes.to_vec(),
        identity,
    };

    // 4. Call the plugin.
    match handler(preq).await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
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
            tracing::warn!(plugin = %plugin_id, error = %e, "plugin handler error");
            // The SDK owns the error → status mapping (SdkError::status), so the
            // core and plugins agree on 400/401/403/404/409/500.
            let status = StatusCode::from_u16(e.status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

/// Percent-decode one path segment (captures). Unlike the query decoder this
/// does NOT treat `+` as a space — that is form-encoding, and a `+` in a path is
/// a literal plus.
fn decode_path(s: &str) -> String {
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
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Core routes
// ---------------------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "adjutant" }))
}

/// Plugin registry + route/permission inventory (SPEC §5.3: admin surface).
/// Anonymous callers previously got the whole route table; now the same
/// `core:admin` gate as every other admin route applies.
async fn list_plugins(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let reg = state.registry.read().await;
    Json(json!({
        "plugins": reg.infos(),
        "retired_libraries": reg.retired_count(),
        // Plugin ids with at least one bound event subscription. Disable aborts
        // them and enable re-binds, so this is the observable proof of that.
        "bound_subscriptions": state.bus.subscriber_ids(),
    }))
    .into_response()
}

/// Last N events with cursor replay: `?since=<id>&limit=<1..500>`
/// (SPEC §15 M2: event bus persistence + replay). Admin-gated: event payloads
/// carry plugin data (member ids, messages) and the route is not public API.
async fn recent_events(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let params: HashMap<String, String> = req
        .uri()
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
    let since: i64 = params
        .get("since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .clamp(1, 500);

    let rows: Vec<(i64, String, serde_json::Value, String, String)> = sqlx::query_as(
        "SELECT id, event_type, payload, source_plugin, created_at::text \
         FROM core.events WHERE id > $1 ORDER BY id ASC LIMIT $2",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(state.pool.as_ref())
    .await
    .unwrap_or_default();

    let last = rows.last().map(|r| r.0).unwrap_or(since);
    let events: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, event_type, payload, source, created_at)| {
            json!({
                "id": id, "event_type": event_type, "payload": payload,
                "source": source, "created_at": created_at,
            })
        })
        .collect();

    Json(json!({ "events": events, "cursor": last })).into_response()
}

/// Recompute the audit hash chain (SPEC §15 M2: tamper-evident audit log).
/// FAILS CLOSED: if the verification query itself errors, the endpoint answers
/// 500 — never `{"ok":true,"rows_checked":0}`, which is what the old
/// `unwrap_or((None, 0))` produced and would have masked a broken verifier.
async fn audit_verify(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let row: Result<(Option<i64>, i64), sqlx::Error> =
        sqlx::query_as("SELECT first_bad, rows_checked FROM core.audit_verify()")
            .fetch_one(state.pool.as_ref())
            .await;
    match row {
        Ok((first_bad, rows)) => Json(json!({
            "ok": first_bad.is_none(),
            "first_bad": first_bad,
            "rows_checked": rows,
        }))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "audit verification query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("audit verification unavailable: {e}") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Admin lifecycle (all require core:admin; every action is audit-logged)
// ---------------------------------------------------------------------------

async fn enable_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;
    let changed = {
        let mut reg = state.registry.write().await;
        reg.set_enabled(&name, true)
    };
    if !changed {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "plugin not loaded" }))).into_response();
    }
    state.identity.set_enabled(&name, true);
    // Re-bind subscriptions that disable aborted (only if none are bound, so a
    // repeated enable cannot double-subscribe).
    if !state.bus.subscriber_ids().iter().any(|id| id == &name) {
        let reg = state.registry.read().await;
        if let Some(lp) = reg.plugins.iter().find(|p| p.info.id == name) {
            for sub in lp.plugin.subscriptions() {
                state.bus.subscribe(&name, sub);
            }
        }
    }
    let dbres = sqlx::query(
        "UPDATE core.plugins SET enabled = true, updated_at = now() WHERE id = $1",
    )
    .bind(&name)
    .execute(state.pool.as_ref())
    .await;
    if let Err(e) = dbres {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
            .into_response();
    }
    if let Err(e) = state
        .audit
        .log(identity.as_ref(), "plugin.enable", "plugin", &name, json!({}))
        .await
    {
        tracing::error!(action = "plugin.enable", plugin = %&name, error = %e, "audit write failed");
    }
    Json(json!({ "plugin": name, "enabled": true })).into_response()
}

async fn disable_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    // With the dev-header stub off, the identity providers ARE the only way to
    // authenticate. Removing the last one makes every authenticated route —
    // including the admin route that would undo this — unreachable until the
    // process restarts. Refuse, and say how to proceed.
    if !state.config.allow_dev_headers && state.identity.is_sole_enabled_provider(&name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "refusing to remove the only identity provider while dev headers are off",
                "hint": "register/enable another identity provider, or set ADJUTANT_DEV_HEADERS=true for a dev instance",
            })),
        )
            .into_response();
    }
    let identity = state.resolve_identity(req.headers()).await;
    let changed = {
        let mut reg = state.registry.write().await;
        reg.set_enabled(&name, false)
    };
    if !changed {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "plugin not loaded" }))).into_response();
    }
    state.identity.set_enabled(&name, false);
    // Stop the plugin's event handlers too: routes 404-ing while its handlers keep
    // appending audit rows and writing to its schema is not "disabled".
    state.bus.clear_plugin(&name).await;
    let dbres = sqlx::query(
        "UPDATE core.plugins SET enabled = false, updated_at = now() WHERE id = $1",
    )
    .bind(&name)
    .execute(state.pool.as_ref())
    .await;
    if let Err(e) = dbres {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
            .into_response();
    }
    if let Err(e) = state
        .audit
        .log(identity.as_ref(), "plugin.disable", "plugin", &name, json!({}))
        .await
    {
        tracing::error!(action = "plugin.disable", plugin = %&name, error = %e, "audit write failed");
    }
    Json(json!({ "plugin": name, "enabled": false })).into_response()
}

/// Uninstall: routes stop resolving immediately, the library is retired (kept
/// mapped), and the plugin's event subscriptions are aborted. **Data is
/// archived, never dropped** (SPEC §5.2) — the schema and `core.plugins` row
/// stay; only `uninstalled` flips, so a later boot/reload skips it.
async fn uninstall_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    // With the dev-header stub off, the identity providers ARE the only way to
    // authenticate. Removing the last one makes every authenticated route —
    // including the admin route that would undo this — unreachable until the
    // process restarts. Refuse, and say how to proceed.
    if !state.config.allow_dev_headers && state.identity.is_sole_enabled_provider(&name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "refusing to remove the only identity provider while dev headers are off",
                "hint": "register/enable another identity provider, or set ADJUTANT_DEV_HEADERS=true for a dev instance",
            })),
        )
            .into_response();
    }
    let identity = state.resolve_identity(req.headers()).await;
    let removed = {
        let mut reg = state.registry.write().await;
        reg.uninstall(&name)
    };
    if !removed {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "plugin not loaded" }))).into_response();
    }
    // NOTE: `enabled` is deliberately NOT flipped here. Uninstall means
    // "not installed"; a later reinstall (clear the flag + reload) must come
    // back serving, which is only true if the previous enabled state survives.
    // The old `enabled = false` made the documented reinstall path load the
    // plugin into a disabled state (routes 404) with nothing in the docs to say so.
    let dbres = sqlx::query(
        "UPDATE core.plugins SET uninstalled = true, updated_at = now() \
         WHERE id = $1",
    )
    .bind(&name)
    .execute(state.pool.as_ref())
    .await;
    if let Err(e) = dbres {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
            .into_response();
    }
    state.bus.clear_plugin(&name).await;
    state.identity.remove(&name);
    if let Err(e) = state
        .audit
        .log(identity.as_ref(), "plugin.uninstall", "plugin", &name, json!({}))
        .await
    {
        tracing::error!(action = "plugin.uninstall", plugin = %&name, error = %e, "audit write failed");
    }
    Json(json!({
        "plugin": name,
        "uninstalled": true,
        "data": "archived (schema and rows preserved)",
    }))
    .into_response()
}

/// Hot-reload: rescan `plugin_dir`, load the new set OUTSIDE the lock (old
/// registry keeps serving meanwhile), then swap and rebind subscriptions.
/// On failure the old registry stays live — reload is all-or-nothing.
async fn reload_plugins(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;

    let fresh = match load_all(
        &state.config.plugin_dir,
        state.pool.clone(),
        state.bus.sender(),
        serde_json::Value::Object(Default::default()),
        state.identity.clone(),
        state.http.clone(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("reload failed, old registry kept: {e}") })),
            )
                .into_response();
        }
    };
    let route_count: usize = fresh.plugins.iter().map(|p| p.routes.len()).sum();
    let ids: Vec<String> = fresh.plugins.iter().map(|p| p.info.id.clone()).collect();
    let versions: HashMap<String, String> = fresh
        .plugins
        .iter()
        .map(|p| (p.info.id.clone(), p.info.version.clone()))
        .collect();

    // Swap, then rebind subscriptions: abort every old task first so no
    // event is handled by both the old and the new instance.
    {
        let mut reg = state.registry.write().await;
        reg.replace_all(fresh);
    }
    // Providers from retired plugins stop answering; live ones re-registered
    // themselves during load_all's init (same owner key → replaced in place).
    {
        let reg = state.registry.read().await;
        let live: std::collections::HashSet<String> =
            reg.plugins.iter().map(|p| p.info.id.clone()).collect();
        state.identity.retain(&live);
    }
    for old_id in state.bus.subscriber_ids() {
        state.bus.clear_plugin(&old_id).await;
    }
    {
        let reg = state.registry.read().await;
        for lp in &reg.plugins {
            for sub in lp.plugin.subscriptions() {
                state.bus.subscribe(lp.plugin.id(), sub);
            }
        }
    }

    if let Err(e) = state
        .audit
        .log(
            identity.as_ref(),
            "plugin.reload",
            "plugin",
            "*",
            json!({ "reloaded": ids, "routes": route_count }),
        )
        .await
    {
        tracing::error!(action = "plugin.reload", error = %e, "audit write failed");
    }
    tracing::info!(routes = route_count, "registry hot-reloaded");
    Json(json!({
        "reloaded": ids,
        "versions": versions,
        "routes": route_count,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::decode_path;

    #[test]
    fn captures_are_percent_decoded_once() {
        assert_eq!(decode_path("%31"), "1");
        assert_eq!(decode_path("a%2Fb"), "a/b", "an encoded slash decodes to a slash");
        assert_eq!(decode_path("a%20b"), "a b");
        // `+` is a literal plus in a path (unlike a query string).
        assert_eq!(decode_path("a+b"), "a+b");
        // Malformed escapes are left alone rather than dropped.
        assert_eq!(decode_path("%zz"), "%zz");
        assert_eq!(decode_path("plain"), "plain");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("plugin runtime: {0}")]
    Plugin(#[from] crate::plugin_runtime::PluginRuntimeError),
}

