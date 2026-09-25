//! # The store plugin's scholarship draw, against a real database
//!
//! The probes the mock host cannot write: **one statement** completes (or comps) an
//! order and enqueues its scholarship draw as an outbox intent, so a failure on
//! either side leaves neither; a replay of the same completion is one order, one
//! intent and the same intent id; and the worklist lists a draw that is in flight
//! and drops it once the intent has landed — decided by the intent's durable state,
//! not by a notification that may never arrive.
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_dev_test \
//!   cargo test -p adjutant-store --test outbox_draw -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.outbox`, `core.outbox_enqueue` and the declared `svc.store.draw`
//! principal exist), finance's (the payload names funds by code, so this plugin
//! never reads finance — but the ladder applies every plugin's migrations), and
//! the `store` plugin's own role created with its secret stored in
//! `core.plugins.db_secret`. The live ladder produces exactly that:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes then connect **as the plugin's own role**, because
//! `core.outbox_enqueue` derives the producer from `session_user`: a statement run
//! as anything else would be refused, and that refusal is the security property,
//! not an obstacle to be worked around. They read the stored secret rather than
//! rotating it, so a later boot of the same database still works.
//!
//! They write only rows named by a `probe_` order, and each probe clears its own
//! before and after.

use std::sync::Arc;

use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use adjutant_store::{
    migrations, StorePlugin, DRAW_BOOKED, DRAW_INTENT_ENQUEUED, DRAW_PRINCIPAL, FUND_GENERAL,
    FUND_SCHOLARSHIP, OUTBOX_DELIVERED, OUTBOX_EVENT_PREFIX, STATUS_COMPED, STATUS_OPEN,
};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `core.outbox_enqueue` derives it from `session_user`.
const PLUGIN_ROLE: &str = "adjutant_plugin_store";
/// The principal the core declares for this plugin's draw only.
const DRAW_KEY_PREFIX: &str = "store-order-";

/// Serialises the probes: one of them revokes the service principal, and a
/// queue-wide claim would interleave with another's intent.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the outbox probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test database \
             a boot has already prepared (they are #[ignore]d; run with `-- --ignored`)"
        )
    });
    assert!(
        !url.trim().is_empty(),
        "the database URL is set but empty; set it to a _test database or unset it"
    );
    let name = url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    if !name.ends_with("_test") {
        println!("[store outbox probe] WARNING: running against {name:?}, which does not end in `_test`");
    }
    url
}

/// The same URL with the plugin role's credentials, so the connection's
/// `session_user` is the plugin — which is what the enqueue function checks.
fn with_role(url: &str, role: &str, secret: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let after = match rest.split_once('@') {
                Some((_, host)) => host,
                None => rest,
            };
            format!("{scheme}://{role}:{secret}@{after}")
        }
        None => url.to_string(),
    }
}

async fn admin_pool() -> PgPool {
    PgPool::connect(&test_database_url())
        .await
        .expect("the database URL is set but unreachable")
}

/// One plugin's pool: **its own role**, with the secret a boot stored.
async fn plugin_pool(admin: &PgPool) -> PgPool {
    let url = test_database_url();
    let plugin_id = "store";
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = $1")
            .bind(plugin_id)
            .fetch_optional(admin)
            .await
            .expect("read the plugin's stored secret")
            .flatten();
    let Some(secret) = secret else {
        panic!(
            "this test database has no bootstrapped role for the `{plugin_id}` plugin (no \
             core.plugins.db_secret row). Run the live ladder against it first: \
             `ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`"
        );
    };
    let role = format!("adjutant_plugin_{plugin_id}");
    PgPool::connect(&with_role(&url, &role, &secret))
        .await
        .unwrap_or_else(|e| {
            panic!("could not connect as {role}: {e}; run the live ladder against this database")
        })
}

/// The core's own preconditions, stated rather than assumed.
async fn ensure_core_ready(admin: &PgPool) {
    let enqueue: Option<String> = sqlx::query_scalar(
        "SELECT p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
          WHERE n.nspname = 'core' AND p.proname = 'outbox_enqueue'",
    )
    .fetch_optional(admin)
    .await
    .expect("look for core.outbox_enqueue");
    assert!(
        enqueue.is_some(),
        "core migration 9 is not applied to this database (no core.outbox_enqueue)"
    );
    let (producer, declared): (Option<String>, Option<bool>) = sqlx::query_as(
        "SELECT producer_plugin, declared_by_core FROM core.service_principals WHERE principal = $1",
    )
    .bind(DRAW_PRINCIPAL)
    .fetch_optional(admin)
    .await
    .expect("read the declared service principal")
    .map(|(a, b)| (Some(a), Some(b)))
    .unwrap_or((None, None));
    assert_eq!(
        producer.as_deref(),
        Some("store"),
        "{DRAW_PRINCIPAL} is not declared for the `store` producer — run the live ladder \
         against this database so the core seeds its service principals"
    );
    assert_eq!(declared, Some(true), "the declaration must be the core's own");
}

/// Apply this plugin's migrations, as the plugin role, in its own schema — the
/// same shape the runner uses (one transaction, `SET LOCAL search_path`).
///
/// A database the ladder prepared has them already, and every statement is
/// idempotent (version 2 drops the check it replaces), so this is a no-op there.
async fn ensure_plugin_schema(admin: &PgPool, plugin: &PgPool) {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS store AUTHORIZATION {PLUGIN_ROLE}"
    ))
    .execute(admin)
    .await
    .expect("the store schema, as `bootstrap-isolation` creates it");
    sqlx::query(&format!("ALTER SCHEMA store OWNER TO {PLUGIN_ROLE}"))
        .execute(admin)
        .await
        .expect("the plugin role owns its own schema");
    let mut conn = plugin.acquire().await.expect("plugin connection");
    for migration in migrations::all() {
        let script = format!(
            "BEGIN; SET LOCAL search_path TO \"store\"; {}; COMMIT;",
            migration.sql
        );
        use sqlx::Executor;
        Executor::execute(&mut *conn, sqlx::raw_sql(&script))
            .await
            .unwrap_or_else(|e| panic!("migration {} ({}): {e}", migration.version, migration.name));
    }
}

// ---------------------------------------------------------------------------
// The rows a probe writes, and reads back
// ---------------------------------------------------------------------------

/// Clear anything a previous run left behind. Called **before** the order is
/// written (and again after), never between the insert and the assertion.
async fn cleanup_orders(admin: &PgPool, member_id: &str) {
    let _ = sqlx::query("DELETE FROM store.orders WHERE member_id = $1")
        .bind(member_id)
        .execute(admin)
        .await;
}

async fn cleanup_intent(admin: &PgPool, key: &str) {
    let _ = sqlx::query("DELETE FROM core.outbox WHERE idempotency_key = $1")
        .bind(key)
        .execute(admin)
        .await;
}

/// One order, written the way a placement writes it: a $20 price, charged in
/// full, nothing funded, no draw.
async fn insert_order(admin: &PgPool, member_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO store.orders \
           (member_id, placed_by, status, currency, price_tier, price_cents, charged_cents, \
            funded_cents, fund_code, draw_status) \
         VALUES ($1, $1, 'open', 'cad', 'standard', 2000, 2000, 0, $2, 'none') \
         RETURNING id",
    )
    .bind(member_id)
    .bind(FUND_GENERAL)
    .fetch_one(admin)
    .await
    .expect("insert the probe order")
}

async fn order_state(admin: &PgPool, order_id: i64) -> (String, String, Option<i64>) {
    sqlx::query_as(
        "SELECT status, draw_status, draw_intent_id FROM store.orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_one(admin)
    .await
    .expect("read the order")
}

async fn count_intents(admin: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)::bigint FROM core.outbox WHERE producer_plugin = 'store' \
          AND idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(admin)
    .await
    .expect("count intents")
}

// ---------------------------------------------------------------------------
// The host the plugin runs against: a real database, no outbound HTTP
// ---------------------------------------------------------------------------

/// The core's own decoding order (`server/src/host.rs`), so the plugin sees
/// exactly the rows it sees in production.
fn decode_value(row: &PgRow, idx: usize) -> Value {
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
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    Value::Null
}

/// `HostDb` over the **plugin's** pool, binding exactly as the core does.
struct PgDb(PgPool);

#[async_trait]
impl HostDb for PgDb {
    async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
        let mut q = sqlx::query(&sql);
        for p in params {
            q = match p {
                SqlValue::Null => q.bind(Option::<String>::None),
                SqlValue::NullInt => q.bind(Option::<i64>::None),
                SqlValue::NullBool => q.bind(Option::<bool>::None),
                SqlValue::NullUuid => q.bind(Option::<sqlx::types::Uuid>::None),
                SqlValue::Bool(b) => q.bind(b),
                SqlValue::Int(n) => q.bind(n),
                SqlValue::Float(f) => q.bind(f),
                SqlValue::Text(s) => q.bind(s),
                SqlValue::Uuid(s) => q.bind(
                    sqlx::types::Uuid::parse_str(&s)
                        .map_err(|e| SdkError::BadRequest(format!("invalid uuid {s:?}: {e}")))?,
                ),
                SqlValue::IntArray(v) => q.bind(v),
                SqlValue::TextArray(v) => q.bind(v),
                SqlValue::Json(j) => q.bind(j),
            };
        }
        let res = q
            .execute(&self.0)
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;
        Ok(res.rows_affected())
    }

    async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
        let mut q = sqlx::query(&sql);
        for p in params {
            q = match p {
                SqlValue::Null => q.bind(Option::<String>::None),
                SqlValue::NullInt => q.bind(Option::<i64>::None),
                SqlValue::NullBool => q.bind(Option::<bool>::None),
                SqlValue::NullUuid => q.bind(Option::<sqlx::types::Uuid>::None),
                SqlValue::Bool(b) => q.bind(b),
                SqlValue::Int(n) => q.bind(n),
                SqlValue::Float(f) => q.bind(f),
                SqlValue::Text(s) => q.bind(s),
                SqlValue::Uuid(s) => q.bind(
                    sqlx::types::Uuid::parse_str(&s)
                        .map_err(|e| SdkError::BadRequest(format!("invalid uuid {s:?}: {e}")))?,
                ),
                SqlValue::IntArray(v) => q.bind(v),
                SqlValue::TextArray(v) => q.bind(v),
                SqlValue::Json(j) => q.bind(j),
            };
        }
        let rows = q
            .fetch_all(&self.0)
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

/// A comp makes **no** outbound call: the draw is an intent the core's relay
/// delivers, so a probe that reached HTTP would be proving the wrong mechanism.
/// This handle refuses loudly rather than quietly passing.
#[derive(Default)]
struct NoHttp {
    calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl HostHttp for NoHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        _headers: Vec<(String, String)>,
        _body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        self.calls
            .lock()
            .expect("http calls")
            .push(format!("{method} {url}"));
        Err(SdkError::Internal(
            "the store probe's draw is an outbox intent; nothing should be called".into(),
        ))
    }
}

/// Events and identity registration are not what these probes are about; the
/// plugin still gets real handles, so nothing is skipped by a mock.
#[derive(Default)]
struct InertEvents;

#[async_trait]
impl HostEvents for InertEvents {
    async fn publish(&self, _event_type: String, _payload: Value) -> Result<(), SdkError> {
        Ok(())
    }
    async fn replay(&self, _since_id: i64, _limit: i64) -> Result<Vec<Event>, SdkError> {
        Ok(Vec::new())
    }
}

struct NoIdentity;

impl IdentityRegistrar for NoIdentity {
    fn register(&self, _owner: &str, _provider: Arc<dyn IdentityProvider>) {}
}

/// The plugin's `core.plugins.config` block, as a boot hands it over.
fn config_json() -> Value {
    json!({ "base_url": "http://127.0.0.1:8787", "fund_code": FUND_GENERAL })
}

/// A real `PluginContext`, with the **two** database hosts the core hands a
/// plugin (`plugin_runtime::build_context`): `ctx.db` is the plugin's own role —
/// its schema, and the `session_user` `core.outbox_enqueue` derives the producer
/// from — while the audit and permission services run on the **core's** pool,
/// exactly as they do in production.
fn context(admin: &PgPool, plugin: &PgPool, http: Arc<NoHttp>) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "store".to_string(),
        db: DbHandle::new(plugin_db, "store".to_string()),
        config: config_json(),
        events: EventBusHandle::new(Arc::new(InertEvents), "store".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "store".to_string()),
        identity: Arc::new(NoIdentity),
        http,
    }
}

async fn routes_of(ctx: &PluginContext) -> Vec<RouteDefinition> {
    let mut plugin = StorePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    plugin.routes()
}

/// One comp through the real handler.
async fn comp(
    ctx: &PluginContext,
    order_id: i64,
    reason: &str,
) -> Result<Value, SdkError> {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/store/order/{id}/comp")
        .expect("the comp route");
    let response = (route.handler)(
        TestRequest::post("/api/store/order/{id}/comp")
            .param("id", &order_id.to_string())
            .identity("probe-treasurer", &["commander"])
            .json(&json!({ "reason": reason }))
            .build(),
    )
    .await?;
    Ok(response_json(&response))
}

/// The worklist route's own answer, driven through the plugin's handler.
async fn worklist(ctx: &PluginContext) -> Value {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/store/orders/unsettled")
        .expect("the worklist route");
    let response = (route.handler)(
        // `older_than_minutes=0`: every order is in the window at once, so the
        // probe reads the worklist's contents rather than its age filter.
        TestRequest::get("/api/store/orders/unsettled")
            .query_param("older_than_minutes", "0")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await
    .expect("the worklist handler");
    response_json(&response)
}

/// The row for `order_id` in a worklist answer, if it is listed at all.
fn listed(worklist: &Value, order_id: i64) -> Option<Value> {
    worklist["orders"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["id"].as_i64() == Some(order_id))
        .cloned()
}

// ===========================================================================
// 1. One statement: the intent commits with the order, or neither does
// ===========================================================================

/// **The atomicity probe.** The enqueue is an expression in the order's own
/// `UPDATE`, so PostgreSQL runs both in one implicit transaction. A failure on
/// either side leaves **neither** change behind — which is the whole reason a
/// completed order cannot be left with its scholarship draw unbooked, and the
/// reason no transaction API was added to the SDK to get it.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_failed_enqueue_leaves_the_order_untransitioned_and_no_intent() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let http = Arc::new(NoHttp::default());
    let ctx = context(&admin, &plugin, http.clone());

    let member = "probe_store_atomic";
    cleanup_orders(&admin, member).await;
    let order_id = insert_order(&admin, member).await;
    let key = format!("{DRAW_KEY_PREFIX}{order_id}-draw");

    // --- the enqueue side fails: a revoked principal is refused *inside the
    // producer's own transaction*, so the comp is not recorded either.
    sqlx::query("UPDATE core.service_principals SET revoked_at = now() WHERE principal = $1")
        .bind(DRAW_PRINCIPAL)
        .execute(&admin)
        .await
        .expect("revoke the principal (the operator's act, simulated)");
    let error = comp(&ctx, order_id, "probe: the enqueue must fail")
        .await
        .expect_err("a revoked principal must refuse the statement");
    let error = error.to_string();
    assert!(
        error.contains("revoked"),
        "the refusal must be the operator's revocation, in the core's own words: {error}"
    );
    assert_eq!(
        order_state(&admin, order_id).await,
        (STATUS_OPEN.to_string(), "none".to_string(), None),
        "the order must not transition when its draw intent cannot be enqueued: one statement, \
         so neither"
    );
    assert_eq!(count_intents(&admin, &key).await, 0);
    // Leave the money path armed: this probe must not disarm the deployment.
    sqlx::query("UPDATE core.service_principals SET revoked_at = NULL WHERE principal = $1")
        .bind(DRAW_PRINCIPAL)
        .execute(&admin)
        .await
        .expect("re-declare the principal");

    // --- the order side fails: an empty comp reason is refused before the
    // statement even runs, and nothing is left behind either.
    let routes = routes_of(&ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/store/order/{id}/comp")
        .expect("the comp route");
    let refused = (route.handler)(
        TestRequest::post("/api/store/order/{id}/comp")
            .param("id", &order_id.to_string())
            .identity("probe-treasurer", &["commander"])
            .json(&json!({ "reason": "   " }))
            .build(),
    )
    .await
    .expect("a blank reason is a 400, not an error");
    assert_eq!(refused.status, 400);
    assert_eq!(
        order_state(&admin, order_id).await,
        (STATUS_OPEN.to_string(), "none".to_string(), None),
        "a refused comp writes nothing and enqueues nothing"
    );
    assert_eq!(count_intents(&admin, &key).await, 0);

    // --- and with the principal armed, the same statement does both.
    let body = comp(&ctx, order_id, "the tent was needed that weekend")
        .await
        .expect("the comp");
    let (status, draw_status, intent_id) = order_state(&admin, order_id).await;
    assert_eq!(status, STATUS_COMPED, "{body}");
    assert_eq!(draw_status, DRAW_INTENT_ENQUEUED, "{body}");
    let intent_id = intent_id.expect("the order names its intent");
    assert!(intent_id > 0);
    assert_eq!(
        count_intents(&admin, &key).await,
        1,
        "the comp and its intent were written by one statement"
    );
    assert!(
        http.calls.lock().expect("http calls").is_empty(),
        "the draw is an intent: nothing was called"
    );

    cleanup_orders(&admin, member).await;
    cleanup_intent(&admin, &key).await;
}

// ===========================================================================
// 2. A replay: one order, one intent, the same intent id
// ===========================================================================

/// **The idempotency probe.** The completion statement run twice — a producer
/// retrying its own write — must not produce a second intent. The key is the
/// order's own identifier (`store-order-<id>-draw`), so the second run is handed
/// back the intent it already has, and the payload it carries is complete: both
/// fund **codes**, the funded amount, the description and the date, with no
/// overdraft and no `fiscal_year`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_replayed_completion_yields_one_intent_and_the_same_intent_id() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let http = Arc::new(NoHttp::default());
    let ctx = context(&admin, &plugin, http.clone());

    let member = "probe_store_replay";
    cleanup_orders(&admin, member).await;
    let order_id = insert_order(&admin, member).await;
    let key = format!("{DRAW_KEY_PREFIX}{order_id}-draw");

    let first = comp(&ctx, order_id, "probe: the first completion")
        .await
        .expect("the first comp");
    let first_intent = first["order"]["draw_intent_id"]
        .as_i64()
        .expect("the order must name its intent");
    assert_eq!(first["draw"]["status"], DRAW_INTENT_ENQUEUED);

    // The same statement again: the guard on the order's status would make this a
    // 409, so the probe puts the order back where the first run found it — which
    // is exactly the replay a producer's own retry is (a producer re-running the
    // write it could not confirm).
    sqlx::query("UPDATE store.orders SET status = $2 WHERE id = $1")
        .bind(order_id)
        .bind(STATUS_OPEN)
        .execute(&admin)
        .await
        .expect("put the order back where a replay finds it");
    let second = comp(&ctx, order_id, "probe: the replay")
        .await
        .expect("the replay");
    assert_eq!(
        second["order"]["draw_intent_id"].as_i64(),
        Some(first_intent),
        "a replay is handed back the intent the order already has: {second}"
    );
    assert_eq!(
        count_intents(&admin, &key).await,
        1,
        "one intent, keyed on the order's own identifier"
    );
    assert!(
        http.calls.lock().expect("http calls").is_empty(),
        "the draw is an intent: nothing was called"
    );

    // The intent is complete now, because the relay cannot read for it.
    let (principal, route, payload): (String, String, Value) = sqlx::query_as(
        "SELECT principal, target_route, payload FROM core.outbox \
          WHERE producer_plugin = 'store' AND idempotency_key = $1",
    )
    .bind(&key)
    .fetch_one(&admin)
    .await
    .expect("the intent row");
    assert_eq!(principal, DRAW_PRINCIPAL);
    assert_eq!(route, "/api/finance/transfer");
    assert_eq!(payload["from_fund_code"], FUND_SCHOLARSHIP);
    assert_eq!(payload["to_fund_code"], FUND_GENERAL);
    assert_eq!(payload["amount_cents"], 2000, "the whole price, funded");
    assert_eq!(
        payload["description"],
        format!("Store order {order_id} scholarship draw")
    );
    assert_eq!(payload["allow_overdraft"], false, "a machine decided nothing");
    assert!(
        payload["occurred_on"].as_str().is_some_and(|d| d.len() == 10),
        "the completion date: {payload}"
    );
    assert!(
        payload.get("fiscal_year").is_none(),
        "finance derives the fiscal year; this plugin does not guess it: {payload}"
    );

    cleanup_orders(&admin, member).await;
    cleanup_intent(&admin, &key).await;
}

// ===========================================================================
// 3. The worklist: in flight, then landed — the durable intent decides
// ===========================================================================

/// **The worklist probe.** An order whose draw intent is in flight is neither
/// booked nor unbooked: it is listed with its `draw_intent_id` and the intent's
/// own state, and it leaves the list when the intent's **durable state** is
/// `delivered` — before any notification arrives, and whether or not one ever
/// does. The notification then settles `draw_status` to `booked` with finance's
/// own `transfer_group`, and a replay of it changes nothing.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn the_worklist_lists_an_in_flight_draw_and_drops_it_once_the_intent_is_delivered() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let http = Arc::new(NoHttp::default());
    let ctx = context(&admin, &plugin, http.clone());

    let member = "probe_store_worklist";
    cleanup_orders(&admin, member).await;
    let order_id = insert_order(&admin, member).await;
    let key = format!("{DRAW_KEY_PREFIX}{order_id}-draw");

    let body = comp(&ctx, order_id, "probe: an in-flight draw")
        .await
        .expect("the comp");
    let intent_id = body["order"]["draw_intent_id"]
        .as_i64()
        .expect("the order's intent");

    // In flight: listed explicitly, with the intent's id and state, and counted
    // apart from the rows that have no intent at all.
    let listed_row = listed(&worklist(&ctx).await, order_id).expect("the order must be listed");
    assert_eq!(listed_row["draw_status"], DRAW_INTENT_ENQUEUED);
    assert_eq!(listed_row["draw_intent_id"], intent_id);
    assert_eq!(listed_row["unsettled_reason"], "draw_in_flight");
    assert_eq!(listed_row["intent_state"], "pending");
    assert_eq!(listed_row["intent_attempts"], 0);
    assert!(worklist(&ctx).await["in_flight"].as_i64().unwrap_or(0) >= 1);

    // The relay attempts it and fails: the row stays, and now carries the reason.
    sqlx::query(
        "UPDATE core.outbox SET state = 'pending', attempts = 1, last_error = 'finance is down', \
         next_attempt_at = now() + interval '15 seconds' WHERE id = $1",
    )
    .bind(intent_id)
    .execute(&admin)
    .await
    .expect("an attempted delivery");
    let retried = listed(&worklist(&ctx).await, order_id).expect("still listed");
    assert_eq!(retried["intent_last_error"], "finance is down");

    // The relay delivers it: finance answered 2xx. The order drops out **on the
    // intent's state**, and its own `draw_status` is untouched — no notification
    // has run.
    sqlx::query(
        "UPDATE core.outbox SET state = 'delivered', attempts = 2, answer_status = 201, \
         delivered_at = now(), \
         answer = '{\"transfer_group\": \"9c1e2f00-0000-0000-0000-0000000000ff\"}'::jsonb \
         WHERE id = $1",
    )
    .bind(intent_id)
    .execute(&admin)
    .await
    .expect("the relay's record of a delivery");
    assert!(
        listed(&worklist(&ctx).await, order_id).is_none(),
        "an intent that landed is not a worklist item, whatever a notification said"
    );
    assert_eq!(
        order_state(&admin, order_id).await.1,
        DRAW_INTENT_ENQUEUED,
        "the worklist does not depend on the notification having arrived"
    );

    // The notification arrives, and settles `draw_status` from the answer the relay
    // recorded.
    let mut store = StorePlugin::new();
    store.init(ctx.clone()).await.expect("init");
    let subscriptions = store.subscriptions();
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].filter, OUTBOX_EVENT_PREFIX);
    let outcome = |state: &str, producer: &str, answer: Value, error: Value| Event {
        id: 0,
        event_type: format!("{OUTBOX_EVENT_PREFIX}{state}"),
        payload: json!({
            "intent_id": intent_id,
            "producer": producer,
            "principal": DRAW_PRINCIPAL,
            "state": state,
            "http_status": 201,
            "error": error,
            "answer": answer,
        }),
        source: "core".to_string(),
        timestamp: Utc::now(),
    };
    // A non-terminal state, and another producer's outcome, are both passed over.
    (subscriptions[0].handler)(outcome("pending", "store", Value::Null, Value::Null))
        .await
        .expect("a non-terminal outcome is ignored");
    (subscriptions[0].handler)(outcome(OUTBOX_DELIVERED, "stripe", Value::Null, Value::Null))
        .await
        .expect("another producer's outcome is ignored");
    assert_eq!(
        order_state(&admin, order_id).await.1,
        DRAW_INTENT_ENQUEUED,
        "neither wrote a word"
    );

    (subscriptions[0].handler)(outcome(
        OUTBOX_DELIVERED,
        "store",
        json!({ "transfer_group": "9c1e2f00-0000-0000-0000-0000000000ff" }),
        Value::Null,
    ))
    .await
    .expect("the outcome subscription");

    let (status, draw_status, _) = order_state(&admin, order_id).await;
    assert_eq!(status, STATUS_COMPED);
    assert_eq!(
        draw_status, DRAW_BOOKED,
        "finance answered 2xx, so the draw is booked"
    );
    let draw_ref: String = sqlx::query_scalar("SELECT draw_ref FROM store.orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&admin)
        .await
        .expect("the order's draw reference");
    assert_eq!(
        draw_ref, "9c1e2f00-0000-0000-0000-0000000000ff",
        "finance's own transfer_group is the draw's reference"
    );

    // A replayed notification is a no-op: one intent, one answer.
    (subscriptions[0].handler)(outcome(
        OUTBOX_DELIVERED,
        "store",
        json!({ "transfer_group": "9c1e2f00-0000-0000-0000-0000000000ff" }),
        Value::Null,
    ))
    .await
    .expect("the replayed outcome");
    assert_eq!(order_state(&admin, order_id).await.1, DRAW_BOOKED);

    cleanup_orders(&admin, member).await;
    cleanup_intent(&admin, &key).await;
}
