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

/// Plugin ids the core keeps for itself, or for a first-party plugin whose
/// grants are keyed on its id.
///
/// **Enforced for untrusted plugins only** — sandboxed WASM guests, whose id
/// comes from a manifest they declare ([`validate_untrusted_plugin_id`]). It is
/// deliberately NOT enforced for native `.so` plugins: they run in-process and
/// unsandboxed, the SDK docs already state native loading "is not a security
/// boundary", and the first-party `auth`/`membership` plugins legitimately use
/// these ids ([`validate_plugin_id`] applies only the shape rule to them).
///
/// The list exists because [`crate::schema::core_grants`] keys its `core.*`
/// allowlist on the plugin id string: a guest that declared `id: "auth"` would
/// inherit `SELECT/INSERT/UPDATE` on `core.users` and the session/role grants.
/// One shared list, enforced where the plugin is untrusted, is the fix for
/// issue #21. The deeper fix — keying grants on something a plugin cannot
/// declare — is part of the isolation work (#17/#18), not this PR.
pub const RESERVED_IDS: &[&str] =
    &["plugins", "events", "audit", "core", "sdk", "auth", "membership"];

/// A loaded plugin: the boxed trait object, the library that owns its code,
/// its routes, its admin snapshot, and its enabled flag.
pub struct LoadedPlugin {
    pub plugin: Box<dyn AdjutantPlugin>,
    /// Kept alive for the library-lifetime rule (see module docs): never
    /// read, never dropped until the registry retires it. `None` for WASM
    /// plugins, whose code is owned by the wasmtime store inside the plugin.
    #[allow(dead_code)]
    pub(crate) library: Option<Arc<libloading::Library>>,
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
    /// `native` (trusted cdylib) or `wasm` (sandboxed).
    pub kind: String,
    pub permissions: Vec<String>,
    /// Full route table — lets `adjutant test-plugin` enumerate every route
    /// without hardcoding plugin knowledge. A path containing a capture is
    /// reported as `skipped` by the harness (it has no concrete value to probe
    /// with); it is not counted as a pass.
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

/// Normalise a route path for collision detection: every capture becomes `{}`,
/// so `/api/x/{id}` and `/api/x/{other}` are recognised as the same shape (the
/// second would silently shadow the first). Literal paths are unchanged, because
/// a literal is *meant* to win over a capture.
fn normalized_route_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                "{}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
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

    /// Is this plugin currently live? A read-only check for handlers that must
    /// validate before any side effect (the lifecycle routes audit first, then
    /// apply, so "not loaded" has to be decided without mutating).
    pub fn contains(&self, id: &str) -> bool {
        self.plugins.iter().any(|p| p.info.id == id)
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
                // Literal fast-path only for capture-free routes: a request whose
                // path equals the template text (`…/{id}`) must still go through
                // match_path, so a templated route always delivers its captures.
                if !r.path.contains('{') && r.path == path {
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

/// Open one plugin file into a trait object. Native `.so`s are trusted code and
/// are parked for the process lifetime; WASM guests are sandboxed. Returns the
/// plugin, the (optional) library handle, and whether it is a WASM guest.
async fn open_plugin(
    path: &Path,
) -> Result<(Box<dyn AdjutantPlugin>, Option<Arc<libloading::Library>>, bool), PluginRuntimeError> {
    let is_wasm = path.extension().is_some_and(|x| x == "wasm");
    if is_wasm {
        let p = crate::wasm::WasmPlugin::open(path)
            .await
            .map_err(|e| PluginRuntimeError::Load(path.display().to_string(), e))?;
        Ok((Box::new(p), None, true))
    } else {
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| PluginRuntimeError::Load(path.display().to_string(), e.to_string()))?;
        let lib = Arc::new(lib);
        park(lib.clone()); // mapped until process exit, whatever happens below

        // ABI handshake: resolve the SDK ABI symbol BEFORE touching the plugin
        // vtable. A stale build is refused with a clear error instead of running
        // against a mismatched layout.
        check_sdk_abi(&lib, path)?;

        // Scope the libloading `Symbol` so it cannot be live across an await.
        let plugin: Box<dyn AdjutantPlugin> = {
            let factory =
                unsafe { lib.get::<adjutant_sdk::PluginFactory>(adjutant_sdk::ENTRY_SYMBOL) }
                    .map_err(|e| {
                        PluginRuntimeError::Load(path.display().to_string(), e.to_string())
                    })?;
            unsafe { Box::from_raw(factory()) }
        };
        Ok((plugin, Some(lib), false))
    }
}

/// Read the id and version of every plugin in `dir`, without a database. Used by
/// `adjutant bootstrap-isolation` to learn which roles to create. Applies the
/// same id rules as [`load_all`].
pub async fn discover_plugins(dir: &Path) -> Result<Vec<DiscoveredPlugin>, PluginRuntimeError> {
    if !dir.exists() {
        return Err(PluginRuntimeError::DirMissing(dir.display().to_string()));
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| PluginRuntimeError::Io(dir.display().to_string(), e))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "so" || x == "wasm"))
        .collect();
    entries.sort();

    let mut out = Vec::new();
    for path in entries {
        let (plugin, _library, is_wasm) = open_plugin(&path).await?;
        let id = plugin.id().to_string();
        if is_wasm {
            validate_untrusted_plugin_id(&id)
                .map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        } else {
            validate_plugin_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        }
        out.push(DiscoveredPlugin { id, version: plugin.version().to_string() });
    }
    Ok(out)
}

/// A plugin found on disk (id + version), for `bootstrap-isolation`.
pub struct DiscoveredPlugin {
    pub id: String,
    pub version: String,
}

/// Load every *installable* plugin in `dir`. Fails the boot on any invalid
/// plugin — a half-loaded plugin set is worse than refusing to start (SPEC §14:
/// fail loud). Uninstalled plugins are skipped before side effects.
///
/// Each plugin is loaded onto its own connection pool authenticated as its
/// `adjutant_plugin_<id>` role, using the credential `bootstrap-isolation`
/// stored in `core.plugins.db_secret`. A plugin with no credential fails the
/// load: there is no unisolated path to fall back to (design §3.7).
pub async fn load_all(
    dir: &Path,
    database_url: &str,
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
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x == "so" || x == "wasm")
        })
        .collect();
    entries.sort();

    for path in entries {
        let (mut plugin, library, is_wasm) = open_plugin(&path).await?;
        let id = plugin.id().to_string();

        // --- validation -----------------------------------------------------
        // The reserved-id check applies to the untrusted path only: a WASM guest
        // declares its own id, so it must not be able to name itself into a
        // first-party plugin's `core.*` grants. A native `.so` is trusted code
        // (see `RESERVED_IDS`), so it gets the shape rule only — the first-party
        // `auth`/`membership` plugins use those ids.
        if is_wasm {
            validate_untrusted_plugin_id(&id)
                .map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        } else {
            validate_plugin_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        }
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

        // --- credential + per-plugin pool (design §3.1-3.3) -----------------
        // The credential is stored by `bootstrap-isolation`. There is no
        // unisolated fallback, so a missing one is a load error, not a warning.
        let secret: Option<String> =
            sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = $1")
                .bind(&id)
                .fetch_optional(pool.as_ref())
                .await
                .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?
                .flatten();
        let Some(secret) = secret else {
            return Err(PluginRuntimeError::NotBootstrapped(id));
        };
        let plugin_pool = crate::host::plugin_pool(database_url, &id, &secret, 2)
            .await
            .map_err(|e| PluginRuntimeError::Pool(id.clone(), e.to_string()))?;
        // The secret must not leak into anything the plugin receives: the row's
        // `config` below is separate from `db_secret`.
        drop(secret);

        // --- per-plugin config (DB row) + enabled state ---------------------
        // Read BEFORE ctx construction: init() needs ctx.config (OIDC settings
        // etc. are per-plugin and admin-editable via the config column). The
        // secret is a separate column and is never merged into config.
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
            // `ctx.db` runs on the plugin's own pool, authenticated as its role;
            // permissions/audit/events stay core-mediated on the core pool.
            db: adjutant_sdk::DbHandle::new(
                crate::host::CoreDb::new(plugin_pool.clone()),
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

        // --- migrations, on the plugin's own pool ---------------------------
        // DDL runs as the plugin role, so it can only touch the plugin's own
        // schema; the bookkeeping call is validated against the caller. Applied
        // versions are read from the core pool (the plugin role cannot read
        // core.schema_migrations).
        let migrations = plugin.migrations();
        validate_migrations(&id, &migrations)?;
        let applied: std::collections::HashSet<i64> = crate::db::applied_migrations(&pool, &id)
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?
            .into_iter()
            .collect();
        for m in migrations {
            if applied.contains(&m.version) {
                continue;
            }
            crate::db::run_plugin_migration(&plugin_pool, &id, m.version, &m.name, &m.sql)
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
        validate_declaration(&id, &granted, &routes, &mut seen_routes)?;

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
            kind: if is_wasm { "wasm".into() } else { "native".into() },
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
            library,
            routes,
            enabled,
            info,
        });
    }

    Ok(PluginRegistry::new(plugins))
}

/// **Native** (trusted) load-time id check: the shape rule only.
///
/// A native `.so` plugin is trusted code, so it may use a reserved name — the
/// first-party `auth` and `membership` plugins do. The reserved list applies to
/// the untrusted path in [`validate_untrusted_plugin_id`].
pub fn validate_plugin_id(id: &str) -> Result<(), String> {
    validate_id(id)
}

/// **Untrusted** load-time id check (sandboxed WASM guests): the shape rule plus
/// [`RESERVED_IDS`]. A guest whose manifest declares a reserved id is refused, so
/// it cannot inherit another plugin's `core.*` grants (issue #21).
pub fn validate_untrusted_plugin_id(id: &str) -> Result<(), String> {
    validate_plugin_id(id)?;
    if RESERVED_IDS.contains(&id) {
        return Err(format!("id {id:?} is reserved by the core"));
    }
    Ok(())
}

/// Resolve and verify the SDK ABI symbol **before** the plugin factory is
/// called. A stale build (plugin not rebuilt after an SDK change) is refused
/// with a clear error instead of running against a mismatched vtable.
pub(crate) fn check_sdk_abi(
    lib: &libloading::Library,
    path: &Path,
) -> Result<(), PluginRuntimeError> {
    let abi = unsafe { lib.get::<extern "C" fn() -> u32>(adjutant_sdk::ABI_SYMBOL) }.map_err(|_| {
        PluginRuntimeError::Load(
            path.display().to_string(),
            format!(
                "missing `{}` symbol: built against an older adjutant-sdk; rebuild against {}",
                String::from_utf8_lossy(adjutant_sdk::ABI_SYMBOL),
                adjutant_sdk::SDK_VERSION,
            ),
        )
    })?;
    let found = abi();
    if found != adjutant_sdk::SDK_ABI_VERSION {
        return Err(PluginRuntimeError::Load(
            path.display().to_string(),
            format!(
                "SDK ABI mismatch: plugin reports {found}, core requires {} (adjutant-sdk {}); rebuild the plugin",
                adjutant_sdk::SDK_ABI_VERSION,
                adjutant_sdk::SDK_VERSION,
            ),
        ));
    }
    Ok(())
}

/// Validate migrations without running them: version >= 1 and versions unique.
pub fn validate_migrations(
    id: &str,
    migrations: &[adjutant_sdk::Migration],
) -> Result<(), PluginRuntimeError> {
    let mut seen = std::collections::HashSet::new();
    for m in migrations {
        if m.version < 1 {
            return Err(PluginRuntimeError::Invalid(
                id.to_string(),
                format!("migration '{}' version must be >= 1", m.name),
            ));
        }
        if !seen.insert(m.version) {
            return Err(PluginRuntimeError::Invalid(
                id.to_string(),
                format!("duplicate migration version {} ('{}')", m.version, m.name),
            ));
        }
    }
    Ok(())
}

/// Validate a plugin declaration (id, route namespace, captures, permission
/// references, duplicate routes) without a database. `seen_routes` accumulates
/// across plugins during a boot; pass a fresh map to validate one plugin in
/// isolation (`adjutant validate-plugin`).
pub fn validate_declaration(
    id: &str,
    granted: &[adjutant_sdk::Permission],
    routes: &[RouteDefinition],
    seen_routes: &mut HashMap<(String, String), ()>,
) -> Result<(), PluginRuntimeError> {
    validate_plugin_id(id).map_err(|e| PluginRuntimeError::Invalid(id.to_string(), e))?;
    for r in routes {
        let prefix = format!("/api/{id}");
        if !(r.path == prefix || r.path.starts_with(&format!("{prefix}/"))) {
            return Err(PluginRuntimeError::Invalid(
                id.to_string(),
                format!("route {} escapes plugin namespace (must start with {prefix})", r.path),
            ));
        }
        validate_route_path(&r.path).map_err(|e| PluginRuntimeError::Invalid(id.to_string(), e))?;
        if let Some(required) = &r.required_permission {
            if !granted.iter().any(|p| &p.id == required) {
                return Err(PluginRuntimeError::Invalid(
                    id.to_string(),
                    format!(
                        "route {} requires permission '{required}' which the plugin does not grant",
                        r.path
                    ),
                ));
            }
        }
        let key = (r.method.as_str().to_string(), normalized_route_path(&r.path));
        if seen_routes.contains_key(&key) {
            return Err(PluginRuntimeError::Invalid(
                id.to_string(),
                format!("duplicate route {} {}", key.0, key.1),
            ));
        }
        seen_routes.insert(key, ());
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
    #[error(
        "plugin {0} has no database credential: run `adjutant bootstrap-isolation` \
         (see docs/design/plugin-isolation.md)"
    )]
    NotBootstrapped(String),
    #[error("plugin {0} database pool failed: {1}")]
    Pool(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use adjutant_sdk::{
        async_trait, route_handler, EventSubscription, PluginRequest, PluginResponse, SdkError,
    };
    use std::path::Path;
    use std::sync::{Arc, OnceLock};

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
            kind: "native".into(),
            permissions: perm.map(|p| vec![p.to_string()]).unwrap_or_default(),
            route_list: vec![RouteInfo {
                method: method.into(),
                path: path.into(),
                permission: perm.map(String::from),
            }],
        };
        LoadedPlugin {
            plugin: Box::new(TestPlugin::new()),
            library: Some(lib),
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
    fn braces_request_still_delivers_a_capture() {
        // A request whose path equals the template text must not take the literal
        // fast-path: the handler would otherwise see an EMPTY params map.
        let reg = PluginRegistry::new(vec![loaded(
            "missions",
            true,
            "GET",
            "/api/missions/{id}",
            None,
        )]);
        match reg.find("GET", "/api/missions/{id}") {
            RouteLookup::Found { params, .. } => {
                assert_eq!(params.get("id").map(String::as_str), Some("{id}"));
            }
            _ => panic!("templated route must always deliver its captures"),
        }
    }

    #[test]
    fn normalized_shapes_catch_colliding_templates() {
        assert_eq!(normalized_route_path("/api/x/{id}"), "/api/x/{}");
        assert_eq!(normalized_route_path("/api/x/{other}"), "/api/x/{}");
        assert_eq!(normalized_route_path("/api/x/current"), "/api/x/current");
        // Different shapes stay distinct; a literal is not a template.
        assert_ne!(normalized_route_path("/api/x/{a}/y"), normalized_route_path("/api/x/{a}"));
        assert_ne!(normalized_route_path("/api/x/literal"), normalized_route_path("/api/x/{}"));
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
    fn native_ids_use_the_shape_rule_only() {
        // Native `.so` plugins are trusted code: the first-party `auth` and
        // `membership` plugins use these ids, so the loader must accept them.
        for r in RESERVED_IDS {
            assert!(validate_id(r).is_ok(), "{r} must be shape-valid");
            assert!(
                validate_plugin_id(r).is_ok(),
                "native id {r} must be accepted (native is trusted by design)"
            );
        }
        assert!(validate_plugin_id("hello").is_ok());
        assert!(validate_plugin_id("Hello").is_err());
        assert!(validate_plugin_id("drop table").is_err());
    }

    #[test]
    fn untrusted_ids_cannot_take_a_reserved_name() {
        for r in RESERVED_IDS {
            assert!(validate_id(r).is_ok(), "{r} must be shape-valid");
            assert!(
                validate_untrusted_plugin_id(r).is_err(),
                "a WASM guest may not declare reserved id {r}"
            );
        }
        // The ids that carry `core.*` grants are the point of the list (#21).
        for privileged in ["auth", "membership", "sdk"] {
            assert!(
                RESERVED_IDS.contains(&privileged),
                "{privileged} must stay in the single reserved list"
            );
            assert!(
                validate_untrusted_plugin_id(privileged).is_err(),
                "guest id {privileged:?} must be rejected at load"
            );
        }
        assert!(validate_untrusted_plugin_id("hello").is_ok());
        assert!(validate_untrusted_plugin_id("Hello").is_err());
        assert!(validate_untrusted_plugin_id("drop table").is_err());
    }

    /// Write a WASM guest (compiled from WAT, so no prebuilt fixture or external
    /// toolchain is needed) whose manifest declares `id`.
    fn write_guest_declaring(dir: &Path, id: &str) -> std::path::PathBuf {
        let manifest =
            serde_json::json!({ "id": id, "name": "Impersonator", "version": "0.0.1" })
                .to_string();
        let len = manifest.len();
        let escaped = manifest.replace('\\', "\\\\").replace('"', "\\\"");
        let wat = format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escaped}")
                (func (export "adjutant_alloc") (param i32) (result i32) (i32.const 2048))
                (func (export "adjutant_free") (param i32 i32))
                (func (export "adjutant_describe") (param i32 i32) (result i32)
                    (if (result i32) (i32.eqz (local.get 0))
                        (then (i32.const -{len}))
                        (else
                            (memory.copy (local.get 0) (i32.const 1024) (i32.const {len}))
                            (i32.const {len}))))
                (func (export "adjutant_handle") (param i32 i32 i32 i32) (result i32) (i32.const 2))
            )"#
        );
        let path = dir.join(format!("{id}.wasm"));
        std::fs::write(&path, wat).expect("write guest wat");
        path
    }

    /// Issue #21 (narrowed): the loader must reject a WASM guest that declares a
    /// reserved id. Validation happens before the first DB access, so a lazy pool
    /// is enough to drive the real `load_all` path.
    #[tokio::test(flavor = "multi_thread")]
    async fn load_all_rejects_a_wasm_guest_with_a_reserved_id() {
        let dir = std::env::temp_dir().join(format!("adjutant-reserved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        write_guest_declaring(&dir, "auth");

        let pool = Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://adjutant:adjutant@127.0.0.1:1/adjutant_test")
                .expect("lazy pool URL parses"),
        );
        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        let err = match load_all(
            &dir,
            "postgres://adjutant:adjutant@127.0.0.1:1/adjutant_test",
            pool,
            tx,
            serde_json::Value::Null,
            crate::identity::IdentityHub::new(),
            crate::host::CoreHttp::new(),
        )
        .await
        {
            Ok(_) => panic!("a guest declaring a reserved id must be rejected at load"),
            Err(e) => e,
        };

        match err {
            PluginRuntimeError::Invalid(id, msg) => {
                assert_eq!(id, "auth");
                assert!(msg.contains("reserved"), "got: {msg}");
            }
            other => panic!("expected reserved-id Invalid error, got {other}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
