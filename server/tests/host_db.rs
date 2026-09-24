//! DB-backed coverage for the host I/O layer — the code path behind M3 bug 2
//! (`text[]` arrays decoded to Null, which silently emptied every role list and
//! broke permission checks).
//!
//! These tests are `#[ignore]`d on purpose (issue #25). A bare
//! `cargo test --workspace` reports them as **ignored**, never as passed, so a
//! green local run cannot hide a database test that did not execute. CI runs
//! them explicitly against a throwaway `_test` database:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//!   cargo test -p adjutant-server --test host_db -- --ignored
//! ```
//!
//! Under `--ignored` a missing, empty, or unreachable
//! `ADJUTANT_TEST_DATABASE_URL` is a hard failure — never a skip.
use std::sync::Arc;

use adjutant_sdk::{
    schedule_handler, HostDb, Identity, PermissionService, RoleGrant, Schedule, Scope, SqlValue,
};
use adjutant_server::host::CoreDb;
use adjutant_server::scheduler::Scheduler;
use adjutant_server::scope_hierarchy::ScopeHierarchy;
use adjutant_server::{db, host, schema};

async fn repository_pool() -> Arc<sqlx::PgPool> {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to run the DB-gated host tests \
         (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("ADJUTANT_TEST_DATABASE_URL is set but unreachable");
    Arc::new(pool)
}

async fn db() -> Arc<CoreDb> {
    CoreDb::new(repository_pool().await)
}

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn decode_covers_every_supported_type() {
    let db = db().await;
    let rows = db
        .query(
            "SELECT '{\"a\":1}'::jsonb AS j, \
                    true AS b, \
                    7::bigint AS i, \
                    1.5::float8 AS f, \
                    'txt'::text AS t, \
                    ARRAY['x','y']::text[] AS arr, \
                    '2026-09-22 00:00:00+00'::timestamptz::text AS ts, \
                    NULL::text AS n"
                .to_string(),
            vec![],
        )
        .await
        .expect("query");
    let row = rows.first().expect("one row");
    assert_eq!(row["j"], serde_json::json!({"a": 1}), "jsonb decodes");
    assert_eq!(row["b"], serde_json::json!(true), "bool decodes");
    assert_eq!(row["i"], serde_json::json!(7), "i64 decodes");
    assert_eq!(row["f"], serde_json::json!(1.5), "f64 decodes");
    assert_eq!(row["t"], serde_json::json!("txt"), "text decodes");
    // The regression: before Vec<String> was added to the fallback chain this
    // came back as Null.
    assert_eq!(row["arr"], serde_json::json!(["x", "y"]), "text[] decodes");
    assert!(row["ts"].is_string(), "timestamptz cast to text decodes");
    assert_eq!(row["n"], serde_json::Value::Null, "NULL stays Null");
}

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn bind_params_round_trips_every_variant() {
    let db = db().await;
    let out = db
        .query(
            "SELECT $1::text AS t, $2::bigint AS i, $3::bool AS b, $4::text[] AS arr, $5::jsonb AS j"
                .to_string(),
            vec![
                SqlValue::Text("hello".into()),
                SqlValue::Int(42),
                SqlValue::Bool(true),
                SqlValue::TextArray(vec!["a".into(), "b".into()]),
                SqlValue::Json("{\"k\":1}".into()),
            ],
        )
        .await
        .expect("query");
    let row = out.first().expect("one row");
    assert_eq!(row["t"], serde_json::json!("hello"));
    assert_eq!(row["i"], serde_json::json!(42));
    assert_eq!(row["b"], serde_json::json!(true));
    assert_eq!(row["arr"], serde_json::json!(["a", "b"]));
    assert_eq!(row["j"], serde_json::json!({"k": 1}));
}

// ---------------------------------------------------------------------------
// Confinement probes (issues #17/#18; design docs/design/plugin-isolation.md §5)
//
// The plugin's connection IS the restricted principal. Each probe below fails
// if the mechanism regresses. They need a database whose connecting role can
// `CREATEROLE` (a throwaway superuser container); a failure to establish the
// boundary fails the test rather than skipping (issue #25).
// ---------------------------------------------------------------------------

fn base_url() -> String {
    std::env::var("ADJUTANT_TEST_DATABASE_URL").expect("ADJUTANT_TEST_DATABASE_URL")
}

/// Serializes fixture setup. `CREATE SCHEMA/TABLE IF NOT EXISTS` still races
/// between sessions, so concurrent probes would both try to migrate `core` and
/// one would fail; the lock makes setup strict. Probes themselves still run in
/// parallel.
static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes the probes that mutate the shared `core.scope_hierarchy` table
/// (they assert on its global contents, so they must not interleave).
static HIERARCHY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Migrate `core` on the test database and bootstrap a plugin fixture role/schema
/// for each id, returning `(admin_pool, plugin_pools)`. Mirrors a real
/// deployment's first `bootstrap-isolation` run.
async fn setup(ids: &[&str]) -> (Arc<sqlx::PgPool>, Vec<Arc<sqlx::PgPool>>) {
    let _guard = SETUP.lock().await;
    let cfg = adjutant_server::config::Config {
        database_url: base_url(),
        ..Default::default()
    };
    let admin = db::connect_and_migrate(&cfg)
        .await
        .expect("core migrations on the test database");

    let mut pools = Vec::new();
    for id in ids {
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{id}\" CASCADE"))
            .execute(admin.as_ref())
            .await;
        let secret = schema::bootstrap_role(admin.as_ref(), id, None, false)
            .await
            .expect("bootstrap plugin role (needs CREATEROLE/superuser)");
        let pool = host::plugin_pool(&base_url(), id, &secret, 2)
            .await
            .expect("plugin pool");
        pools.push(pool);
    }
    (admin, pools)
}

/// 1. A plugin migration that writes `core.*` must fail (E1). The DDL runs as
/// the plugin role on its own pool, so it has no rights in `core`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_migration_cannot_create_a_core_table() {
    let (admin, pools) = setup(&["iso_mig"]).await;
    let pool = pools[0].as_ref();

    let err = db::run_plugin_migration(
        pool,
        "iso_mig",
        1,
        "evil",
        "CREATE TABLE core.pwned_mig (id INT);",
    )
    .await
    .expect_err("a plugin migration must not create a core table");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );

    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('core.pwned_mig')::TEXT")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(exists.is_none(), "core.pwned_mig must not exist");
}

/// 2. The E2 escape: a `DO` block that `SET LOCAL ROLE`s the deployment role and
/// then writes `core.*`. The session user is the plugin role, which is not a
/// member of the deployment role, so `SET ROLE` is refused and nothing lands.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_do_block_cannot_set_role_and_write_core() {
    let (admin, pools) = setup(&["iso_do"]).await;
    let deployment: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(admin.as_ref())
        .await
        .expect("current_user");

    let evil = format!(
        "DO $$ BEGIN EXECUTE 'SET LOCAL ROLE \"{deployment}\"'; \
         EXECUTE 'CREATE TABLE core.pwned_do (id INT)'; END $$;"
    );
    let err = host::CoreDb::new(pools[0].clone())
        .execute(evil, vec![])
        .await
        .expect_err("the DO-block escape must be refused");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );

    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('core.pwned_do')::TEXT")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(exists.is_none(), "core.pwned_do must not exist");
}

/// 3. Plugin SQL cannot `CREATE ROLE … SUPERUSER` (E3).
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_plugin_sql_cannot_create_a_superuser_role() {
    let (_admin, pools) = setup(&["iso_createrole"]).await;
    let err = host::CoreDb::new(pools[0].clone())
        .execute("CREATE ROLE iso_evil SUPERUSER LOGIN".to_string(), vec![])
        .await
        .expect_err("plugin SQL must not create a role");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 4. `RESET ROLE` is inert (the session user is already the plugin role) and
/// `SET ROLE <other>` is refused.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_reset_role_is_inert_and_set_role_is_refused() {
    let (admin, pools) = setup(&["iso_role"]).await;
    let plugin = host::CoreDb::new(pools[0].clone());
    let expected = schema::role_for("iso_role");

    plugin
        .execute("RESET ROLE".to_string(), vec![])
        .await
        .expect("RESET ROLE is allowed but must be inert");
    let rows = plugin
        .query("SELECT session_user AS u, current_user AS c".to_string(), vec![])
        .await
        .expect("session_user query");
    assert_eq!(rows[0]["u"], serde_json::json!(expected));
    assert_eq!(rows[0]["c"], serde_json::json!(expected));

    let deployment: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(admin.as_ref())
        .await
        .expect("current_user");
    let err = plugin
        .execute(format!("SET ROLE \"{deployment}\""), vec![])
        .await
        .expect_err("SET ROLE to another role must be refused");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 5. Cross-schema read of another plugin's table is denied by PostgreSQL.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_cross_schema_read_is_denied() {
    let (_admin, pools) = setup(&["iso_a", "iso_b"]).await;
    let alpha = host::CoreDb::new(pools[0].clone());
    let beta = host::CoreDb::new(pools[1].clone());

    alpha
        .execute("CREATE TABLE secret (id INT)".to_string(), vec![])
        .await
        .expect("alpha creates a table in its own schema");
    let err = beta
        .query("SELECT id FROM iso_a.secret".to_string(), vec![])
        .await
        .expect_err("beta must not read alpha's table");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 6. A `core.*` table outside the plugin's allowlist is denied.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_unlisted_core_table_is_denied() {
    let (_admin, pools) = setup(&["iso_unlisted"]).await;
    let err = host::CoreDb::new(pools[0].clone())
        .query("SELECT count(*) FROM core.users".to_string(), vec![])
        .await
        .expect_err("core.users is not in the plugin's allowlist");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 7. Every plugin connection is the plugin role — a host regression (e.g. a
/// plugin pool built from the wrong URL) is caught even if no plugin tries to
/// escape.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_every_plugin_connection_is_the_plugin_role() {
    let (_admin, pools) = setup(&["iso_conn"]).await;
    let plugin = host::CoreDb::new(pools[0].clone());
    let expected = serde_json::json!(schema::role_for("iso_conn"));

    // Two concurrent queries force at least two pooled connections.
    let (r1, r2) = tokio::join!(
        plugin.query("SELECT session_user AS u".to_string(), vec![]),
        plugin.query("SELECT session_user AS u".to_string(), vec![]),
    );
    assert_eq!(r1.expect("query 1")[0]["u"], expected);
    assert_eq!(r2.expect("query 2")[0]["u"], expected);
}

/// #19: `membership`'s role must have no access to `core.user_roles` at all —
/// it had `INSERT`, which made `membership:manage` a path to granting `chief`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_membership_role_cannot_write_user_roles() {
    let (_admin, pools) = setup(&["membership"]).await;
    let membership = host::CoreDb::new(pools[0].clone());

    let err = membership
        .execute(
            "INSERT INTO core.user_roles (user_id, role_id) \
             VALUES ('00000000-0000-0000-0000-000000000000', 'chief')"
                .to_string(),
            vec![],
        )
        .await
        .expect_err("membership must not write core.user_roles");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// #33: `scope_id` is opaque TEXT owned by the plugin, `NULL` means troop-wide,
/// and the invalid combinations are unstorable. A lodge-scoped grant with a
/// bigint-looking id round-trips through storage and is honoured by
/// `has_in_scope` — the case the UUID column was blocking.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn probe_scope_id_is_opaque_text_and_round_trips() {
    let (admin, _pools) = setup(&[]).await;
    let uid = "11111111-1111-1111-1111-111111111111";

    sqlx::query("DELETE FROM core.user_roles").execute(admin.as_ref()).await.expect("clear");
    sqlx::query("INSERT INTO core.users (id, display_name) VALUES ($1::uuid, 'Bea') ON CONFLICT DO NOTHING")
        .bind(uid)
        .execute(admin.as_ref())
        .await
        .expect("user");
    // The role must hold a permission for `has_in_scope` to allow.
    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) VALUES ('scout', 'core:admin') \
         ON CONFLICT DO NOTHING",
    )
    .execute(admin.as_ref())
    .await
    .expect("grant permission to scout");

    // A bigint lodge id is storable as text.
    sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'scout', 'lodge', '1')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await
    .expect("lodge-scoped row stores a text id");

    let (role_id, scope_type, scope_id): (String, String, Option<String>) = sqlx::query_as(
        "SELECT role_id, scope_type, scope_id FROM core.user_roles WHERE user_id = $1::uuid",
    )
    .bind(uid)
    .fetch_one(admin.as_ref())
    .await
    .expect("read the row back");
    assert_eq!(role_id, "scout");
    assert_eq!(scope_type, "lodge");
    assert_eq!(scope_id.as_deref(), Some("1"), "the opaque id round-trips verbatim");

    // Build the identity the way a session would and check coverage.
    let identity = Identity::from_grants(
        uid,
        vec![RoleGrant { role_id, scope: Scope::lodge(scope_id.expect("id")) }],
    );
    let perms = PermissionService::new(host::CoreDb::new(admin.clone()));
    assert!(
        perms.has_in_scope(Some(&identity), "core:admin", &Scope::lodge("1")).await,
        "a lodge-scoped grant covers its own lodge"
    );
    assert!(
        !perms.has_in_scope(Some(&identity), "core:admin", &Scope::lodge("2")).await,
        "and not another lodge"
    );
    assert!(
        !perms.has_in_scope(Some(&identity), "core:admin", &Scope::troop()).await,
        "a lodge grant does not cover troop"
    );

    // Invalid combinations are unstorable (#33 acceptance).
    let lodge_without_id = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type) VALUES ($1::uuid, 'chief', 'lodge')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(lodge_without_id.is_err(), "a non-troop scope needs a scope_id");

    let troop_with_id = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'chief', 'troop', '7')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(troop_with_id.is_err(), "a troop scope must have a NULL scope_id");

    let troop_with_null = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'chief', 'troop', NULL)",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(troop_with_null.is_ok(), "troop-wide is scope_id NULL");
}

/// Hierarchical coverage: with a declared lodge→patrol edge, a lodge-scoped
/// grant covers the patrol inside it (and not another), and coverage stays
/// downward (a patrol grant does not cover its lodge). Edges are core-owned
/// data; no plugin is consulted at authorization time.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn probe_scope_hierarchy_lodge_covers_its_patrols() {
    let _h = HIERARCHY.lock().await;
    let (admin, _pools) = setup(&[]).await;
    sqlx::query("DELETE FROM core.scope_hierarchy").execute(admin.as_ref()).await.expect("clear");
    sqlx::query(
        "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
         VALUES ('lodge', '3', 'patrol', '7')",
    )
    .execute(admin.as_ref())
    .await
    .expect("declare the edge");

    // A self-edge is unstorable.
    let self_edge = sqlx::query(
        "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
         VALUES ('patrol', '7', 'patrol', '7')",
    )
    .execute(admin.as_ref())
    .await;
    assert!(self_edge.is_err(), "a self-edge must be rejected by the CHECK");

    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) \
         VALUES ('chief', 'core:admin'), ('scout', 'core:admin') ON CONFLICT DO NOTHING",
    )
    .execute(admin.as_ref())
    .await
    .expect("permissions");

    let hierarchy = ScopeHierarchy::load(admin.as_ref()).await.expect("load the hierarchy");
    let perms = PermissionService::new(host::CoreDb::new(admin.clone()));

    let lodge_grant = Identity::from_grants(
        "u",
        vec![RoleGrant { role_id: "chief".into(), scope: Scope::lodge("3") }],
    );
    let expanded = hierarchy.expand(&lodge_grant);
    assert!(
        perms.has_in_scope(Some(&expanded), "core:admin", &Scope::patrol("7")).await,
        "a lodge grant covers the patrol declared inside it"
    );
    assert!(
        !perms.has_in_scope(Some(&expanded), "core:admin", &Scope::patrol("8")).await,
        "and not a patrol that is not declared inside it"
    );

    let patrol_grant = Identity::from_grants(
        "u",
        vec![RoleGrant { role_id: "scout".into(), scope: Scope::patrol("7") }],
    );
    let patrol_expanded = hierarchy.expand(&patrol_grant);
    assert!(
        !perms.has_in_scope(Some(&patrol_expanded), "core:admin", &Scope::lodge("3")).await,
        "covering is downward only: a patrol grant does not cover its lodge"
    );
}

/// #37: hierarchy edges are owned per scope type. A plugin granted the
/// `core.scope_hierarchy` table but owning no scope type is refused — by the
/// trigger on raw SQL and by the declare function — with the edge absent, while
/// the owning plugin still succeeds and no plugin can write the ownership map.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_scope_edge_ownership_is_enforced() {
    let _h = HIERARCHY.lock().await;
    let (admin, pools) = setup(&["hello", "membership"]).await;
    let hello = pools[0].clone();
    let membership = pools[1].clone();
    let hello_db = host::CoreDb::new(hello);

    // The core seeds the ownership map; a plugin cannot write it.
    let seeded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_owners WHERE scope_type IN ('lodge','patrol')",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("read scope_owners");
    assert_eq!(seeded, 2, "the core seeds the ownership map");

    let map_write = hello_db
        .execute(
            "INSERT INTO core.scope_owners (scope_type, plugin_id) VALUES ('rogue', 'hello')"
                .to_string(),
            vec![],
        )
        .await;
    assert!(map_write.is_err(), "a plugin has no grant on core.scope_owners");

    // Simulate a future plugin granted the edge table but owning no scope type.
    sqlx::query("GRANT SELECT, INSERT, DELETE ON core.scope_hierarchy TO adjutant_plugin_hello")
        .execute(admin.as_ref())
        .await
        .expect("grant the edge table to hello");

    // Raw SQL is refused by the trigger, with a message naming the type/owner.
    let raw_err = hello_db
        .execute(
            "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
             VALUES ('lodge', '30', 'patrol', '90')"
                .to_string(),
            vec![],
        )
        .await
        .expect_err("hello does not own lodge/patrol")
        .to_string();
    assert!(raw_err.contains("may not declare"), "clear trigger error: {raw_err}");
    println!("[edge-ownership] trigger refusal: {raw_err}");

    // The declare API refuses it too, with its own clear message.
    let api_err = hello_db
        .execute(
            "SELECT core.declare_scope_parent('lodge', '30', 'patrol', '90')".to_string(),
            vec![],
        )
        .await
        .expect_err("the declare API refuses a type the plugin does not own")
        .to_string();
    assert!(api_err.contains("may not declare"), "clear API error: {api_err}");
    println!("[edge-ownership] declare-API refusal: {api_err}");

    // Query the table, not only the errors: the refused edge must be absent.
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_hierarchy \
         WHERE parent_type = 'lodge' AND parent_id = '30' \
           AND child_type = 'patrol' AND child_id = '90'",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("count refused edge");
    assert_eq!(present, 0, "the refused edge must not be in the table");

    // The legitimate owner still declares through the API, and the edge lands.
    host::CoreDb::new(membership)
        .execute(
            "SELECT core.declare_scope_parent('lodge', '30', 'patrol', '70')".to_string(),
            vec![],
        )
        .await
        .expect("membership owns lodge/patrol");
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_hierarchy \
         WHERE parent_type = 'lodge' AND parent_id = '30' \
           AND child_type = 'patrol' AND child_id = '70'",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("count legit edge");
    assert_eq!(present, 1, "the legitimate edge lands");

    let _ = sqlx::query("DELETE FROM core.scope_hierarchy WHERE parent_id = '30'")
        .execute(admin.as_ref())
        .await;
    let _ = sqlx::query("REVOKE ALL ON core.scope_hierarchy FROM adjutant_plugin_hello")
        .execute(admin.as_ref())
        .await;
}

/// #45: the core scheduler runs a declared schedule repeatedly (driven by a
/// channel and the run table, never by waiting on wall-clock minutes), the
/// handler observes the plugin's own role, a failure is recorded without
/// crashing or spinning, and stopping the plugin stops the ticks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_scheduler_runs_records_and_stops() {
    let (admin, pools) = setup(&["sched_probe"]).await;
    let plugin_pool = pools[0].clone();
    let role = schema::role_for("sched_probe");
    let scheduler = Scheduler::new();

    // A failing tick must not take the server down or spin: the handler returns
    // an error; the scheduler records it and waits for the next interval.
    let fail_sched = Schedule::new(
        "boom",
        std::time::Duration::from_millis(100),
        schedule_handler(|| async { Err::<(), _>(adjutant_sdk::SdkError::Internal("boom".into())) }),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    let db = host::CoreDb::new(plugin_pool);
    let tick = Schedule::new(
        "tick",
        std::time::Duration::from_millis(100),
        schedule_handler(move || {
            let db = db.clone();
            let tx = tx.clone();
            async move {
                let rows = db.query("SELECT session_user AS u".to_string(), vec![]).await?;
                let who = rows[0]["u"].as_str().unwrap_or_default().to_string();
                let _ = tx.send(who).await;
                Ok(())
            }
        }),
    );
    scheduler.start("sched_probe", vec![tick, fail_sched], admin.clone());

    // Two ticks within a few seconds: repeated execution, observed live.
    for _ in 0..2 {
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a scheduled run within 5s")
            .expect("channel open");
        assert_eq!(got, role, "the handler runs as the plugin's own role");
    }

    // Durable evidence: rows in core.scheduled_runs. The handler signals the
    // channel before the scheduler writes the row, so poll briefly.
    let mut ok_runs: i64 = 0;
    for _ in 0..50 {
        ok_runs = sqlx::query_scalar(
            "SELECT count(*) FROM core.scheduled_runs WHERE plugin_id='sched_probe' AND schedule='tick' AND ok",
        )
        .fetch_one(admin.as_ref())
        .await
        .expect("count ok runs");
        if ok_runs >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ok_runs >= 2, "at least two successful runs recorded, got {ok_runs}");

    // The admin view (what PluginInfo reports) has last_run and next_run.
    let infos = scheduler.infos("sched_probe");
    assert_eq!(infos.len(), 2, "both schedules listed");
    let tick_info = infos.iter().find(|s| s.name == "tick").expect("tick listed");
    assert!(tick_info.last_run.is_some(), "last_run is reported");
    assert!(tick_info.next_run.is_some(), "next_run is reported");

    // The failing schedule records failures but does not spin.
    let count_bad = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM core.scheduled_runs \
             WHERE plugin_id='sched_probe' AND schedule='boom' AND NOT ok AND error IS NOT NULL",
        )
        .fetch_one(admin.as_ref())
        .await
        .expect("count failed runs")
    };
    let mut bad_runs = 0;
    for _ in 0..50 {
        bad_runs = count_bad().await;
        if bad_runs >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(bad_runs >= 1, "a failure is recorded");
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let bad_after = count_bad().await;
    assert!(
        bad_after <= bad_runs + 8,
        "a failing schedule must not spin: {bad_runs} -> {bad_after} in ~0.45s"
    );
    let boom = scheduler.infos("sched_probe").into_iter().find(|s| s.name == "boom").unwrap();
    assert!(boom.last_error.is_some(), "last_error is reported");

    // Stop: the run count must stop rising (allow an in-flight insert to land).
    scheduler.stop("sched_probe");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM core.scheduled_runs WHERE plugin_id='sched_probe'")
            .fetch_one(admin.as_ref())
            .await
            .expect("count before");
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM core.scheduled_runs WHERE plugin_id='sched_probe'")
            .fetch_one(admin.as_ref())
            .await
            .expect("count after");
    assert_eq!(before, after, "a stopped plugin's schedules must not keep firing");
}
