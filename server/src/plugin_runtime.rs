//! Plugin runtime: discovery, dynamic loading, validation, init, migrations.
//!
//! Boot order per plugin (SPEC §5.2 lifecycle):
//! 1. Discover `*.so` in `plugin_dir`
//! 2. `dlopen` + resolve `adjutant_plugin_create` (sdk::ENTRY_SYMBOL)
//! 3. Validate id, route namespaces, duplicate routes/permissions
//! 4. Create PostgreSQL schema, run pending migrations
//! 5. Register granted permissions into `core.permissions`
//! 6. Upsert `core.plugins`, read `enabled`
//! 7. `init(ctx)` with scoped services
//! 8. Collect routes + subscriptions (subscriptions registered by caller)
//!
//! The `Library` handle is kept alive for the process lifetime — unloading a
//! `cdylib` whose `Box<dyn AdjutantPlugin>` is still alive is UB. Milestone 1
//! has no hot-reload; disable = skip routes at boot, not unload.

use std::collections::HashMap;
use std::path::Path;

use sqlx::PgPool;
use std::sync::Arc;

use adjutant_sdk::{
    AdjutantPlugin, EventBusHandle, PermissionService, PluginContext, RouteDefinition,
};

use crate::db::run_migration;

/// A loaded plugin: the boxed trait object + the library that owns its code.
pub struct LoadedPlugin {
    pub plugin: Box<dyn AdjutantPlugin>,
    _library: std::sync::Arc<libloading::Library>,
    pub routes: Vec<RouteDefinition>,
    pub enabled: bool,
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
}

pub struct PluginRegistry {
    pub plugins: Vec<LoadedPlugin>,
    pub infos: Vec<PluginInfo>,
}

impl PluginRegistry {
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Load every plugin in `dir`. Fails the boot on any invalid plugin — a
/// half-loaded plugin set is worse than refusing to start (SPEC §14: fail loud).
pub async fn load_all(
    dir: &Path,
    pool: Arc<PgPool>,
    event_tx: tokio::sync::broadcast::Sender<adjutant_sdk::Event>,
    config: serde_json::Value,
) -> Result<PluginRegistry, PluginRuntimeError> {
    let mut plugins: Vec<LoadedPlugin> = Vec::new();
    let mut infos: Vec<PluginInfo> = Vec::new();
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
        let lib = std::sync::Arc::new(lib);

        let factory = unsafe { lib.get::<adjutant_sdk::PluginFactory>(adjutant_sdk::ENTRY_SYMBOL) }
            .map_err(|e| PluginRuntimeError::Load(path.display().to_string(), e.to_string()))?;

        let mut plugin: Box<dyn AdjutantPlugin> = unsafe { Box::from_raw(factory()) };
        let id = plugin.id().to_string();

        // --- validation -----------------------------------------------------
        validate_id(&id).map_err(|e| PluginRuntimeError::Invalid(id.clone(), e))?;
        if seen_ids.contains_key(&id) {
            return Err(PluginRuntimeError::Invalid(id, "duplicate plugin id".into()));
        }

        // Routes must be declared before init for namespace validation; the
        // SDK documents routes() as callable after id/name/version only when
        // ctx is set, so we init first below — but namespace checks need the
        // paths. Convention: routes() may be called pre-init for *metadata*,
        // so require plugins to tolerate it OR validate post-init. We
        // validate post-init (after ctx is stored) — see below.

        // --- schema + migrations -------------------------------------------
        run_migration(&pool, &id, 0, "create_schema", &format!("CREATE SCHEMA IF NOT EXISTS \"{id}\";"))
            .await
            .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;

        let ctx = PluginContext {
            plugin_id: id.clone(),
            // Host-mediated: these Arc<dyn Host…> impls live in the core, so no
            // sqlx/tokio is ever linked into the plugin (see SDK host-I/O note).
            db: adjutant_sdk::DbHandle::new(core_db.clone(), id.clone()),
            config: config.clone(),
            events: EventBusHandle::new(
                crate::host::CoreEvents::new(pool.clone(), event_tx.clone(), id.clone()),
                id.clone(),
            ),
            permissions: PermissionService::new(core_db.clone()),
            audit: adjutant_sdk::AuditService::new(core_db.clone(), id.clone()),
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
        let granted = plugin.permissions_granted();
        for perm in &granted {
            sqlx::query(
                "INSERT INTO core.permissions (id, description) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET description = EXCLUDED.description",
            )
            .bind(&perm.id)
            .bind(&perm.description)
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

        // --- enabled state ---------------------------------------------------
        let enabled: bool = sqlx::query_scalar(
            "INSERT INTO core.plugins (id, version) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, updated_at = now() \
             RETURNING enabled",
        )
        .bind(&id)
        .bind(plugin.version())
        .fetch_one(pool.as_ref())
        .await
        .map_err(|e| PluginRuntimeError::Migration(id.clone(), e.to_string()))?;

        tracing::info!(
            id,
            version = plugin.version(),
            routes = routes.len(),
            permissions = granted.len(),
            enabled,
            "plugin loaded"
        );

        infos.push(PluginInfo {
            id: id.clone(),
            name: plugin.name().to_string(),
            version: plugin.version().to_string(),
            enabled,
            routes: routes.len(),
            permissions: granted.iter().map(|p| p.id.clone()).collect(),
        });
        seen_ids.insert(id, ());

        plugins.push(LoadedPlugin { plugin, _library: lib, routes, enabled });
    }

    Ok(PluginRegistry { plugins, infos })
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

    #[test]
    fn id_validation_matches_schema_rule() {
        assert!(validate_id("hello").is_ok());
        assert!(validate_id("shared_postgres").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("Hello").is_err());
        assert!(validate_id("0day").is_err());
        assert!(validate_id("drop table").is_err());
    }
}
