//! # The finance plugin's write SQL, against a real database
//!
//! The probes no gate ran before: `POST /api/finance/transaction` and
//! `POST /api/finance/transfer` are driven through the plugin's **real route
//! handlers**, with the plugin's own database role, and what lands in
//! `finance.transactions` is read back from PostgreSQL.
//!
//! ## Why this exists
//!
//! Every other gate on this plugin proves the handler's *decisions* — a mock
//! host answers, and the SQL is a string nobody executes. That is enough to
//! catch a wrong branch and useless against a statement PostgreSQL refuses: an
//! alias-qualified `SET` target, a parameter cast the binder cannot infer, a
//! `RETURNING` column that no longer exists. The sibling `store` plugin shipped
//! exactly that class of fault, green on every gate, and found it only when a
//! probe finally executed the statement. These are the same probes for
//! `finance`: they assert on the **rows** that land, and on refusals by their
//! own reason — never on a frozen copy of the statement's text, so a change to
//! the SQL (the optional `external_ref` on a transfer, say) cannot make this
//! file lie in either direction.
//!
//! What they cover, in the shapes the API documents:
//!
//! * a transaction named by `fund_id` and one named by `fund_code` land on the
//!   same fund row, and an unknown code writes nothing and names the reference;
//! * a transfer named by **code on both legs** writes exactly two rows in one
//!   `transfer_group`, summing to zero, each leg's `counterparty_fund_id`
//!   naming the other fund, and the resolved ids on `transactions.fund_id`;
//! * a same-fund transfer is refused both ways (two equal codes, and an id with
//!   the code resolving to one fund) and writes nothing;
//! * the overdraft guard refuses the out-leg that would take a fund negative,
//!   writes nothing, and leaves the balance exactly as it was;
//! * a replayed `external_ref` on the transaction route leaves exactly one row.
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_dev_test \
//!   cargo test -p adjutant-finance --test route_sql -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.audit_log` exists — these handlers audit every write) and the `finance`
//! plugin's own role created with its secret stored in `core.plugins.db_secret`.
//! The live ladder produces exactly that, and it is why the CI step sits beside
//! the other DB-backed probes rather than with the unit tests:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes then connect **as the plugin's own role** — the connection whose
//! `session_user` is `adjutant_plugin_finance`, so what they prove is what the
//! plugin's statements do under the privileges they actually run with. They read
//! the stored secret rather than rotating it, so a later boot of the same
//! database still works.
//!
//! They write only rows whose `description` begins with a `probe:` marker, and
//! each probe clears its own before and after.

use std::sync::Arc;

use adjutant_finance::{FinancePlugin, FUND_GENERAL, FUND_SCHOLARSHIP};
use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_finance";
/// The caller every probe acts as. Not a UUID on purpose: the audit write then
/// attributes the actor in `details` instead of `core.audit_log.user_id`, which
/// is what a non-member probe caller is.
const CALLER: &str = "probe-finance-treasurer";
/// Every row this file writes carries it in `description`, so a probe can clear
/// and count exactly what it wrote and nothing else.
const PROBE_MARK: &str = "probe:finance-route-sql:";
/// A fixed date and year, so `fiscal_year` is stated rather than derived from
/// the clock — a probe that only passes on a particular day is a probe that
/// breaks later for no reason.
const OCCURRED_ON: &str = "2026-03-15";
/// The fiscal year `OCCURRED_ON` falls in.
const FISCAL_YEAR: i64 = 2026;

/// Serialises the probes: they assert on totals (`SUM(amount_cents)` of a fund),
/// so two running at once would be reading each other's rows.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the finance route probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test \
             database a boot has already prepared (they are #[ignore]d; run with `-- --ignored`)"
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
        println!(
            "[finance route probe] WARNING: running against {name:?}, which does not end in `_test`"
        );
    }
    url
}

/// The same URL with the plugin role's credentials, so the connection's
/// `session_user` is the plugin.
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

/// The admin pool the probes read rows back with — the operator's own
/// connection, so nothing here depends on a grant the plugin does not hold.
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
    let plugin_id = "finance";
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

/// The core's own preconditions, stated rather than assumed. Every route here
/// audits its write, so `core.audit_log` has to exist.
async fn ensure_core_ready(admin: &PgPool) {
    let audit: Option<String> = sqlx::query_scalar("SELECT to_regclass('core.audit_log')::text")
        .fetch_one(admin)
        .await
        .expect("look for core.audit_log");
    assert!(
        audit.is_some(),
        "the core's migrations are not applied to this database (no core.audit_log), so no write \
         here could be audited — run the live ladder against it: \
         `ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`"
    );
}

/// Apply this plugin's migrations, as the plugin role, in its own schema — the
/// same shape the runner uses (one transaction, `SET LOCAL search_path`).
///
/// A database the ladder prepared has them already, and every statement is
/// idempotent (the seed funds are `ON CONFLICT (code) DO NOTHING`), so this is a
/// no-op there.
///
/// `CREATE SCHEMA` is deliberately **not** done as the plugin role: a plugin
/// role does not own the database and has no `CREATE` on it (`bootstrap-isolation`
/// creates the schema as the operator, `AUTHORIZATION adjutant_plugin_<id>`), so
/// the probe does what the operator does and then migrates as the plugin, which
/// owns what is inside its own schema.
async fn ensure_plugin_schema(admin: &PgPool, plugin: &PgPool) {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS finance AUTHORIZATION {PLUGIN_ROLE}"
    ))
    .execute(admin)
    .await
    .expect("the finance schema, as `bootstrap-isolation` creates it");
    sqlx::query(&format!("ALTER SCHEMA finance OWNER TO {PLUGIN_ROLE}"))
        .execute(admin)
        .await
        .expect("the plugin role owns its own schema");
    let mut conn = plugin.acquire().await.expect("plugin connection");
    for migration in FinancePlugin::new().migrations() {
        let script = format!(
            "BEGIN; SET LOCAL search_path TO \"finance\"; {}; COMMIT;",
            migration.sql
        );
        use sqlx::Executor;
        Executor::execute(&mut *conn, sqlx::raw_sql(&script))
            .await
            .unwrap_or_else(|e| panic!("migration {} ({}): {e}", migration.version, migration.name));
    }
    let seeded: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM finance.funds")
        .fetch_one(admin)
        .await
        .expect("count the seeded funds");
    assert!(
        seeded >= 6,
        "finance's six funds are not seeded (found {seeded}) — the finance migration must be \
         applied, which the live ladder does"
    );
}

// ---------------------------------------------------------------------------
// The rows a probe writes, and reads back
// ---------------------------------------------------------------------------

/// One ledger row, as this file cares about it.
#[derive(Debug)]
struct Leg {
    id: i64,
    fund_id: i64,
    amount_cents: i64,
    kind: String,
    transfer_group: Option<String>,
    counterparty_fund_id: Option<i64>,
    external_ref: Option<String>,
}

/// Clear anything a previous run left behind. Called **before** a probe writes
/// and again after, never between a write and the assertion on it.
async fn cleanup(admin: &PgPool, mark: &str) {
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE description LIKE $1")
        .bind(format!("{PROBE_MARK}{mark}%"))
        .execute(admin)
        .await;
}

/// Every row a probe wrote, in id order.
async fn written(admin: &PgPool, mark: &str) -> Vec<Leg> {
    let rows = sqlx::query(
        "SELECT id, fund_id, amount_cents, kind, transfer_group::text AS transfer_group, \
                counterparty_fund_id, external_ref \
         FROM finance.transactions WHERE description LIKE $1 ORDER BY id",
    )
    .bind(format!("{PROBE_MARK}{mark}%"))
    .fetch_all(admin)
    .await
    .expect("read the rows the probe wrote");
    rows.iter()
        .map(|row| Leg {
            id: row.get("id"),
            fund_id: row.get("fund_id"),
            amount_cents: row.get("amount_cents"),
            kind: row.get("kind"),
            transfer_group: row.get("transfer_group"),
            counterparty_fund_id: row.get("counterparty_fund_id"),
            external_ref: row.get("external_ref"),
        })
        .collect()
}

/// The fund a code names, as the seed wrote it.
async fn fund_id(admin: &PgPool, code: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM finance.funds WHERE code = $1")
        .bind(code)
        .fetch_one(admin)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "finance's {code:?} fund: {e} — the finance schema must be migrated (run the live \
                 ladder against this database)"
            )
        })
}

/// A fund's derived balance, read from the ledger exactly as the plugin derives
/// it.
async fn balance_cents(admin: &PgPool, fund: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_cents), 0)::bigint FROM finance.transactions \
         WHERE fund_id = $1",
    )
    .bind(fund)
    .fetch_one(admin)
    .await
    .expect("sum a fund's ledger")
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

/// Neither route here calls another plugin: a transaction and a transfer are
/// one statement each, so a probe that reached HTTP would be proving the wrong
/// mechanism. This handle refuses loudly rather than quietly passing.
#[derive(Default)]
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
            "the finance route probes write one statement each; nothing should be called \
             ({method} {url})"
        )))
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

/// The plugin's `core.plugins.config` block, as a boot hands it over. Every
/// amount and date in these probes is passed per request, so nothing here
/// depends on a configured number.
fn config_json() -> Value {
    json!({
        "fiscal_year_start_month": 1,
        "membership_cost_cents": 25_000,
        "dues_fund_code": FUND_GENERAL,
    })
}

/// A real `PluginContext`, with the **two** database hosts the core hands a
/// plugin (`plugin_runtime::build_context`): `ctx.db` is the plugin's own role —
/// its schema, and the `session_user` every statement runs as — while the audit
/// and permission services run on the **core's** pool, exactly as they do in
/// production. That matters here: the plugin role holds no grant on
/// `core.audit_log`, and a probe that ran everything on one pool would be
/// testing a shape the core never builds.
fn context(admin: &PgPool, plugin: &PgPool) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "finance".to_string(),
        db: DbHandle::new(plugin_db, "finance".to_string()),
        config: config_json(),
        events: EventBusHandle::new(Arc::new(InertEvents), "finance".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "finance".to_string()),
        identity: Arc::new(NoIdentity),
        http: Arc::new(NoHttp),
    }
}

/// The plugin's own route list, built the way the core builds it. The probes
/// call the handlers **out of this list**, so an edit to the statement behind a
/// route is what is exercised — not a copy of it.
async fn routes_of(ctx: &PluginContext) -> Vec<RouteDefinition> {
    let mut plugin = FinancePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    plugin.routes()
}

/// One request through a real handler. A route that is not there is a defect in
/// this file, not a silent skip.
async fn call(
    routes: &[RouteDefinition],
    path: &str,
    body: &Value,
) -> Result<PluginResponse, SdkError> {
    let route = routes
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| panic!("the plugin serves no route {path:?}"));
    (route.handler)(
        TestRequest::post(path)
            .identity(CALLER, &["treasurer"])
            .json(body)
            .build(),
    )
    .await
}

/// One request that must be **accepted**: a 2xx, decoded as JSON.
async fn accepted(routes: &[RouteDefinition], path: &str, body: &Value) -> Value {
    match call(routes, path, body).await {
        Ok(response) if (200..300).contains(&response.status) => response_json(&response),
        Ok(response) => panic!(
            "{path} answered {} — expected it to be accepted: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => panic!("{path} returned an error — expected it to be accepted: {e}"),
    }
}

/// One refusal: its status **and its own reason**, whichever shape it crosses
/// the boundary in — a validation failure is an `SdkError` (a 400), while a
/// guarded write that wrote nothing is a `4xx` response body.
#[derive(Debug)]
struct Refusal {
    status: u16,
    reason: String,
}

async fn refused(routes: &[RouteDefinition], path: &str, body: &Value) -> Refusal {
    match call(routes, path, body).await {
        Ok(response) if response.status >= 400 => Refusal {
            status: response.status,
            reason: response_json(&response)["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&response.body).to_string()),
        },
        Ok(response) => panic!(
            "{path} answered {} — expected a refusal: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => Refusal {
            status: e.status(),
            reason: e.to_string(),
        },
    }
}

/// A transaction body, with the marker that makes it this probe's row.
fn transaction_body(mark: &str, fund: Value, amount_cents: i64, external_ref: Option<&str>) -> Value {
    let mut body = json!({
        "kind": "income",
        "amount_cents": amount_cents,
        "category": "dues",
        "member_id": "probe-member",
        "description": format!("{PROBE_MARK}{mark}"),
        "occurred_on": OCCURRED_ON,
        "fiscal_year": FISCAL_YEAR,
    });
    if let Value::Object(map) = fund {
        for (k, v) in map {
            body[k] = v;
        }
    }
    if let Some(reference) = external_ref {
        body["external_ref"] = json!(reference);
    }
    body
}

/// A transfer body, with the marker that makes both its legs this probe's rows.
fn transfer_body(mark: &str, from: Value, to: Value, amount_cents: i64) -> Value {
    let mut body = json!({
        "amount_cents": amount_cents,
        "description": format!("{PROBE_MARK}{mark}"),
        "occurred_on": OCCURRED_ON,
        "fiscal_year": FISCAL_YEAR,
    });
    for (prefix, leg) in [("from_fund_", from), ("to_fund_", to)] {
        if let Value::Object(map) = leg {
            for (k, v) in map {
                body[format!("{prefix}{k}")] = v;
            }
        }
    }
    body
}

// ===========================================================================
// 1. A transaction: named by id or by code, one fund row either way
// ===========================================================================

/// **The reference probe.** `fund_id` and `fund_code` are two spellings of one
/// answer, and both must land on the same row in `finance.funds` — the code
/// resolved *inside the insert's own statement*, which is what lets a producer
/// that holds only a code write without reading finance first. An unknown code
/// writes nothing and names the reference it could not resolve.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_transaction_lands_on_the_same_fund_whether_named_by_id_or_by_code() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    let general = fund_id(&admin, FUND_GENERAL).await;

    let mark = "transaction-refs";
    let missing = "transaction-unknown-code";
    cleanup(&admin, mark).await;
    cleanup(&admin, missing).await;

    // --- by id
    let by_id = accepted(
        &routes,
        "/api/finance/transaction",
        &transaction_body(
            mark,
            json!({ "fund_id": general }),
            1_234,
            Some("probe_finance_route_by_id"),
        ),
    )
    .await;
    assert_eq!(by_id["transaction"]["fund_id"], general, "{by_id}");
    assert_eq!(by_id["fund_id"], general, "{by_id}");
    assert_eq!(by_id["transaction"]["kind"], "income", "{by_id}");
    assert_eq!(
        by_id["transaction"]["amount_cents"], 1_234,
        "income is stored positive: {by_id}"
    );

    // --- by code, and it is the **same** fund row
    let by_code = accepted(
        &routes,
        "/api/finance/transaction",
        &transaction_body(
            mark,
            json!({ "fund_code": FUND_GENERAL }),
            4_321,
            Some("probe_finance_route_by_code"),
        ),
    )
    .await;
    assert_eq!(
        by_code["transaction"]["fund_id"], general,
        "a code resolves to the same fund id an id names: {by_code}"
    );
    assert_eq!(by_code["fund_id"], general, "{by_code}");

    // The rows themselves agree with both answers.
    let rows = written(&admin, mark).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().all(|leg| leg.fund_id == general),
        "both entries landed in the general fund: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|leg| leg.kind == "income" && leg.amount_cents > 0),
        "income is one positive row each: {rows:?}"
    );
    let mut amounts: Vec<i64> = rows.iter().map(|leg| leg.amount_cents).collect();
    amounts.sort_unstable();
    assert_eq!(amounts, vec![1_234, 4_321]);
    assert_eq!(
        rows.iter()
            .filter_map(|leg| leg.external_ref.clone())
            .collect::<Vec<_>>(),
        vec![
            "probe_finance_route_by_id".to_string(),
            "probe_finance_route_by_code".to_string()
        ],
        "each row carries the reference the caller stated: {rows:?}"
    );
    for leg in &rows {
        assert!(leg.transfer_group.is_none() && leg.counterparty_fund_id.is_none());
    }

    // --- an unknown code: refused, naming the reference, and nothing written.
    let unknown = refused(
        &routes,
        "/api/finance/transaction",
        &transaction_body(
            missing,
            json!({ "fund_code": "probe_no_such_fund" }),
            100,
            Some("probe_finance_route_unknown"),
        ),
    )
    .await;
    assert_eq!(unknown.status, 404, "{unknown:?}");
    assert!(
        unknown.reason.contains("no such fund") && unknown.reason.contains("probe_no_such_fund"),
        "the refusal names the reference it could not resolve: {unknown:?}"
    );
    assert_eq!(
        written(&admin, missing).await.len(),
        0,
        "a fund that does not exist has no row for the entry to land in"
    );
    let by_reference: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM finance.transactions WHERE external_ref = $1",
    )
    .bind("probe_finance_route_unknown")
    .fetch_one(&admin)
    .await
    .expect("count the refused reference");
    assert_eq!(by_reference, 0);

    cleanup(&admin, mark).await;
    cleanup(&admin, missing).await;
}

// ===========================================================================
// 2. A transfer: two legs, one statement, one group
// ===========================================================================

/// **The conservation probe.** Both legs are named by **code** and must resolve
/// to two fund rows, be written as two rows under one `transfer_group`, sum to
/// zero, and name each other in `counterparty_fund_id` — with the resolved ids
/// on `transactions.fund_id`, which is the only place a code-named leg's id
/// appears.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_code_named_transfer_writes_two_legs_in_one_group_that_cancel() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    let general = fund_id(&admin, FUND_GENERAL).await;
    let scholarship = fund_id(&admin, FUND_SCHOLARSHIP).await;

    let seed = "transfer-seed";
    let mark = "transfer-legs";
    cleanup(&admin, seed).await;
    cleanup(&admin, mark).await;

    // The out-leg needs money to move: one income entry, through the route.
    accepted(
        &routes,
        "/api/finance/transaction",
        &transaction_body(
            seed,
            json!({ "fund_code": FUND_GENERAL }),
            5_000,
            Some("probe_finance_transfer_seed"),
        ),
    )
    .await;

    let body = accepted(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            mark,
            json!({ "code": FUND_GENERAL }),
            json!({ "code": FUND_SCHOLARSHIP }),
            2_000,
        ),
    )
    .await;
    assert_eq!(
        body["entries"].as_array().map(Vec::len),
        Some(2),
        "the statement's own answer is two legs: {body}"
    );
    assert_eq!(body["sum_cents"], 0, "{body}");

    let rows = written(&admin, mark).await;
    assert_eq!(rows.len(), 2, "two rows, one statement: {rows:?}");
    let group = rows[0]
        .transfer_group
        .clone()
        .expect("a transfer leg names its group");
    assert_eq!(
        body["transfer_group"].as_str(),
        Some(group.as_str()),
        "the answer and the rows agree about the group: {body}"
    );
    assert!(
        rows.iter()
            .all(|leg| leg.transfer_group.as_deref() == Some(group.as_str())),
        "both legs share the one group: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .map(|leg| leg.amount_cents)
            .sum::<i64>(),
        0,
        "conservation is a property of the statement: {rows:?}"
    );

    let out = rows
        .iter()
        .find(|leg| leg.amount_cents < 0)
        .expect("the out-leg");
    let into = rows
        .iter()
        .find(|leg| leg.amount_cents > 0)
        .expect("the in-leg");
    assert_eq!((out.amount_cents, into.amount_cents), (-2_000, 2_000));
    assert_eq!(
        out.fund_id, general,
        "the out-leg is the fund the from-code named, resolved in the statement: {rows:?}"
    );
    assert_eq!(into.fund_id, scholarship, "{rows:?}");
    assert_eq!(
        out.counterparty_fund_id,
        Some(scholarship),
        "each leg names the other fund: {rows:?}"
    );
    assert_eq!(into.counterparty_fund_id, Some(general), "{rows:?}");
    assert!(
        rows.iter().all(|leg| leg.kind == "transfer"),
        "both legs are transfer entries: {rows:?}"
    );

    // Exactly two rows in the whole table carry that group — nothing else was
    // written under it.
    let in_group: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM finance.transactions WHERE transfer_group = $1::uuid",
    )
    .bind(&group)
    .fetch_one(&admin)
    .await
    .expect("count the group");
    assert_eq!(in_group, 2, "a transfer is exactly two rows");

    cleanup(&admin, seed).await;
    cleanup(&admin, mark).await;
}

// ===========================================================================
// 3. Two references that resolve to one fund, and a reference that resolves to
//    nothing: refused, and neither leg written
// ===========================================================================

/// **The refusal probe.** A transfer to itself is not a transfer. Two equal
/// codes are refused before the statement; an id and a code that resolve to one
/// fund can only be compared once the code is resolved, which the statement's
/// own guard does — and in both cases the guard sits on the single `SELECT`, so
/// **neither** leg is written. An unresolvable reference is refused too, naming
/// the reference, and writes nothing either.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn same_fund_and_unknown_reference_transfers_are_refused_and_write_nothing() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    let general = fund_id(&admin, FUND_GENERAL).await;

    let codes = "same-fund-codes";
    let mixed = "same-fund-mixed";
    let unknown_from = "unknown-from";
    let unknown_to = "unknown-to";
    for mark in [codes, mixed, unknown_from, unknown_to] {
        cleanup(&admin, mark).await;
    }

    // --- two equal codes: the handler can compare them, and refuses.
    let equal = refused(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            codes,
            json!({ "code": FUND_GENERAL }),
            json!({ "code": FUND_GENERAL }),
            100,
        ),
    )
    .await;
    assert_eq!(equal.status, 400, "{equal:?}");
    assert!(
        equal.reason.contains("two different funds"),
        "the refusal states its own reason: {equal:?}"
    );
    assert_eq!(
        written(&admin, codes).await.len(),
        0,
        "a transfer to itself writes nothing"
    );

    // --- an id and a code that name one fund: only the statement can see this,
    // and its guard refuses both legs at once.
    let same = refused(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            mixed,
            json!({ "id": general }),
            json!({ "code": FUND_GENERAL }),
            100,
        ),
    )
    .await;
    assert_eq!(same.status, 409, "{same:?}");
    assert!(
        same.reason.contains("same fund"),
        "the resolution — not a status code — is the reason: {same:?}"
    );
    assert_eq!(
        written(&admin, mixed).await.len(),
        0,
        "the guard sits on the single SELECT, so neither leg is written"
    );

    // --- a reference that resolves to nothing: refused by name, nothing written.
    let missing = refused(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            unknown_from,
            json!({ "code": "probe_no_such_fund" }),
            json!({ "code": FUND_SCHOLARSHIP }),
            100,
        ),
    )
    .await;
    assert_eq!(missing.status, 404, "{missing:?}");
    assert!(
        missing.reason.contains("no such fund") && missing.reason.contains("probe_no_such_fund"),
        "the refusal names the reference: {missing:?}"
    );
    assert_eq!(written(&admin, unknown_from).await.len(), 0);

    let missing = refused(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            unknown_to,
            json!({ "code": FUND_GENERAL }),
            json!({ "id": 9_999_999 }),
            100,
        ),
    )
    .await;
    assert_eq!(missing.status, 404, "{missing:?}");
    assert!(
        missing.reason.contains("no such fund") && missing.reason.contains("9999999"),
        "an id that names no fund is refused the same way: {missing:?}"
    );
    assert_eq!(written(&admin, unknown_to).await.len(), 0);

    for mark in [codes, mixed, unknown_from, unknown_to] {
        cleanup(&admin, mark).await;
    }
}

// ===========================================================================
// 4. The overdraft guard: refused, nothing written, the balance untouched
// ===========================================================================

/// **The overdraft probe.** The guard is one `AND` on the statement's own
/// `SELECT`, so an out-leg that would take its fund negative writes **neither**
/// leg — not a half-transfer to be compensated later. One cent past the fund's
/// balance is over, and the balance afterwards is exactly what it was.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn an_overdrafting_transfer_is_refused_and_writes_nothing() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    let general = fund_id(&admin, FUND_GENERAL).await;

    let mark = "overdraft";
    cleanup(&admin, mark).await;

    // One cent more than the fund holds (and one cent at all, when the fund is
    // already overdrawn) — over the line either way, without assuming a balance.
    let balance = balance_cents(&admin, general).await;
    let too_much = balance.max(0) + 1;

    let refusal = refused(
        &routes,
        "/api/finance/transfer",
        &transfer_body(
            mark,
            json!({ "code": FUND_GENERAL }),
            json!({ "code": FUND_SCHOLARSHIP }),
            too_much,
        ),
    )
    .await;
    assert_eq!(refusal.status, 409, "{refusal:?}");
    assert!(
        refusal.reason.contains("may not go negative"),
        "the refusal is the overdraft guard's own words: {refusal:?}"
    );
    assert!(
        refusal.reason.contains("allow_overdraft"),
        "and it says how a person would authorise one: {refusal:?}"
    );
    assert_eq!(
        written(&admin, mark).await.len(),
        0,
        "the guard is on the statement: neither leg is written"
    );
    let after = balance_cents(&admin, general).await;
    assert_eq!(
        after, balance,
        "a refused transfer moves no money: {balance} → {after}"
    );

    cleanup(&admin, mark).await;
}

// ===========================================================================
// 5. A replayed external_ref: one row, and the answer says which one
// ===========================================================================

/// **The idempotency probe.** `external_ref` is a unique key — finance's own
/// `external_ref`, which the ledger path promises a producer's replayed write
/// against — so the same entry posted twice is one row, and the second answer is
/// handed back the row it already has rather than writing a second deposit.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_replayed_external_ref_leaves_one_row_and_returns_it() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    fund_id(&admin, FUND_GENERAL).await;

    let mark = "replay";
    cleanup(&admin, mark).await;
    let reference = format!("{PROBE_MARK}{mark}:external");
    let body = transaction_body(
        mark,
        json!({ "fund_code": FUND_GENERAL }),
        777,
        Some(&reference),
    );

    let first = accepted(&routes, "/api/finance/transaction", &body).await;
    let id = first["transaction"]["id"]
        .as_i64()
        .expect("the first write names its row");
    assert!(id > 0, "{first}");

    // The same entry again — what a producer retrying its own unconfirmed write
    // sends.
    let second = accepted(&routes, "/api/finance/transaction", &body).await;
    assert_eq!(second["duplicate"], true, "{second}");
    assert_eq!(second["recorded"], false, "{second}");
    assert_eq!(
        second["transaction"]["id"].as_i64(),
        Some(id),
        "the replayed write is handed back the row it already has: {second}"
    );

    let rows = written(&admin, mark).await;
    assert_eq!(
        rows.len(),
        1,
        "one row however many times the entry is posted: {rows:?}"
    );
    assert_eq!(rows[0].id, id);

    cleanup(&admin, mark).await;
}
