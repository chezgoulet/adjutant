//! # The dues funding rule, against a real database
//!
//! The probes the mock host cannot write, because the statements are real SQL
//! against real constraints: a waiver whose caller holds `finance:write` lands
//! with `funded_cents` equal to the tier's assessment **and** books its draw as
//! two ledger rows in one transfer group summing to zero; a retry carrying the
//! draw's deterministic `external_ref` writes nothing a second time; the rows
//! already waived at zero are untouched until the guarded repair route is called;
//! and the constraint swap migration 2 performs holds on the database itself.
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_waiver_test \
//!   cargo test -p adjutant-finance --test dues_funding -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.roles`, `core.permissions`, `core.role_permissions` and
//! `finance.funds` exist) and the `finance` plugin's own role created with its
//! secret stored in `core.plugins.db_secret`. The live ladder produces exactly
//! that, and applies migration 2 — the funding columns and the constraint swap —
//! on the way:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes then connect **as the plugin's own role** for the plugin's schema
//! (its tables, its migrations) and on the operator's connection for `core.*`,
//! which is the pair of database handles the core itself hands a plugin. They
//! read the stored secret rather than rotating it, so a later boot of the same
//! database still works. Every row they write is named `probe_`, and each probe
//! clears its own before and after.

use std::sync::Arc;

use adjutant_finance::{
    dues_draw_reference, FinancePlugin, DUES_KIND_MEMBER, FUND_GENERAL, FUND_SCHOLARSHIP,
    STATUS_ASSESSED, STATUS_WAIVED, TIER_STANDARD, TIER_SUPPORTED,
};
use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_finance";
/// A role the probe declares for its own caller, and grants `finance:write`.
/// Deleting the role cascades the grant away, so the probe leaves nothing.
const PROBE_ROLE: &str = "probe_dues_treasurer";
/// The year every row in these probes belongs to.
const FISCAL_YEAR: i32 = 2026;
/// The membership cost the probe configures, so a `standard` assessment is it.
const BASE_CENTS: i64 = 60_000;
/// A scholarship balance large enough that a draw never needs an overdraft.
const SCHOLARSHIP_BALANCE: i64 = 5_000_000;

/// Serialises the probes: they write ledger rows and read group sums, and two
/// runs interleaving would see each other's money.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the dues funding probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test \
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
            "[dues funding probe] WARNING: running against {name:?}, which does not end in `_test`"
        );
    }
    url
}

/// The same URL with the plugin role's credentials.
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
async fn plugin_pool(admin: &PgPool) -> PgPool {
    let url = test_database_url();
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = $1")
            .bind("finance")
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

/// The core's own preconditions, stated rather than assumed.
///
/// `finance:write` must be a registered permission (the ladder registers every
/// plugin's declared vocabulary) for the grant the probe makes to mean anything.
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
         database so the finance plugin's vocabulary is registered"
    );
}

/// Declare the probe's caller and grant it `finance:write` — a real grant row,
/// evaluated by `PermissionService` exactly as a deployment's would be.
async fn ensure_probe_role(admin: &PgPool) {
    for sql in [
        "INSERT INTO core.roles (id, display_name) VALUES ($1, 'probe dues treasurer') \
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

/// Apply this plugin's migrations as the plugin role, in its own schema — the
/// shape the runner uses (one transaction, `SET LOCAL search_path`).
///
/// A database the ladder prepared has them already, and every statement is
/// idempotent, so this is a no-op there. It is what makes the probes independent
/// of the ladder having run migration 2.
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
    let mut declared = FinancePlugin::new();
    // `init` is what the probe needs migrations from; the context's database is
    // not read by `migrations()`.
    let ctx = context(admin, plugin).await;
    declared.init(ctx).await.expect("init");
    let migrations = declared.migrations();
    let mut conn = plugin.acquire().await.expect("plugin connection");
    for migration in migrations {
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

/// Clear anything a previous run left behind, for one member and year.
async fn cleanup_member(admin: &PgPool, member_id: &str) {
    let reference = dues_draw_reference(FISCAL_YEAR, member_id);
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE external_ref = $1")
        .bind(&reference)
        .execute(admin)
        .await;
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE member_id = $1")
        .bind(member_id)
        .execute(admin)
        .await;
    let _ = sqlx::query("DELETE FROM finance.dues WHERE member_id = $1")
        .bind(member_id)
        .execute(admin)
        .await;
}

/// Give `scholarship` a real balance, so a draw is never refused for an
/// overdraft the probe did not mean to test. Removed again at the end.
const SCHOLARSHIP_DEPOSIT_REF: &str = "probe:dues_funding:scholarship_deposit";

async fn fund_scholarship(admin: &PgPool) {
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE external_ref = $1")
        .bind(SCHOLARSHIP_DEPOSIT_REF)
        .execute(admin)
        .await;
    sqlx::query(
        "INSERT INTO finance.transactions \
           (fund_id, amount_cents, kind, category, description, member_id, fiscal_year, \
            occurred_on, recorded_by, external_ref) \
         SELECT f.id, $1, 'income', 'donation', 'probe scholarship deposit', '', $2, \
                ($3 || '-01-15')::date, 'probe', $4 \
         FROM finance.funds f WHERE f.code = $5",
    )
    .bind(SCHOLARSHIP_BALANCE)
    .bind(FISCAL_YEAR)
    .bind(FISCAL_YEAR.to_string())
    .bind(SCHOLARSHIP_DEPOSIT_REF)
    .bind(FUND_SCHOLARSHIP)
    .execute(admin)
    .await
    .expect("fund the scholarship fund for the probe");
}

async fn unfund_scholarship(admin: &PgPool) {
    let _ = sqlx::query("DELETE FROM finance.transactions WHERE external_ref = $1")
        .bind(SCHOLARSHIP_DEPOSIT_REF)
        .execute(admin)
        .await;
}

// ---------------------------------------------------------------------------
// The host the plugin runs against: a real database
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

/// `HostDb` over a pool, binding exactly as the core does.
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

/// A real `PluginContext`, with the two database hosts the core hands a plugin
/// (`plugin_runtime::build_context`): `ctx.db` is the plugin's own role and
/// schema, while the audit and permission services run on the **core's** pool —
/// which is what makes the in-handler `finance:write` check a real check.
async fn context(admin: &PgPool, plugin: &PgPool) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "finance".to_string(),
        db: DbHandle::new(plugin_db, "finance".to_string()),
        config: json!({
            "membership_cost_cents": BASE_CENTS,
            "dues_fund_code": FUND_GENERAL,
        }),
        events: EventBusHandle::new(Arc::new(InertEvents), "finance".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "finance".to_string()),
        identity: Arc::new(NoIdentity),
        http: Arc::new(NoHttp),
    }
}

/// A draw is a call to finance's *own* transfer route, in process: nothing in
/// these probes reaches HTTP, so a request would prove the wrong mechanism.
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
            "a dues draw is finance's own transfer route, in process: nothing should call \
             {method} {url}"
        )))
    }
}

async fn routes_of(ctx: &PluginContext) -> Vec<RouteDefinition> {
    let mut plugin = FinancePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    plugin.routes()
}

/// The caller every booking probe uses: the probe's role, which holds
/// `finance:write` by a real `core.role_permissions` row.
fn treasurer() -> TestRequest {
    TestRequest::post("/api/finance/dues/assess").identity_grants(
        "probe-treasurer",
        vec![RoleGrant {
            role_id: PROBE_ROLE.into(),
            scope: Scope::troop(),
        }],
    )
}

/// One assessment through the real handler.
async fn assess(ctx: &PluginContext, member_id: &str, tier: &str, status: &str) -> Value {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/dues/assess")
        .expect("the assess route");
    let request = treasurer()
        .json(&json!({
            "member_id": member_id,
            "tier": tier,
            "status": status,
            "fiscal_year": FISCAL_YEAR,
        }))
        .build();
    let response = (route.handler)(request).await.expect("the assess handler");
    assert_eq!(response.status, 200, "{:?}", response_json(&response));
    response_json(&response)
}

/// One assessment through the real handler — the shape a refusal crosses the
/// boundary in, without the `200` assertion [`assess`] makes.
async fn assess_status(
    ctx: &PluginContext,
    member_id: &str,
    tier: &str,
    status: &str,
) -> (u16, Value) {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/dues/assess")
        .expect("the assess route");
    let request = treasurer()
        .json(&json!({
            "member_id": member_id,
            "tier": tier,
            "status": status,
            "fiscal_year": FISCAL_YEAR,
        }))
        .build();
    match (route.handler)(request).await {
        Ok(response) => (response.status, response_json(&response)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// `GET /api/finance/dues` through the real handler: the standing the SQL derives
/// from the row and the ledger — what proves `outstanding_cents` and `settled`
/// read the member's own share rather than the assessment.
async fn dues_list(ctx: &PluginContext) -> Value {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/dues")
        .expect("the dues list route");
    let response = (route.handler)(
        TestRequest::get("/api/finance/dues")
            .query_param("fiscal_year", &FISCAL_YEAR.to_string())
            .identity("probe-treasurer", &[PROBE_ROLE])
            .build(),
    )
    .await
    .expect("the dues list handler");
    assert_eq!(response.status, 200, "{:?}", response_json(&response));
    response_json(&response)
}

/// One transfer through the real handler — the route the draw books through.
async fn transfer(
    ctx: &PluginContext,
    amount_cents: i64,
    external_ref: &str,
) -> (u16, Value) {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/transfer")
        .expect("the transfer route");
    let request = TestRequest::post("/api/finance/transfer")
        .identity_grants(
            "probe-treasurer",
            vec![RoleGrant {
                role_id: PROBE_ROLE.into(),
                scope: Scope::troop(),
            }],
        )
        .json(&json!({
            "from_fund_code": FUND_SCHOLARSHIP,
            "to_fund_code": FUND_GENERAL,
            "amount_cents": amount_cents,
            "fiscal_year": FISCAL_YEAR,
            "external_ref": external_ref,
        }))
        .build();
    match (route.handler)(request).await {
        Ok(response) => (response.status, response_json(&response)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// The repair route, through the real handler.
async fn repair(ctx: &PluginContext, body: Value) -> Value {
    let routes = routes_of(ctx).await;
    let route = routes
        .iter()
        .find(|r| r.path == "/api/finance/dues/repair-waivers")
        .expect("the repair route");
    let response = (route.handler)(
        TestRequest::post("/api/finance/dues/repair-waivers")
            .identity("probe-treasurer", &[PROBE_ROLE])
            .json(&body)
            .build(),
    )
    .await
    .expect("the repair handler");
    assert_eq!(response.status, 200, "{:?}", response_json(&response));
    response_json(&response)
}

/// One `finance.dues` row, as the columns the rule is about.
async fn dues_row(admin: &PgPool, member_id: &str) -> Option<(i64, i64, String, Option<String>)> {
    sqlx::query_as(
        "SELECT assessed_cents, funded_cents, draw_status, draw_ref::text FROM finance.dues \
          WHERE member_id = $1 AND fiscal_year = $2 AND dues_kind = 'member'",
    )
    .bind(member_id)
    .bind(FISCAL_YEAR)
    .fetch_optional(admin)
    .await
    .expect("read the dues row")
}

/// One transfer group's own arithmetic: how many rows, and what they sum to.
async fn group_arithmetic(admin: &PgPool, group: &str) -> (i64, i64) {
    sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(amount_cents), 0)::bigint \
         FROM finance.transactions WHERE transfer_group = $1::uuid",
    )
    .bind(group)
    .fetch_one(admin)
    .await
    .expect("read the transfer group")
}

// ===========================================================================
// 1. A waiver funds its tier, and the draw is one balanced transfer
// ===========================================================================

/// **The funding probe.** A waiver whose caller holds `finance:write` writes the
/// tier's assessment (not a zero), records `funded_cents = assessed_cents`, and
/// books the draw **in the same flow as that caller**: two ledger rows in one
/// `transfer_group` summing to zero, from `scholarship` into the configured dues
/// fund, carrying the draw's deterministic reference.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_waived_assessment_funds_its_tier_and_books_one_balanced_transfer() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_role(&admin).await;
    let ctx = context(&admin, &plugin).await;

    let member = "probe_dues_waiver";
    cleanup_member(&admin, member).await;
    fund_scholarship(&admin).await;

    let body = assess(&ctx, member, TIER_STANDARD, STATUS_WAIVED).await;

    // The row: the tier's assessment, funded in full, with the draw booked.
    let (assessed, funded, draw_status, draw_ref) = dues_row(&admin, member)
        .await
        .expect("the waived row landed");
    assert_eq!(
        assessed, BASE_CENTS,
        "a waiver assesses its tier, not zero: {body}"
    );
    assert_eq!(
        funded, BASE_CENTS,
        "and funds the whole of it, so the member's own share is zero: {body}"
    );
    assert_eq!(draw_status, "booked", "{body}");
    let group = draw_ref.expect("a booked draw names its transfer group");
    assert_eq!(body["funded_cents"], json!(BASE_CENTS), "{body}");
    assert_eq!(body["draw_status"], json!("booked"), "{body}");
    assert_eq!(body["member_share_cents"], json!(0), "{body}");

    // The draw: **two** ledger rows in **one** group summing to zero.
    let (entries, sum) = group_arithmetic(&admin, &group).await;
    assert_eq!(entries, 2, "a draw is two legs, not one: group {group}");
    assert_eq!(sum, 0, "and a transfer moves money: group {group}");

    // The legs' own shape: `scholarship` out, the dues fund in, one category, and
    // the deterministic reference on the out-leg alone (the unique index is per
    // row, so both legs cannot carry it).
    let (out_code, out_ref, out_amount, in_code, in_ref, in_amount): (
        String,
        Option<String>,
        i64,
        String,
        Option<String>,
        i64,
    ) = sqlx::query_as(
        "SELECT fo.code, o.external_ref, o.amount_cents, fi.code, i.external_ref, i.amount_cents \
         FROM finance.transactions o \
         JOIN finance.funds fo ON fo.id = o.fund_id \
         JOIN finance.transactions i ON i.transfer_group = o.transfer_group \
              AND i.amount_cents > 0 \
         JOIN finance.funds fi ON fi.id = i.fund_id \
         WHERE o.transfer_group = $1::uuid AND o.amount_cents < 0",
    )
    .bind(&group)
    .fetch_one(&admin)
    .await
    .expect("the draw's two legs");
    assert_eq!(out_code, FUND_SCHOLARSHIP, "the subsidy leaves scholarship");
    assert_eq!(in_code, FUND_GENERAL, "and lands in the configured dues fund");
    assert_eq!(out_amount, -BASE_CENTS);
    assert_eq!(in_amount, BASE_CENTS);
    assert_eq!(
        out_ref.as_deref(),
        Some(dues_draw_reference(FISCAL_YEAR, member).as_str()),
        "the draw carries its deterministic reference, so a retry is recognisable"
    );
    assert_eq!(
        in_ref, None,
        "and only the out-leg: the reference's index is unique per row"
    );

    // And the standing the SQL derives reads the member's own share: a waiver
    // funds the whole assessment, so nothing is owed and the row is `settled`
    // without a payment row pretending otherwise.
    let list = dues_list(&ctx).await;
    let listed = list["dues"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["member_id"].as_str() == Some(member))
        .cloned()
        .unwrap_or_else(|| panic!("the member is listed: {list}"));
    assert_eq!(
        listed["outstanding_cents"],
        json!(0),
        "a funded waiver owes nothing: {listed}"
    );
    assert_eq!(
        listed["settled"],
        json!(true),
        "and is settled by the funding, not by a payment: {listed}"
    );
    assert_eq!(listed["funded_cents"], json!(BASE_CENTS), "{listed}");

    cleanup_member(&admin, member).await;
    unfund_scholarship(&admin).await;
}

// ===========================================================================
// 2. A retry carrying the same reference does not double-post
// ===========================================================================

/// **The lost-answer probe.** A transfer whose answer a caller never saw is
/// retried — the same `external_ref`, the same amount — and it writes **no**
/// second pair of legs: the guard in the statement finds the reference and the
/// route answers with the transfer that already exists. Re-assessing the member
/// through the real handler is the same retry from the other side, and the row
/// keeps the group it had.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_retried_draw_with_the_same_reference_does_not_double_post() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_role(&admin).await;
    let ctx = context(&admin, &plugin).await;

    let member = "probe_dues_retry";
    cleanup_member(&admin, member).await;
    fund_scholarship(&admin).await;

    let first = assess(&ctx, member, TIER_STANDARD, STATUS_WAIVED).await;
    let (_, _, _, group) = dues_row(&admin, member).await.expect("the waived row");
    let group = group.expect("booked");
    let reference = dues_draw_reference(FISCAL_YEAR, member);

    // The retry at the transfer route itself: the same key, the same money.
    let (status, duplicate) = transfer(&ctx, BASE_CENTS, &reference).await;
    assert_eq!(status, 200, "{duplicate}");
    assert_eq!(
        duplicate["duplicate"],
        json!(true),
        "a reference the ledger holds is the same transfer, not a new one: {duplicate}"
    );
    assert_eq!(
        duplicate["transfer_group"],
        json!(group),
        "and the answer is the group it already made: {duplicate}"
    );

    // Nothing was written twice: one row holds the reference, and the group is
    // still the two legs it started as.
    let holding: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM finance.transactions WHERE external_ref = $1")
            .bind(&reference)
            .fetch_one(&admin)
            .await
            .expect("count the reference's rows");
    assert_eq!(holding, 1, "one money move, one key: {first}");
    let (entries, sum) = group_arithmetic(&admin, &group).await;
    assert_eq!(entries, 2, "the retry added no legs");
    assert_eq!(sum, 0);

    // And the other half of the retry: re-assessing through the handler. The
    // draw is re-attempted with the same reference, so it is answered with the
    // same group and the row does not lose that it was booked.
    let again = assess(&ctx, member, TIER_STANDARD, STATUS_WAIVED).await;
    let (assessed, funded, draw_status, draw_ref) = dues_row(&admin, member).await.expect("the row");
    assert_eq!(assessed, BASE_CENTS, "{again}");
    assert_eq!(funded, BASE_CENTS);
    assert_eq!(draw_status, "booked", "a re-assessment does not un-book a draw: {again}");
    assert_eq!(
        draw_ref.as_deref(),
        Some(group.as_str()),
        "and the group is the one the first attempt made: {again}"
    );
    let (entries, _) = group_arithmetic(&admin, &group).await;
    assert_eq!(entries, 2, "still two legs, however many attempts");

    cleanup_member(&admin, member).await;
    unfund_scholarship(&admin).await;
}

// ===========================================================================
// 3. The rows already waived at zero: untouched until the repair is called
// ===========================================================================

/// **The historic-row probe.** A row waived at zero (the shape migration 2
/// leaves on a deployed database) is left exactly as it is until the guarded
/// repair route recomputes it from its own `base_cents` and `tier`; a row with
/// no membership cost to fund is left alone even then; and the repair is
/// idempotent — the second call has nothing to do.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn an_already_waived_zero_row_is_untouched_until_the_repair_route_runs() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_role(&admin).await;
    let ctx = context(&admin, &plugin).await;

    let historic = "probe_dues_historic";
    let no_base = "probe_dues_no_membership_cost";
    let zero_tier = "probe_dues_zero_tier";
    cleanup_member(&admin, historic).await;
    cleanup_member(&admin, no_base).await;
    cleanup_member(&admin, zero_tier).await;

    // The shapes a deployed database has, all waived at zero with no draw. The
    // first is repairable; the second's tier assesses nothing (so there is
    // nothing for the fund to cover); the third has no membership cost to fund
    // and the worklist does not even select it.
    for (member, base, tier) in [
        (historic, BASE_CENTS, TIER_STANDARD),
        (zero_tier, BASE_CENTS, "hardship"),
        (no_base, 0, TIER_STANDARD),
    ] {
        sqlx::query(
            "INSERT INTO finance.dues \
               (fiscal_year, dues_kind, member_id, tier, base_cents, assessed_cents, \
                funded_cents, draw_status, self_reported, status, recorded_by) \
             VALUES ($1, $2, $3, $4, $5, 0, 0, 'none', false, $6, 'probe')",
        )
        .bind(FISCAL_YEAR)
        .bind(DUES_KIND_MEMBER)
        .bind(member)
        .bind(tier)
        .bind(base)
        .bind(STATUS_WAIVED)
        .execute(&admin)
        .await
        .expect("write the historic waiver as a deployed database has it");
    }

    // Nothing recomputes them at startup: they are still zeros, and nothing has
    // been booked for them.
    assert_eq!(
        dues_row(&admin, historic).await,
        Some((0, 0, "none".to_string(), None)),
        "migration 2 must not guess a tier's share"
    );

    // The repair, on demand, from each row's own base and tier.
    let repaired = repair(&ctx, json!({ "fiscal_year": FISCAL_YEAR })).await;
    assert!(
        repaired["rows"]
            .as_array()
            .map(|rows| rows.iter().any(|row| {
                row["dues"]["member_id"] == json!(historic)
            }))
            .unwrap_or(false),
        "the repairable row is named in the answer: {repaired}"
    );
    assert_eq!(
        dues_row(&admin, historic).await,
        Some((
            BASE_CENTS,
            BASE_CENTS,
            "unbooked".to_string(),
            None
        )),
        "a repaired waiver assesses its tier and records the funding, outstanding: {repaired}"
    );
    // The row whose tier assesses nothing is left alone: there is no assessment
    // for the fund to cover.
    assert_eq!(
        dues_row(&admin, zero_tier).await,
        Some((0, 0, "none".to_string(), None)),
        "a tier that assesses nothing funds nothing: {repaired}"
    );
    assert_eq!(
        repaired["nothing_to_fund"],
        json!(1),
        "and the repair says so: {repaired}"
    );
    // The row with no membership cost is left alone — the worklist does not even
    // select it, because there is nothing it could fund.
    assert_eq!(
        dues_row(&admin, no_base).await,
        Some((0, 0, "none".to_string(), None)),
        "a row with no base has nothing to fund: {repaired}"
    );
    assert_eq!(
        repaired["candidates"],
        json!(2),
        "one repairable and one whose tier funds nothing: {repaired}"
    );

    // Idempotent: the repaired row no longer matches the worklist, and a second
    // call writes nothing. The row whose tier assesses nothing is still *listed*
    // — it is a candidate every time, and every time there is nothing to fund —
    // but nothing is written for it and the repairable row is not touched again.
    let again = repair(&ctx, json!({ "fiscal_year": FISCAL_YEAR })).await;
    assert_eq!(
        again["repaired"],
        json!(0),
        "nothing left to repair: {again}"
    );
    assert_eq!(
        again["nothing_to_fund"],
        json!(1),
        "the zero-tier row is listed again, and still funds nothing: {again}"
    );
    assert_eq!(
        dues_row(&admin, historic).await,
        Some((BASE_CENTS, BASE_CENTS, "unbooked".to_string(), None)),
        "and the repaired row is untouched by the second call"
    );

    cleanup_member(&admin, historic).await;
    cleanup_member(&admin, no_base).await;
}

// ===========================================================================
// 4. The constraint swap, on the database itself
// ===========================================================================

/// **The constraint probe.** Migration 2's three constraints are enforced by
/// PostgreSQL, not merely written down: a waiver that does not name what it
/// funded is refused, a subsidy larger than the assessment is refused, a
/// negative subsidy is refused — and a waiver whose funding equals its
/// assessment, which is the shape the rule produces, is accepted.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn the_constraint_swap_holds_on_the_real_database() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;

    let member = "probe_dues_constraints";
    cleanup_member(&admin, member).await;

    let refused = |e: sqlx::Error| e.to_string();
    // "Waived but owing" stays unrepresentable: a waiver must name what it funded.
    let error = sqlx::query(
        "INSERT INTO finance.dues \
           (fiscal_year, dues_kind, member_id, tier, base_cents, assessed_cents, funded_cents, \
            draw_status, status) \
         VALUES ($1, 'member', $2, $3, $4, 100, 0, 'none', $5)",
    )
    .bind(FISCAL_YEAR)
    .bind(member)
    .bind(TIER_STANDARD)
    .bind(BASE_CENTS)
    .bind(STATUS_WAIVED)
    .execute(&admin)
    .await
    .expect_err("a waiver that funds nothing of an assessment must be refused");
    assert!(
        refused(error).contains("dues_waived_is_funded"),
        "the refusal must name the constraint that keeps the invariant"
    );

    // A subsidy cannot exceed what is owed.
    let error = sqlx::query(
        "INSERT INTO finance.dues \
           (fiscal_year, dues_kind, member_id, tier, base_cents, assessed_cents, funded_cents, \
            draw_status, status) \
         VALUES ($1, 'member', $2, $3, $4, 100, 200, 'none', 'assessed')",
    )
    .bind(FISCAL_YEAR)
    .bind(member)
    .bind(TIER_STANDARD)
    .bind(BASE_CENTS)
    .execute(&admin)
    .await
    .expect_err("funding more than the assessment must be refused");
    assert!(
        refused(error).contains("dues_funded_within_assessment"),
        "and name it"
    );

    // A subsidy cannot be negative.
    let error = sqlx::query(
        "INSERT INTO finance.dues \
           (fiscal_year, dues_kind, member_id, tier, base_cents, assessed_cents, funded_cents, \
            draw_status, status) \
         VALUES ($1, 'member', $2, $3, $4, 0, -1, 'none', 'assessed')",
    )
    .bind(FISCAL_YEAR)
    .bind(member)
    .bind(TIER_STANDARD)
    .bind(BASE_CENTS)
    .execute(&admin)
    .await
    .expect_err("a negative subsidy must be refused");
    assert!(refused(error).contains("dues_funded_valid"), "and name it");

    // And the shape the rule produces is representable: a waiver that funds its
    // whole assessment.
    sqlx::query(
        "INSERT INTO finance.dues \
           (fiscal_year, dues_kind, member_id, tier, base_cents, assessed_cents, funded_cents, \
            draw_status, status) \
         VALUES ($1, 'member', $2, $3, $4, $5, $5, 'unbooked', $6)",
    )
    .bind(FISCAL_YEAR)
    .bind(member)
    .bind(TIER_STANDARD)
    .bind(BASE_CENTS)
    .bind(BASE_CENTS)
    .bind(STATUS_WAIVED)
    .execute(&admin)
    .await
    .expect("a waiver that funds its assessment is the shape the rule produces");
    assert_eq!(
        dues_row(&admin, member).await,
        Some((BASE_CENTS, BASE_CENTS, "unbooked".to_string(), None))
    );

    cleanup_member(&admin, member).await;
    assert_eq!(STATUS_ASSESSED, "assessed");
}

// ===========================================================================
// 5. A booked draw's funding is not re-derived in place
// ===========================================================================

/// **The booked-draw guard.** Once a draw has moved money, its amount cannot be
/// re-derived in place: the draw's reference is deterministic
/// (`dues:{year}:{member}`), so a *different* amount for the same member and year
/// would be answered by the transfer route with the transfer that already exists
/// — correctly — and the row would end up `booked` against a group whose amount is
/// the previous figure, leaving `funded_cents` (and the Annual Report's spend)
/// disagreeing with the ledger, silently.
///
/// The statement refuses that write and the route says why: a `409` naming the
/// group the money is under and the act that would settle it. Nothing is written —
/// the row keeps its assessment, its state and its group, and the ledger keeps the
/// one draw it had. Re-posting the *same* funding still passes, because the guard
/// is about the change and not about re-assessment.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_booked_draws_funding_is_not_re_derived_in_place() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_role(&admin).await;
    let ctx = context(&admin, &plugin).await;

    let member = "probe_dues_booked_guard";
    cleanup_member(&admin, member).await;
    fund_scholarship(&admin).await;

    // 1. A waiver at `standard`, booked in the same flow: the state the guard is
    // about.
    let booked = assess(&ctx, member, TIER_STANDARD, STATUS_WAIVED).await;
    let (assessed, funded, draw_status, draw_ref) = dues_row(&admin, member)
        .await
        .expect("the waived row landed");
    assert_eq!((assessed, funded), (BASE_CENTS, BASE_CENTS), "{booked}");
    assert_eq!(draw_status, "booked", "{booked}");
    let group = draw_ref.expect("a booked draw names its transfer group");

    // 2. The same member, the same year, a *lower* tier: the funding would change,
    // and that is refused — by name.
    let (status, refusal) = assess_status(&ctx, member, TIER_SUPPORTED, STATUS_WAIVED).await;
    assert_eq!(status, 409, "{refusal}");
    let reason = refusal["error"].as_str().unwrap_or_default();
    assert!(
        reason.contains("already booked"),
        "the refusal names the state the row is in: {refusal}"
    );
    assert!(
        reason.contains(&group),
        "and the group the money is under: {refusal}"
    );
    assert!(
        reason.contains("Move the difference"),
        "and the act that would settle it: {refusal}"
    );

    // 3. Nothing was written: the row is exactly as it was, and the ledger still
    // holds one draw under the one group.
    let after = dues_row(&admin, member)
        .await
        .expect("the row is still there");
    assert_eq!(
        after,
        (BASE_CENTS, BASE_CENTS, "booked".to_string(), Some(group.clone())),
        "a refused re-assessment does not touch the row"
    );
    let (entries, sum) = group_arithmetic(&admin, &group).await;
    assert_eq!(
        (entries, sum),
        (2, 0),
        "and does not move money: group {group}"
    );
    let groups: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM finance.transactions WHERE external_ref = $1",
    )
    .bind(dues_draw_reference(FISCAL_YEAR, member))
    .fetch_one(&admin)
    .await
    .expect("count the draws that carry the reference");
    assert_eq!(
        groups, 1,
        "one money move carries the draw's reference, however many times it is asked for"
    );

    // 4. Re-posting the **same** funding still passes: the guard is about the
    // change, and a retry of the same assessment keeps the booked draw booked.
    let (again_status, again) = assess_status(&ctx, member, TIER_STANDARD, STATUS_WAIVED).await;
    assert_eq!(again_status, 200, "{again}");
    assert_eq!(
        again["draw_status"],
        json!("booked"),
        "the booked draw stays booked: {again}"
    );
    assert_eq!(again["draw_ref"], json!(group), "{again}");

    cleanup_member(&admin, member).await;
    unfund_scholarship(&admin).await;
}
