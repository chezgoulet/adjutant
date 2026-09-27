//! # The finance plugin's receipts, against a real database
//!
//! `finance.receipts` is the one place this plugin's rules are **not** a
//! handler's decision: a receipt cannot be edited or deleted, a correction is a
//! new receipt that supersedes one for the same ledger entry, and a receipt
//! cannot exist for money that was never recorded. Those are triggers, a foreign
//! key and a unique index — so no mock host can prove them, and a gate on the
//! handler would keep passing if the migration lost them.
//!
//! These probes drive the plugin's **real route handlers** (`FinancePlugin::
//! routes()`) against PostgreSQL, with the plugin's own role, and then read
//! `finance.receipts` back. They assert on:
//!
//! * a receipt issued through `POST /api/finance/receipt` from an income entry —
//!   its number, its fund, its amount and its addressee snapshotted from the
//!   ledger, and the wording the giver reads claiming nothing about tax;
//! * an entry that is not money received (an expense) and an entry that does not
//!   exist, both refused with nothing written, and a receipt for a ledger entry
//!   that does not exist refused by the **database**, not by a handler;
//! * `UPDATE` and `DELETE` on a receipt refused **by the database**, for the
//!   plugin's own role and for the operator, with the row unchanged;
//! * a correction through `POST /api/finance/receipt/{id}/supersede`: a second
//!   row that names the first, both still there, a second correction of the same
//!   receipt refused, and a correction pointed at another entry's receipt refused
//!   by the trigger;
//! * the two **read** routes' ownership branch — "a giver sees their own and no
//!   one else's; the treasurer sees all" — against real `core.roles` /
//!   `core.role_permissions` rows, asserting the refusals (another member's
//!   receipt, and the whole troop, reach a member as nothing) as hard as the
//!   grants;
//! * what a correction carries over and what is its own: the superseded
//!   receipt's addressee and `purpose` are kept, while the wording is derived
//!   from the troop's **current** declaration and `issued_on` is the correction's
//!   own date — measured with the declaration moved between the issue and the
//!   correction, which is what tells "carried over" from "derived now".
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_receipts_test \
//!   cargo test -p adjutant-finance --test receipt_sql -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.audit_log` exists — the issue route audits every write) and the
//! `finance` plugin's own role created with its secret stored in
//! `core.plugins.db_secret`. The live ladder produces exactly that:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes connect **as the plugin's own role** — the connection whose
//! `session_user` is `adjutant_plugin_finance` — because that is who the
//! triggers see in production. Cleanup runs on a *dedicated* connection with
//! `session_replication_role = replica` rather than `ALTER TABLE … DISABLE
//! TRIGGER`: a panic between a disable and an enable would leave the
//! immutability trigger off and turn the probe that proves it into a lie.

use std::sync::Arc;

use adjutant_finance::{FinancePlugin, FUND_GENERAL, FUND_SCHOLARSHIP};
use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, Connection, PgPool, Row, ValueRef};

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_finance";
/// The caller every probe acts as. Not a UUID on purpose: the audit write then
/// attributes the actor in `details` instead of `core.audit_log.user_id`.
const CALLER: &str = "probe-finance-treasurer";
/// Every row this file writes carries it in `description`/`purpose`, so a probe
/// can clear and count exactly what it wrote and nothing else.
const PROBE_MARK: &str = "probe:finance-receipt-sql:";
/// A fixed date and year, so `fiscal_year` is stated rather than derived from the
/// clock — a probe that only passes on a particular day is a probe that breaks
/// later for no reason.
const OCCURRED_ON: &str = "2026-03-15";
/// The fiscal year `OCCURRED_ON` falls in — what a receipt number carries.
const FISCAL_YEAR: i64 = 2026;
/// A role the probe declares for its member caller: `finance:read`, and nothing
/// else. It is what makes "your own, and no one else's" a real refusal rather
/// than a claim — the member holds no `finance:read_all` row for the read
/// routes' authority branch to find.
const MEMBER_ROLE: &str = "probe_receipt_member";
/// A role the probe declares for the treasurer: `finance:read` **and**
/// `finance:read_all`, evaluated by the same `PermissionService` a deployment's
/// `core.role_permissions` rows feed.
const TREASURER_ROLE: &str = "probe_receipt_treasurer";

/// Serialises the probes: they assert on values the whole table shares (the
/// number sequence, and how many receipts a transaction has), so two running at
/// once would be reading each other's rows.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the finance receipt probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test \
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
            "[finance receipt probe] WARNING: running against {name:?}, which does not end in `_test`"
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

/// The core's own preconditions, stated rather than assumed. The issue route
/// audits its write, so `core.audit_log` has to exist — and the read routes'
/// authority branch is checked against real grant rows, so the vocabulary the
/// plugin declares must be registered for those grants to mean anything.
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
    let registered: Vec<String> =
        sqlx::query_scalar("SELECT id FROM core.permissions WHERE id = ANY($1)")
            .bind(vec!["finance:read".to_string(), "finance:read_all".to_string()])
            .fetch_all(admin)
            .await
            .expect("read the registered finance permissions");
    for permission in ["finance:read", "finance:read_all"] {
        assert!(
            registered.iter().any(|id| id == permission),
            "`{permission}` is not registered in core.permissions — run the live ladder \
             (`ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`) against \
             this database so the finance plugin's vocabulary is registered"
        );
    }
}

/// Declare the probe's two callers and their grants — real `core.roles` and
/// `core.role_permissions` rows, evaluated by `PermissionService` exactly as a
/// deployment's are.
///
/// The member holds `finance:read` and **not** `finance:read_all`: that absence
/// is the whole refusal. The treasurer holds both.
async fn ensure_probe_roles(admin: &PgPool) {
    for (role, display_name, permissions) in [
        (MEMBER_ROLE, "probe receipt member", &["finance:read"][..]),
        (
            TREASURER_ROLE,
            "probe receipt treasurer",
            &["finance:read", "finance:read_all"][..],
        ),
    ] {
        sqlx::query(
            "INSERT INTO core.roles (id, display_name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        )
        .bind(role)
        .bind(display_name)
        .execute(admin)
        .await
        .expect("declare the probe's role");
        for permission in permissions {
            sqlx::query(
                "INSERT INTO core.role_permissions (role_id, permission_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(role)
            .bind(*permission)
            .execute(admin)
            .await
            .expect("grant the probe's permission");
        }
    }
}

/// Remove the probe's roles. `core.role_permissions.role_id` is
/// `ON DELETE CASCADE`, so the grants go with them and the probe leaves nothing.
async fn clear_probe_roles(admin: &PgPool) {
    for role in [MEMBER_ROLE, TREASURER_ROLE] {
        let _ = sqlx::query("DELETE FROM core.roles WHERE id = $1")
            .bind(role)
            .execute(admin)
            .await;
    }
}

/// Apply this plugin's migrations, as the plugin role, in its own schema — the
/// same shape the runner uses (one transaction, `SET LOCAL search_path`).
///
/// A database the ladder prepared has them already, and every statement is
/// idempotent (the seed funds are `ON CONFLICT (code) DO NOTHING`, the receipt
/// triggers are `DROP … IF EXISTS` then `CREATE`), so this is a no-op there.
///
/// `CREATE SCHEMA` is deliberately **not** done as the plugin role: a plugin
/// role owns the database's schema but not the database, so the probe does what
/// the operator does and then migrates as the plugin.
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
    // The rules this file exists to prove, asserted as *installed* rather than
    // assumed: a migration that lost the trigger would otherwise make the probe
    // that proves it pass for the wrong reason.
    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT tgname FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'finance' AND c.relname = 'receipts' AND NOT t.tgisinternal \
         ORDER BY tgname",
    )
    .fetch_all(admin)
    .await
    .expect("read the receipts triggers");
    for expected in ["receipts_correction_same_money", "receipts_immutable"] {
        assert!(
            triggers.iter().any(|name| name == expected),
            "finance.receipts has no {expected} trigger (found {triggers:?}) — migration 3 must be \
             applied, and these are the rules, not a convention"
        );
    }
}

// ---------------------------------------------------------------------------
// The rows a probe writes, and reads back
// ---------------------------------------------------------------------------

/// One receipt, as this file cares about it.
#[derive(Debug)]
struct Receipt {
    id: i64,
    number: String,
    transaction_id: i64,
    fund_id: i64,
    amount_cents: i64,
    member_id: String,
    payer_name: String,
    tax_statement: String,
    /// The document's own date, read as text so the probe compares it against a
    /// stated `YYYY-MM-DD` without depending on a date type's decoding.
    issued_on: String,
    supersedes_id: Option<i64>,
    superseded_by: Option<i64>,
}

/// Clear anything a previous run left behind. Called **before** a probe writes
/// and again after, never between a write and the assertion on it.
///
/// The probe's rows are receipts and the ledger entries they are issued from —
/// both, in that order.
async fn cleanup(url: &str, mark: &str) {
    force_cleanup(url, "finance.receipts", "purpose", mark).await;
    force_cleanup(url, "finance.transactions", "description", mark).await;
}

/// A `DELETE` past the triggers, on a **dedicated** connection.
///
/// `session_replication_role = replica` is superuser-only and lasts only as long
/// as the connection, so this cannot leave the immutability trigger off the way
/// an `ALTER TABLE … DISABLE TRIGGER` abandoned by a panic would. The marker is
/// bound, never interpolated.
async fn force_cleanup(url: &str, table: &str, column: &str, mark: &str) {
    // `table` and `column` are literals in this file, never request values.
    let mut conn = sqlx::PgConnection::connect(url)
        .await
        .unwrap_or_else(|e| {
            panic!("cleanup could not reach the test database (run the ladder first): {e}")
        });
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut conn)
        .await
        .expect("replica mode for cleanup");
    sqlx::query(&format!("DELETE FROM {table} WHERE {column} LIKE $1"))
        .bind(format!("{PROBE_MARK}{mark}%"))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("clear {table} for {mark:?}: {e}"));
    sqlx::query("SET session_replication_role = DEFAULT")
        .execute(&mut conn)
        .await
        .expect("restore the trigger mode");
    let _ = conn.close().await;
}

/// Every receipt a probe wrote, in id order — with `superseded_by` derived
/// exactly as the read routes derive it.
async fn receipts_written(admin: &PgPool, mark: &str) -> Vec<Receipt> {
    let rows = sqlx::query(
        "SELECT r.id, r.number, r.transaction_id, r.fund_id, r.amount_cents, r.member_id, \
                r.payer_name, r.tax_statement, r.issued_on::text AS issued_on, r.supersedes_id, \
                (SELECT c.id FROM finance.receipts c WHERE c.supersedes_id = r.id LIMIT 1) \
                    AS superseded_by \
         FROM finance.receipts r WHERE r.purpose LIKE $1 ORDER BY r.id",
    )
    .bind(format!("{PROBE_MARK}{mark}%"))
    .fetch_all(admin)
    .await
    .expect("read the receipts the probe wrote");
    rows.iter()
        .map(|row| Receipt {
            id: row.get("id"),
            number: row.get("number"),
            transaction_id: row.get("transaction_id"),
            fund_id: row.get("fund_id"),
            amount_cents: row.get("amount_cents"),
            member_id: row.get("member_id"),
            payer_name: row.get("payer_name"),
            tax_statement: row.get("tax_statement"),
            issued_on: row.get("issued_on"),
            supersedes_id: row.get("supersedes_id"),
            superseded_by: row.get("superseded_by"),
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

/// The id of the ledger entry a probe wrote, read back from its marker.
async fn probe_transaction_id(admin: &PgPool, mark: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT id FROM finance.transactions WHERE description LIKE $1 AND kind = 'income' \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(format!("{PROBE_MARK}{mark}%"))
    .fetch_one(admin)
    .await
    .unwrap_or_else(|e| panic!("the probe's ledger entry for {mark:?}: {e}"))
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

/// Neither route here calls another plugin: issuing a receipt writes one row
/// from the plugin's own schema. This handle refuses loudly rather than quietly
/// passing.
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
            "the finance receipt probes write one row each; nothing should be called \
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

/// The plugin's `core.plugins.config` block, as a boot hands it over.
///
/// **No `receipt_tax_statement`**: the troop in these probes has declared
/// nothing, which is precisely the case that must print no tax claim.
fn config_json() -> Value {
    config_declaring(None)
}

/// The same block, with the troop declaring a tax-status wording — or not.
///
/// The carry-over probe needs both: whether a correction's wording comes from the
/// receipt it supersedes or from the troop's declaration **now** is only
/// distinguishable when the declaration has moved between the two writes.
fn config_declaring(statement: Option<&str>) -> Value {
    let mut config = json!({
        "fiscal_year_start_month": 1,
        "membership_cost_cents": 25_000,
        "dues_fund_code": FUND_GENERAL,
    });
    if let Some(statement) = statement {
        config["receipt_tax_statement"] = json!(statement);
    }
    config
}

/// A real `PluginContext`, with the **two** database hosts the core hands a
/// plugin (`plugin_runtime::build_context`): `ctx.db` is the plugin's own role,
/// while the audit service runs on the **core's** pool — the plugin role holds
/// no grant on `core.audit_log`, and a probe that ran everything on one pool
/// would be testing a shape the core never builds.
fn context(admin: &PgPool, plugin: &PgPool) -> PluginContext {
    context_with(admin, plugin, config_json())
}

/// The same context against a stated config — what a probe that moves the troop's
/// declaration between two writes needs (see [`config_declaring`]).
fn context_with(admin: &PgPool, plugin: &PgPool, config: Value) -> PluginContext {
    let plugin_db: Arc<dyn HostDb> = Arc::new(PgDb(plugin.clone()));
    let core_db: Arc<dyn HostDb> = Arc::new(PgDb(admin.clone()));
    PluginContext {
        plugin_id: "finance".to_string(),
        db: DbHandle::new(plugin_db, "finance".to_string()),
        config,
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
    params: &[(&str, String)],
    body: &Value,
) -> Result<PluginResponse, SdkError> {
    let route = routes
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| panic!("the plugin serves no route {path:?}"));
    let mut request = TestRequest::post(path).identity(CALLER, &["treasurer"]);
    for (key, value) in params {
        request = request.param(key, value);
    }
    (route.handler)(request.json(body).build()).await
}

/// One request that must be **accepted**: a 2xx, decoded as JSON.
async fn accepted(
    routes: &[RouteDefinition],
    path: &str,
    params: &[(&str, String)],
    body: &Value,
) -> Value {
    match call(routes, path, params, body).await {
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

async fn refused(
    routes: &[RouteDefinition],
    path: &str,
    params: &[(&str, String)],
    body: &Value,
) -> Refusal {
    match call(routes, path, params, body).await {
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

/// One **GET** through a real read handler, as an explicit caller holding a real
/// troop-scoped grant — the identity shape the authority branch is checked
/// against. The route is called out of the plugin's own list, so an edit to the
/// branch is what is exercised, not a copy of it.
async fn read(
    routes: &[RouteDefinition],
    path: &str,
    caller: &str,
    role: &str,
    params: &[(&str, String)],
    query: &[(&str, String)],
) -> Result<PluginResponse, SdkError> {
    let route = routes
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| panic!("the plugin serves no route {path:?}"));
    let mut request = TestRequest::get(path).identity_grants(
        caller,
        vec![RoleGrant {
            role_id: role.into(),
            scope: Scope::troop(),
        }],
    );
    for (key, value) in params {
        request = request.param(key, value);
    }
    for (key, value) in query {
        request = request.query_param(key, value);
    }
    (route.handler)(request.build()).await
}

/// A read that must be **answered**: a 2xx, decoded as JSON.
async fn read_ok(
    routes: &[RouteDefinition],
    path: &str,
    caller: &str,
    role: &str,
    params: &[(&str, String)],
    query: &[(&str, String)],
) -> Value {
    match read(routes, path, caller, role, params, query).await {
        Ok(response) if (200..300).contains(&response.status) => response_json(&response),
        Ok(response) => panic!(
            "{path} answered {} as {caller} — expected it to be accepted: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => panic!("{path} returned an error as {caller} — expected it to be accepted: {e}"),
    }
}

/// A read that must be **refused**, with its status and its own reason — a
/// guarded read returns `SdkError::Forbidden` (a 403), so the permission the
/// caller lacked is named rather than a generic failure.
async fn read_refused(
    routes: &[RouteDefinition],
    path: &str,
    caller: &str,
    role: &str,
    params: &[(&str, String)],
    query: &[(&str, String)],
) -> Refusal {
    match read(routes, path, caller, role, params, query).await {
        Ok(response) if response.status >= 400 => Refusal {
            status: response.status,
            reason: response_json(&response)["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&response.body).to_string()),
        },
        Ok(response) => panic!(
            "{path} answered {} as {caller} — expected a refusal: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => Refusal {
            status: e.status(),
            reason: e.to_string(),
        },
    }
}

/// An income entry and the receipt issued from it, through the real routes — the
/// money first, then the receipt — with the marker that makes both this probe's.
///
/// `payer_name` addresses a giver outside the troop (no roster identity), which
/// is the case a member can never reach by identity.
async fn issue_receipt(
    routes: &[RouteDefinition],
    admin: &PgPool,
    mark: &str,
    member_id: &str,
    payer_name: Option<&str>,
) -> Receipt {
    accepted(
        routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(mark, FUND_GENERAL, 1_000, member_id),
    )
    .await;
    let transaction_id = probe_transaction_id(admin, mark).await;
    accepted(
        routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(mark, transaction_id, payer_name),
    )
    .await;
    let rows = receipts_written(admin, mark).await;
    assert_eq!(rows.len(), 1, "one receipt for {mark}: {rows:?}");
    rows.into_iter().next().expect("the receipt")
}

/// An income entry, with the marker that makes it this probe's row, recorded
/// through the plugin's own route.
///
/// `member_id` is stated rather than assumed: the money an outside giver leaves
/// (a parent, a business) carries no roster identity, which is what makes the
/// receipt's addressee the name they gave.
fn transaction_body(
    mark: &str,
    fund_code: &str,
    amount_cents: i64,
    member_id: &str,
) -> Value {
    json!({
        "kind": "income",
        "fund_code": fund_code,
        "amount_cents": amount_cents,
        "category": "donation",
        "member_id": member_id,
        "description": format!("{PROBE_MARK}{mark}"),
        "occurred_on": OCCURRED_ON,
        "fiscal_year": FISCAL_YEAR,
    })
}

/// A receipt body for a ledger entry, with the marker that makes its purpose
/// this probe's. No `tax_statement`: the troop has declared nothing.
fn receipt_body(mark: &str, transaction_id: i64, payer_name: Option<&str>) -> Value {
    let mut body = json!({
        "transaction_id": transaction_id,
        "purpose": format!("{PROBE_MARK}{mark}"),
        "issued_on": OCCURRED_ON,
    });
    if let Some(name) = payer_name {
        body["payer_name"] = json!(name);
    }
    body
}

// ===========================================================================
// 1. A receipt is issued from the ledger record, numbered from one authority
// ===========================================================================

/// **The issue probe.** A receipt comes from an income entry: the fund, the
/// amount, the fiscal year and the addressee are snapshotted from the ledger
/// inside the issuing statement, the number carries the year and is unique, and
/// the wording the giver reads claims nothing about tax. An expense, and an
/// entry that does not exist, are refused and write nothing — and the database
/// refuses a receipt for money that was never recorded, whatever a handler does.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_receipt_is_issued_from_the_ledger_record_with_a_number_and_a_name() {
    let _guard = DB.lock().await;
    let url = test_database_url();
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;
    let general = fund_id(&admin, FUND_GENERAL).await;

    let mark = "issue";
    let expense = "issue-expense";
    let missing = "issue-missing";
    for key in [mark, expense, missing] {
        cleanup(&url, key).await;
    }

    // --- the money, through the ledger route
    let entry = accepted(
        &routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(mark, FUND_GENERAL, 5_000, "probe-member"),
    )
    .await;
    let transaction_id = entry["transaction"]["id"]
        .as_i64()
        .expect("the entry names its own row");
    assert_eq!(entry["transaction"]["fund_id"], general, "{entry}");

    // --- the receipt, from that row
    let issued = accepted(
        &routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(mark, transaction_id, None),
    )
    .await;
    let number = issued["number"].as_str().unwrap_or_default().to_string();
    assert!(
        number.starts_with(&format!("R-{FISCAL_YEAR}-")) && number.len() == 13,
        "a receipt number carries its fiscal year and the sequence: {issued}"
    );
    assert_eq!(issued["receipt"]["transaction_id"], transaction_id, "{issued}");
    assert_eq!(
        issued["receipt"]["fund_id"], general,
        "the fund is the ledger entry's, not the caller's: {issued}"
    );
    assert_eq!(
        issued["receipt"]["amount_cents"], 5_000,
        "the amount is the ledger entry's, not the caller's: {issued}"
    );
    assert_eq!(
        issued["issued_to"], "probe-member",
        "a member is addressed by their roster identity, which the entry carried: {issued}"
    );
    assert_eq!(issued["member_id"], "probe-member", "{issued}");
    assert_eq!(issued["payer_name"], "", "{issued}");
    assert_eq!(
        issued["tax_statement_declared"], false,
        "the troop declared no status, so no statement: {issued}"
    );
    assert_eq!(issued["superseded_by"], Value::Null, "{issued}");
    let wording = issued["wording"].as_str().unwrap_or_default();
    assert!(wording.contains(&number), "{wording}");
    assert!(wording.contains("$50.00"), "{wording}");
    assert!(
        !wording.to_lowercase().contains("tax"),
        "the wording a giver reads must make no tax claim by default: {wording}"
    );

    // --- the row itself agrees
    let rows = receipts_written(&admin, mark).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let receipt = &rows[0];
    assert_eq!(receipt.number, number);
    assert_eq!(receipt.transaction_id, transaction_id);
    assert_eq!(
        receipt.fund_id, general,
        "the row lands in the ledger entry's own fund: {receipt:?}"
    );
    assert_eq!(receipt.amount_cents, 5_000);
    assert_eq!(receipt.member_id, "probe-member");
    assert!(receipt.payer_name.is_empty());
    assert!(receipt.tax_statement.is_empty());
    assert!(receipt.supersedes_id.is_none() && receipt.superseded_by.is_none());

    // --- an expense is not money received: refused, and nothing written
    let spend = accepted(
        &routes,
        "/api/finance/transaction",
        &[],
        &json!({
            "kind": "expense",
            "fund_code": FUND_GENERAL,
            "amount_cents": 100,
            "description": format!("{PROBE_MARK}{expense}"),
            "occurred_on": OCCURRED_ON,
            "fiscal_year": FISCAL_YEAR,
        }),
    )
    .await;
    let expense_id = spend["transaction"]["id"].as_i64().unwrap_or_default();
    let refusal = refused(
        &routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(expense, expense_id, None),
    )
    .await;
    assert_eq!(refusal.status, 409, "{refusal:?}");
    assert!(
        refusal.reason.contains("money the troop received"),
        "the refusal is the rule's own words: {refusal:?}"
    );
    assert_eq!(receipts_written(&admin, expense).await.len(), 0);

    // --- an entry that does not exist: refused, and nothing written
    let refusal = refused(
        &routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(missing, 999_999_999, Some("Nobody")),
    )
    .await;
    assert_eq!(refusal.status, 404, "{refusal:?}");
    assert!(
        refusal.reason.contains("no such ledger entry"),
        "{refusal:?}"
    );
    assert_eq!(receipts_written(&admin, missing).await.len(), 0);

    // --- and the database refuses a receipt for money that was never recorded,
    // whoever is writing it: the foreign key is the rule, not a handler.
    let forged = sqlx::query(
        "INSERT INTO finance.receipts \
           (number, fiscal_year, transaction_id, fund_id, amount_cents, issued_on, member_id, \
            purpose) \
         VALUES ('R-0000-000000', $1, 999999999, $2, 100, $3::date, 'probe-member', $4)",
    )
    .bind(FISCAL_YEAR as i32)
    .bind(general)
    .bind(OCCURRED_ON)
    .bind(format!("{PROBE_MARK}forged"))
    .execute(&admin)
    .await;
    assert!(
        forged.is_err(),
        "a receipt for a ledger entry that does not exist must be refused by the database"
    );

    for key in [mark, expense, missing] {
        cleanup(&url, key).await;
    }
}

// ===========================================================================
// 2. A receipt cannot be updated or deleted — by anyone
// ===========================================================================

/// **The immutability probe.** The trigger refuses `UPDATE` and `DELETE`
/// unconditionally — it is not a list of protected columns a later migration has
/// to remember to extend — so a correction has to be a new row. Both the plugin's
/// own role and the operator are refused, and the row is byte-for-byte what it
/// was.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_receipt_cannot_be_updated_or_deleted() {
    let _guard = DB.lock().await;
    let url = test_database_url();
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;

    let mark = "immutable";
    cleanup(&url, mark).await;
    accepted(
        &routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(mark, FUND_GENERAL, 1_000, "probe-member"),
    )
    .await;
    let transaction_id = probe_transaction_id(&admin, mark).await;
    let issued = accepted(
        &routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(mark, transaction_id, Some("Jnae Doe")),
    )
    .await;
    let id = issued["receipt"]["id"].as_i64().expect("the receipt's id");
    let before = receipts_written(&admin, mark).await;
    assert_eq!(before.len(), 1, "{before:?}");

    // The plugin's own role — the connection the plugin's statements run on.
    let update = sqlx::query("UPDATE finance.receipts SET amount_cents = 1 WHERE id = $1")
        .bind(id)
        .execute(&plugin)
        .await;
    let update_error = update.expect_err("the plugin's own role must not be able to edit a receipt");
    assert!(
        update_error.to_string().contains("cannot be modified"),
        "the refusal says why: {update_error}"
    );
    let delete = sqlx::query("DELETE FROM finance.receipts WHERE id = $1")
        .bind(id)
        .execute(&plugin)
        .await;
    let delete_error = delete.expect_err("the plugin's own role must not be able to delete a receipt");
    assert!(
        delete_error.to_string().contains("cannot be deleted"),
        "the refusal says why: {delete_error}"
    );

    // The operator: this is not a privilege question, it is the record.
    let update = sqlx::query("UPDATE finance.receipts SET payer_name = 'Someone Else' WHERE id = $1")
        .bind(id)
        .execute(&admin)
        .await;
    assert!(
        update.is_err(),
        "an operator may not edit a receipt either — that is what immutability means"
    );
    let delete = sqlx::query("DELETE FROM finance.receipts WHERE id = $1")
        .bind(id)
        .execute(&admin)
        .await;
    assert!(delete.is_err(), "an operator may not delete a receipt either");

    let after = receipts_written(&admin, mark).await;
    assert_eq!(after.len(), 1, "the receipt is still there: {after:?}");
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].number, before[0].number);
    assert_eq!(after[0].amount_cents, before[0].amount_cents);
    assert_eq!(after[0].payer_name, before[0].payer_name);

    cleanup(&url, mark).await;
}

// ===========================================================================
// 3. A correction supersedes, and both receipts remain
// ===========================================================================

/// **The correction probe.** A correction is a new receipt naming the one it
/// supersedes; the superseded receipt is untouched and still readable, so what
/// the giver was first told is on the record beside what they were told next.
/// One correction per receipt (the unique index), and a correction may only
/// supersede a receipt for the *same* ledger entry (the trigger) — both refused
/// by the database.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_correction_supersedes_and_both_receipts_remain() {
    let _guard = DB.lock().await;
    let url = test_database_url();
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;

    let mark = "correction";
    let other = "correction-other";
    cleanup(&url, mark).await;
    cleanup(&url, other).await;

    accepted(
        &routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(mark, FUND_GENERAL, 2_500, ""),
    )
    .await;
    let transaction_id = probe_transaction_id(&admin, mark).await;
    let issued = accepted(
        &routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(mark, transaction_id, Some("Jnae Doe")),
    )
    .await;
    let original_id = issued["receipt"]["id"].as_i64().expect("the receipt's id");
    let original_number = issued["number"].as_str().unwrap_or_default().to_string();
    assert_eq!(issued["issued_to"], "Jnae Doe", "{issued}");

    // --- the correction: a new receipt, with the name as it should have been
    let corrected = accepted(
        &routes,
        "/api/finance/receipt/{id}/supersede",
        &[("id", original_id.to_string())],
        &json!({
            "reason": "the giver's name was misspelt",
            "payer_name": "Jane Doe",
        }),
    )
    .await;
    let correction_id = corrected["receipt"]["id"]
        .as_i64()
        .expect("the correction's id");
    assert_ne!(correction_id, original_id, "{corrected}");
    assert_eq!(
        corrected["supersedes_id"], original_id,
        "a correction names the receipt it supersedes: {corrected}"
    );
    assert_eq!(corrected["issued_to"], "Jane Doe", "{corrected}");
    assert_ne!(corrected["number"], original_number, "{corrected}");

    // --- both remain, and the chain reads backwards from the correction
    let rows = receipts_written(&admin, mark).await;
    assert_eq!(
        rows.len(),
        2,
        "a correction does not replace the receipt it corrects: {rows:?}"
    );
    let original = rows
        .iter()
        .find(|row| row.id == original_id)
        .expect("the superseded receipt is still there");
    let correction = rows
        .iter()
        .find(|row| row.id == correction_id)
        .expect("the correction is there");
    assert_eq!(original.number, original_number);
    assert_eq!(
        original.payer_name, "Jnae Doe",
        "the superseded receipt reads exactly as it was issued: {original:?}"
    );
    assert!(original.supersedes_id.is_none());
    assert_eq!(
        original.superseded_by,
        Some(correction_id),
        "the successor is derived, not written onto the receipt: {original:?}"
    );
    assert_eq!(correction.supersedes_id, Some(original_id));
    assert!(correction.superseded_by.is_none());
    assert_eq!(correction.amount_cents, 2_500, "{correction:?}");

    // --- correcting the already-corrected receipt is refused, and writes nothing
    let refusal = refused(
        &routes,
        "/api/finance/receipt/{id}/supersede",
        &[("id", original_id.to_string())],
        &json!({ "reason": "a second bite" }),
    )
    .await;
    assert_eq!(refusal.status, 409, "{refusal:?}");
    assert!(
        refusal.reason.contains("already superseded"),
        "{refusal:?}"
    );
    assert_eq!(
        receipts_written(&admin, mark).await.len(),
        2,
        "a refused correction writes no third receipt"
    );

    // --- the database refuses a second correction of the same receipt too
    let second = sqlx::query(
        "INSERT INTO finance.receipts \
           (number, fiscal_year, transaction_id, fund_id, amount_cents, issued_on, payer_name, \
            purpose, supersedes_id) \
         VALUES ('R-0000-000001', $1, $2, (SELECT fund_id FROM finance.transactions WHERE id = $2), \
                 2500, $3::date, 'Jane Doe', $4, $5)",
    )
    .bind(FISCAL_YEAR as i32)
    .bind(transaction_id)
    .bind(OCCURRED_ON)
    .bind(format!("{PROBE_MARK}correction-second"))
    .bind(original_id)
    .execute(&admin)
    .await;
    assert!(
        second.is_err(),
        "one correction per receipt: a second is unrepresentable, not merely discouraged"
    );

    // --- and a correction may not supersede a receipt for different money
    accepted(
        &routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(other, FUND_SCHOLARSHIP, 1_000, ""),
    )
    .await;
    let other_transaction = probe_transaction_id(&admin, other).await;
    let stranger = sqlx::query(
        "INSERT INTO finance.receipts \
           (number, fiscal_year, transaction_id, fund_id, amount_cents, issued_on, payer_name, \
            purpose, supersedes_id) \
         VALUES ('R-0000-000002', $1, $2, (SELECT fund_id FROM finance.transactions WHERE id = $2), \
                 1000, $3::date, 'Jane Doe', $4, $5)",
    )
    .bind(FISCAL_YEAR as i32)
    .bind(other_transaction)
    .bind(OCCURRED_ON)
    .bind(format!("{PROBE_MARK}correction-stranger"))
    .bind(original_id)
    .execute(&admin)
    .await;
    let stranger_error = stranger.expect_err(
        "a correction supersedes a receipt for the same ledger entry — the trigger says so",
    );
    assert!(
        stranger_error.to_string().contains("same money"),
        "the refusal says why: {stranger_error}"
    );

    cleanup(&url, mark).await;
    cleanup(&url, other).await;
}

// ===========================================================================
// 4. The read routes' ownership branch, against real core.roles rows
// ===========================================================================

/// **The ownership probe.** "A giver sees their own and no one else's; the
/// treasurer sees all" is a *handler* decision, made by
/// `PermissionService.reach(identity, "finance:read_all", &Scope::troop())`,
/// which reads `core.roles` / `core.role_permissions`. The mock-host test
/// (`finance.rs::a_giver_reads_their_own_receipts_and_the_treasurer_reads_all`)
/// answers that query from a scripted row; this one answers it from real grant
/// rows the probe declares, and drives the same two read routes.
///
/// It asserts the refusals as hard as the grants: a member asking for another
/// member's receipts — by id or by `member_id` — and for the whole troop gets
/// nothing, and a receipt addressed to a giver outside the troop (by
/// `payer_name`) is only ever the treasurer's to read.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn the_read_routes_honour_the_receipt_ownership_rule_against_real_roles() {
    let _guard = DB.lock().await;
    let url = test_database_url();
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    ensure_probe_roles(&admin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;

    let list = "/api/finance/receipts";
    let one = "/api/finance/receipt/{id}";

    let bea_mark = "access-bea";
    let carl_mark = "access-carl";
    let outside_mark = "access-outside";
    let marks = [bea_mark, carl_mark, outside_mark];
    for mark in marks {
        cleanup(&url, mark).await;
    }

    // --- three receipts, issued through the ledger and receipt routes: one for
    // each of two members, and one for a giver outside the troop with no account.
    let bea = issue_receipt(&routes, &admin, bea_mark, "probe-receipt-bea", None).await;
    let carl = issue_receipt(&routes, &admin, carl_mark, "probe-receipt-carl", None).await;
    let outside = issue_receipt(&routes, &admin, outside_mark, "", Some("Jane Doe")).await;
    assert_eq!(bea.member_id, "probe-receipt-bea", "{bea:?}");
    assert_eq!(carl.member_id, "probe-receipt-carl", "{carl:?}");
    assert!(
        outside.member_id.is_empty() && outside.payer_name == "Jane Doe",
        "{outside:?}"
    );

    // --- the list: a member naming themselves is an ownership check, not a grant,
    // so it is answered — and the page holds only their own receipt.
    let page = read_ok(
        &routes,
        list,
        "probe-receipt-bea",
        MEMBER_ROLE,
        &[],
        &[("member_id", "probe-receipt-bea".to_string())],
    )
    .await;
    assert_eq!(page["count"], json!(1), "the giver sees their own: {page}");
    assert_eq!(page["receipts"][0]["number"], json!(bea.number), "{page}");
    assert_eq!(
        page["receipts"][0]["issued_to"], json!("probe-receipt-bea"),
        "{page}"
    );

    // --- a member asking for someone else's receipts is refused, and so is the
    // whole troop's list: both need `finance:read_all`, which the member lacks.
    let refusal = read_refused(
        &routes,
        list,
        "probe-receipt-bea",
        MEMBER_ROLE,
        &[],
        &[("member_id", "probe-receipt-carl".to_string())],
    )
    .await;
    assert_eq!(refusal.status, 403, "another member's receipts: {refusal:?}");
    assert!(refusal.reason.contains("finance:read_all"), "{refusal:?}");

    let refusal = read_refused(&routes, list, "probe-receipt-bea", MEMBER_ROLE, &[], &[]).await;
    assert_eq!(refusal.status, 403, "the whole troop's receipts: {refusal:?}");
    assert!(refusal.reason.contains("finance:read_all"), "{refusal:?}");

    // --- the treasurer holds `finance:read_all`, so the whole troop's list is
    // answered and the page carries every giver's receipt — the members' and the
    // outsider's.
    let page = read_ok(
        &routes,
        list,
        "probe-receipt-treasurer",
        TREASURER_ROLE,
        &[],
        &[],
    )
    .await;
    let numbers: Vec<&str> = page["receipts"]
        .as_array()
        .expect("a page of receipts")
        .iter()
        .filter_map(|row| row["number"].as_str())
        .collect();
    for receipt in [&bea, &carl, &outside] {
        assert!(
            numbers.contains(&receipt.number.as_str()),
            "the treasurer sees every giver's receipt, including {}: {page}",
            receipt.number
        );
    }
    let page = read_ok(
        &routes,
        list,
        "probe-receipt-treasurer",
        TREASURER_ROLE,
        &[],
        &[("member_id", "probe-receipt-carl".to_string())],
    )
    .await;
    assert_eq!(page["receipts"][0]["number"], json!(carl.number), "{page}");

    // --- one receipt: a giver reads their own...
    let body = read_ok(
        &routes,
        one,
        "probe-receipt-bea",
        MEMBER_ROLE,
        &[("id", bea.id.to_string())],
        &[],
    )
    .await;
    assert_eq!(body["issued_to"], json!("probe-receipt-bea"), "{body}");
    assert_eq!(body["number"], json!(bea.number), "{body}");

    // --- ...and no one else's without the grant: another member's by id...
    let refusal = read_refused(
        &routes,
        one,
        "probe-receipt-bea",
        MEMBER_ROLE,
        &[("id", carl.id.to_string())],
        &[],
    )
    .await;
    assert_eq!(refusal.status, 403, "another member's receipt: {refusal:?}");
    assert!(refusal.reason.contains("finance:read_all"), "{refusal:?}");

    // --- ...the treasurer reads another member's by id...
    let body = read_ok(
        &routes,
        one,
        "probe-receipt-treasurer",
        TREASURER_ROLE,
        &[("id", carl.id.to_string())],
        &[],
    )
    .await;
    assert_eq!(body["issued_to"], json!("probe-receipt-carl"), "{body}");

    // --- ...and a receipt addressed to a giver outside the troop has no account to
    // match, so it is the treasurer's and never a member's by identity.
    let refusal = read_refused(
        &routes,
        one,
        "probe-receipt-bea",
        MEMBER_ROLE,
        &[("id", outside.id.to_string())],
        &[],
    )
    .await;
    assert_eq!(refusal.status, 403, "an outsider's receipt: {refusal:?}");
    assert!(refusal.reason.contains("finance:read_all"), "{refusal:?}");

    let body = read_ok(
        &routes,
        one,
        "probe-receipt-treasurer",
        TREASURER_ROLE,
        &[("id", outside.id.to_string())],
        &[],
    )
    .await;
    assert_eq!(body["issued_to"], json!("Jane Doe"), "{body}");

    for mark in marks {
        cleanup(&url, mark).await;
    }
    clear_probe_roles(&admin).await;
}

// ===========================================================================
// 5. What a correction carries over, and what is this document's own
// ===========================================================================

/// **The carry-over probe.** What a correction carries over from the receipt it
/// supersedes is its **identity** — the addressee (`member_id`, `payer_name`) and
/// the `purpose` — and this probe measures both halves of that against the real
/// handlers and the rows they leave:
///
/// * the `purpose` and the addressee an un-restated correction keeps are the
///   superseded receipt's, and a restated name is the one-field typo fix the
///   route's own comment promises;
/// * the wording is **not** carried over: `tax_statement` is derived from the
///   troop's declaration **now**, exactly as issuing derives it — a receipt
///   issued while the troop declared a wording and corrected after it withdrew
///   it carries no statement, and one issued with no wording and corrected after
///   the troop declared one carries the new wording. That is what makes "carried
///   over from the row" and "derived from the current declaration"
///   distinguishable: one config could not tell them apart;
/// * `issued_on` is the correction's own date, not the superseded receipt's;
/// * the superseded row still reads exactly as it was issued — the wording the
///   giver first read, and the date it was issued on — so nothing a correction
///   does rewrites what a giver was told.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn a_correction_carries_the_addressee_and_the_purpose_but_states_its_own_wording() {
    let _guard = DB.lock().await;
    let url = test_database_url();
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;

    const DECLARED: &str = "nothing in this receipt implies a tax deduction";
    const MEMBER: &str = "probe-carry-member";
    let withdrawn = "carry-over";
    let newly_declared = "carry-declared";
    cleanup(&url, withdrawn).await;
    cleanup(&url, newly_declared).await;
    let today = Utc::now().date_naive().to_string();

    // Two route sets over one database, differing only in what the troop
    // declares: the receipt is issued through one and corrected through the other.
    let declaring = context_with(&admin, &plugin, config_declaring(Some(DECLARED)));
    let declaring_routes = routes_of(&declaring).await;
    let quiet = context_with(&admin, &plugin, config_declaring(None));
    let quiet_routes = routes_of(&quiet).await;

    // --- 1. declared when the receipt was issued, withdrawn when it is
    //        corrected. The giver is a member, so the receipt is addressed by
    //        their roster identity and the correction restates nothing.
    accepted(
        &declaring_routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(withdrawn, FUND_GENERAL, 1_500, MEMBER),
    )
    .await;
    let transaction_id = probe_transaction_id(&admin, withdrawn).await;
    let issued = accepted(
        &declaring_routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(withdrawn, transaction_id, None),
    )
    .await;
    let original_id = issued["receipt"]["id"].as_i64().expect("the receipt's id");
    assert_eq!(
        issued["tax_statement"], DECLARED,
        "the receipt states what the troop declared when it was issued: {issued}"
    );
    assert_eq!(issued["issued_to"], MEMBER, "{issued}");
    assert_eq!(
        issued["issued_on"], OCCURRED_ON,
        "the probe states the date it issued the receipt on: {issued}"
    );

    let corrected = accepted(
        &quiet_routes,
        "/api/finance/receipt/{id}/supersede",
        &[("id", original_id.to_string())],
        &json!({ "reason": "the ledger entry was described wrongly" }),
    )
    .await;
    let correction_id = corrected["receipt"]["id"]
        .as_i64()
        .expect("the correction's id");
    assert_eq!(
        corrected["tax_statement"], "",
        "a correction is issued now, so it states the troop's current declaration — nothing — \
         rather than carrying over a wording the troop has withdrawn: {corrected}"
    );
    assert_eq!(
        corrected["tax_statement_declared"], false,
        "and the view says so, so the sentence the giver reads claims nothing: {corrected}"
    );
    assert_eq!(
        corrected["purpose"],
        format!("{PROBE_MARK}{withdrawn}"),
        "the purpose is carried over: the correction restated none: {corrected}"
    );
    assert_eq!(
        corrected["member_id"], MEMBER,
        "the addressee is carried over, not re-derived: {corrected}"
    );
    assert_eq!(
        corrected["issued_on"], today,
        "the correction is a document issued now, so its date is its own (today, UTC) and not \
         the superseded receipt's ({OCCURRED_ON}): {corrected}"
    );

    // --- both rows stay, each as it was written
    let rows = receipts_written(&admin, withdrawn).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let original = rows
        .iter()
        .find(|row| row.id == original_id)
        .expect("the superseded receipt is still there");
    assert_eq!(
        original.tax_statement, DECLARED,
        "the wording the giver first read is untouched: {original:?}"
    );
    assert_eq!(
        original.issued_on, OCCURRED_ON,
        "so is the date it was issued on: {original:?}"
    );
    assert_eq!(original.superseded_by, Some(correction_id), "{original:?}");
    let correction = rows
        .iter()
        .find(|row| row.id == correction_id)
        .expect("the correction is there");
    assert!(correction.tax_statement.is_empty(), "{correction:?}");
    assert_eq!(correction.issued_on, today, "{correction:?}");

    // --- 2. the other direction: nothing declared when issued, declared when
    //        corrected — so the wording is the declaration in force now. The
    //        giver is not a member this time, and the correction exists to fix
    //        the name they gave: the one-field correction.
    accepted(
        &quiet_routes,
        "/api/finance/transaction",
        &[],
        &transaction_body(newly_declared, FUND_GENERAL, 900, ""),
    )
    .await;
    let transaction_id = probe_transaction_id(&admin, newly_declared).await;
    let issued = accepted(
        &quiet_routes,
        "/api/finance/receipt",
        &[],
        &receipt_body(newly_declared, transaction_id, Some("Jnae Doe")),
    )
    .await;
    let original_id = issued["receipt"]["id"].as_i64().expect("the receipt's id");
    assert_eq!(
        issued["tax_statement"], "",
        "no declaration, no sentence: {issued}"
    );
    assert_eq!(issued["issued_to"], "Jnae Doe", "{issued}");

    let corrected = accepted(
        &declaring_routes,
        "/api/finance/receipt/{id}/supersede",
        &[("id", original_id.to_string())],
        &json!({
            "reason": "the giver's name was misspelt",
            "payer_name": "Jane Doe",
        }),
    )
    .await;
    assert_eq!(
        corrected["issued_to"], "Jane Doe",
        "the name the correction restates is the addressee — the one-field typo fix: {corrected}"
    );
    assert_eq!(
        corrected["purpose"],
        format!("{PROBE_MARK}{newly_declared}"),
        "and the purpose is carried over even when only the name is restated: {corrected}"
    );
    assert_eq!(
        corrected["tax_statement"], DECLARED,
        "the correction states the troop's declaration now, not the superseded row's (empty): \
         {corrected}"
    );
    assert_eq!(corrected["issued_on"], today, "{corrected}");

    cleanup(&url, withdrawn).await;
    cleanup(&url, newly_declared).await;
}
