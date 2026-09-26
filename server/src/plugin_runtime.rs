//! Plugin runtime: discovery, dynamic loading, validation, init, migrations,
//! and the **lifecycle registry** (SPEC §15 M2: load, enable, disable,
//! uninstall, hot-reload).
//!
//! Boot / reload order per plugin (SPEC §5.2 lifecycle), as of issue #89:
//! 1. Discover `*.so` in `plugin_dir`
//! 2. **Skip if `core.plugins.enabled = false`, before `dlopen`** — the id comes
//!    from the staged file name (`libadjutant_<id>.so`) and the DB row, so a
//!    disabled plugin is never opened, never pooled, never migrated, and only
//!    its *record* reaches the registry (the `continue` path `uninstalled`
//!    already used, one step earlier)
//! 3. `dlopen` + resolve `adjutant_plugin_create` (sdk::ENTRY_SYMBOL)
//! 4. Validate id (incl. reserved core names), route namespaces, duplicates
//! 5. Skip if the DB row says `uninstalled` (before any side effects)
//! 6. Re-check `enabled` against the *authoritative* id from the library, so a
//!    file that does not follow the naming convention is still refused before
//!    any side effect
//! 7. Create PostgreSQL schema, run pending migrations
//! 8. Register granted permissions into `core.permissions`
//! 9. Upsert `core.plugins`
//! 10. `init(ctx)`, collect routes + subscriptions
//!
//! **`enabled` decides what is loaded, not only what answers** (issue #89). A
//! disabled plugin has no schema content (its migrations never ran), no pool, no
//! `init`, no routes and no permissions upsert; the registry keeps a *record* of
//! it so the admin surface can list it and the enable endpoint can find it by
//! id. Runtime disable tears the executable half down; runtime enable is a
//! *load* (dlopen + init + migrate + register + routes), the same body boot
//! uses. See [`PluginSlot`] for the two states that make "enabled but not
//! loaded" and "loaded but not enabled" unrepresentable.
//!
//! **Roles and empty schemas are provisioning, not content.** `bootstrap-isolation`
//! creates a `LOGIN` role, its schema and its stored secret for every plugin on
//! disk (the runtime has no `CREATEROLE`, so it cannot create one lazily), and
//! `core.permissions` rows are declarations. Neither follows the enable flag:
//! only the plugin's own objects (the tables its migrations create) do.
//!
//! **Library lifetime rule:** unloading a `cdylib` whose futures or trait
//! objects are still reachable is UB. Superseded and uninstalled plugins move
//! to `retired` and their `.so` stays mapped for the process lifetime; a
//! *disabled* plugin's mapping also stays (it is parked at load), and only its
//! pool, routes and instance go away. Cost: each hot-reload keeps one extra
//! mapping (~1 MB) — bounded by reload count, documented, and strictly safer
//! than unloading under live requests.

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

/// Close a retired or disabled plugin's pool, leaving its library mapped.
///
/// `PgPool::close()` drains gracefully: a connection already checked out by an
/// in-flight request finishes, new checkouts are refused. It is `async` while
/// `uninstall`/`replace_all` are sync (they run under the registry lock), so the
/// close runs on a spawned task. Retiring the *library* is deliberate and
/// unchanged; only the pool closes, so a reload can no longer leak two
/// connections per plugin (issue #30).
///
/// Public because a runtime *disable* closes its pool from the server after it
/// has quiesced the plugin (issue #89 requirement #1: drain, then close).
pub fn close_pool(p: &LoadedPlugin) {
    let Some(pool) = p.pool.clone() else {
        return;
    };
    tokio::spawn(async move {
        pool.close().await;
    });
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

/// A loaded plugin's executable half: the boxed trait object, the library that
/// owns its code, its routes, its pool, and its admin snapshot.
///
/// **There is deliberately no `enabled` field** (issue #89). A `LoadedPlugin`
/// exists only inside a [`PluginSlot::Live`], so its existence *is* the claim
/// "enabled and serving"; a separate flag could contradict it. The other slot
/// variant ([`PluginSlot::Known`]) holds no executable parts at all.
pub struct LoadedPlugin {
    pub plugin: Box<dyn AdjutantPlugin>,
    /// Kept alive for the library-lifetime rule (see module docs): never
    /// read, never dropped until the registry retires it. `None` for WASM
    /// plugins, whose code is owned by the wasmtime store inside the plugin.
    #[allow(dead_code)]
    pub library: Option<Arc<libloading::Library>>,
    /// The plugin's own database pool. Closed when the plugin is retired or
    /// disabled (the *library* stays mapped, the pool does not): a plugin that
    /// is not serving is never read again, and leaking 2 connections per reload
    /// exhausts PostgreSQL (issue #30). `None` only for test fixtures.
    pub pool: Option<Arc<sqlx::PgPool>>,
    pub routes: Vec<RouteDefinition>,
    /// The library file this plugin was loaded from. Kept so a runtime *disable*
    /// can hand the path back to the record it leaves behind (a later enable
    /// loads exactly this file).
    pub path: std::path::PathBuf,
    pub info: PluginInfo,
}

/// A plugin the registry **knows but has not loaded**: the record, plus where
/// its library lives so a later enable can load it.
///
/// This is the disabled state. It carries only what can be known without
/// opening the library — id and version (from `core.plugins`), name (the id,
/// until a load can supply the pretty name), kind (from the file extension) —
/// and **nothing executable**. `info.enabled` is always `false` and
/// `info.loaded` always `false`: a disabled plugin cannot be reached through
/// this type in any state that claims otherwise.
pub struct PluginRecord {
    pub info: PluginInfo,
    /// The library file this record came from. `enable` loads exactly this
    /// path. The staging convention (`libadjutant_<id>.so`, from
    /// `scripts/stage-plugins.py`) is what lets boot decide the enabled case
    /// from the file name, without a `dlopen`.
    pub path: std::path::PathBuf,
}

/// The registry's per-plugin slot — exactly two states, and that is the design
/// (issue #89, owner requirement): **"enabled but not loaded" and "loaded but
/// not enabled" are unrepresentable**.
///
/// * [`PluginSlot::Live`] — loaded and serving. Holds every executable part,
///   and has no flag that could say otherwise.
/// * [`PluginSlot::Known`] — disabled. Holds the record and nothing runnable;
///   there is no `plugin`, no `pool`, no `routes` to accidentally serve from.
///
/// The previous shape was one struct with independent `enabled` and loaded
/// fields, which could express all four combinations; two of those four were
/// bugs waiting to be reached. Making the state a sum type means a transition
/// is a *move between variants* — enable necessarily produces a `Live` (the
/// load must have happened, since that is the only constructor), and disable
/// necessarily destroys the executable parts.
pub enum PluginSlot {
    Live(LoadedPlugin),
    Known(PluginRecord),
}

impl PluginSlot {
    /// The admin snapshot / registry record for this plugin, loaded or not.
    pub fn info(&self) -> &PluginInfo {
        match self {
            Self::Live(p) => &p.info,
            Self::Known(r) => &r.info,
        }
    }

    /// The executable half, when this plugin is loaded. Read-only callers (the
    /// admin list, subscription rebinding, the scheduler) go through this so a
    /// `Known` slot simply yields nothing instead of needing a flag check.
    pub fn live(&self) -> Option<&LoadedPlugin> {
        match self {
            Self::Live(p) => Some(p),
            Self::Known(_) => None,
        }
    }

    pub fn live_mut(&mut self) -> Option<&mut LoadedPlugin> {
        match self {
            Self::Live(p) => Some(p),
            Self::Known(_) => None,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }
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
    /// True when the plugin loaded on its own restricted role/pool; a plugin
    /// that could not be isolated never loads, so this is always true for a
    /// live plugin and the admin surface can show isolation is active (#31).
    /// It is also true for a `Known` record: the role exists for every plugin
    /// on disk, because `bootstrap-isolation` provisions it (issue #89).
    pub isolated: bool,
    /// **The distinction issue #89 exists to create** (owner requirement #10):
    /// `true` when the plugin is *loaded* — library open, pool open, routes in
    /// the registry; `false` when it is merely *known* — the record exists and
    /// nothing of it runs. An operator can tell "off" from "wrong".
    pub loaded: bool,
    /// Why the last load attempt failed, when one did (requirement #3: "record
    /// the reason where the admin will see it"). `None` after a successful load
    /// and for a plugin that was merely disabled.
    pub last_error: Option<String>,
    pub permissions: Vec<String>,
    /// Declared schedules with their last run / last error / next run (#45).
    /// Filled at request time from the core [`crate::scheduler::Scheduler`], so
    /// the operator sees when a timer last fired.
    pub schedules: Vec<ScheduleInfo>,
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
    /// `troop` (grant must cover troop) or `any` (the handler checks the
    /// object's scope). Derived from [`RouteDefinition::required_scope`].
    pub scope: String,
}

/// Admin view of one declared schedule (#45): what it is, when it last ran,
/// whether that run failed, and when it is next due.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScheduleInfo {
    pub name: String,
    pub every_secs: u64,
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub next_run: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of resolving a request path against the registry.
///
/// There is no `Disabled` variant (issue #89): a disabled plugin is not loaded,
/// so it has no routes for the resolver to find. Its route answers exactly as it
/// does when the library is absent from disk — [`RouteLookup::NotFound`] — rather
/// than through a special case that would have to know a plugin exists in order
/// to refuse it.
pub enum RouteLookup {
    Found {
        plugin_id: String,
        required_permission: Option<String>,
        /// `Some(scope)` = a troop-covering grant (or the declared scope);
        /// `None` = the permission at any scope, checked in the handler.
        required_scope: Option<adjutant_sdk::Scope>,
        handler: RouteHandler,
        /// Captures from a templated route (`/api/missions/{id}`).
        params: HashMap<String, String>,
    },
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
    /// Every plugin the core knows, loaded or merely known — see [`PluginSlot`].
    pub plugins: Vec<PluginSlot>,
    /// Superseded / uninstalled plugins. Kept mapped, never referenced again —
    /// see the library lifetime rule above.
    pub(crate) retired: Vec<LoadedPlugin>,
}

impl PluginRegistry {
    /// Build a registry from already-decided slots. Public so the DB-gated
    /// pool-lifecycle probe can drive retirement directly.
    pub fn new(plugins: Vec<PluginSlot>) -> Self {
        Self { plugins, retired: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Is this plugin **known** (loaded, or a disabled record)? A read-only
    /// check for handlers that must validate before any side effect (the
    /// lifecycle routes audit first, then apply, so "unknown" has to be decided
    /// without mutating). Deliberately true for a disabled plugin too: the
    /// enable endpoint looks *it* up by id, and the admin surface lists it.
    pub fn contains(&self, id: &str) -> bool {
        self.plugins.iter().any(|s| s.info().id == id)
    }

    /// The admin snapshot for one plugin, loaded or merely known.
    pub fn info(&self, id: &str) -> Option<&PluginInfo> {
        self.plugins.iter().map(PluginSlot::info).find(|i| i.id == id)
    }

    /// The disabled record for `id` — the library path a later enable loads.
    /// `None` when the plugin is unknown or already loaded.
    pub fn record(&self, id: &str) -> Option<&PluginRecord> {
        self.plugins.iter().find_map(|s| match s {
            PluginSlot::Known(r) if r.info.id == id => Some(r),
            _ => None,
        })
    }

    /// Admin snapshots for **every known plugin**, loaded or not (issue #89
    /// requirement #10: an operator must be able to tell "off" from "wrong", so
    /// a disabled plugin stays listed with `loaded = false`).
    pub fn infos(&self) -> Vec<PluginInfo> {
        self.plugins.iter().map(|s| s.info().clone()).collect()
    }

    /// Ids of the loaded plugins — the ones anything can be dispatched to.
    pub fn live_ids(&self) -> std::collections::HashSet<String> {
        self.plugins
            .iter()
            .filter_map(|s| s.live())
            .map(|p| p.info.id.clone())
            .collect()
    }

    /// Route keys (`METHOD`, normalised path) already served by loaded plugins.
    /// A runtime enable seeds its collision check from this, so a plugin loaded
    /// later can never shadow a route that is already answering.
    pub fn live_route_keys(&self) -> HashMap<(String, String), ()> {
        let mut keys = HashMap::new();
        for p in self.plugins.iter().filter_map(PluginSlot::live) {
            for r in &p.routes {
                keys.insert(
                    (r.method.as_str().to_string(), normalized_route_path(&r.path)),
                    (),
                );
            }
        }
        keys
    }

    /// Resolve `METHOD path` against **loaded** plugin routes. A disabled plugin
    /// has no routes here at all, so its paths fall through to `NotFound` —
    /// identical to a library that is absent from disk (issue #89 acceptance).
    ///
    /// Literal routes win over templated ones, so a specific path is never
    /// shadowed by a capture. Captures are delivered to the handler.
    pub fn find(&self, method: &str, path: &str) -> RouteLookup {
        let mut templated: Option<(&LoadedPlugin, &RouteDefinition, HashMap<String, String>)> = None;
        for p in self.plugins.iter().filter_map(PluginSlot::live) {
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

    /// Every route reached through [`PluginSlot::Live`] is served: there is no
    /// enabled flag left to check, because a `Known` slot has no routes and a
    /// `Live` one exists only while the plugin is enabled.
    fn resolve(p: &LoadedPlugin, r: &RouteDefinition, params: HashMap<String, String>) -> RouteLookup {
        RouteLookup::Found {
            plugin_id: p.info.id.clone(),
            required_permission: r.required_permission.clone(),
            required_scope: r.required_scope.clone(),
            handler: r.handler.clone(),
            params,
        }
    }

    /// Install a freshly loaded plugin, replacing its disabled record. This is
    /// the *only* way a plugin becomes live at runtime, and it takes a
    /// [`LoadedPlugin`] — so "enabled" cannot be set without the load having
    /// happened (issue #89 requirement #3). Returns false when the plugin is
    /// unknown (it is still installed, but the caller should not claim it was
    /// validating a record).
    pub fn install(&mut self, loaded: LoadedPlugin) -> bool {
        let id = loaded.info.id.clone();
        if let Some(slot) = self.plugins.iter_mut().find(|s| s.info().id == id) {
            *slot = PluginSlot::Live(loaded);
            true
        } else {
            self.plugins.push(PluginSlot::Live(loaded));
            false
        }
    }

    /// Stop routing for `id` and hand back its executable half, so the caller can
    /// quiesce it (drain in-flight requests) and only then close its pool. The
    /// slot becomes a disabled record immediately — from this instant no new
    /// request can reach the handler — while the returned parts stay alive for
    /// the requests already inside it.
    ///
    /// `None` when the plugin is unknown *or already known-not-loaded*: both are
    /// no-ops for the caller, which has already validated existence.
    pub fn stop_live(&mut self, id: &str) -> Option<LoadedPlugin> {
        let i = self.plugins.iter().position(|s| s.info().id == id)?;
        if !self.plugins[i].is_live() {
            return None;
        }
        let taken = std::mem::replace(
            &mut self.plugins[i],
            PluginSlot::Known(PluginRecord::from_info(placeholder_info(), std::path::PathBuf::new())),
        );
        let PluginSlot::Live(p) = taken else {
            unreachable!("slot was just checked to be Live")
        };
        // The record keeps whatever the load knew — name, version, kind and the
        // declared permissions — and nothing executable.
        self.plugins[i] = PluginSlot::Known(PluginRecord::from_loaded(&p));
        Some(p)
    }

    /// Record why a load attempt failed, where the admin surface will show it
    /// (issue #89 requirement #3). Never creates a slot.
    pub fn record_error(&mut self, id: &str, message: &str) {
        if let Some(slot) = self.plugins.iter_mut().find(|s| s.info().id == id) {
            match slot {
                PluginSlot::Known(r) => {
                    r.info.last_error = Some(message.to_string());
                    r.info.loaded = false;
                }
                PluginSlot::Live(p) => p.info.last_error = Some(message.to_string()),
            }
        }
    }

    /// Uninstall: remove from the registry (routes stop resolving) but keep the
    /// library mapped. A loaded plugin is retired (its pool closed); a disabled
    /// record is simply dropped — nothing of it was running. Returns false when
    /// the plugin is unknown.
    pub fn uninstall(&mut self, id: &str) -> bool {
        let Some(i) = self.plugins.iter().position(|s| s.info().id == id) else {
            return false;
        };
        match self.plugins.remove(i) {
            PluginSlot::Live(p) => {
                close_pool(&p);
                tracing::info!(plugin = id, "uninstalled (library retired, data archived)");
                self.retired.push(p);
            }
            PluginSlot::Known(r) => {
                tracing::info!(plugin = %r.info.id, "uninstalled while disabled (record dropped)");
            }
        }
        true
    }

    /// Swap in a freshly loaded registry (hot-reload). The old live set is
    /// retired, not dropped, so in-flight requests keep valid code; the old
    /// disabled records are dropped (nothing of them was running).
    pub fn replace_all(&mut self, fresh: PluginRegistry) {
        let PluginRegistry { plugins, retired: _ } = fresh;
        let old = std::mem::take(&mut self.plugins);
        for slot in &old {
            if let PluginSlot::Live(p) = slot {
                close_pool(p);
            }
        }
        self.retired
            .extend(old.into_iter().filter_map(|s| match s {
                PluginSlot::Live(p) => Some(p),
                PluginSlot::Known(_) => None,
            }));
        self.plugins = plugins;
    }

    pub fn retired_count(&self) -> usize {
        self.retired.len()
    }
}

/// A throwaway snapshot, only ever used as the value `std::mem::replace` leaves
/// behind before the real one is written. Building it costs nothing and it is
/// never observable.
fn placeholder_info() -> PluginInfo {
    PluginInfo {
        id: String::new(),
        name: String::new(),
        version: String::new(),
        enabled: false,
        routes: 0,
        kind: "native".into(),
        isolated: true,
        loaded: false,
        last_error: None,
        permissions: Vec::new(),
        schedules: Vec::new(),
        route_list: Vec::new(),
    }
}

impl PluginRecord {
    /// The record for a plugin that is known but not loaded. Whatever the caller
    /// cannot know without a load is left empty rather than guessed:
    /// `name` falls back to the id until a load supplies the pretty name, and
    /// `permissions` is empty for a plugin disabled at boot (its declaration is
    /// only readable through its code). `version` comes from `core.plugins` and
    /// is therefore **not refreshed while disabled** — reading the on-disk
    /// version needs the library loaded. That is accepted (issue #89 §2): the
    /// version column catches up on the next enable.
    pub fn from_info(mut info: PluginInfo, path: std::path::PathBuf) -> Self {
        info.enabled = false;
        info.loaded = false;
        info.routes = 0;
        info.route_list = Vec::new();
        Self { info, path }
    }

    /// The record a runtime *disable* leaves behind: everything the load knew
    /// (name, version, kind, declared permissions, library path), minus every
    /// executable part.
    pub fn from_loaded(p: &LoadedPlugin) -> Self {
        Self::from_info(p.info.clone(), p.path.clone())
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

/// Everything one plugin load needs from the core, bundled so the per-plugin
/// load body can be shared by boot ([`load_all`]) and a runtime enable
/// ([`load_plugin`]) without a nine-argument function.
pub struct LoadEnv<'a> {
    pub database_url: &'a str,
    pub pool: Arc<PgPool>,
    pub event_tx: tokio::sync::broadcast::Sender<adjutant_sdk::Event>,
    pub config: serde_json::Value,
    pub identity: Arc<crate::identity::IdentityHub>,
    pub http: Arc<crate::host::CoreHttp>,
}

/// The id a **staged native library** names, read from its file name without
/// opening it: `libadjutant_<id>.so` → `id`.
///
/// `scripts/stage-plugins.py` derives the name from the crate, and every
/// first-party plugin declares the matching id, which is what lets boot decide
/// "disabled" **before a `dlopen`** (issue #89). It is a *hint*: a library that
/// does not follow the convention yields `None` and takes the normal path —
/// opened, and then the authoritative id from the library is checked against the
/// row before any side effect. So the decision is always correct; the
/// convention only decides how early it is made.
pub fn staged_plugin_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".so")?;
    let id = stem.strip_prefix("libadjutant_")?;
    if validate_id(id).is_ok() {
        Some(id.to_string())
    } else {
        None
    }
}

/// The snapshot of a plugin the registry knows but has not loaded. Id and
/// version come from `core.plugins`, `kind` from the file extension; the name
/// falls back to the id until a load can supply the pretty one, and the
/// declared permissions are unknown without the library, so the list is empty.
fn record_info(id: &str, version: &str, path: &Path) -> PluginInfo {
    let is_wasm = path.extension().is_some_and(|x| x == "wasm");
    PluginInfo {
        id: id.to_string(),
        name: id.to_string(),
        version: version.to_string(),
        enabled: false,
        routes: 0,
        kind: if is_wasm { "wasm".into() } else { "native".into() },
        isolated: true,
        loaded: false,
        last_error: None,
        permissions: Vec::new(),
        schedules: Vec::new(),
        route_list: Vec::new(),
    }
}

/// The record a **disabled** plugin leaves at boot, decided *without opening
/// the library* (issue #89 requirement: no `dlopen`, no pool, no `init`, no
/// migrations, no permissions).
///
/// `None` means "not decided here" — either the file name does not give an id
/// the convention admits, or the row says enabled/uninstalled, in which case the
/// caller takes the normal path (and `uninstalled` keeps behaving exactly as it
/// did: opened, then skipped). Deliberately **not** conflated with `uninstalled`:
/// that one is permanent until reinstall, this one is a flag an admin flips.
async fn disabled_record_at_boot(
    path: &Path,
    pool: &PgPool,
) -> Result<Option<PluginRecord>, PluginRuntimeError> {
    let Some(id) = staged_plugin_id(path) else {
        return Ok(None);
    };
    let row: Option<(bool, bool, String)> =
        sqlx::query_as("SELECT enabled, uninstalled, version FROM core.plugins WHERE id = $1")
            .bind(&id)
            .fetch_optional(pool)
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
    match row {
        Some((false, false, version)) => Ok(Some(PluginRecord::from_info(
            record_info(&id, &version, path),
            path.to_path_buf(),
        ))),
        _ => Ok(None),
    }
}

/// Load every *installable* plugin in `dir`. Fails the boot on any invalid
/// plugin — a half-loaded plugin set is worse than refusing to start (SPEC §14:
/// fail loud).
///
/// **A disabled plugin is not loaded at all** (issue #89): it is skipped before
/// the `dlopen`, and only its record reaches the registry — no pool, no `init`,
/// no migrations (so no schema content), no permission upsert, no routes.
/// Uninstalled plugins are skipped before any side effect, exactly as before.
///
/// Each loaded plugin gets its own connection pool authenticated as its
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
    let env = LoadEnv {
        database_url,
        pool: pool.clone(),
        event_tx,
        config,
        identity,
        http,
    };
    let mut slots: Vec<PluginSlot> = Vec::new();
    let mut seen_ids: HashMap<String, ()> = HashMap::new();
    let mut seen_routes: HashMap<(String, String), ()> = HashMap::new();

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
        // --- disabled? skip BEFORE the dlopen --------------------------------
        // The id comes from the staged file name plus the DB row, so nothing of
        // the library runs — not even its initializers. The log line says
        // "skipping", not "loaded": the whole point of the issue is that a
        // disabled plugin was reported as loaded while it held a pool and had
        // migrated its schema.
        if let Some(rec) = disabled_record_at_boot(&path, env.pool.as_ref()).await? {
            tracing::info!(
                plugin = %rec.info.id,
                "skipping disabled plugin (enabled = false): not loaded"
            );
            seen_ids.insert(rec.info.id.clone(), ());
            slots.push(PluginSlot::Known(rec));
            continue;
        }

        let (plugin, library, is_wasm) = open_plugin(&path).await?;
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
        seen_ids.insert(id.clone(), ());

        // --- uninstalled / disabled, on the authoritative id -----------------
        // (drop order: plugin before library — `plugin` is declared later.)
        let pre: Option<(bool, bool, String)> =
            sqlx::query_as("SELECT enabled, uninstalled, version FROM core.plugins WHERE id = $1")
                .bind(&id)
                .fetch_optional(env.pool.as_ref())
                .await
                .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
        if let Some((_, true, _)) = &pre {
            tracing::info!(plugin = %id, "skipping uninstalled plugin (.so still on disk)");
            continue;
        }
        // The authoritative re-check: the file name decided nothing, or decided
        // something else, so the row read through the plugin's own id is what
        // settles it — and it is still read before the first side effect.
        if let Some((false, false, version)) = &pre {
            tracing::info!(
                plugin = %id,
                "skipping disabled plugin (enabled = false): not loaded"
            );
            slots.push(PluginSlot::Known(PluginRecord::from_info(
                record_info(&id, version, &path),
                path.clone(),
            )));
            continue;
        }

        let loaded = load_opened(plugin, library, is_wasm, &path, &env, &mut seen_routes).await?;
        slots.push(PluginSlot::Live(loaded));
    }

    Ok(PluginRegistry::new(slots))
}

/// Load **one** plugin from `path` — a runtime enable.
///
/// This is exactly the boot body for a single plugin: `dlopen`, validate, open
/// its pool, `init`, apply pending migrations as the plugin's own role, register
/// its declared permissions, validate and keep its routes. Migrations are
/// idempotent (`applied_migrations` gates each version), so re-enabling after an
/// upgrade applies only what is new, and a plugin disabled before it was
/// upgraded comes up on the new schema or fails here — leaving the caller's
/// previous state (still disabled) in place.
///
/// The caller owns the lifecycle lock, the audit row and the `core.plugins`
/// flag write; this function has no opinion about them.
pub async fn load_plugin(
    path: &Path,
    env: &LoadEnv<'_>,
    seen_routes: &mut HashMap<(String, String), ()>,
) -> Result<LoadedPlugin, PluginRuntimeError> {
    let (plugin, library, is_wasm) = open_plugin(path).await?;
    let id = plugin.id().to_string();
    if is_wasm {
        validate_untrusted_plugin_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
    } else {
        validate_plugin_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
    }
    load_opened(plugin, library, is_wasm, path, env, seen_routes).await
}

/// The per-plugin load body, shared by boot and runtime enable.
///
/// `enabled = true` is written on insert (a plugin reaches this function only
/// because it is, or is being, enabled). An existing row is left to the caller's
/// flag write — so a plugin being enabled stays `false` in the database until
/// the load has actually succeeded (issue #89 requirement #3).
async fn load_opened(
    mut plugin: Box<dyn AdjutantPlugin>,
    library: Option<Arc<libloading::Library>>,
    is_wasm: bool,
    path: &Path,
    env: &LoadEnv<'_>,
    seen_routes: &mut HashMap<(String, String), ()>,
) -> Result<LoadedPlugin, PluginRuntimeError> {
    let id = plugin.id().to_string();
    // One shared host DB impl for every plugin context (cheap: Arc clone).
    let core_db = crate::host::CoreDb::new(env.pool.clone());

    // --- credential + per-plugin pool (design §3.1-3.3) ---------------------
    // The credential is stored by `bootstrap-isolation`. There is no unisolated
    // fallback, so a missing one is a load error, not a warning.
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = $1")
            .bind(&id)
            .fetch_optional(env.pool.as_ref())
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?
            .flatten();
    let Some(secret) = secret else {
        return Err(PluginRuntimeError::NotBootstrapped(id));
    };
    let plugin_pool = crate::host::plugin_pool(env.database_url, &id, &secret, 2)
        .await
        .map_err(|e| PluginRuntimeError::Pool(id.clone(), e.to_string()))?;
    // The secret must not leak into anything the plugin receives: the row's
    // `config` below is separate from `db_secret`.
    drop(secret);

    // --- per-plugin config (DB row) -----------------------------------------
    // Read BEFORE ctx construction: init() needs ctx.config (OIDC settings etc.
    // are per-plugin and admin-editable via the config column). The secret is a
    // separate column and is never merged into config.
    let row_config: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT config FROM core.plugins WHERE id = $1")
            .bind(&id)
            .fetch_optional(env.pool.as_ref())
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
    let row_config = row_config.unwrap_or(serde_json::Value::Null);
    sqlx::query(
        "INSERT INTO core.plugins (id, version, enabled) VALUES ($1, $2, true) \
         ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, updated_at = now()",
    )
    .bind(&id)
    .bind(plugin.version())
    .execute(env.pool.as_ref())
    .await
    .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
    // A non-empty row config wins over the global default from the caller.
    let plugin_config = match &row_config {
        v if v.as_object().map(|o| !o.is_empty()).unwrap_or(false) => v.clone(),
        _ => env.config.clone(),
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
            crate::host::CoreEvents::new(env.pool.clone(), env.event_tx.clone(), id.clone()),
            id.clone(),
        ),
        permissions: PermissionService::new(core_db.clone()),
        audit: adjutant_sdk::AuditService::new(core_db.clone(), id.clone()),
        identity: env.identity.clone(),
        http: env.http.clone(),
    };

    plugin
        .init(ctx)
        .await
        .map_err(|e| PluginRuntimeError::Init(id.clone(), e.to_string()))?;

    // --- migrations, on the plugin's own pool -------------------------------
    // DDL runs as the plugin role, so it can only touch the plugin's own schema;
    // the bookkeeping call is validated against the caller. Applied versions are
    // read from the core pool (the plugin role cannot read
    // core.schema_migrations). Idempotent: each version is applied at most once,
    // so this is safe to run again on every enable.
    let migrations = plugin.migrations();
    validate_migrations(&id, &migrations)?;
    let applied: std::collections::HashSet<i64> = crate::db::applied_migrations(&env.pool, &id)
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

    // --- permissions --------------------------------------------------------
    // Owned rows, indexed loop: holding a `slice::Iter` across the await below
    // poisons the generator's auto-trait proof (rustc reports "Send not general
    // enough" and axum then refuses the Handler).
    //
    // **A disabled plugin keeps its rows** (issue #89 §6): deleting them would
    // cascade through `core.role_permissions` and silently revoke grants a troop
    // configured, and a permission nothing serves is inert — the route that
    // would consume it is not loaded. So this upsert only ever adds/refreshes;
    // nothing here removes.
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
        .execute(env.pool.as_ref())
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;
    }

    // --- route validation ---------------------------------------------------
    let routes = plugin.routes();
    validate_declaration(&id, &granted, &routes, seen_routes)?;

    tracing::info!(
        id,
        version = plugin.version(),
        routes = routes.len(),
        permissions = granted.len(),
        "plugin loaded"
    );

    let info = PluginInfo {
        id: id.clone(),
        name: plugin.name().to_string(),
        version: plugin.version().to_string(),
        enabled: true,
        routes: routes.len(),
        kind: if is_wasm { "wasm".into() } else { "native".into() },
        isolated: true,
        loaded: true,
        last_error: None,
        permissions: granted.iter().map(|p| p.id.clone()).collect(),
        // Filled at request time from the scheduler.
        schedules: Vec::new(),
        route_list: routes
            .iter()
            .map(|r| RouteInfo {
                method: r.method.as_str().to_string(),
                path: r.path.clone(),
                permission: r.required_permission.clone(),
                scope: match &r.required_scope {
                    Some(_) => "troop".into(),
                    None => "any".into(),
                },
            })
            .collect(),
    };

    Ok(LoadedPlugin {
        plugin,
        library,
        pool: Some(plugin_pool),
        routes,
        path: path.to_path_buf(),
        info,
    })
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

    /// A registry slot fixture. `enabled = true` produces the *loaded* state
    /// ([`PluginSlot::Live`]); `false` produces the *known* state
    /// ([`PluginSlot::Known`]) — the two are different types' worth of
    /// difference, which is the point of the split.
    fn loaded(id: &str, enabled: bool, method: &str, path: &str, perm: Option<&str>) -> PluginSlot {
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
            isolated: true,
            loaded: enabled,
            last_error: None,
            permissions: perm.map(|p| vec![p.to_string()]).unwrap_or_default(),
            schedules: Vec::new(),
            route_list: vec![RouteInfo {
                method: method.into(),
                path: path.into(),
                permission: perm.map(String::from),
                scope: "troop".into(),
            }],
        };
        if !enabled {
            return PluginSlot::Known(PluginRecord::from_info(
                info,
                std::path::PathBuf::from(format!("/plugins-built/libadjutant_{id}.so")),
            ));
        }
        PluginSlot::Live(LoadedPlugin {
            plugin: Box::new(TestPlugin::new()),
            library: Some(lib),
            pool: None,
            routes: vec![route],
            path: std::path::PathBuf::from(format!("/plugins-built/libadjutant_{id}.so")),
            info,
        })
    }

    #[test]
    fn find_reports_live_known_and_unknown() {
        let reg = PluginRegistry::new(vec![
            loaded("alpha", true, "GET", "/api/alpha/thing", Some("alpha:read")),
            loaded("beta", false, "GET", "/api/beta/thing", None),
        ]);

        match reg.find("GET", "/api/alpha/thing") {
            RouteLookup::Found { plugin_id, required_permission, .. } => {
                assert_eq!(plugin_id, "alpha");
                assert_eq!(required_permission.as_deref(), Some("alpha:read"));
            }
            _ => panic!("a loaded route must resolve"),
        }
        // A disabled plugin has no routes at all, so its path is NotFound —
        // exactly what an absent library answers (issue #89 acceptance).
        assert!(matches!(reg.find("GET", "/api/beta/thing"), RouteLookup::NotFound));
        assert!(matches!(reg.find("GET", "/api/nope"), RouteLookup::NotFound));
        // Method mismatch is NotFound, not a silent match.
        assert!(matches!(reg.find("POST", "/api/alpha/thing"), RouteLookup::NotFound));
        // But the registry still KNOWS beta: the admin surface lists it and the
        // enable endpoint can look it up by id.
        assert!(reg.contains("beta"), "a disabled plugin stays known");
        let info = reg.info("beta").expect("record");
        assert!(!info.enabled && !info.loaded, "known, not loaded");
        assert_eq!(info.routes, 0);
        assert!(reg.record("beta").is_some(), "the record carries the library path");
        assert!(reg.record("alpha").is_none(), "a loaded plugin has no record");
    }

    #[test]
    fn stop_live_keeps_the_record_and_install_restores_the_load() {
        let mut reg = PluginRegistry::new(vec![loaded(
            "alpha",
            true,
            "GET",
            "/api/alpha/thing",
            None,
        )]);
        let stopped = reg.stop_live("alpha").expect("live plugin yields its parts");
        assert!(stopped.pool.is_none());
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::NotFound));
        assert!(reg.contains("alpha"), "the record survives the teardown");
        assert!(!reg.info("alpha").unwrap().loaded);
        assert!(reg.stop_live("alpha").is_none(), "already known: no-op");
        assert!(reg.stop_live("ghost").is_none(), "unknown: no-op");

        // Install back: the only constructor of the live state takes the loaded
        // parts, so "enabled" cannot be claimed without a load.
        assert!(reg.install(stopped));
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::Found { .. }));
        assert!(reg.info("alpha").unwrap().loaded);

        // Uninstall removes it entirely, retiring the library.
        assert!(reg.uninstall("alpha"));
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::NotFound));
        assert_eq!(reg.retired_count(), 1, "library must be retired, never dropped");
        assert!(reg.is_empty());
        assert!(!reg.uninstall("ghost"));
    }

    #[test]
    fn record_error_is_where_the_admin_sees_a_failed_enable() {
        let mut reg = PluginRegistry::new(vec![loaded("store", false, "GET", "/api/store/health", None)]);
        reg.record_error("store", "migration 2 failed: permission denied");
        let info = reg.info("store").expect("record");
        assert_eq!(info.last_error.as_deref(), Some("migration 2 failed: permission denied"));
        assert!(!info.loaded && !info.enabled, "a failed enable leaves it disabled");
    }

    #[test]
    fn staged_name_gives_the_id_without_opening_the_library() {
        // The convention that makes the no-dlopen skip possible.
        assert_eq!(
            staged_plugin_id(Path::new("/plugins-built/libadjutant_store.so")),
            Some("store".to_string())
        );
        assert_eq!(
            staged_plugin_id(Path::new("/plugins-built/libadjutant_auth.so")),
            Some("auth".to_string())
        );
        // Anything else yields None and takes the normal, authoritative path.
        assert_eq!(staged_plugin_id(Path::new("/plugins-built/store.so")), None);
        assert_eq!(staged_plugin_id(Path::new("/plugins-built/libadjutant_.so")), None);
        assert_eq!(staged_plugin_id(Path::new("/plugins-built/guest.wasm")), None);
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
        let parts = reg.stop_live("alpha").expect("live");
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::NotFound));
        assert!(reg.contains("alpha"), "still known while disabled");
        assert!(reg.install(parts), "re-enable installs the loaded parts");
        assert!(matches!(reg.find("GET", "/api/alpha/thing"), RouteLookup::Found { .. }));
        assert!(reg.stop_live("ghost").is_none(), "unknown plugin must report None");

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
