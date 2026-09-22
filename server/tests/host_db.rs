//! DB-backed coverage for the host I/O layer — the code path behind M3 bug 2
//! (`text[]` arrays decoded to Null, which silently emptied every role list and
//! broke permission checks).
//!
//! Runs only when `ADJUTANT_TEST_DATABASE_URL` is set, so `cargo test
//! --workspace` stays green on a machine without PostgreSQL. The skip is
//! printed, never silent.
use std::sync::Arc;

use adjutant_sdk::{HostDb, SqlValue};
use adjutant_server::host::CoreDb;

async fn db() -> Option<Arc<CoreDb>> {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("ADJUTANT_TEST_DATABASE_URL is set but unreachable");
    Some(CoreDb::new(Arc::new(pool)))
}

#[tokio::test]
async fn decode_covers_every_supported_type() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED decode_covers_every_supported_type: set ADJUTANT_TEST_DATABASE_URL");
        return;
    };
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
async fn bind_params_round_trips_every_variant() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED bind_params_round_trips_every_variant: set ADJUTANT_TEST_DATABASE_URL");
        return;
    };
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
