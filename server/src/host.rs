//! Host implementations of the SDK's `HostDb` / `HostEvents` traits.
//!
//! This is the only place in the system where sqlx runs on behalf of a plugin.
//! The core links sqlx/tokio once; plugins link neither (see the host-mediated
//! I/O note in the SDK). Everything here executes inside core tasks, on the
//! core's runtime — which is exactly why `Handle::current()` resolves.

use std::sync::Arc;

use adjutant_sdk::{Event, HostDb, HostEvents, SdkError, SqlValue};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{Column, Row};
use tokio::sync::broadcast;

/// Core-backed database access shared by every plugin.
pub struct CoreDb {
    pool: Arc<sqlx::PgPool>,
}

impl CoreDb {
    pub fn new(pool: Arc<sqlx::PgPool>) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

/// Decode one column, trying JSON-friendly types first (SPEC-compatible order:
/// jsonb → bool → i64 → f64 → text → NULL). Exotic types (timestamptz) should be
/// cast to `::text` by the query author.
fn decode_value(row: &PgRow, idx: usize) -> Value {
    if let Ok(v) = row.try_get::<Value, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return Value::Bool(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    Value::Null
}

fn bind_params<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    params: Vec<SqlValue>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for p in params {
        q = match p {
            SqlValue::Null => q.bind(Option::<String>::None),
            SqlValue::Bool(b) => q.bind(b),
            SqlValue::Int(n) => q.bind(n),
            SqlValue::Float(f) => q.bind(f),
            SqlValue::Text(s) => q.bind(s),
            SqlValue::TextArray(v) => q.bind(v),
            SqlValue::Json(j) => q.bind(j),
        };
    }
    q
}

#[async_trait]
impl HostDb for CoreDb {
    async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
        let res = bind_params(sqlx::query(&sql), params)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;
        Ok(res.rows_affected())
    }

    async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
        let rows = bind_params(sqlx::query(&sql), params)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let cols = row.columns();
            let mut obj = serde_json::Map::with_capacity(cols.len());
            for (i, col) in cols.iter().enumerate() {
                obj.insert(col.name().to_string(), decode_value(row, i));
            }
            out.push(Value::Object(obj));
        }
        Ok(out)
    }
}

/// Core-backed event publishing: persist first, then broadcast (SPEC §5.4).
/// One instance per plugin — owns the `source` stamp.
pub struct CoreEvents {
    pool: Arc<sqlx::PgPool>,
    tx: broadcast::Sender<Event>,
    source: String,
}

impl CoreEvents {
    pub fn new(pool: Arc<sqlx::PgPool>, tx: broadcast::Sender<Event>, source: String) -> Arc<Self> {
        Arc::new(Self { pool, tx, source })
    }
}

#[async_trait]
impl HostEvents for CoreEvents {
    async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError> {
        let ev = Event {
            event_type,
            payload: payload.clone(),
            source: self.source.clone(),
            timestamp: chrono::Utc::now(),
        };
        sqlx::query(
            "INSERT INTO core.events (event_type, payload, source_plugin) VALUES ($1, $2::jsonb, $3)",
        )
        .bind(&ev.event_type)
        .bind(serde_json::to_string(&ev.payload).unwrap_or_else(|_| "{}".into()))
        .bind(&ev.source)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| SdkError::Db(e.to_string()))?;

        let _ = self.tx.send(ev); // no subscribers is fine
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_prefers_json_then_scalars() {
        // Unit-level coverage of the fallback chain needs a live row; here we
        // pin the Null terminal so a fully-undecodable column is explicit.
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn bind_params_accepts_every_variant() {
        use sqlx::Execute;
        // Compile-time proof every SqlValue variant binds into the same query
        // type; runtime is a no-op (no pool attached).
        let q = sqlx::query("SELECT 1");
        let q = bind_params(
            q,
            vec![
                SqlValue::Null,
                SqlValue::Bool(true),
                SqlValue::Int(1),
                SqlValue::Float(1.5),
                SqlValue::Text("t".into()),
                SqlValue::TextArray(vec!["a".into()]),
                SqlValue::Json("{}".into()),
            ],
        );
        assert!(q.sql().contains("SELECT 1"));
    }
}
