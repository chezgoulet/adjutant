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

/// Schema isolation is enforced by PostgreSQL, not by convention: a plugin's
/// `SET LOCAL ROLE` handle can read its own schema but is denied another
/// plugin's. The test database role must be able to manage roles
/// (`CREATEROLE`/superuser); if it cannot, the isolation proof cannot run and
/// the test fails rather than skipping (issue #25 — a skipped proof is not
/// coverage).
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn plugin_role_isolation_denies_cross_schema_access() {
    use adjutant_server::schema;

    let pool = repository_pool().await;

    for s in ["iso_alpha", "iso_beta"] {
        sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"))
            .execute(pool.as_ref())
            .await
            .expect("drop schema");
        sqlx::query(&format!("CREATE SCHEMA \"{s}\""))
            .execute(pool.as_ref())
            .await
            .expect("create schema");
    }

    let alpha_role = schema::ensure_isolation(&pool, "iso_alpha").await.expect(
        "the test database role must manage roles (CREATEROLE/superuser) so the \
         isolation proof actually runs; use a throwaway superuser database",
    );
    let _ = schema::ensure_isolation(&pool, "iso_beta").await;

    sqlx::query("CREATE TABLE iso_alpha.t (id BIGINT PRIMARY KEY)")
        .execute(pool.as_ref())
        .await
        .expect("alpha table");
    sqlx::query("CREATE TABLE iso_beta.t (id BIGINT PRIMARY KEY)")
        .execute(pool.as_ref())
        .await
        .expect("beta table");
    sqlx::query("INSERT INTO iso_alpha.t VALUES (1)")
        .execute(pool.as_ref())
        .await
        .expect("alpha row");
    sqlx::query("INSERT INTO iso_beta.t VALUES (2)")
        .execute(pool.as_ref())
        .await
        .expect("beta row");
    // Migrations run after the first grant pass, so refresh (as load_all does).
    schema::grant_schema_objects(&pool, "iso_alpha", &alpha_role)
        .await
        .expect("refresh alpha grants");

    let alpha = CoreDb::for_plugin(pool.clone(), "iso_alpha".into(), Some(alpha_role));

    // Own schema reachable via a bare table name.
    let rows = alpha
        .query("SELECT id FROM t".to_string(), vec![])
        .await
        .expect("alpha can read its own table");
    assert_eq!(rows[0]["id"], serde_json::json!(1));

    // Another plugin's schema is denied by the database.
    let err = alpha
        .query("SELECT id FROM iso_beta.t".to_string(), vec![])
        .await
        .expect_err("alpha must not read beta's schema");
    let msg = err.to_string();
    assert!(
        msg.contains("permission denied") || msg.contains("does not exist"),
        "expected a permission error, got: {msg}"
    );

    for s in ["iso_alpha", "iso_beta"] {
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"))
            .execute(pool.as_ref())
            .await;
    }
}
