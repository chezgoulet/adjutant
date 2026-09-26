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
use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use adjutant_sdk::{AuditService, Identity, PermissionService, PluginRequest};

use crate::config::Config;
use crate::db;
use crate::events::EventBus;
use crate::middleware::{request_log, RateLimiter};
use crate::permissions::{authorize, extract_identity};
use crate::plugin_runtime::{
    close_pool, load_all, load_plugin, LoadEnv, PluginRegistry, PluginSlot, RouteLookup,
};

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
    /// Declared scope hierarchy (lodge→patrol), loaded at boot and on reload.
    /// Used to expand a caller's grants before the gate and handlers see them.
    pub hierarchy: RwLock<crate::scope_hierarchy::ScopeHierarchy>,
    /// Runs plugin-declared schedules (#45): started on load, aborted on
    /// disable/uninstall/reload.
    pub scheduler: Arc<crate::scheduler::Scheduler>,
    /// The last mismatch count the outbox reconciliation pass published, so a
    /// `core.outbox.mismatch` event is raised only when the set *changes* and is
    /// not empty (see `outbox::reconcile_and_raise`). Plain `Mutex`: `AppState`
    /// is already inside an `Arc`.
    pub outbox_mismatches: Mutex<i64>,
    /// The outbox relay: one drain loop per process, held here (like
    /// `scheduler`) so `shutdown()` can stop it.
    pub relay: Arc<crate::outbox::Relay>,
    /// In-flight request counts per plugin, so `disable` can stop routing and
    /// let the requests already inside a plugin finish before its pool closes
    /// (issue #89 requirement #1: quiesce, then close).
    pub in_flight: Arc<InFlight>,
    /// One lifecycle lock per plugin id: enable/disable/re-enable of the same
    /// plugin are serialized, so two admins (or a double-click) cannot run two
    /// loads or two migration passes against one schema (requirement #2).
    pub lifecycles: LifecycleLocks,
}

/// Per-plugin in-flight request counts.
///
/// A request *enters* while the registry read lock is held (so the counter can
/// never miss a dispatch that a concurrent `disable` would otherwise race), and
/// leaves when its handler returns — via [`InFlightGuard`], which decrements on
/// drop, including on a panic or an early return.
#[derive(Default)]
pub struct InFlight {
    counts: Mutex<HashMap<String, usize>>,
}

impl InFlight {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Count one request against `plugin` until the returned guard drops.
    pub fn enter(self: &Arc<Self>, plugin: &str) -> InFlightGuard {
        *self
            .counts
            .lock()
            .expect("in-flight map poisoned")
            .entry(plugin.to_string())
            .or_insert(0) += 1;
        InFlightGuard { inner: self.clone(), plugin: plugin.to_string() }
    }

    pub fn count(&self, plugin: &str) -> usize {
        self.counts
            .lock()
            .expect("in-flight map poisoned")
            .get(plugin)
            .copied()
            .unwrap_or(0)
    }

    /// Wait (polling, bounded) until no request is inside `plugin`. Returns how
    /// long the wait took when it drained, `None` if `timeout` expired first.
    ///
    /// Polling rather than a notification is deliberate: the counter is touched
    /// on every dispatch, and a 10 ms poll costs nothing next to the pool close
    /// that follows it. The bound is what keeps a hung handler from holding the
    /// admin's request open forever — the caller decides what to do when it
    /// expires (the server keeps the pool open until the request really ends;
    /// see `disable_plugin`).
    pub async fn wait_idle(&self, plugin: &str, timeout: std::time::Duration) -> Option<std::time::Duration> {
        let start = std::time::Instant::now();
        loop {
            if self.count(plugin) == 0 {
                return Some(start.elapsed());
            }
            if start.elapsed() >= timeout {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Decrements its plugin's in-flight count when the request ends. Held from the
/// moment the registry resolves a route to the moment the handler returns.
pub struct InFlightGuard {
    inner: Arc<InFlight>,
    plugin: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut counts = self.inner.counts.lock().expect("in-flight map poisoned");
        if let Some(n) = counts.get_mut(&self.plugin) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.plugin);
            }
        }
    }
}

/// One mutex per plugin id, created on demand. The guard from
/// [`LifecycleLocks::get`] is held across an entire enable/disable, including
/// the load and its migrations — that is the serialization the requirement asks
/// for. Lock order is always **lifecycle → registry**; nothing takes a registry
/// lock and then a lifecycle lock.
#[derive(Default)]
pub struct LifecycleLocks {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl LifecycleLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock for `id`, creating it on first use. Callers await
    /// `lock_owned().await` and hold the guard for the whole transition.
    pub fn get(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .expect("lifecycle map poisoned")
            .entry(id.to_string())
            .or_default()
            .clone()
    }
}

/// Plugins an admin **cannot disable**, with the reason the refusal carries
/// (issue #89 requirement #6).
///
/// A declarative list, not an `if id == "auth"`: adding a plugin to the minimum
/// is a one-line change here, and the refusal message is the entry's reason —
/// so the operator is told *why*, never just "refused".
///
/// `auth` is certain: with the dev-header stub off, disabling it makes every
/// authenticated route (including the admin route that would undo the change)
/// unreachable until a restart. `membership` is the roster every grant
/// addresses — a troop whose roster plugin is off cannot name a member to grant
/// anything to. `membership`'s membership in this set is the owner's judgement
/// call, flagged for review in the PR.
pub const REQUIRED_PLUGINS: &[(&str, &str)] = &[
    (
        "auth",
        "authentication: disabling it locks every admin out of the server until a restart",
    ),
    (
        "membership",
        "the roster: every grant addresses a member it names",
    ),
];

/// Declared plugin dependencies: `(dependent, dependency)`. Disabling a
/// dependency while an enabled plugin depends on it is **refused** with the
/// names (issue #89 requirement #7); enabling the dependent while its
/// dependency is not loaded is refused too, so a plugin can never be left
/// configured to call nothing. Declarative, like [`REQUIRED_PLUGINS`].
pub const PLUGIN_DEPENDENCIES: &[(&str, &str)] = &[("store", "stripe")];

/// Why `name` may not be disabled, when it may not be. Returns the reason.
fn disable_refusal(name: &str, registry: &PluginRegistry) -> Option<String> {
    if let Some((_, why)) = REQUIRED_PLUGINS.iter().find(|(id, _)| *id == name) {
        return Some(format!("{name} is required and cannot be disabled: {why}"));
    }
    // A loaded dependent means disabling this plugin would leave it calling
    // nothing. Refuse explicitly rather than cascade silently.
    let dependents: Vec<&str> = PLUGIN_DEPENDENCIES
        .iter()
        .filter(|(_, dep)| *dep == name)
        .map(|(dependent, _)| *dependent)
        .filter(|dependent| registry.info(dependent).is_some_and(|i| i.loaded))
        .collect();
    if !dependents.is_empty() {
        return Some(format!(
            "{name} is required by {}: disable {} first",
            dependents.join(", "),
            dependents.join(", ")
        ));
    }
    None
}

/// Why `name` may not be enabled, when it may not be. Returns the reason.
fn enable_refusal(name: &str, registry: &PluginRegistry) -> Option<String> {
    let missing: Vec<&str> = PLUGIN_DEPENDENCIES
        .iter()
        .filter(|(dependent, _)| *dependent == name)
        .map(|(_, dependency)| *dependency)
        .filter(|dependency| !registry.info(dependency).is_some_and(|i| i.loaded))
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "{name} needs {}: enable it first",
            missing.join(", ")
        ));
    }
    None
}

impl AppState {
    /// Resolve the caller's identity the SAME way `dispatch` does: plugin
    /// providers first (auth sessions via cookie/Bearer), dev headers only as
    /// the gated fallback. Every consumer (permission gate, audit attribution)
    /// must use this — reading dev headers directly meant admin routes 401'd
    /// for real sessions once `allow_dev_headers=false`, and audit rows lost
    /// their actor entirely.
    pub(crate) async fn resolve_identity(&self, headers: &axum::http::HeaderMap) -> Option<adjutant_sdk::Identity> {
        let map: HashMap<String, String> = headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut identity = self.identity.identify(&map).await;
        if identity.is_none() && self.config.allow_dev_headers {
            identity = extract_identity(headers);
        }
        // Resolve hierarchical coverage in the core: expand each grant with the
        // declared descendants of its scope (lodge -> its patrols). The SDK's
        // flat `covers` then resolves the hierarchy for both the gate and every
        // in-handler check, with no plugin call and no ABI change.
        match identity {
            Some(id) => Some(self.hierarchy.read().await.expand(&id)),
            None => None,
        }
    }

    /// Graceful shutdown: stop event handlers, then call every plugin's
    /// documented `shutdown()` hook (live and retired generations alike) before
    /// the process exits. Without this, that hook was dead code.
    pub async fn shutdown(&self) {
        self.bus.shutdown();
        self.scheduler.stop_all();
        // The outbox relay is a core task, like the scheduler's: stop it before
        // the plugins it delivers to are torn down.
        self.relay.stop();
        let mut reg = self.registry.write().await;
        // Destructure the guard so the two vecs borrow independently.
        let PluginRegistry { plugins, retired } = &mut *reg;
        // A disabled plugin has no instance to shut down: it was already dropped
        // when it was disabled (issue #89). Retired generations keep theirs, so
        // their documented `shutdown()` hook still runs.
        for lp in plugins.iter_mut().filter_map(crate::plugin_runtime::PluginSlot::live_mut).chain(retired.iter_mut()) {
            if let Err(e) = lp.plugin.shutdown().await {
                tracing::warn!(plugin = %lp.info.id, error = %e, "plugin shutdown failed");
            }
        }
        tracing::info!("plugins shut down");
    }

    /// Admin gate: `core:admin` permission. Returns a ready 401/403 response
    /// when the caller isn't allowed, `None` when allowed.
    pub(crate) async fn require_admin(&self, headers: &axum::http::HeaderMap) -> Option<Response> {
        let identity = self.resolve_identity(headers).await;
        match authorize(identity.as_ref(), &self.permissions, "core:admin", Some(&adjutant_sdk::Scope::troop())).await {
            Ok(()) => None,
            Err(status) => {
                let msg = if status == 401 {
                    "authentication required"
                } else {
                    "insufficient permissions"
                };
                Some(error_response(StatusCode::from_u16(status).unwrap(), msg))
            }
        }
    }
}

/// Refuse to boot on a PostgreSQL superuser connection.
///
/// A plugin escape became *total* compromise while the connecting role was a
/// superuser (design `plugin-isolation.md` §1, E3), so the default is to refuse;
/// `ADJUTANT_ALLOW_SUPERUSER=true` (`--allow-superuser`) is the explicit opt-out
/// for a throwaway database. Checked **before** any migration runs.
pub fn superuser_refusal(is_superuser: bool, allow_superuser: bool) -> Result<(), String> {
    if is_superuser && !allow_superuser {
        return Err(
            "refusing to boot on a PostgreSQL superuser connection: a plugin escape \
             would be total compromise. Connect as a dedicated non-superuser role \
             (see docs/deployment.md), or set ADJUTANT_ALLOW_SUPERUSER=true to override \
             for a throwaway database."
                .into(),
        );
    }
    Ok(())
}

/// Build the full application: connect DB, load plugins, assemble router.
pub async fn build_app(cfg: &Config) -> Result<(Router, Arc<AppState>), BuildError> {
    let pool = db::connect(cfg).await.map_err(BuildError::Db)?;

    // Refuse a superuser before running any DDL.
    let is_superuser: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_one(pool.as_ref())
            .await
            .map_err(BuildError::Db)?;
    superuser_refusal(is_superuser, cfg.allow_superuser).map_err(BuildError::Superuser)?;

    db::migrate_core(pool.as_ref()).await.map_err(BuildError::Db)?;

    let bus = EventBus::new();
    let identity = crate::identity::IdentityHub::new();
    let http = crate::host::CoreHttp::new();
    let scheduler = crate::scheduler::Scheduler::new();

    let registry = load_all(
        &cfg.plugin_dir,
        &cfg.database_url,
        pool.clone(),
        bus.sender(),
        serde_json::Value::Object(Default::default()),
        identity.clone(),
        http.clone(),
    )
    .await
    .map_err(BuildError::Plugin)?;

    // Bind event subscriptions before any traffic flows — but only for plugins
    // that are actually **loaded**. A disabled plugin is not in the live set at
    // all now (issue #89), so this is no longer a flag check that could drift: a
    // `Known` slot simply has no plugin to ask for subscriptions or schedules.
    for slot in &registry.plugins {
        let Some(lp) = slot.live() else {
            tracing::info!(
                plugin = %slot.info().id,
                "disabled at boot; not loaded, so no subscriptions and no schedules"
            );
            continue;
        };
        for sub in lp.plugin.subscriptions() {
            bus.subscribe(lp.plugin.id(), sub);
        }
        // Same discipline as subscriptions: only loaded plugins get their
        // schedules started, and they are aborted on disable/uninstall/reload.
        scheduler.start(lp.plugin.id(), lp.plugin.schedules(), pool.clone());
    }
    let route_count: usize = registry
        .plugins
        .iter()
        .filter_map(|s| s.live())
        .map(|p| p.routes.len())
        .sum();

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

    // Declare the core's service principals (the money path's machine
    // identities) right beside that grant: `finance:write` exists now that the
    // finance plugin has loaded and registered its permissions, and nothing is
    // ever re-granted, so an operator's revocation is not undone by a boot.
    crate::outbox::seed_service_principals(pool.as_ref())
        .await
        .map_err(BuildError::Db)?;

    let permissions = PermissionService::new(crate::host::CoreDb::new(pool.clone()));
    let audit = AuditService::new(crate::host::CoreDb::new(pool.clone()), "core".into());

    // Plugins declared their scope edges during `init` (load_all above); read
    // them now into the resolution map.
    let hierarchy = crate::scope_hierarchy::ScopeHierarchy::load(pool.as_ref())
        .await
        .map_err(BuildError::Db)?;

    let relay = crate::outbox::Relay::new();

    let state = Arc::new(AppState {
        pool: pool.clone(),
        permissions,
        audit,
        registry: RwLock::new(registry),
        bus,
        config: Arc::new(cfg.clone()),
        identity,
        http,
        hierarchy: RwLock::new(hierarchy),
        scheduler,
        outbox_mismatches: Mutex::new(0),
        relay: relay.clone(),
        in_flight: InFlight::new(),
        lifecycles: LifecycleLocks::new(),
    });

    // The core's own subscription to the relay's terminal-outcome events, bound
    // under `outbox::SUBSCRIBER_OWNER` so a reload's sweep (which clears every
    // *plugin* generation) leaves it alone. Started after the state exists:
    // `start` takes a `Weak` to it.
    state
        .bus
        .subscribe(crate::outbox::SUBSCRIBER_OWNER, crate::outbox::outcome_subscription());
    relay.start(&state);

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
        // The outbox operator surface: the queue and its states, reconciliation
        // (the control of last resort), and the hand that re-arms an intent.
        .route("/api/outbox/intents", get(crate::outbox::list_intents))
        .route(
            "/api/outbox/reconciliation",
            get(crate::outbox::reconciliation_route),
        )
        .route(
            "/api/outbox/intent/{id}/retry",
            post(crate::outbox::retry_intent),
        )
        // The notification record (#46, slice 1): the recipient's own inbox and
        // its read marker. Ownership, not a grant (SPEC §9.2) — the handler
        // filters on `recipient = caller`, so no permission is declared. See
        // docs/design/notifications.md.
        .route(
            "/api/notifications",
            get(crate::notifications::list_notifications),
        )
        .route(
            "/api/notifications/{id}/read",
            post(crate::notifications::mark_notification_read),
        )
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

/// Clear every **plugin's** event subscriptions before rebinding the new
/// generation after a reload (or an uninstall): without it both generations
/// handle the same event.
///
/// **The core's own subscription is skipped.** `core.outbox.` is the first
/// subscription the core owns rather than a plugin, and it is registered under
/// [`crate::outbox::SUBSCRIBER_OWNER`]. A sweep that cleared it would delete the
/// core's own handler on the very first reload, and nothing would notice until
/// an outcome event went unheard.
pub(crate) async fn clear_plugin_subscriptions(bus: &EventBus) {
    for old_id in bus.subscriber_ids() {
        if old_id == crate::outbox::SUBSCRIBER_OWNER {
            continue;
        }
        bus.clear_plugin(&old_id).await;
    }
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

/// The one error envelope every core and plugin error uses: `{"error": "..."}`.
/// Keeping construction in one place means clients can rely on the shape, and
/// it is the single spot to change if the envelope evolves.
pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// 5xx responses must not leak internals (SQL, paths, driver messages) to
/// clients. The detail is logged; the client gets a generic message.
pub(crate) fn internal_error(what: &str, err: &dyn std::fmt::Display) -> Response {
    tracing::error!(operation = what, error = %err, "internal error");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// Write the audit row for a **state-changing** action; a failed write fails the
/// request.
///
/// `Some(response)` means "stop and return this 5xx" — the same shape as
/// [`AppState::require_admin`] returns, and it keeps a large `Response` out of an
/// `Err` variant (`clippy::result_large_err`).
///
/// Invariant: **a state change is never applied unless its audit row was
/// written.** This must be called *before* the caller mutates anything; the
/// caller applies the change only once it returns `None`. Read paths are
/// unaffected — they stay best-effort and may warn and continue, because a
/// failed read cannot hide a mutation.
///
/// Residual, deliberate: because the audit row is written first, it can precede
/// an apply that subsequently fails (a concurrent uninstall, or a DB write
/// error). The log then records an attempt whose effect did not land. Attempts
/// are auditable; unaudited changes are not acceptable. There is no
/// compensation/rollback for the in-memory registry by design.
pub(crate) async fn audit_state_change(
    audit: &AuditService,
    identity: Option<&Identity>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    details: serde_json::Value,
) -> Option<Response> {
    match audit
        .log(identity, action, resource_type, resource_id, details)
        .await
    {
        Ok(()) => None,
        Err(e) => {
            tracing::error!(
                action,
                resource = resource_id,
                error = %e,
                "audit write failed; failing the request"
            );
            Some(internal_error("audit write", &e))
        }
    }
}

async fn dynamic_dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Resolve under the read lock, clone what we need, release BEFORE awaiting
    // the handler (a slow plugin must not block reload/uninstall writers).
    //
    // The in-flight count is taken under the SAME read lock that resolved the
    // route: that is what makes a concurrent disable unable to miss a request —
    // either it took the write lock first (this request then finds no route and
    // 404s), or the count is already incremented and the drain will see it.
    let lookup = {
        let reg = state.registry.read().await;
        match reg.find(&method, &path) {
            RouteLookup::Found {
                required_permission,
                required_scope,
                handler,
                plugin_id,
                params,
            } => Ok((
                plugin_id.clone(),
                required_permission,
                required_scope,
                handler,
                params,
                state.in_flight.enter(&plugin_id),
            )),
            RouteLookup::NotFound => Err((StatusCode::NOT_FOUND, "route not found".to_string())),
        }
    };

    let (plugin_id, required, required_scope, handler, params, _in_flight) = match lookup {
        Ok(x) => x,
        Err((status, msg)) => {
            return error_response(status, msg);
        }
    };

    dispatch(state, plugin_id, required, required_scope, handler, params, req).await
}

/// Permission gate → build SDK request → call handler.
async fn dispatch(
    state: Arc<AppState>,
    plugin_id: String,
    required: Option<String>,
    required_scope: Option<adjutant_sdk::Scope>,
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
        if let Err(status) =
            authorize(identity.as_ref(), &state.permissions, perm, required_scope.as_ref()).await
        {
            let msg = if status == 401 {
                "authentication required"
            } else {
                "insufficient permissions"
            };
            return error_response(StatusCode::from_u16(status).unwrap(), msg);
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
            // The SDK owns the error → status mapping (SdkError::status), so the
            // core and plugins agree on 400/401/403/404/409/500.
            let status = StatusCode::from_u16(e.status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if status.is_server_error() {
                // Never echo Db/Internal detail (it can carry SQL) to the client.
                internal_error("plugin handler", &e)
            } else {
                tracing::debug!(plugin = %plugin_id, error = %e, "plugin rejected the request");
                error_response(status, e.to_string())
            }
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
    let plugins: Vec<serde_json::Value> = reg
        .infos()
        .into_iter()
        .map(|mut info| {
            // Schedules are runtime state (last run / next run), so fill them at
            // request time rather than from the load-time snapshot.
            info.schedules = state.scheduler.infos(&info.id);
            serde_json::to_value(&info).unwrap_or(serde_json::Value::Null)
        })
        .collect();
    Json(json!({
        "plugins": plugins,
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
        Err(e) => internal_error("audit verification", &e),
    }
}

// ---------------------------------------------------------------------------
// Admin lifecycle (all require core:admin; every action — and every refusal —
// is audit-logged, and each plugin's transitions are serialized)
// ---------------------------------------------------------------------------

/// Bound on the in-flight drain a `disable` performs before closing the pool
/// (issue #89 requirement #1). Five seconds is longer than any handler this core
/// ships, and the timeout is not a failure: the pool stays open until the last
/// request really ends, so a request can never meet a closed pool.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Audit a **refusal** (issue #89 requirement #8). Best effort by design: the
/// refusal stands even when its audit row cannot be written, because refusing is
/// the safe direction — the write failure is logged loudly either way.
async fn audit_refusal(
    state: &AppState,
    identity: Option<&Identity>,
    action: &str,
    name: &str,
    reason: &str,
) {
    if let Err(e) = state
        .audit
        .log(identity, action, "plugin", name, json!({ "refused": reason }))
        .await
    {
        tracing::error!(action, plugin = name, error = %e, "refusal audit write failed");
    }
}

/// A refusal an admin can act on: 409 with the reason, never a bare "refused".
fn refusal(name: &str, reason: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": reason, "plugin": name })),
    )
        .into_response()
}

/// Enable = **load** (issue #89). `dlopen` + `init` + migrate + register
/// permissions + routes — the same body boot uses — and only once that has
/// succeeded is the `core.plugins.enabled` flag written.
///
/// Serialized per plugin by `AppState::lifecycles`, so a double-click cannot run
/// two migrations against one schema. On a load failure the plugin stays
/// disabled, the reason is recorded on its record (visible in
/// `GET /api/plugins`) and the attempt is audited.
async fn enable_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;

    // One lifecycle per plugin: held for the whole load, migrations included.
    let lock = state.lifecycles.get(&name);
    let _guard = lock.lock().await;

    // Validate before any side effect.
    {
        let reg = state.registry.read().await;
        let Some(info) = reg.info(&name) else {
            return error_response(StatusCode::NOT_FOUND, "unknown plugin");
        };
        if info.loaded {
            // Already loaded and serving: enabling is idempotent, and a second
            // load would run a second migration pass for no reason.
            return Json(json!({
                "plugin": name, "enabled": true, "loaded": true,
                "note": "already loaded",
            }))
            .into_response();
        }
        if let Some(reason) = enable_refusal(&name, &reg) {
            drop(reg);
            audit_refusal(&state, identity.as_ref(), "plugin.enable.refused", &name, &reason).await;
            return refusal(&name, &reason);
        }
    }

    // Where the library is. A record always carries the path it was discovered
    // at; if the file has gone, the enable cannot be honoured — refuse with the
    // reason rather than loading something else.
    let path = match state.registry.read().await.record(&name) {
        Some(record) => record.path.clone(),
        None => return error_response(StatusCode::NOT_FOUND, "unknown plugin"),
    };
    if !path.exists() {
        let reason = format!("{name} cannot be enabled: its library {} is missing", path.display());
        audit_refusal(&state, identity.as_ref(), "plugin.enable.refused", &name, &reason).await;
        return refusal(&name, &reason);
    }

    // Audit, then apply. The load below is the state change this row precedes;
    // as with a reload, a failed load leaves an audited attempt whose effect did
    // not land (see `audit_state_change`).
    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "plugin.enable",
        "plugin",
        &name,
        json!({ "library": path.display().to_string() }),
    )
    .await
    {
        return resp;
    }

    // Load first, flag after (requirement #3). The collision check is seeded
    // from the routes already being served, so an enable can never shadow one.
    let env = LoadEnv {
        database_url: &state.config.database_url,
        pool: state.pool.clone(),
        event_tx: state.bus.sender(),
        config: serde_json::Value::Object(Default::default()),
        identity: state.identity.clone(),
        http: state.http.clone(),
    };
    let mut seen_routes = state.registry.read().await.live_route_keys();
    let loaded = match load_plugin(&path, &env, &mut seen_routes).await {
        Ok(l) => l,
        Err(e) => {
            let reason = format!("{name} stays disabled: {e}");
            tracing::error!(plugin = %name, error = %e, "enable failed; plugin stays disabled");
            // The reason lands on the record, where the admin surface shows it.
            state.registry.write().await.record_error(&name, &reason);
            audit_refusal(&state, identity.as_ref(), "plugin.enable.refused", &name, &reason).await;
            return error_response(StatusCode::CONFLICT, reason);
        }
    };
    if loaded.info.id != name {
        // The file declares another id: installing it would create a plugin the
        // admin did not ask for. Refuse; the record is untouched.
        let reason = format!(
            "{name} cannot be enabled: {} declares plugin id {}",
            path.display(),
            loaded.info.id
        );
        tracing::error!(plugin = %name, declared = %loaded.info.id, "enable refused: id mismatch");
        state.registry.write().await.record_error(&name, &reason);
        audit_refusal(&state, identity.as_ref(), "plugin.enable.refused", &name, &reason).await;
        return refusal(&name, &reason);
    }

    // Apply: the slot moves from Known to Live. Nothing else can make it live.
    state.registry.write().await.install(loaded);
    state.identity.set_enabled(&name, true);
    {
        // Re-bind subscriptions (only if none are bound, so a repeated enable
        // cannot double-subscribe) and restart schedules (idempotent).
        let reg = state.registry.read().await;
        let live = reg
            .plugins
            .iter()
            .filter_map(PluginSlot::live)
            .find(|p| p.info.id == name);
        if let Some(lp) = live {
            if !state.bus.subscriber_ids().iter().any(|id| id == &name) {
                for sub in lp.plugin.subscriptions() {
                    state.bus.subscribe(&name, sub);
                }
            }
            let schedules = lp.plugin.schedules();
            if !schedules.is_empty() {
                state.scheduler.start(&name, schedules, state.pool.clone());
            }
        }
    }

    if let Err(e) = sqlx::query("UPDATE core.plugins SET enabled = true, updated_at = now() WHERE id = $1")
        .bind(&name)
        .execute(state.pool.as_ref())
        .await
    {
        // The flag is the durable truth (a restart reads it). If it cannot be
        // written, undo the load so no state contradicts the database.
        tracing::error!(plugin = %name, error = %e, "enable flag write failed; reverting the load");
        let parts = { state.registry.write().await.stop_live(&name) };
        if let Some(p) = parts {
            close_pool(&p);
        }
        state.bus.clear_plugin(&name).await;
        state.identity.set_enabled(&name, false);
        state.scheduler.stop(&name);
        return internal_error("plugin lifecycle", &e);
    }
    tracing::info!(plugin = %name, "plugin enabled (loaded)");
    Json(json!({ "plugin": name, "enabled": true, "loaded": true })).into_response()
}

/// Disable = **tear down** (issue #89). Routing stops first, the requests
/// already inside the plugin finish (bounded, and the wait is reported), and
/// only then does the pool close. Nothing is deleted: schema, rows and grants
/// survive any number of enable/disable cycles (disable is not uninstall).
///
/// The `enabled` flag is written **before** the teardown: there is no fallible
/// step after it, so a crash mid-transition leaves the database saying
/// "disabled" — which is what a restart would load — and never the reverse.
async fn disable_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;

    // One lifecycle per plugin: a disable cannot race an enable's load.
    let lock = state.lifecycles.get(&name);
    let _guard = lock.lock().await;

    // Validate before any side effect.
    {
        let reg = state.registry.read().await;
        if !reg.contains(&name) {
            return error_response(StatusCode::NOT_FOUND, "unknown plugin");
        }
        if let Some(reason) = disable_refusal(&name, &reg) {
            drop(reg);
            audit_refusal(&state, identity.as_ref(), "plugin.disable.refused", &name, &reason).await;
            return refusal(&name, &reason);
        }
    }

    // The declarative minimum is checked first (above), so `auth` is always
    // refused for the permanent reason — it is required — rather than for the
    // situational one. This guard catches what the set cannot: a *third-party*
    // plugin that happens to be the last enabled identity provider. With the
    // dev-header stub off the providers ARE the only way to authenticate, so
    // removing the last one makes every authenticated route — including the
    // admin route that would undo this — unreachable until a process restart.
    if !state.config.allow_dev_headers && state.identity.is_sole_enabled_provider(&name) {
        let reason =
            "refusing to remove the only identity provider while dev headers are off;              register/enable another identity provider, or set ADJUTANT_DEV_HEADERS=true              for a dev instance"
                .to_string();
        audit_refusal(&state, identity.as_ref(), "plugin.disable.refused", &name, &reason).await;
        return refusal(&name, &reason);
    }

    // Audit, then apply (see `audit_state_change`).
    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "plugin.disable",
        "plugin",
        &name,
        json!({}),
    )
    .await
    {
        return resp;
    }

    // Flag first: past this point the teardown cannot fail.
    if let Err(e) = sqlx::query("UPDATE core.plugins SET enabled = false, updated_at = now() WHERE id = $1")
        .bind(&name)
        .execute(state.pool.as_ref())
        .await
    {
        return internal_error("plugin lifecycle", &e);
    }

    // 1. Stop routing: the slot becomes a record, so no new request can reach a
    //    handler. The parts we take are still alive for the requests inside.
    let parts = { state.registry.write().await.stop_live(&name) };
    state.identity.set_enabled(&name, false);
    // Stop its event handlers too: routes 404-ing while its handlers keep
    // appending audit rows and writing to its schema is not "disabled".
    state.bus.clear_plugin(&name).await;
    state.scheduler.stop(&name);

    // 2. Quiesce: let the requests already inside finish, bounded.
    let mut drained = true;
    let mut wait_ms = 0u128;
    if let Some(parts) = parts {
        match state.in_flight.wait_idle(&name, DRAIN_TIMEOUT).await {
            Some(wait) => wait_ms = wait.as_millis(),
            None => {
                drained = false;
                tracing::warn!(
                    plugin = %name,
                    timeout_ms = DRAIN_TIMEOUT.as_millis(),
                    "in-flight drain timed out; the pool stays open until the last request ends"
                );
            }
        }
        // 3. Close the pool. `PgPool::close()` waits for checked-out connections
        //    to be returned, so the choice on timeout is safe rather than
        //    lossy: a request that outlasted the bounded drain still finishes
        //    against a live pool, and the pool closes when it ends. Nothing is
        //    ever closed under a request.
        close_pool(&parts);
    }
    tracing::info!(plugin = %name, drained, wait_ms, "plugin disabled (torn down, nothing deleted)");
    Json(json!({
        "plugin": name,
        "enabled": false,
        "loaded": false,
        "drained": drained,
        "drain_ms": wait_ms,
        "data": "preserved (schema, rows and grants survive)",
    }))
    .into_response()
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
    // Validate before any side effect: the plugin must be loaded.
    if !state.registry.read().await.contains(&name) {
        return error_response(StatusCode::NOT_FOUND, "plugin not loaded");
    }

    // Audit, then apply (see `audit_state_change`).
    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "plugin.uninstall",
        "plugin",
        &name,
        json!({}),
    )
    .await
    {
        return resp;
    }

    {
        let mut reg = state.registry.write().await;
        if !reg.uninstall(&name) {
            return error_response(StatusCode::NOT_FOUND, "plugin not loaded");
        }
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
        return internal_error("plugin lifecycle", &e);
    }
    state.bus.clear_plugin(&name).await;
    state.identity.remove(&name);
    state.scheduler.forget(&name);
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
        &state.config.database_url,
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
            tracing::error!(error = %e, "reload failed; old registry kept");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reload failed; old registry kept",
            );
        }
    };
    let route_count: usize = fresh
        .plugins
        .iter()
        .filter_map(PluginSlot::live)
        .map(|p| p.routes.len())
        .sum();
    let ids: Vec<String> = fresh.plugins.iter().map(|p| p.info().id.clone()).collect();
    let versions: HashMap<String, String> = fresh
        .plugins
        .iter()
        .map(|p| (p.info().id.clone(), p.info().version.clone()))
        .collect();

    // `load_all` ran each plugin's `init`, which re-declares its scope edges;
    // refresh the resolution map before the new generation serves.
    let fresh_hierarchy = match crate::scope_hierarchy::ScopeHierarchy::load(state.pool.as_ref()).await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "reload failed to load scope hierarchy; old registry kept");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reload failed; old registry kept",
            );
        }
    };

    // Audit, then apply. `load_all` above is the validation/preparation step (it
    // refuses with a 5xx on a load error, before this point); the live-registry
    // swap below is the state change this row precedes. Residual: `load_all` has
    // already run migrations/upserts, so a failed audit can leave an attempt whose
    // live effect did not land — the accepted trade (see `audit_state_change`).
    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "plugin.reload",
        "plugin",
        "*",
        json!({ "reloaded": ids, "routes": route_count }),
    )
    .await
    {
        return resp;
    }

    // Swap, then rebind subscriptions and schedules: abort every old task first
    // so no event is handled and no timer fires for both generations.
    {
        let mut reg = state.registry.write().await;
        reg.replace_all(fresh);
    }
    *state.hierarchy.write().await = fresh_hierarchy;
    // A reload replaces every generation: abort all old schedule tasks, then
    // start the new set below.
    state.scheduler.stop_all();
    // Providers from retired plugins stop answering; live ones re-registered
    // themselves during load_all's init (same owner key → replaced in place).
    {
        let reg = state.registry.read().await;
        let live: std::collections::HashSet<String> =
            reg.plugins.iter().map(|p| p.info().id.clone()).collect();
        state.identity.retain(&live);
    }
    clear_plugin_subscriptions(&state.bus).await;
    {
        let reg = state.registry.read().await;
        // Only **loaded** plugins get subscriptions and schedules: a disabled
        // plugin is a record with nothing to bind (issue #89). This is what the
        // old `enabled` flag check was approximating.
        for lp in reg.plugins.iter().filter_map(PluginSlot::live) {
            for sub in lp.plugin.subscriptions() {
                state.bus.subscribe(lp.plugin.id(), sub);
            }
            state.scheduler.start(lp.plugin.id(), lp.plugin.schedules(), state.pool.clone());
        }
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
    use super::{
        audit_state_change, decode_path, disable_refusal, enable_plugin, enable_refusal,
        superuser_refusal, AppState, InFlight, LifecycleLocks, PLUGIN_DEPENDENCIES,
        REQUIRED_PLUGINS,
    };
    use adjutant_sdk::{
        async_trait, AdjutantPlugin, AuditService, EventSubscription, HostDb, Identity, Migration,
        Permission, PermissionService, PluginContext, RouteDefinition, SdkError, SqlValue,
    };
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use std::sync::{Arc, Mutex};
    use tokio::sync::RwLock;

    use crate::config::Config;
    use crate::events::EventBus;
    use crate::identity::IdentityHub;
    use crate::plugin_runtime::{LoadedPlugin, PluginInfo, PluginRegistry, PluginSlot};

    #[test]
    fn superuser_boot_is_refused_unless_explicitly_allowed() {
        assert!(superuser_refusal(false, false).is_ok());
        assert!(superuser_refusal(true, true).is_ok(), "the opt-out permits it");
        let err = superuser_refusal(true, false).expect_err("a superuser must be refused");
        assert!(err.contains("superuser"), "names the reason: {err}");
        assert!(err.contains("ADJUTANT_ALLOW_SUPERUSER"), "names the opt-out: {err}");
    }

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

    /// A host DB whose audit write always fails, so the real failure path runs.
    struct FailingAuditDb;

    #[async_trait]
    impl HostDb for FailingAuditDb {
        async fn execute(&self, _sql: String, _params: Vec<SqlValue>) -> Result<u64, SdkError> {
            Err(SdkError::Db("audit table is read-only".into()))
        }
        async fn query(
            &self,
            _sql: String,
            _params: Vec<SqlValue>,
        ) -> Result<Vec<serde_json::Value>, SdkError> {
            Err(SdkError::Db("core.users unavailable".into()))
        }
    }

    /// A host DB that lets every permission check pass (`n = 1`), so
    /// `require_admin` succeeds without a database.
    struct AllowPermissionsDb;

    #[async_trait]
    impl HostDb for AllowPermissionsDb {
        async fn execute(&self, _sql: String, _params: Vec<SqlValue>) -> Result<u64, SdkError> {
            Ok(1)
        }
        async fn query(
            &self,
            _sql: String,
            _params: Vec<SqlValue>,
        ) -> Result<Vec<serde_json::Value>, SdkError> {
            Ok(vec![serde_json::json!({ "n": 1 })])
        }
    }

    /// A minimal plugin for the registry fixture; `init` is never called.
    struct TinyPlugin;

    #[async_trait]
    impl AdjutantPlugin for TinyPlugin {
        fn id(&self) -> &str {
            "hello"
        }
        fn name(&self) -> &str {
            "Hello"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        async fn init(&mut self, _ctx: PluginContext) -> Result<(), SdkError> {
            Ok(())
        }
        fn routes(&self) -> Vec<RouteDefinition> {
            Vec::new()
        }
        fn migrations(&self) -> Vec<Migration> {
            Vec::new()
        }
        fn permissions_granted(&self) -> Vec<Permission> {
            Vec::new()
        }
        fn subscriptions(&self) -> Vec<EventSubscription> {
            Vec::new()
        }
    }

    /// A path that exists, so the enable handler gets past its "library is
    /// present?" validation and reaches the audit step this test is about.
    fn existing_path() -> std::path::PathBuf {
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/bin/sh"))
    }

    /// A slot fixture for the lifecycle tests: `loaded = false` yields the
    /// disabled *record*; `true` yields the loaded half.
    fn loaded_hello(loaded: bool) -> PluginSlot {
        slot("hello", loaded)
    }

    /// A registry slot with an arbitrary id, for the refusal rules.
    fn slot(id: &str, loaded: bool) -> PluginSlot {
        let info = PluginInfo {
            id: id.into(),
            name: id.into(),
            version: "0.0.1".into(),
            enabled: loaded,
            routes: 0,
            kind: "native".into(),
            isolated: true,
            loaded,
            last_error: None,
            permissions: Vec::new(),
            schedules: Vec::new(),
            route_list: Vec::new(),
        };
        if loaded {
            PluginSlot::Live(LoadedPlugin {
                plugin: Box::new(TinyPlugin),
                library: None,
                pool: None,
                routes: Vec::new(),
                path: std::path::PathBuf::new(),
                info,
            })
        } else {
            PluginSlot::Known(crate::plugin_runtime::PluginRecord::from_info(
                info,
                existing_path(),
            ))
        }
    }

    /// An `AppState` whose audit writes always fail and whose pool is lazy (never
    /// connected), so `enable_plugin` can be driven end to end: the permission
    /// gate passes, the audit write fails, and any mutation would be a bug.
    fn failing_audit_state() -> Arc<AppState> {
        let pool = Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://adjutant:***@127.0.0.1:1/adjutant_test")
                .expect("lazy pool URL parses"),
        );
        let config = Config {
            allow_dev_headers: true,
            ..Config::default()
        };
        Arc::new(AppState {
            pool,
            permissions: PermissionService::new(Arc::new(AllowPermissionsDb)),
            audit: AuditService::new(Arc::new(FailingAuditDb), "core".into()),
            registry: RwLock::new(PluginRegistry {
                plugins: vec![loaded_hello(false)],
                retired: Vec::new(),
            }),
            bus: EventBus::new(),
            config: Arc::new(config),
            identity: IdentityHub::new(),
            http: crate::host::CoreHttp::new(),
            hierarchy: RwLock::new(crate::scope_hierarchy::ScopeHierarchy::default()),
            scheduler: crate::scheduler::Scheduler::new(),
            outbox_mismatches: Mutex::new(0),
            relay: crate::outbox::Relay::new(),
            in_flight: InFlight::new(),
            lifecycles: LifecycleLocks::new(),
        })
    }

    fn dev_admin_request() -> axum::extract::Request {
        axum::http::Request::builder()
            .header("x-dev-user", "christopher")
            .header("x-dev-role", "chief")
            .body(axum::body::Body::empty())
            .expect("request builds")
    }

    /// Issue #24: the lifecycle routes used to log the audit failure and still
    /// answer success, so a mutation could commit with no audit row. The shared
    /// helper they call must instead tell the caller the action failed (5xx).
    #[tokio::test]
    async fn failed_audit_write_fails_a_state_changing_request() {
        let audit = AuditService::new(Arc::new(FailingAuditDb), "core".into());
        let id = Identity::new("christopher", vec!["chief".into()]);
        let resp = audit_state_change(
            &audit,
            Some(&id),
            "plugin.enable",
            "plugin",
            "hello",
            serde_json::json!({}),
        )
        .await
        .expect("a failed audit write must fail the request");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Issue #24 follow-up: the order must be validate → audit → apply. A failed
    /// audit write must leave the plugin untouched. This drives the real
    /// `enable_plugin` handler end to end (permission gate included); the old
    /// order applied the registry change first, so this test would have found the
    /// plugin enabled after a 5xx.
    #[tokio::test]
    async fn failed_audit_write_does_not_apply_the_change() {
        let state = failing_audit_state();
        let resp = enable_plugin(
            State(state.clone()),
            Path("hello".to_string()),
            dev_admin_request(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a failed audit write must fail the request"
        );

        let reg = state.registry.read().await;
        let info = reg.info("hello").expect("fixture plugin");
        assert!(!info.enabled, "the flag must not be flipped on audit failure");
        assert!(!info.loaded, "nothing may be loaded on audit failure");
        drop(reg);
        assert!(
            !state.bus.subscriber_ids().iter().any(|id| id == "hello"),
            "no subscription may be bound on audit failure"
        );
    }

    /// Issue #89 requirement #1: the drain a `disable` performs is observable
    /// and bounded. The guard decrements on drop, so a handler that panics or
    /// returns early cannot leave a plugin looking busy forever.
    #[tokio::test]
    async fn in_flight_counts_and_drains() {
        let flights = InFlight::new();
        let a = flights.enter("store");
        let b = flights.enter("store");
        assert_eq!(flights.count("store"), 2);
        assert!(
            flights
                .wait_idle("store", std::time::Duration::from_millis(50))
                .await
                .is_none(),
            "a busy plugin reports the bounded drain as timed out"
        );
        drop(a);
        assert_eq!(flights.count("store"), 1);
        drop(b);
        assert!(
            flights
                .wait_idle("store", std::time::Duration::from_millis(50))
                .await
                .is_some(),
            "an idle plugin drains immediately"
        );
        assert_eq!(flights.count("store"), 0);
        // Another plugin's requests are not this plugin's business.
        let _other = flights.enter("hello");
        assert!(
            flights
                .wait_idle("store", std::time::Duration::from_millis(50))
                .await
                .is_some()
        );
    }

    /// Issue #89 requirement #6: the non-disableable minimum is a *declarative*
    /// set and the refusal carries its reason, so an operator is told why.
    #[test]
    fn the_required_plugins_refuse_with_their_reason() {
        let reg = PluginRegistry::new(vec![slot("auth", true), slot("membership", true)]);
        for (id, why) in REQUIRED_PLUGINS {
            let reason = disable_refusal(id, &reg)
                .unwrap_or_else(|| panic!("{id} must not be disableable"));
            assert!(reason.contains(why), "the reason is carried: {reason}");
            assert!(reason.contains("required"), "and says it is required: {reason}");
        }
        // A plugin outside the set is not refused.
        assert!(disable_refusal("hello", &reg).is_none());
        // The set is the one the PR names; adding a plugin is a one-line change.
        assert!(REQUIRED_PLUGINS.iter().any(|(id, _)| *id == "auth"));
        assert!(REQUIRED_PLUGINS.iter().any(|(id, _)| *id == "membership"));
    }

    /// Issue #89 requirement #7: `store` needs `stripe`. Disabling a dependency
    /// an enabled plugin needs is refused by name, and enabling the dependent
    /// while its dependency is not loaded is refused too — never a silent
    /// dependent calling nothing.
    #[test]
    fn dependencies_are_refused_in_both_directions() {
        assert!(PLUGIN_DEPENDENCIES.contains(&("store", "stripe")));

        // stripe disabled while store is loaded: refuse, naming store.
        let reg = PluginRegistry::new(vec![slot("store", true), slot("stripe", false)]);
        let reason = disable_refusal("stripe", &reg).expect("stripe is needed by store");
        assert!(reason.contains("store"), "names the dependent: {reason}");
        println!("[dependencies] disable refusal: {reason}");

        // stripe disabled and store disabled: nothing needs it, so it is allowed.
        let reg = PluginRegistry::new(vec![slot("store", false), slot("stripe", false)]);
        assert!(disable_refusal("stripe", &reg).is_none());

        // store enabled while stripe is not loaded: refuse, naming stripe.
        let reg = PluginRegistry::new(vec![slot("store", false), slot("stripe", false)]);
        let reason = enable_refusal("store", &reg).expect("store needs stripe");
        assert!(reason.contains("stripe"), "names the dependency: {reason}");
        println!("[dependencies] enable refusal: {reason}");

        // stripe loaded: store may be enabled.
        let reg = PluginRegistry::new(vec![slot("store", false), slot("stripe", true)]);
        assert!(enable_refusal("store", &reg).is_none());

        // A plugin with no dependencies is unaffected either way.
        let reg = PluginRegistry::new(vec![slot("hello", true)]);
        assert!(disable_refusal("hello", &reg).is_none());
        assert!(enable_refusal("hello", &reg).is_none());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("plugin runtime: {0}")]
    Plugin(#[from] crate::plugin_runtime::PluginRuntimeError),
    #[error("{0}")]
    Superuser(String),
}

