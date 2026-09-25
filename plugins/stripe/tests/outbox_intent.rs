//! # The stripe plugin's ledger booking, against a real database
//!
//! The probes the mock host cannot write: **one statement** records a confirmed
//! payment and enqueues its ledger booking as an outbox intent, so a failure on
//! either side leaves neither; a redelivered webhook is one payment and one
//! intent; and the worklist lists an in-flight intent and drops it once the
//! intent has landed — decided by the intent's durable state, not by a
//! notification that may never arrive.
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_dev_test \
//!   cargo test -p adjutant-stripe --test outbox_intent -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.outbox`, `core.outbox_enqueue` and the declared `svc.stripe.ledger`
//! exist) and the `stripe` plugin's own role created with its secret stored in
//! `core.plugins.db_secret`. The live ladder produces exactly that:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes then connect **as the plugin's own role**, because
//! `core.outbox_enqueue` derives the producer from `session_user`: a statement
//! run as anything else would be refused, and that refusal is the security
//! property, not an obstacle to be worked around. They read the stored secret
//! rather than rotating it, so a later boot of the same database still works.
//!
//! They write only rows named by a `probe_` payment id, and each probe clears its
//! own before and after.

use std::collections::HashMap;
use std::sync::Arc;

use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use adjutant_stripe::{
    handle_webhook, sign, StripeConfig, StripePlugin, LEDGER_BOOKED, LEDGER_INTENT_ENQUEUED,
    LEDGER_PRINCIPAL, MECHANISM_OUTBOX, OUTBOX_DELIVERED, SIGNATURE_HEADER,
};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `core.outbox_enqueue` derives it from `session_user`.
const PLUGIN_ROLE: &str = "adjutant_plugin_stripe";
/// Not a real signing secret.
const WEBHOOK_SECRET: &str = "whsec_NOTAREALSECRET0000";
/// The fund finance's stub route reports; the id is what a complete payload needs.
const FUND_ID: i64 = 3;
/// The fund code the payload names.
const FUND_CODE: &str = "general";

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
        println!("[stripe outbox probe] WARNING: running against {name:?}, which does not end in `_test`");
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

/// The plugin's pool: **its own role**, with the secret a boot stored.
///
/// No password is written: `bootstrap_role` keeps an existing secret, so a later
/// boot of this database still authenticates.
async fn plugin_pool(admin: &PgPool) -> PgPool {
    let url = test_database_url();
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = 'stripe'")
            .fetch_optional(admin)
            .await
            .expect("read the stripe plugin's stored secret")
            .flatten();
    let Some(secret) = secret else {
        panic!(
            "this test database has no bootstrapped role for the `stripe` plugin (no \
             core.plugins.db_secret row). Run the live ladder against it first: \
             `ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`"
        );
    };
    PgPool::connect(&with_role(&url, PLUGIN_ROLE, &secret))
        .await
        .unwrap_or_else(|e| {
            panic!("could not connect as {PLUGIN_ROLE}: {e}; run the live ladder against this database")
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
    .bind(LEDGER_PRINCIPAL)
    .fetch_optional(admin)
    .await
    .expect("read the declared service principal")
    .map(|(a, b)| (Some(a), Some(b)))
    .unwrap_or((None, None));
    assert_eq!(
        producer.as_deref(),
        Some("stripe"),
        "{LEDGER_PRINCIPAL} is not declared for the `stripe` producer — run the live ladder \
         against this database so the core seeds its service principals"
    );
    assert_eq!(declared, Some(true), "the declaration must be the core's own");
}

/// Apply this plugin's migrations, as the plugin role, in its own schema — the
/// same shape the runner uses (one transaction, `SET LOCAL search_path`).
///
/// A database the ladder prepared has them already, and every statement is
/// idempotent, so this is a no-op there.
///
/// `CREATE SCHEMA` is deliberately **not** done as the plugin role: a plugin
/// role does not own the database and has no `CREATE` on it (`bootstrap-isolation`
/// creates the schema as the operator, `AUTHORIZATION adjutant_plugin_<id>`), so
/// the probe does the same thing the operator does and then migrates as the
/// plugin — which owns what is inside its own schema.
async fn ensure_plugin_schema(admin: &PgPool, plugin: &PgPool) {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS stripe AUTHORIZATION {PLUGIN_ROLE}"
    ))
    .execute(admin)
    .await
    .expect("the stripe schema, as `bootstrap-isolation` creates it");
    sqlx::query(&format!("ALTER SCHEMA stripe OWNER TO {PLUGIN_ROLE}"))
        .execute(admin)
        .await
        .expect("the plugin role owns its own schema");
    let mut conn = plugin.acquire().await.expect("plugin connection");
    for migration in adjutant_stripe::migrations::all() {
        let script = format!(
            "BEGIN; SET LOCAL search_path TO \"stripe\"; {}; COMMIT;",
            migration.sql
        );
        use sqlx::Executor;
        Executor::execute(&mut *conn, sqlx::raw_sql(&script))
            .await
            .unwrap_or_else(|e| panic!("migration {} ({}): {e}", migration.version, migration.name));
    }
}

async fn cleanup(admin: &PgPool, payment_id: &str, event_id: &str) {
    for sql in [
        "DELETE FROM stripe.payments WHERE payment_id = $1",
        "DELETE FROM core.outbox WHERE idempotency_key = $1",
        "DELETE FROM stripe.webhook_events WHERE event_id = $1",
    ] {
        let _ = sqlx::query(sql).bind(payment_id).execute(admin).await;
        let _ = sqlx::query(sql).bind(event_id).execute(admin).await;
    }
}

async fn count_payments(admin: &PgPool, payment_id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*)::bigint FROM stripe.payments WHERE payment_id = $1")
        .bind(payment_id)
        .fetch_one(admin)
        .await
        .expect("count payments")
}

async fn count_intents(admin: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)::bigint FROM core.outbox WHERE producer_plugin = 'stripe' \
          AND idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(admin)
    .await
    .expect("count intents")
}

async fn ledger_status(admin: &PgPool, payment_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT ledger_status FROM stripe.payments WHERE payment_id = $1")
        .bind(payment_id)
        .fetch_optional(admin)
        .await
        .expect("read the payment's ledger status")
}

// ---------------------------------------------------------------------------
// The host the plugin runs against: a real database, a stub finance
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
                // As the core binds it (`server/src/host.rs` → `bind_params`): a
                // text NULL cannot be assigned to a uuid column, and the SDK
                // passes this variant for an unattributable audit row.
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
                // As the core binds it (`server/src/host.rs` → `bind_params`): a
                // text NULL cannot be assigned to a uuid column, and the SDK
                // passes this variant for an unattributable audit row.
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

/// finance's funds route, answered by the probe: the read the enqueue-time
/// resolution makes. Only the fields the resolution reads are here — the point
/// is the fund **id** it returns.
struct FundsStub;

#[async_trait]
impl HostHttp for FundsStub {
    async fn request(
        &self,
        _method: String,
        _url: String,
        _headers: Vec<(String, String)>,
        _body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "funds": [
                    { "id": FUND_ID, "code": FUND_CODE, "name": "General Fund", "active": true },
                ]
            }))
            .unwrap_or_default(),
        })
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
///
/// `secret_key` is deliberately not shaped like a Stripe key: nothing in these
/// probes calls Stripe, and a literal that looks like one invites a scanner to
/// treat it as a credential. The worklist threshold is passed per request
/// (`older_than_minutes=0`) rather than configured, so a payment is inside the
/// window the moment it is confirmed and the probe does not wait one out — a
/// configured `0` is not a threshold this plugin accepts (`unbooked_after_minutes`
/// must be positive).
fn config_json() -> Value {
    json!({
        "secret_key": "probe-not-a-stripe-key",
        "webhook_secret": WEBHOOK_SECRET,
        "base_url": "http://finance.stub",
        "dues_fund_code": FUND_CODE,
        "unbooked_after_minutes": 30,
    })
}

fn config() -> StripeConfig {
    let (cfg, warnings) = StripeConfig::from_value(&config_json());
    assert!(warnings.is_empty(), "{warnings:?}");
    cfg
}

/// A real `PluginContext`, with the **two** database hosts the core hands a
/// plugin (`plugin_runtime::build_context`): `ctx.db` is the plugin's own role —
/// its schema, and the `session_user` `core.outbox_enqueue` derives the producer
/// from — while the audit and permission services run on the **core's** pool,
/// exactly as they do in production. That matters here: `core_grants("stripe")`
/// is `None`, so the plugin role holds no grant on `core.audit_log`, and a probe
/// that ran everything on one pool would be testing a shape the core never
/// builds.
fn context(admin: &PgPool, plugin: &PgPool) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "stripe".to_string(),
        db: DbHandle::new(plugin_db, "stripe".to_string()),
        // The routes read their config from the context and the webhook calls
        // take it explicitly; both come from `config_json()`, so the probe cannot
        // be testing two different configurations.
        config: config_json(),
        events: EventBusHandle::new(Arc::new(InertEvents), "stripe".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "stripe".to_string()),
        identity: Arc::new(NoIdentity),
        http: Arc::new(FundsStub),
    }
}

/// A signed delivery, as Stripe renders one.
fn webhook_payload(event_id: &str, payment_id: &str, dues_year: Option<&str>) -> Value {
    let mut metadata = json!({
        "purpose": "dues",
        "member_id": "42",
        "fund_code": FUND_CODE,
        "category": "dues",
        "description": "probe dues",
    });
    if let Some(year) = dues_year {
        metadata["dues_year"] = json!(year);
    }
    json!({
        "id": event_id,
        "object": "event",
        "type": "payment_intent.succeeded",
        "api_version": "2024-06-20",
        "created": Utc::now().timestamp(),
        "livemode": false,
        "data": {
            "object": {
                "id": payment_id,
                "object": "payment_intent",
                "amount_received": 2500,
                "amount": 2500,
                "currency": "cad",
                "metadata": metadata,
            }
        }
    })
}

fn signed_webhook(payload: &Value) -> PluginRequest {
    let body = serde_json::to_vec(payload).expect("serialize the payload");
    let timestamp = Utc::now().timestamp();
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(&body);
    let header = format!("t={timestamp},v1={}", sign(WEBHOOK_SECRET, &message));
    TestRequest::post("/api/stripe/webhook")
        .json(payload)
        .header(SIGNATURE_HEADER, &header)
        .build()
}

/// One delivery through the real handler.
async fn deliver(ctx: &PluginContext, payload: &Value) -> Result<Value, SdkError> {
    let cfg = config();
    let response = handle_webhook(ctx, &cfg, &signed_webhook(payload)).await?;
    Ok(response_json(&response))
}

/// One delivery that is expected to be refused (an `Err` from the handler).
async fn deliver_expecting_error(ctx: &PluginContext, payload: &Value) -> String {
    let cfg = config();
    match handle_webhook(ctx, &cfg, &signed_webhook(payload)).await {
        Ok(response) => panic!("expected the statement to fail, answered {:?}", response.status),
        Err(e) => e.to_string(),
    }
}

/// The worklist route's own answer, driven through the plugin's handler.
async fn worklist(ctx: &PluginContext) -> Value {
    let mut plugin = StripePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    let routes = plugin.routes();
    let route = routes
        .iter()
        .find(|r| r.path == "/api/stripe/unbooked")
        .expect("the worklist route");
    let response = (route.handler)(
        // `older_than_minutes=0`: every payment is in the window at once, so the
        // probe reads the worklist's contents rather than its age filter.
        TestRequest::get("/api/stripe/unbooked")
            .query_param("older_than_minutes", "0")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await
    .expect("the worklist handler");
    response_json(&response)
}

// ===========================================================================
// 1. One statement: the intent commits with the fact, or neither does
// ===========================================================================

/// **The atomicity probe.** The enqueue is an expression in the payment's own
/// `INSERT`, so PostgreSQL runs both in one implicit transaction. A failure on
/// either side leaves **neither** row behind — which is the whole reason the
/// money path cannot be two statements, and the reason no transaction API was
/// added to the SDK to get it.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_failed_enqueue_leaves_no_payment_and_a_failed_payment_insert_leaves_no_intent() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);

    // --- the enqueue side fails: a revoked principal is refused *inside the
    // producer's own transaction*, so the payment is not written either.
    let payment_id = "pi_probe_atomic_revoked";
    let event_id = "evt_probe_atomic_revoked";
    cleanup(&admin, payment_id, event_id).await;
    sqlx::query("UPDATE core.service_principals SET revoked_at = now() WHERE principal = $1")
        .bind(LEDGER_PRINCIPAL)
        .execute(&admin)
        .await
        .expect("revoke the principal (the operator's act, simulated)");
    let error = deliver_expecting_error(
        &ctx,
        &webhook_payload(event_id, payment_id, None),
    )
    .await;
    assert!(
        error.contains("revoked"),
        "the refusal must be the operator's revocation, in the core's own words: {error}"
    );
    assert_eq!(
        count_payments(&admin, payment_id).await,
        0,
        "the fact must not be recorded when its intent cannot be enqueued: one statement, so \
         neither"
    );
    assert_eq!(count_intents(&admin, payment_id).await, 0);
    // Leave the money path armed: this probe must not disarm the deployment.
    sqlx::query("UPDATE core.service_principals SET revoked_at = NULL WHERE principal = $1")
        .bind(LEDGER_PRINCIPAL)
        .execute(&admin)
        .await
        .expect("re-declare the principal");

    // --- the payment side fails: a dues year outside the table's CHECK fails the
    // statement, and the intent that same statement would have enqueued is not
    // left behind either.
    let payment_id = "pi_probe_atomic_duesyears";
    let event_id = "evt_probe_atomic_duesyears";
    cleanup(&admin, payment_id, event_id).await;
    let error =
        deliver_expecting_error(&ctx, &webhook_payload(event_id, payment_id, Some("3000"))).await;
    assert!(
        error.contains("dues_year") || error.contains("check"),
        "the payment insert must be what failed: {error}"
    );
    assert_eq!(
        count_payments(&admin, payment_id).await,
        0,
        "a statement that violated a constraint writes no payment"
    );
    assert_eq!(
        count_intents(&admin, payment_id).await,
        0,
        "and it leaves no intent: the enqueue cannot outlive the fact it describes"
    );

    cleanup(&admin, payment_id, event_id).await;
}

// ===========================================================================
// 2. A redelivered webhook: one payment, one intent, the same intent id
// ===========================================================================

/// **The idempotency probe.** Two deliveries of the same payment — Stripe
/// redelivering, because it never saw the first answer — must not produce a
/// second payment or a second intent. The intent's idempotency key is the
/// payment's own identifier, so the second delivery is handed back the intent it
/// already has, and the payload it carries is complete: finance's fund **id**,
/// `income`, a positive magnitude, finance's category, and Stripe's payment id as
/// `external_ref`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_redelivered_webhook_yields_one_payment_one_intent_and_the_same_intent_id() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);

    let payment_id = "pi_probe_redelivery";
    let first_event = "evt_probe_redelivery_1";
    let second_event = "evt_probe_redelivery_2";
    cleanup(&admin, payment_id, first_event).await;
    cleanup(&admin, payment_id, second_event).await;

    let first = deliver(&ctx, &webhook_payload(first_event, payment_id, Some("2026")))
        .await
        .expect("the first delivery");
    assert_eq!(first["received"], true, "{first}");
    let intent_id = first["payment"]["ledger_intent_id"]
        .as_i64()
        .unwrap_or_default();
    assert!(intent_id > 0, "the payment must name its intent: {first}");
    assert_eq!(first["payment"]["ledger_status"], LEDGER_INTENT_ENQUEUED);
    assert_eq!(first["payment"]["ledger_mechanism"], MECHANISM_OUTBOX);
    assert_eq!(first["ledger"]["path"], "outbox");
    assert_eq!(first["ledger"]["intent_id"], intent_id);

    // The second delivery carries a **different** receipt for the same payment:
    // Stripe sends both `checkout.session.completed` and `payment_intent.succeeded`
    // for one charge, and it re-delivers the same event when it never saw an
    // answer. A new receipt is not a duplicate — a second *payment* would be, and
    // is not written.
    let second = deliver(&ctx, &webhook_payload(second_event, payment_id, Some("2026")))
        .await
        .expect("the redelivery");
    assert_eq!(second["received"], true, "{second}");
    assert_eq!(
        second["redelivered"], false,
        "a distinct event id is a distinct receipt: {second}"
    );
    assert_eq!(second["duplicate"], true, "but not a second payment");
    assert_eq!(
        second["payment"]["ledger_intent_id"].as_i64(),
        Some(intent_id),
        "a redelivery returns the intent it already has: {second}"
    );

    assert_eq!(
        count_payments(&admin, payment_id).await,
        1,
        "one payment, however many deliveries"
    );
    assert_eq!(
        count_intents(&admin, payment_id).await,
        1,
        "one intent, keyed on the payment's own identifier"
    );

    // The intent the payment names is the one the key holds, and it is complete:
    // the relay cannot read-then-write at delivery, so everything finance needs
    // is here now.
    let (state, principal, payload): (String, String, Value) = sqlx::query_as(
        "SELECT state, principal, payload FROM core.outbox \
          WHERE producer_plugin = 'stripe' AND idempotency_key = $1",
    )
    .bind(payment_id)
    .fetch_one(&admin)
    .await
    .expect("the intent row");
    assert_eq!(
        state, "pending",
        "the fact and its intent committed together, and the relay has not run"
    );
    assert_eq!(principal, LEDGER_PRINCIPAL);
    assert_eq!(payload["fund_id"], FUND_ID, "resolved at enqueue time");
    assert_eq!(payload["kind"], "income");
    assert_eq!(payload["amount_cents"], 2500, "income is positive");
    assert_eq!(payload["category"], "dues");
    assert_eq!(
        payload["external_ref"], payment_id,
        "finance's unique key, so a second delivery cannot double-book"
    );
    assert_eq!(payload["fiscal_year"], 2026);

    // And the payment's own row says the same thing.
    assert_eq!(
        ledger_status(&admin, payment_id).await.as_deref(),
        Some(LEDGER_INTENT_ENQUEUED)
    );

    cleanup(&admin, payment_id, first_event).await;
    cleanup(&admin, payment_id, second_event).await;
}

// ===========================================================================
// 3. The worklist: the in-flight case, and the intent's word, not a notification
// ===========================================================================

/// **The worklist probe.** A payment whose booking is an intent is neither booked
/// nor unbooked: it is listed with its intent and the intent's own state, and it
/// leaves the list when the intent's **durable state** is `delivered` — before
/// any notification arrives, and whether or not one ever does. The notification
/// then settles the payment's `ledger_status` to `booked` with finance's own
/// transaction id.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn the_worklist_lists_an_in_flight_intent_and_drops_it_once_the_intent_is_delivered() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);

    let payment_id = "pi_probe_worklist";
    let event_id = "evt_probe_worklist";
    cleanup(&admin, payment_id, event_id).await;

    let delivery = deliver(&ctx, &webhook_payload(event_id, payment_id, None))
        .await
        .expect("the delivery");
    let intent_id = delivery["payment"]["ledger_intent_id"]
        .as_i64()
        .expect("the payment's intent");

    // In flight: listed explicitly, with the intent's id and state, and counted
    // apart from the payments that have no intent at all.
    let listed = worklist(&ctx).await;
    assert_eq!(listed["in_flight"], 1, "{listed}");
    assert_eq!(listed["payments"][0]["ledger_intent_id"], intent_id);
    assert_eq!(listed["payments"][0]["ledger_status"], LEDGER_INTENT_ENQUEUED);
    assert_eq!(listed["payments"][0]["intent_state"], "pending");
    assert_eq!(listed["payments"][0]["intent_attempts"], 0);

    // The relay attempts it and fails: the row stays in the worklist, and now
    // carries the reason — an in-flight intent that is not working is visible.
    sqlx::query(
        "UPDATE core.outbox SET state = 'pending', attempts = 1, last_error = 'the ledger is \
         down', next_attempt_at = now() + interval '15 seconds' WHERE id = $1",
    )
    .bind(intent_id)
    .execute(&admin)
    .await
    .expect("an attempted delivery");
    let retried = worklist(&ctx).await;
    assert_eq!(retried["in_flight"], 1);
    assert_eq!(retried["payments"][0]["intent_state"], "pending");
    assert_eq!(retried["payments"][0]["intent_last_error"], "the ledger is down");

    // The relay delivers it: finance answered 2xx and its transaction id is on the
    // intent. The worklist drops the payment **on the intent's state**, and the
    // payment's own `ledger_status` is untouched — no notification has run.
    sqlx::query(
        "UPDATE core.outbox SET state = 'delivered', attempts = 2, answer_status = 201, \
         delivered_at = now(), answer = '{\"transaction\": {\"id\": 500}}'::jsonb WHERE id = $1",
    )
    .bind(intent_id)
    .execute(&admin)
    .await
    .expect("the relay's record of a delivery");
    let after = worklist(&ctx).await;
    assert_eq!(
        after["count"], 0,
        "an intent that landed is not a worklist item: {after}"
    );
    assert_eq!(after["in_flight"], 0);
    assert_eq!(
        ledger_status(&admin, payment_id).await.as_deref(),
        Some(LEDGER_INTENT_ENQUEUED),
        "the worklist does not depend on the notification having arrived"
    );

    // The notification arrives, and settles `ledger_status` from the answer the
    // relay recorded.
    let mut plugin = StripePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    let subscriptions = plugin.subscriptions();
    assert_eq!(subscriptions.len(), 1);
    (subscriptions[0].handler)(Event {
        id: 0,
        event_type: "core.outbox.delivered".to_string(),
        payload: json!({
            "intent_id": intent_id,
            "producer": "stripe",
            "principal": LEDGER_PRINCIPAL,
            "state": OUTBOX_DELIVERED,
            "http_status": 201,
            "error": Value::Null,
            "answer": { "transaction": { "id": 500 } },
        }),
        source: "core".to_string(),
        timestamp: Utc::now(),
    })
    .await
    .expect("the outcome subscription");

    assert_eq!(
        ledger_status(&admin, payment_id).await.as_deref(),
        Some(LEDGER_BOOKED),
        "finance answered 2xx, so the payment is booked"
    );
    let transaction_id: Option<String> =
        sqlx::query_scalar("SELECT ledger_transaction_id FROM stripe.payments WHERE payment_id = $1")
            .bind(payment_id)
            .fetch_one(&admin)
            .await
            .expect("the payment's transaction id");
    assert_eq!(transaction_id.as_deref(), Some("500"));
    // A replayed notification is a no-op: one intent, one answer.
    (subscriptions[0].handler)(Event {
        id: 0,
        event_type: "core.outbox.delivered".to_string(),
        payload: json!({
            "intent_id": intent_id,
            "producer": "stripe",
            "state": OUTBOX_DELIVERED,
            "answer": { "transaction": { "id": 500 } },
        }),
        source: "core".to_string(),
        timestamp: Utc::now(),
    })
    .await
    .expect("the replayed outcome");
    assert_eq!(
        ledger_status(&admin, payment_id).await.as_deref(),
        Some(LEDGER_BOOKED)
    );

    cleanup(&admin, payment_id, event_id).await;
}
