//! # The overdraft guard under concurrency — the reproduction of #151
//!
//! **This probe is expected to FAIL on this branch.** It is the acceptance test
//! for #151 (the overdraft guard is not concurrency-safe), and it is committed
//! written-but-red on purpose: the fix is not code this lane can land. See
//! "Why this cannot be fixed here" below.
//!
//! The guard lives in one statement ([`TRANSFER_SQL`] in
//! `plugins/finance/src/lib.rs`): an aggregate read of the fund's ledger with no
//! row lock, no advisory lock and no `SERIALIZABLE` isolation. Two transfers that
//! overlap both read the same balance and both pass. Each transfer is balanced
//! per group, so `ledger_verdict` stays satisfied while a fund goes negative.
//!
//! ## Why this cannot be fixed here
//!
//! The obvious repair — `SELECT … FOR UPDATE` on the fund row **inside the same
//! statement**, before the aggregate — does not work under PostgreSQL's
//! `READ COMMITTED`. A statement's snapshot is taken before it blocks on the row
//! lock, and when it resumes only the locked row is re-fetched; the aggregate
//! still reads the old snapshot. This was measured, not assumed: two sessions
//! running the exact statement with a `LATERAL … FOR UPDATE` on `funds` both
//! passed the guard and left the fund at `-100000` (see the lane's report).
//!
//! A correct guard needs **two commands in one transaction** — lock, then read —
//! or a database-enforced check:
//!
//! * the plugin API has no transaction: `HostDb` is `execute`/`query`, one
//!   statement per call (`plugins/sdk/src/lib.rs:315`), and the store plugin's
//!   own docs state the same limit (`plugins/store/src/lib.rs:1390`);
//! * a constraint trigger on `finance.transactions` that takes the fund row lock
//!   and re-checks the derived balance in a fresh SPI command does work (also
//!   measured: the second concurrent statement aborts and the fund stays at 0).
//!   That is a migration, and migrations belong to the data lane.
//!
//! ## Running it
//!
//! It is `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports it
//! as ignored, not passed. It is deliberately **not registered** in
//! `.github/workflows/ci.yml`: until the guard migration lands it would fail the
//! money-path job. Run it by hand against a database a boot has prepared:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_dev_test \
//!   cargo test -p adjutant-finance --test overdraft_race -- --ignored --nocapture
//! ```
//!
//! Under `--ignored` a missing or unreachable database is a hard failure, never
//! a skip — the same rule as the other DB-backed probes.
//!
//! **Unexercised as committed:** this lane had no database to run it against, so
//! the assertion below has not been observed red-then-green here. The two-session
//! measurement in the lane report is the evidence that the race is real.

use std::sync::Arc;

use adjutant_finance::{FinancePlugin, FUND_GENERAL, FUND_SCHOLARSHIP};
use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use adjutant_sdk::RouteHandler;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_finance";
/// The role this probe declares for its own caller, granted `finance:write`.
const PROBE_ROLE: &str = "probe_overdraft_treasurer";
/// The caller id the route records as `recorded_by`.
const CALLER: &str = "probe-treasurer";
/// The whole balance moved by each of the two racing transfers.
const BALANCE_CENTS: i64 = 100_000;
/// How many times two concurrent transfers are fired. The race is a window, not
/// a probability: one round that double-spends is proof, and zero rounds does not
/// prove its absence. The two-session measurement is the deterministic evidence.
const ROUNDS: usize = 12;
const DEPOSIT_REF: &str = "probe:overdraft_race:deposit";
const DESCRIPTION: &str = "overdraft race probe";

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the overdraft race probe is DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test \
             database a boot has already prepared (it is #[ignore]d; run with `-- --ignored`)"
        )
    });
    assert!(
        !url.trim().is_empty(),
        "the database URL is set but empty; set it to a _test database or unset it"
    );
    url
}

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

async fn plugin_pool(admin: &PgPool) -> PgPool {
    let url = test_database_url();
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = 'finance'")
            .fetch_optional(admin)
            .await
            .expect("read the plugin's stored secret")
            .flatten();
    let Some(secret) = secret else {
        panic!(
            "this test database has no bootstrapped role for the `finance` plugin (no \
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

async fn ensure_core_ready(admin: &PgPool) {
    let permission: Option<String> =
        sqlx::query_scalar("SELECT id FROM core.permissions WHERE id = 'finance:write'")
            .fetch_optional(admin)
            .await
            .expect("look for the finance:write permission");
    assert!(
        permission.is_some(),
        "`finance:write` is not registered in core.permissions — run the live ladder \
         (`ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`) against this \
         database"
    );
}

async fn ensure_probe_role(admin: &PgPool) {
    for sql in [
        "INSERT INTO core.roles (id, display_name) VALUES ($1, 'probe overdraft treasurer') \
         ON CONFLICT (id) DO NOTHING",
        "INSERT INTO core.role_permissions (role_id, permission_id) \
         VALUES ($1, 'finance:write') ON CONFLICT DO NOTHING",
    ] {
        sqlx::query(sql)
            .bind(PROBE_ROLE)
            .execute(admin)
            .await
            .expect("declare the probe's role and its grant");
    }
}

async fn ensure_plugin_schema(admin: &PgPool, plugin: &PgPool) {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS finance AUTHORIZATION {PLUGIN_ROLE}"
    ))
    .execute(admin)
    .await
    .expect("the finance schema");
    sqlx::query(&format!("ALTER SCHEMA finance OWNER TO {PLUGIN_ROLE}"))
        .execute(admin)
        .await
        .expect("the plugin role owns its own schema");
    let ctx = context(admin, plugin).await;
    let mut declared = FinancePlugin::new();
    declared.init(ctx).await.expect("init");
    let mut conn = plugin.acquire().await.expect("plugin connection");
    for migration in declared.migrations() {
        let script = format!(
            "BEGIN; SET LOCAL search_path TO \"finance\"; {}; COMMIT;",
            migration.sql
        );
        use sqlx::Executor;
        Executor::execute(&mut *conn, sqlx::raw_sql(&script))
            .await
            .unwrap_or_else(|e| panic!("migration {} ({}): {e}", migration.version, migration.name));
    }
}

// ---------------------------------------------------------------------------
// The plugin, exactly as the core builds it
// ---------------------------------------------------------------------------

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
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    Value::Null
}

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

struct NoHttp;

#[async_trait]
impl HostHttp for NoHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        _headers: Vec<(String, String)>,
        _body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        Err(SdkError::Internal(format!(
            "the transfer route is finance's own, in process: nothing should call {method} {url}"
        )))
    }
}

async fn context(admin: &PgPool, plugin: &PgPool) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "finance".to_string(),
        db: DbHandle::new(plugin_db, "finance".to_string()),
        config: json!({ "dues_fund_code": FUND_GENERAL }),
        events: EventBusHandle::new(Arc::new(InertEvents), "finance".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "finance".to_string()),
        identity: Arc::new(NoIdentity),
        http: Arc::new(NoHttp),
    }
}

async fn routes_of(ctx: &PluginContext) -> Vec<RouteDefinition> {
    let mut plugin = FinancePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    plugin.routes()
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

async fn balance_cents(admin: &PgPool, code: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(t.amount_cents), 0)::bigint \
           FROM finance.transactions t JOIN finance.funds f ON f.id = t.fund_id \
          WHERE f.code = $1",
    )
    .bind(code)
    .fetch_one(admin)
    .await
    .expect("read the fund's derived balance")
}

/// Put the general fund back at exactly one transfer's worth, clearing anything
/// the previous round wrote.
async fn reset_general(admin: &PgPool) {
    sqlx::query("DELETE FROM finance.transactions WHERE recorded_by = $1")
        .bind(CALLER)
        .execute(admin)
        .await
        .unwrap_or_else(|e| panic!("clear the probe's transfers: {e}"));
    sqlx::query("DELETE FROM finance.transactions WHERE external_ref = $1")
        .bind(DEPOSIT_REF)
        .execute(admin)
        .await
        .unwrap_or_else(|e| panic!("clear the probe's deposit: {e}"));
    sqlx::query(
        "INSERT INTO finance.transactions \
           (fund_id, amount_cents, kind, category, description, member_id, fiscal_year, \
            occurred_on, recorded_by, external_ref) \
         SELECT f.id, $1, 'income', 'donation', 'probe overdraft race deposit', '', 2026, \
                '2026-01-15'::date, 'probe', $2 \
           FROM finance.funds f WHERE f.code = $3",
    )
    .bind(BALANCE_CENTS)
    .bind(DEPOSIT_REF)
    .bind(FUND_GENERAL)
    .execute(admin)
    .await
    .expect("fund the general fund for the probe");
}

async fn clear(admin: &PgPool) {
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE recorded_by = $1")
        .bind(CALLER)
        .execute(admin)
        .await;
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE external_ref = $1")
        .bind(DEPOSIT_REF)
        .execute(admin)
        .await;
}

async fn transfer(handler: &RouteHandler, amount_cents: i64) -> (u16, Value) {
    let request = TestRequest::post("/api/finance/transfer")
        .identity(CALLER, &[PROBE_ROLE])
        .json(&json!({
            "from_fund_code": FUND_GENERAL,
            "to_fund_code": FUND_SCHOLARSHIP,
            "amount_cents": amount_cents,
            "fiscal_year": 2026,
            // No `allow_overdraft`: the fund has exactly one transfer's worth,
            // so the guard must refuse the second one.
            "description": DESCRIPTION,
        }))
        .build();
    match (handler)(request).await {
        Ok(response) => (response.status, response_json(&response)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// **The probe #151 asks for: two concurrent transfers of one fund's whole
/// balance, `allow_overdraft` unset — exactly one may land.**
///
/// On the current code both can pass the guard (they read the same aggregate in
/// their own snapshots), leaving the fund at `-BALANCE_CENTS` with every leg
/// carrying `overdraft_authorized = false`. `ledger_verdict` cannot see it: each
/// transfer's two legs still sum to zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "DB-gated reproduction of #151: expected to fail until the overdraft-guard migration lands"]
async fn two_concurrent_transfers_of_one_balance_land_only_one() {
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_role(&admin).await;
    let ctx = context(&admin, &plugin).await;
    let routes = routes_of(&ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/transfer")
        .expect("the transfer route");

    clear(&admin).await;
    let mut doubles = Vec::new();
    for round in 0..ROUNDS {
        reset_general(&admin).await;
        let (a, b) = tokio::join!(
            transfer(&route.handler, BALANCE_CENTS),
            transfer(&route.handler, BALANCE_CENTS),
        );
        let landed = [a.0, b.0]
            .iter()
            .filter(|status| (200..300).contains(*status))
            .count();
        if landed != 1 {
            doubles.push((round, a.0, b.0, balance_cents(&admin, FUND_GENERAL).await));
        }
    }
    clear(&admin).await;

    assert!(
        doubles.is_empty(),
        "two concurrent transfers of one fund's whole balance must land exactly one, but {} of \
         {ROUNDS} rounds double-spent: {doubles:?}. This is #151: the guard is an unlocked \
         aggregate read in TRANSFER_SQL (plugins/finance/src/lib.rs), so both statements read the \
         same balance. The fix is a database serialization point (a constraint trigger taking the \
         fund row lock and re-reading the derived balance) — see the lane report",
        doubles.len()
    );
}
