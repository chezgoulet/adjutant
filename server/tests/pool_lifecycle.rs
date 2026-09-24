//! DB-gated pool-lifecycle probe (issue #30).
//!
//! Retiring a plugin (uninstall or `replace_all` on reload) must close its
//! `PgPool`; the *library* stays mapped (in-flight requests keep valid code),
//! but a retired plugin is never read again, and leaking two connections per
//! reload exhausts PostgreSQL's default 100 after ~15 reloads. `retired_count()`
//! counts libraries, not connections — which is why this went unnoticed — so the
//! probe measures `pg_stat_activity` directly.
//!
//! Run with `ADJUTANT_TEST_DATABASE_URL=…` and `-- --ignored`.

use std::sync::Arc;

use adjutant_sdk::{async_trait, AdjutantPlugin, PluginContext, RouteDefinition, SdkError};
use adjutant_server::host;
use adjutant_server::plugin_runtime::{LoadedPlugin, PluginInfo, PluginRegistry};

struct Noop;

#[async_trait]
impl AdjutantPlugin for Noop {
    fn id(&self) -> &str {
        "leak_probe"
    }
    fn name(&self) -> &str {
        "Leak Probe"
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
}

fn fixture(id: &str, pool: Arc<sqlx::PgPool>) -> LoadedPlugin {
    LoadedPlugin {
        plugin: Box::new(Noop),
        library: None,
        pool: Some(pool),
        routes: Vec::new(),
        enabled: true,
        info: PluginInfo {
            id: id.into(),
            name: id.into(),
            version: "0.0.1".into(),
            enabled: true,
            routes: 0,
            kind: "native".into(),
            isolated: true,
            permissions: Vec::new(),
            route_list: Vec::new(),
        },
    }
}

/// Connections currently open as `role` (the plugin's own role).
async fn plugin_connections(admin: &sqlx::PgPool, role: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE usename = $1")
        .bind(role)
        .fetch_one(admin)
        .await
        .expect("pg_stat_activity")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn retiring_plugins_does_not_leak_connections() {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect("ADJUTANT_TEST_DATABASE_URL");
    let cfg = adjutant_server::config::Config {
        database_url: url.clone(),
        ..Default::default()
    };
    let admin = adjutant_server::db::connect_and_migrate(&cfg)
        .await
        .expect("core migrations");

    let id = "leak_probe";
    let secret = adjutant_server::schema::bootstrap_role(admin.as_ref(), id, None, false)
        .await
        .expect("bootstrap plugin role (needs CREATEROLE/superuser)");
    let role = adjutant_server::schema::role_for(id);

    let new_pool = || async {
        host::plugin_pool(&url, id, &secret, 2)
            .await
            .expect("plugin pool")
    };

    let mut reg = PluginRegistry::new(vec![fixture(id, new_pool().await)]);
    let baseline = plugin_connections(&admin, &role).await;
    assert!(baseline >= 1, "the live pool has at least one connection");

    // Ten reloads; each retires the previous generation (and its pool).
    for _ in 0..10 {
        reg.replace_all(PluginRegistry::new(vec![fixture(id, new_pool().await)]));
    }

    // `close()` drains on a spawned task; poll (bounded) until the retired
    // pools are gone. Without the fix this stays ~11 and never returns.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let now = plugin_connections(&admin, &role).await;
        if now <= baseline {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "plugin connections leaked: baseline={baseline}, now={now} (expected <= {baseline})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    assert_eq!(reg.retired_count(), 10, "libraries stay retired");
    assert_eq!(
        plugin_connections(&admin, &role).await,
        baseline,
        "retiring a plugin closes its pool; only the live generation's connections remain"
    );

    // Uninstall closes it too.
    assert!(reg.uninstall(id));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while plugin_connections(&admin, &role).await > 0 {
        assert!(tokio::time::Instant::now() < deadline, "uninstall leaked its pool");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
