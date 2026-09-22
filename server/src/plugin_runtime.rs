//! Plugin runtime: discovery, dynamic loading, validation, init, migrations,
//! and the **lifecycle registry** (SPEC §15 M2: load, enable, disable,
//! uninstall, hot-reload).
//!
//! Boot / reload order per plugin (SPEC §5.2 lifecycle):
//! 1. Discover `*.so` in `plugin_dir`
//! 2. `dlopen` + resolve `adjutant_plugin_create` (sdk::ENTRY_SYMBOL)
//! 3. Validate id (incl. reserved core names), route namespaces, duplicates
//! 4. Skip if the DB row says `uninstalled` (before any side effects)
//! 5. Create PostgreSQL schema, run pending migrations
//! 6. Register granted permissions into `core.permissions`
//! 7. Upsert `core.plugins`, read `enabled`
//! 8. `init(ctx)`, collect routes + subscriptions
//!
//! **Library lifetime rule:** unloading a `cdylib` whose futures or trait
//! objects are still reachable is UB. Superseded and uninstalled plugins move
//! to `retired` and their `.so` stays mapped for the process lifetime. Cost:
//! each hot-reload keeps one extra mapping (~1 MB) — bounded by reload count,
//! documented, and strictly safer than unloading under live requests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use sqlx::PgPool;
use std::sync::Arc;

use adjutant_sdk::{
    AdjutantPlugin, EventBusHandle, PermissionService, PluginContext, RouteHandler,
    RouteDefinition,
};

use crate::db::run_migration;

/// Every opened plugin library, kept mapped for the process lifetime.
///
/// The registry holds live plugins and `retired` holds uninstalled/superseded
/// ones — but a *load error* would drop the only `Arc<Library>` while the core
/// may still own trait objects into that code (an identity provider registered
/// during `init`, for example). Dropping those then calls vtables in unmapped
/// memory: SIGSEGV on the error path (seen while bringing up auth). Parking
/// every library here makes "never unload" unconditional instead of
/// dependent on which locals happen to still be alive.
static PARKED: std::sync::LazyLock<Mutex<Vec<Arc<libloading::Library>>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

fn park(lib: Arc<libloading::Library>) {
    PARKED.lock().expect("parking lot poisoned").push(lib);
}

/// Core-owned route namespaces plugins may never claim.
const RESERVED_IDS: &[&str] = &["plugins", "events", "audit", "core"];

/// A loaded plugin: the boxed trait object, the library that owns its code,
/// its routes, its admin snapshot, and its enabled flag.
pub struct LoadedPlugin {
    pub plugin: Box<dyn AdjutantPlugin>,
    /// Kept alive for the library-lifetime rule (see module docs): never
    /// read, never dropped until the registry retires it.
    #[allow(dead_code)]
    pub(crate) library: Arc<libloading::Library>,
    pub routes: Vec<RouteDefinition>,
    pub enabled: bool,
    pub info: PluginInfo,
}

/// Snapshot for the admin surface / health endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub routes: usize,
    pub permissions: Vec<String>,
    /// Full route table — lets `adjutant test-plugin` probe every route
    /// without hardcoding plugin knowledge.
    pub route_list: Vec<RouteInfo>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteInfo {
    pub method: String,
    pub path: String,
    pub permission: Option<String>,
}

/// Result of resolving a request path against the registry.
pub enum RouteLookup {
    Found {
        plugin_id: String,
        required_permission: Option<String>,
        handler: RouteHandler,
        /// Captures from a templated route (`/api/missions/{id}`).
        params: HashMap<String, String>,
    },
    /// Route exists but its plugin is disabled (SPEC: disable = stop serving).
    Disabled { plugin_id: String },
    NotFound,
}

/// Match a route template against a request path.
///
/// A `{name}` segment captures exactly one non-empty path segment, so a capture
/// can never span a `/`. Different segment counts never match. Returns the
/// captures (empty for a literal route) or `None`.
fn match_path(template: &str, path: &str) -> Option<HashMap<String, String>> {
    let t: Vec<&str> = template.split('/').collect();
    let p: Vec<&str> = path.split('/').collect();
    if t.len() != p.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (ts, ps) in t.iter().zip(p.iter()) {
        if let Some(name) = ts.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if ps.is_empty() {
                return None;
            }
            params.insert(name.to_string(), (*ps).to_string());
        } else if ts != ps {
            return None;
        }
    }
    Some(params)
}

/// Validate `{name}` captures in a route path: a capture must be a whole
/// segment with a non-empty `[a-z0-9_]` name, unique within the path.
fn validate_route_path(path: &str) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        if !seg.contains('{') && !seg.contains('}') {
            continue;
        }
        let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            return Err(format!("route {path}: capture must occupy a whole segment (`{{name}}`)"));
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "route {path}: capture name {name:?} must be [a-z0-9_]+"
            ));
        }
        if seen.contains(&name) {
            return Err(format!("route {path}: duplicate capture {name:?}"));
        }
        seen.push(name);
    }
    Ok(())
}

pub struct PluginRegistry {
    pub plugins: Vec<LoadedPlugin>,
    /// Superseded / uninstalled plugins. Kept mapped, never referenced again —
    /// see the library lifetime rule above.
    pub(crate) retired: Vec<LoadedPlugin>,
}

impl PluginRegistry {
    fn new(plugins: Vec<LoadedPlugin>) -> Self {
        Self { plugins, retired: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Admin snapshots for every live plugin.
    pub fn infos(&self) -> Vec<PluginInfo> {
        self.plugins.iter().map(|p| p.info.clone()).collect()
    }

    /// Resolve `METHOD path` against live plugin routes.
    ///
    /// Literal routes win over templated ones, so a specific path is never
    /// shadowed by a capture. Captures are delivered to the handler.
    pub fn find(&self, method: &str, path: &str) -> RouteLookup {
        let mut templated: Option<(&LoadedPlugin, &RouteDefinition, HashMap<String, String>)> = None;
        for p in &self.plugins {
            for r in &p.routes {
                if r.method.as_str() != method {
                    continue;
                }
                if r.path == path {
                    return Self::resolve(p, r, HashMap::new());
                }
                if templated.is_none() {
                    if let Some(params) = match_path(&r.path, path) {
                        templated = Some((p, r, params));
                    }
                }
            }
        }
        match templated {
            Some((p, r, params)) => Self::resolve(p, r, params),
            None => RouteLookup::NotFound,
        }
    }

    fn resolve(p: &LoadedPlugin, r: &RouteDefinition, params: HashMap<String, String>) -> RouteLookup {
        if p.enabled {
            RouteLookup::Found {
                plugin_id: p.info.id.clone(),
                required_permission: r.required_permission.clone(),
                handler: r.handler.clone(),
                params,
            }
        } else {
            RouteLookup::Disabled { plugin_id: p.info.id.clone() }
        }
    }

    /// Flip `enabled` in memory. Returns false when the plugin isn't live
    /// (caller owns the DB write + audit).
    pub fn set_enabled(&mut self, id: &str, on: bool) -> bool {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.info.id == id) {
            p.enabled = on;
            p.info.enabled = on;
            true
        } else {
            false
        }
    }

    /// Uninstall: remove from the live set (routes stop resolving) but keep
    /// the library mapped. Returns false when not live.
    pub fn uninstall(&mut self, id: &str) -> bool {
        if let Some(i) = self.plugins.iter().position(|p| p.info.id == id) {
            let p = self.plugins.remove(i);
            tracing::info!(plugin = id, "uninstalled (library retired, data archived)");
            self.retired.push(p);
            true
        } else {
            false
        }
    }

    /// Swap in a freshly loaded registry (hot-reload). The old live set is
    /// retired, not dropped, so in-flight requests keep valid code.
    pub fn replace_all(&mut self, fresh: PluginRegistry) {
        let PluginRegistry { plugins, retired: _ } = fresh;
        let old = std::mem::take(&mut self.plugins);
        self.retired.extend(old);
        self.plugins = plugins;
    }

    pub fn retired_count(&self) -> usize {
        self.retired.len()
    }
}

/// Load every *installable* plugin in `dir`. Fails the boot on any invalid
/// plugin — a half-loaded plugin set is worse than refusing to start (SPEC §14:
/// fail loud). Uninstalled plugins are skipped before side effects.
pub async fn load_all(
    dir: &Path,
    pool: Arc<PgPool>,
    event_tx: tokio::sync::broadcast::Sender<adjutant_sdk::Event>,
    config: serde_json::Value,
    identity: std::sync::Arc<crate::identity::IdentityHub>,
    http: std::sync::Arc<crate::host::CoreHttp>,
) -> Result<PluginRegistry, PluginRuntimeError> {
    let mut plugins: Vec<LoadedPlugin> = Vec::new();
    let mut seen_ids: HashMap<String, ()> = HashMap::new();
    let mut seen_routes: HashMap<(String, String), ()> = HashMap::new();
    // One shared host DB impl for every plugin context (cheap: Arc clone).
    let core_db = crate::host::CoreDb::new(pool.clone());

    if !dir.exists() {
        return Err(PluginRuntimeError::DirMissing(dir.display().to_string()));
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| PluginRuntimeError::Io(dir.display().to_string(), e))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "so"))
        .collect();
    entries.sort();

    for path in entries {
        let lib = unsafe { libloading::Library::new(&path) }.map_err(|e| {
            PluginRuntimeError::Load(path.display().to_string(), e.to_string())
        })?;
        let lib = Arc::new(lib);
        park(lib.clone()); // mapped until process exit, whatever happens below

        // Scope the libloading `Symbol` (a raw-pointer borrow) to this block
        // so it cannot be live across any await below — a Symbol in the
        // generator state makes the future unprovable as Send.
        let mut plugin: Box<dyn AdjutantPlugin> = {
            let factory =
                unsafe { lib.get::<adjutant_sdk::PluginFactory>(adjutant_sdk::ENTRY_SYMBOL) }
                    .map_err(|e| {
                        PluginRuntimeError::Load(path.display().to_string(), e.to_string())
                    })?;
            unsafe { Box::from_raw(factory()) }
        };
        let id = plugin.id().to_string();

        // --- validation -----------------------------------------------------
        validate_plugin_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        if seen_ids.contains_key(&id) {
            return Err(PluginRuntimeError::Invalid(id, "duplicate plugin id".into()));
        }

        // --- uninstalled? skip before any side effect -----------------------
        // (drop order: plugin before library — `plugin` is declared later.)
        let pre: Option<(bool, bool)> = sqlx::query_as(
            "SELECT enabled, uninstalled FROM core.plugins WHERE id = $1",
        )
        .bind(&id)
        .fetch_optional(pool.as_ref())
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        if let Some((_, true)) = pre {
            tracing::info!(plugin = %id, "skipping uninstalled plugin (.so still on disk)");
            continue;
        }

        // --- schema + migrations -------------------------------------------
        run_migration(
            &pool,
            &id,
            0,
            "create_schema",
            &format!("CREATE SCHEMA IF NOT EXISTS \"{id}\";"),
        )
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;

        // --- per-plugin config (DB row) + enabled state ---------------------
        // Read BEFORE ctx construction: init() needs ctx.config (OIDC settings
        // etc. are per-plugin and admin-editable via the config column).
        let existing: Option<(bool, serde_json::Value)> = sqlx::query_as(
            "SELECT enabled, config FROM core.plugins WHERE id = $1",
        )
        .bind(&id)
        .fetch_optional(pool.as_ref())
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        let (enabled, row_config) = existing.unwrap_or((true, serde_json::Value::Null));
        sqlx::query(
            "INSERT INTO core.plugins (id, version, enabled) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, updated_at = now()",
        )
        .bind(&id)
        .bind(plugin.version())
        .bind(enabled)
        .execute(pool.as_ref())
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        // A non-empty row config wins over the global default from the caller.
        let plugin_config = match &row_config {
            v if v.as_object().map(|o| !o.is_empty()).unwrap_or(false) => v.clone(),
            _ => config.clone(),
        };

        let ctx = PluginContext {
            plugin_id: id.clone(),
            // Host-mediated: these Arc<dyn Host…> impls live in the core, so no
            // sqlx/tokio is ever linked into the plugin (see SDK host-I/O note).
            db: adjutant_sdk::DbHandle::new(
                crate::host::CoreDb::for_plugin(pool.clone(), id.clone()),
                id.clone(),
            ),
            config: plugin_config.clone(),
            events: EventBusHandle::new(
                crate::host::CoreEvents::new(pool.clone(), event_tx.clone(), id.clone()),
                id.clone(),
            ),
            permissions: PermissionService::new(core_db.clone()),
            audit: adjutant_sdk::AuditService::new(core_db.clone(), id.clone()),
            identity: identity.clone(),
            http: http.clone(),
        };

        plugin
            .init(ctx)
            .await
            .map_err(|e| PluginRuntimeError::Init(id.clone(), e.to_string()))?;

        for m in plugin.migrations() {
            if m.version < 1 {
                return Err(PluginRuntimeError::Invalid(
                    id.clone(),
                    format!("migration '{}' version must be >= 1", m.name),
                ));
            }
            run_migration(&pool, &id, m.version, &m.name, &m.sql)
                .await
                .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        }

        // --- permissions ----------------------------------------------------
        // Owned rows, indexed loop: holding a `slice::Iter` across the await
        // below poisons the generator's auto-trait proof (rustc reports
        // "Send not general enough" and axum then refuses the Handler).
        let granted = plugin.permissions_granted();
        let perm_rows: Vec<(String, String)> = granted
            .iter()
            .map(|p| (p.id.clone(), p.description.clone()))
            .collect();
        // Consume the owned Vec by value: a `slice::Iter<'_, Permission>`
        // held across the awaits poisons the generator's Send proof.
        for (pid, pdesc) in perm_rows {
            sqlx::query(
                "INSERT INTO core.permissions (id, description) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET description = EXCLUDED.description",
            )
            .bind(&pid)
            .bind(&pdesc)
            .execute(pool.as_ref())
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        }

        // --- route validation ------------------------------------------------
        let routes = plugin.routes();
        for r in &routes {
            let prefix = format!("/api/{id}");
            if !(r.path == prefix || r.path.starts_with(&format!("{prefix}/"))) {
                return Err(PluginRuntimeError::Invalid(
                    id.clone(),
                    format!("route {} escapes plugin namespace (must start with {prefix})", r.path),
                ));
            }
            validate_route_path(&r.path)
                .map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
            if let Some(required) = &r.required_permission {
                if !granted.iter().any(|p| &p.id == required) {
                    return Err(PluginRuntimeError::Invalid(
                        id.clone(),
                        format!(
                            "route {} requires permission '{required}' which the plugin does not grant",
                            r.path
                        ),
                    ));
                }
            }
            let key = (r.method.as_str().to_string(), r.path.clone());
            if seen_routes.contains_key(&key) {
                return Err(PluginRuntimeError::Invalid(
                    id.clone(),
                    format!("duplicate route {} {}", key.0, key.1),
                ));
            }
            seen_routes.insert(key, ());
        }

        // --- enabled state (row created above; version tracked at upsert) ---
        tracing::info!(
            id,
            version = plugin.version(),
            routes = routes.len(),
            permissions = granted.len(),
            enabled,
            "plugin loaded"
        );

        let info = PluginInfo {
            id: id.clone(),
            name: plugin.name().to_string(),
            version: plugin.version().to_string(),
            enabled,
            routes: routes.len(),
            permissions: granted.iter().map(|p| p.id.clone()).collect(),
            route_list: routes
                .iter()
                .map(|r| RouteInfo {
                    method: r.method.as_str().to_string(),
                    path: r.path.clone(),
                    permission: r.required_permission.clone(),
                })
                .collect(),
        };
        seen_ids.insert(id, ());

        plugins.push(LoadedPlugin {
            plugin,
            library: lib,
            routes,
            enabled,
            info,
        });
    }

    Ok(PluginRegistry::new(plugins))
}

/// Full load-time id check: shape rule plus the core-reserved names.
fn validate_plugin_id(id: &str) -> Result<(), String> {
    validate_id(id)?;
    if RESERVED_IDS.contains(&id) {
        return Err(format!("id {id:?} is reserved by the core"));
    }
    Ok(())
}

/// Plugin ids: same rule as schema names (they ARE the schema).
fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 31
        && id.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid id {id:?}: expected [a-z][a-z0-9_]{{0,30}}"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginRuntimeError {
    #[error("plugin dir missing: {0} (build plugins first)")]
    DirMissing(String),
    #[error("io error reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("failed to load plugin {0}: {1}")]
    Load(String, String),
    #[error("invalid plugin {0}: {1}")]
    Invalid(String, String),
    #[error("plugin {0} init failed: {1}")]
    Init(String, String),
    #[error("plugin {0} migration failed: {1}")]
    Migration(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use adjutant_sdk::{
        async_trait, route_handler, EventSubscription, PluginRequest, PluginResponse, SdkError,
    };
    use std::sync::OnceLock;

    struct TestPlugin {
        ctx: OnceLock<PluginContext>,
    }

    impl TestPlugin {
        fn new() -> Self {
            Self { ctx: OnceLock::new() }
        }
    }

    #[async_trait]
    impl AdjutantPlugin for TestPlugin {
        fn id(&self) -> &str {
            "test_plugin"
        }
        fn name(&self) -> &str {
            "Test"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
            let _ = self.ctx.set(ctx);
            Ok(())
        }
        fn routes(&self) -> Vec<RouteDefinition> {
            vec![
                RouteDefinition::get(
                    "/api/test_plugin/open",
                    route_handler(|_: PluginRequest| async {
                        PluginResponse::json(200, &serde_json::json!({"ok": true}))
                    }),
                ),
                RouteDefinition::get_protected(
                    "/api/test_plugin/secret",
                    "test_plugin:read",
                    route_handler(|_: PluginRequest| async {
                        PluginResponse::json(200, &serde_json::json!({"ok": true}))
                    }),
                ),
            ]
        }
        fn permissions_granted(&self) -> Vec<adjutant_sdk::Permission> {
            vec![adjutant_sdk::Permission::new("test_plugin:read", "read")]
        }
        fn subscriptions(&self) -> Vec<EventSubscription> {
            Vec::new()
        }
    }

    /// A real `Library` handle for the lifetime field. The field is never read
    /// (it exists so plugin code outlives its handlers), so any valid mapping
    /// does — the test binary itself, with libc as a portable fallback. That is
    /// enough to exercise the registry logic for real instead of against the
    /// empty placeholder this fixture used to be.
    fn test_library() -> Arc<libloading::Library> {
        let exe = std::env::current_exe().expect("current_exe");
        let lib = unsafe { libloading::Library::new(&exe) }
            .or_else(|_| unsafe { libloading::Library::new("libc.so.6") })
            .expect("a Library handle for registry tests");
        Arc::new(lib)
    }

    /// A real `LoadedPlugin` fixture.
    fn loaded(id: &str, enabled: bool, method: &str, path: &str, perm: Option<&str>) -> LoadedPlugin {
        let lib = test_library();
        let handler = route_handler(|_: PluginRequest| async {
            PluginResponse::json(200, &serde_json::json!({"ok": true}))
        });
        let route = match perm {
            Some(p) => RouteDefinition::get_protected(path, p, handler),
            None => RouteDefinition::get(path, handler),
        };
        let info = PluginInfo {
            id: id.into(),
            name: id.into(),
            version: "0.0.1".into(),
            enabled,
            routes: 1,
            permissions: perm.map(|p| vec![p.to_string()]).unwrap_or_default(),
            route_list: vec![RouteInfo {
                method: method.into(),
                path: path.into(),
                permission: perm.map(String::from),
            }],
        };
        LoadedPlugin {
            plugin: Box::new(TestPlugin::new()),
            library: lib,
            routes: vec![route],
            enabled,
            info,
        }
    }

    #[test]
    fn find_reports_enabled_disabled_and_unknown() {
        let reg = PluginRegistry::new(vec![
            loaded("alpha", true, "GET", "/api/alpha/thing", Some("alpha:read")),
            loaded("beta", false, "GET", "/api/beta/thing", None),
        ]);

        match reg.find("GET", "/api/alpha/thing") {
            RouteLookup::Found { plugin_id, required_permission, .. } => {
                assert_eq!(plugin_id, "alpha");
                assert_eq!(required_permission.as_deref(), Some("alpha:read"));
            }
            _ => panic!("enabled route must resolve"),
        }
        // Disabled plugin: the route exists but must not serve (SPEC lifecycle).
        match reg.find("GET", "/api/beta/thing") {
            RouteLookup::Disabled { plugin_id } => assert_eq!(plugin_id, "beta"),
            _ => panic!("disabled plugin must report Disabled, not Found/NotFound"),
        }
        assert!(matches!(reg.find("GET", "/api/nope"), RouteLookup::NotFound));
        // Method mismatch is NotFound, not a silent match.
        assert!(matches!(reg.find("POST", "/api/alpha/thing"), RouteLookup::NotFound));
    }

    #[test]
    fn templated_routes_capture_one_segment() {
        let reg = PluginRegistry::new(vec![loaded(
            "missions",
            true,
            "GET",
            "/api/missions/{id}",
            Some("missions:read"),
        )]);
        match reg.find("GET", "/api/missions/42") {
            RouteLookup::Found { params, plugin_id, .. } => {
                assert_eq!(plugin_id, "missions");
                assert_eq!(params.get("id").map(String::as_str), Some("42"));
            }
            _ => panic!("template must match a one-segment path"),
        }
        // A capture is exactly one segment — never a prefix match across slashes.
        assert!(matches!(reg.find("GET", "/api/missions/42/approve"), RouteLookup::NotFound));
        assert!(matches!(reg.find("GET", "/api/missions"), RouteLookup::NotFound));
        // An empty segment is not a capture.
        assert!(matches!(reg.find("GET", "/api/missions/"), RouteLookup::NotFound));
    }

    #[test]
    fn literal_routes_win_over_templates() {
        let reg = PluginRegistry::new(vec![
            loaded("missions", true, "GET", "/api/missions/{id}", None),
            loaded("missions", true, "GET", "/api/missions/current", Some("missions:read")),
        ]);
        match reg.find("GET", "/api/missions/current") {
            RouteLookup::Found { required_permission, params, .. } => {
                assert_eq!(required_permission.as_deref(), Some("missions:read"));
                assert!(params.is_empty(), "literal match has no captures");
            }
            _ => panic!("literal route must win"),
        }
    }

    #[test]
    fn route_path_validation_rejects_malformed_captures() {
        assert!(validate_route_path("/api/missions/{id}").is_ok());
        assert!(validate_route_path("/api/missions/{mission_id}/approve").is_ok());
        assert!(validate_route_path("/api/missions/plain").is_ok());
        assert!(validate_route_path("/api/missions/{id").is_err(), "unclosed");
        assert!(validate_route_path("/api/missions/x{id}").is_err(), "partial segment");
        assert!(validate_route_path("/api/missions/{}").is_err(), "empty name");
        assert!(validate_route_path("/api/missions/{ID}").is_err(), "uppercase name");
        assert!(validate_route_path("/api/missions/{id}/{id}").is_err(), "duplicate name");
    }

    #[test]
    fn set_enabled_and_uninstall_update_serving() {
        let mut reg = PluginRegistry::new(vec![loaded("alpha", true, "GET", "/api/alpha/thing", None)]);
        assert!(reg.set_enabled("alpha", false));
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::Disabled { .. }));
        assert!(reg.set_enabled("alpha", true));
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::Found { .. }));
        assert!(!reg.set_enabled("ghost", false), "unknown plugin must report false");

        assert!(reg.uninstall("alpha"));
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::NotFound));
        assert_eq!(reg.retired_count(), 1, "library must be retired, never dropped");
        assert!(reg.is_empty());
        assert!(!reg.uninstall("ghost"));
    }

    #[test]
    fn replace_all_retires_previous_generation() {
        let mut reg = PluginRegistry::new(vec![loaded("alpha", true, "GET", "/api/alpha/thing", None)]);
        reg.replace_all(PluginRegistry::new(vec![loaded(
            "alpha",
            true,
            "GET",
            "/api/alpha/thing",
            Some("alpha:read"),
        )]));
        assert_eq!(reg.retired_count(), 1);
        match reg.find("GET", "/api/alpha/thing") {
            RouteLookup::Found { required_permission, .. } => {
                assert_eq!(required_permission.as_deref(), Some("alpha:read"));
            }
            _ => panic!("fresh generation must serve"),
        }
    }

    #[test]
    fn id_validation_matches_schema_rule() {
        assert!(validate_id("hello").is_ok());
        assert!(validate_id("shared_postgres").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("Hello").is_err());
        assert!(validate_id("0day").is_err());
        assert!(validate_id("drop table").is_err());
    }

    #[test]
    fn validate_plugin_id_rejects_reserved_and_malformed() {
        for r in RESERVED_IDS {
            // Each reserved name is shape-valid, so only the reserved check can
            // reject it — which is exactly what the old test failed to assert.
            assert!(validate_id(r).is_ok(), "{r} must be shape-valid for the reserved rule to matter");
            assert!(validate_plugin_id(r).is_err(), "reserved id {r} must be rejected at load");
        }
        assert!(validate_plugin_id("hello").is_ok());
        assert!(validate_plugin_id("Hello").is_err());
        assert!(validate_plugin_id("drop table").is_err());
    }

}
