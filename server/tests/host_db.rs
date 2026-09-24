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

use adjutant_sdk::{HostDb, SqlValue};
use adjutant_server::host::CoreDb;
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

