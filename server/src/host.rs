//! Host implementations of the SDK's `HostDb` / `HostEvents` traits.
//!
//! This is the only place in the system where sqlx runs on behalf of a plugin.
//! The core links sqlx/tokio once; plugins link neither (see the host-mediated
//! I/O note in the SDK). Everything here executes inside core tasks, on the
//! core's runtime — which is exactly why `Handle::current()` resolves.

use std::sync::Arc;

use adjutant_sdk::{Event, HostDb, HostEvents, HostHttp, HttpResponse, SdkError, SqlValue};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{Column, Row, TypeInfo};
use sqlx::ValueRef as _;
use tokio::sync::broadcast;

/// Core-backed database access. The **pool** decides the principal:
/// core services use the deployment pool, a plugin uses the pool authenticated
/// as its own `adjutant_plugin_<id>` role (see [`plugin_pool`]). There is no
/// per-call `SET ROLE`/`search_path` — the connection identity is the boundary
/// (`docs/design/plugin-isolation.md`).
pub struct CoreDb {
    pool: Arc<sqlx::PgPool>,
}

impl CoreDb {
    pub fn new(pool: Arc<sqlx::PgPool>) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

/// Build a plugin's pool, authenticated as `adjutant_plugin_<id>` with the
/// stored secret. Connects eagerly so a missing/rotated credential fails the
/// load loudly instead of surfacing as a 500 on the first request.
///
/// `max_connections` is small on purpose (design §3.3): the budget is
/// `plugins × max_connections`; see `docs/deployment.md`.
pub async fn plugin_pool(
    base_url: &str,
    plugin_id: &str,
    secret: &str,
    max_connections: u32,
) -> Result<Arc<sqlx::PgPool>, sqlx::Error> {
    use std::str::FromStr;
    let opts = sqlx::postgres::PgConnectOptions::from_str(base_url)?
        .username(&crate::schema::role_for(plugin_id))
        .password(secret);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await?;
    Ok(Arc::new(pool))
}

/// Decode one column into JSON.
///
/// sqlx requires an **exact** type match, so this is a whitelist: json/jsonb,
/// bool, int8/int4/int2, float8/float4, text[], the chrono date/time family, and
/// text. Anything else (uuid, numeric, bytea, inet, …) must be cast in SQL
/// (`id::text`) — and if it is not, the fallback below logs it rather than
/// returning a silent NULL, which is how a DATE column once came back null from a
/// NOT NULL column in a shipped route.
fn decode_value(row: &PgRow, idx: usize) -> Value {
    // A genuine SQL NULL is data, not a decode failure: return it without
    // attempting the type-specific paths below or warning about them.
    if let Ok(raw) = row.try_get_raw(idx) {
        if raw.is_null() {
            return Value::Null;
        }
    }
    if let Ok(v) = row.try_get::<Value, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return Value::Bool(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<i32, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<i16, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f32, _>(idx) {
        return Value::from(v as f64);
    }
    // text[] — needed for any array column (e.g. core.user_roles roles).
    // Without this, TEXT[] matched none of the above and fell to Null,
    // which silently emptied role lists and broke permission checks.
    if let Ok(v) = row.try_get::<Vec<String>, _>(idx) {
        return Value::Array(v.into_iter().map(Value::String).collect());
    }
    // chrono family (sqlx `chrono` feature). DATE/TIMESTAMP/TIMESTAMPTZ used to
    // fall through to Null unless every query remembered `::text`.
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return Value::String(v.to_rfc3339());
    }
    // uuid — a very common primary/foreign key; decoded to its canonical string
    // form so plugins don't have to remember `id::text` on every query.
    if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    if let Some(col) = row.columns().get(idx) {
        tracing::warn!(
            column = %col.name(),
            pg_type = %col.type_info().name(),
            "host cannot represent this column as JSON; returning null (cast it in SQL, e.g. ::text)"
        );
    }
    Value::Null
}

fn bind_params<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    params: Vec<SqlValue>,
) -> Result<sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, SdkError> {
    for p in params {
        q = match p {
            SqlValue::Null => q.bind(Option::<String>::None),
            // Typed nulls: a text NULL cannot be assigned to a bigint/bool/uuid
            // column, and casting a bare parameter would make Postgres infer it
            // as text.
            SqlValue::NullInt => q.bind(Option::<i64>::None),
            SqlValue::NullBool => q.bind(Option::<bool>::None),
            SqlValue::NullUuid => q.bind(Option::<uuid::Uuid>::None),
            SqlValue::Bool(b) => q.bind(b),
            SqlValue::Int(n) => q.bind(n),
            SqlValue::Float(f) => q.bind(f),
            SqlValue::Text(s) => q.bind(s),
            // Parse at the boundary: a malformed uuid is the caller's fault (400),
            // not a 500 from a failed SQL bind.
            SqlValue::Uuid(s) => {
                let u = uuid::Uuid::parse_str(&s)
                    .map_err(|e| SdkError::BadRequest(format!("invalid uuid {s:?}: {e}")))?;
                q.bind(u)
            }
            SqlValue::IntArray(v) => q.bind(v),
            SqlValue::TextArray(v) => q.bind(v),
            SqlValue::Json(j) => q.bind(j),
        };
    }
    Ok(q)
}

#[async_trait]
impl HostDb for CoreDb {
    async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
        let res = bind_params(sqlx::query(&sql), params)?
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;
        Ok(res.rows_affected())
    }

    async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
        let rows = bind_params(sqlx::query(&sql), params)?
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

/// Core-backed HTTP for plugins (SPEC: host-mediated I/O — see SDK docs).
/// One shared `reqwest::Client` (connection pooling, 15s timeout, no
/// redirect-following surprises for OIDC endpoints).
pub struct CoreHttp {
    client: reqwest::Client,
}

impl CoreHttp {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
        })
    }
}

#[async_trait]
impl HostHttp for CoreHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| SdkError::BadRequest(format!("bad method {method}: {e}")))?;
        let mut req = self.client.request(m, &url);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = match body {
            Some((ct, bytes)) => req.header("content-type", ct).body(bytes),
            None => req,
        };
        let resp = req
            .send()
            .await
            .map_err(|e| SdkError::Internal(format!("http {method} {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let hdrs: std::collections::HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SdkError::Internal(format!("http body read failed: {e}")))?;
        Ok(HttpResponse {
            status,
            headers: hdrs,
            body: bytes.to_vec(),
        })
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
        let mut ev = Event {
            id: 0,
            event_type,
            payload: payload.clone(),
            source: self.source.clone(),
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&ev.payload).unwrap_or_else(|_| "{}".into());
        // Persist first, capture the row id, then broadcast with the real id
        // (SPEC §5.4: core.events is the durable record; subscribers see ids).
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO core.events (event_type, payload, source_plugin)              VALUES ($1, $2::jsonb, $3) RETURNING id",
        )
        .bind(&ev.event_type)
        .bind(&json)
        .bind(&ev.source)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| SdkError::Db(e.to_string()))?;
        ev.id = id;

        let _ = self.tx.send(ev); // no subscribers is fine
        Ok(())
    }

    async fn replay(&self, since_id: i64, limit: i64) -> Result<Vec<Event>, SdkError> {
        let rows = sqlx::query_as::<
            _,
            (i64, String, Value, String, chrono::DateTime<chrono::Utc>),
        >(
            "SELECT id, event_type, payload, source_plugin, created_at              FROM core.events WHERE id > $1 ORDER BY id ASC LIMIT $2",
        )
        .bind(since_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| SdkError::Db(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(id, event_type, payload, source, created_at)| Event {
                id,
                event_type,
                payload,
                source,
                timestamp: created_at,
            })
            .collect())
    }
}

// Decode-order coverage needs a real PgRow, so it lives in
// `server/tests/host_db.rs` (skipped unless ADJUTANT_TEST_DATABASE_URL is set).
