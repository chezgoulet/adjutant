//! # The store plugin's catalogue SQL, against a real database
//!
//! The probes no gate ran before: `POST /api/store/item` and `PATCH
//! /api/store/item/{id}` are driven through the plugin's **real route
//! handlers**, with the plugin's own database role, and what lands in
//! `store.catalogue_items` is read back from PostgreSQL.
//!
//! ## Why this exists
//!
//! The sibling probe `outbox_draw.rs` executes the **order/draw** writes only.
//! Nothing in the repository ran the catalogue's own writes, so a statement
//! PostgreSQL refuses could ship green — and one did: `POST /api/store/item`
//! answered `500` for every caller because its `INSERT` carried
//! `RETURNING {ITEM_FIELDS}`, and every column there is qualified `i.`, an
//! alias the `INSERT` target did not have (`missing FROM-clause entry for table
//! "i"`). A gate on the handler's *decisions* — a mock host answering, the SQL a
//! string nobody executes — cannot see that class of fault; the fix is to
//! execute the statement. These probes assert on the **rows** that land and on
//! refusals by their own reason, never on a frozen copy of the statement's text,
//! so a change to the SQL cannot make this file lie in either direction.
//!
//! What they cover, in the shapes the API documents:
//!
//! * an item created through `POST /api/store/item` exists in
//!   `store.catalogue_items` with its kind, name, category, sku, price, currency
//!   and fund code, and the answer names the same row;
//! * a **rental** carries the equipment item it rents and nothing else about it;
//! * the catalogue list returns both, and hides an item once it is deactivated —
//!   unless `include_inactive` is asked for;
//! * `PATCH /api/store/item/{id}` corrects the row that comes back, an empty
//!   string leaves a text field as it is, `kind` stays immutable, an edit to an
//!   id that does not exist is a `404` naming that, and no second row is written.
//!
//! ## Running them
//!
//! They are `#[ignore]`d (issue #25), so a bare `cargo test --workspace` reports
//! them as ignored rather than passed, and under `--ignored` a missing or
//! unreachable database is a hard failure, never a skip:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://robot@127.0.0.1:55432/adjutant_dev_test \
//!   cargo test -p adjutant-store --test catalogue_sql -- --ignored --nocapture
//! ```
//!
//! ## What the database has to be
//!
//! One a **boot has already prepared**: the core's migrations applied (so
//! `core.audit_log` exists — these handlers audit every write) and the `store`
//! plugin's own role created with its secret stored in `core.plugins.db_secret`.
//! The live ladder produces exactly that, and it is why the CI step sits beside
//! the other DB-backed probes rather than with the unit tests:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ```
//!
//! The probes then connect **as the plugin's own role** — the connection whose
//! `session_user` is `adjutant_plugin_store`, so what they prove is what the
//! plugin's statements do under the privileges they actually run with. They read
//! the stored secret rather than rotating it, so a later boot of the same
//! database still works.
//!
//! They write only rows whose `description` begins with a `probe:` marker, and
//! each probe clears its own before and after.

use std::sync::Arc;

use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestRequest};
use adjutant_store::{
    migrations, StorePlugin, CATEGORY_GEAR, CATEGORY_PATCH, FUND_GENERAL, KIND_PRODUCT, KIND_RENTAL,
};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, PgPool, Row, ValueRef};

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_store";
/// The caller every probe acts as. Not a UUID on purpose: the audit write then
/// attributes the actor in `details` instead of `core.audit_log.user_id`, which
/// is what a non-member probe caller is.
const CALLER: &str = "probe-store-quartermaster";
/// Every row this file writes carries it in `description`, so a probe can clear
/// and count exactly what it wrote and nothing else.
const PROBE_MARK: &str = "probe:store-catalogue-sql:";

/// Serialises the probes: one of them lists the catalogue, and two running at
/// once would be reading each other's rows.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the store catalogue probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a test \
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
            "[store catalogue probe] WARNING: running against {name:?}, which does not end in `_test`"
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
/// idempotent, so this is a no-op there.
///
/// `CREATE SCHEMA` is deliberately **not** done as the plugin role: a plugin
/// role does not own the database and has no `CREATE` on it (`bootstrap-isolation`
/// creates the schema as the operator, `AUTHORIZATION adjutant_plugin_<id>`), so
/// the probe does what the operator does and then migrates as the plugin, which
/// owns what is inside its own schema.
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

/// One catalogue row, as this file cares about it.
#[derive(Debug)]
struct Item {
    id: i64,
    kind: String,
    sku: Option<String>,
    name: String,
    category: String,
    description: String,
    base_price_cents: i64,
    currency: String,
    fund_code: String,
    equipment_item_id: Option<i64>,
    active: bool,
    created_by: String,
    /// `updated_at >= created_at` — a boolean rather than two timestamps, so the
    /// probe does not depend on a timezone feature of the sqlx driver.
    fresh: bool,
}

/// Clear anything a previous run left behind. Called **before** a probe writes
/// and again after, never between a write and the assertion on it.
async fn cleanup(admin: &PgPool, mark: &str) {
    let _ = sqlx::query("DELETE FROM store.catalogue_items WHERE description LIKE $1")
        .bind(format!("{PROBE_MARK}{mark}%"))
        .execute(admin)
        .await;
}

/// Every row a probe wrote, in id order.
async fn read_back(admin: &PgPool, mark: &str) -> Vec<Item> {
    let rows = sqlx::query(
        "SELECT id, kind, sku, name, category, description, base_price_cents, currency, \
                fund_code, equipment_item_id, active, created_by, \
                (updated_at >= created_at) AS fresh \
         FROM store.catalogue_items WHERE description LIKE $1 ORDER BY id",
    )
    .bind(format!("{PROBE_MARK}{mark}%"))
    .fetch_all(admin)
    .await
    .expect("read the rows the probe wrote");
    rows.iter()
        .map(|row| Item {
            id: row.get("id"),
            kind: row.get("kind"),
            sku: row.get("sku"),
            name: row.get("name"),
            category: row.get("category"),
            description: row.get("description"),
            base_price_cents: row.get("base_price_cents"),
            currency: row.get("currency"),
            fund_code: row.get("fund_code"),
            equipment_item_id: row.get("equipment_item_id"),
            active: row.get("active"),
            created_by: row.get("created_by"),
            fresh: row.get("fresh"),
        })
        .collect()
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

/// Nothing on the catalogue path calls another plugin: an item is one statement,
/// so a probe that reached HTTP would be proving the wrong mechanism. This
/// handle refuses loudly rather than quietly passing.
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
            "the store catalogue probes write one statement each; nothing should be called \
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

/// The plugin's `core.plugins.config` block, as a boot hands it over. No route
/// here reaches a money path, so nothing in these probes depends on it beyond the
/// plugin parsing it.
fn config_json() -> Value {
    json!({ "base_url": "http://127.0.0.1:8787", "fund_code": FUND_GENERAL })
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
        plugin_id: "store".to_string(),
        db: DbHandle::new(plugin_db, "store".to_string()),
        config: config_json(),
        events: EventBusHandle::new(Arc::new(InertEvents), "store".to_string()),
        permissions: PermissionService::new(core_db.clone()),
        audit: AuditService::new(core_db, "store".to_string()),
        identity: Arc::new(NoIdentity),
        http: Arc::new(NoHttp),
    }
}

/// The plugin's own route list, built the way the core builds it. The probes
/// call the handlers **out of this list**, so an edit to the statement behind a
/// route is what is exercised — not a copy of it.
async fn routes_of(ctx: &PluginContext) -> Vec<RouteDefinition> {
    let mut plugin = StorePlugin::new();
    plugin.init(ctx.clone()).await.expect("init");
    plugin.routes()
}

/// The route at `method` + `path`. Both are matched: a path alone is ambiguous
/// here — `/api/store/item/{id}` is a `GET` and a `PATCH` — and a probe that
/// called the wrong one would be proving the wrong handler. A route that is not
/// there is a defect in this file, not a silent skip.
fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.path == path && r.method.as_str() == method)
        .unwrap_or_else(|| panic!("the plugin serves no {method} {path:?}"))
}

/// One request that must be **accepted**: a 2xx, decoded as JSON.
async fn accepted(
    routes: &[RouteDefinition],
    method: &str,
    path: &str,
    req: TestRequest,
) -> Value {
    match (route(routes, method, path).handler)(req.build()).await {
        Ok(response) if (200..300).contains(&response.status) => response_json(&response),
        Ok(response) => panic!(
            "{method} {path} answered {} — expected it to be accepted: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => panic!("{method} {path} returned an error — expected it to be accepted: {e}"),
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
    method: &str,
    path: &str,
    req: TestRequest,
) -> Refusal {
    match (route(routes, method, path).handler)(req.build()).await {
        Ok(response) if response.status >= 400 => Refusal {
            status: response.status,
            reason: response_json(&response)["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&response.body).to_string()),
        },
        Ok(response) => panic!(
            "{method} {path} answered {} — expected a refusal: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        ),
        Err(e) => Refusal {
            status: e.status(),
            reason: e.to_string(),
        },
    }
}

/// A product body, with the marker that makes it this probe's row.
fn product_body(mark: &str, name: &str, category: &str, price_cents: i64) -> Value {
    json!({
        "kind": KIND_PRODUCT,
        "sku": format!("{PROBE_MARK}{mark}"),
        "name": name,
        "category": category,
        "description": format!("{PROBE_MARK}{mark}"),
        "base_price_cents": price_cents,
        "currency": "cad",
        "fund_code": FUND_GENERAL,
    })
}

/// The catalogue list's own answer, driven through the plugin's handler. The
/// limit is the maximum the route accepts, so a probe sees its rows whatever
/// else the (freshly reset) database holds.
async fn catalogue(routes: &[RouteDefinition], include_inactive: bool) -> Value {
    accepted(
        routes,
        "GET",
        "/api/store/items",
        TestRequest::get("/api/store/items")
            .query_param("include_inactive", if include_inactive { "true" } else { "false" })
            .query_param("limit", "200")
            .identity(CALLER, &["chief"]),
    )
    .await
}

/// The catalogue row for `name`, if the list returns it at all.
fn listed(catalogue: &Value, name: &str) -> Option<Value> {
    catalogue["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["name"].as_str() == Some(name))
        .cloned()
}

// ===========================================================================
// 1. An item is created through the route and the row reads back
// ===========================================================================

/// **The reference probe.** `POST /api/store/item` is the shop's way to stock a
/// shelf: without it the catalogue stays empty and a client's Add-item screen
/// gets a `500`. This drives the real handler, reads the row back from
/// `store.catalogue_items`, and proves the two agree — for a product and for a
/// rental alike, and for the list route that shows them.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn an_item_created_through_the_route_lands_in_the_catalogue_and_reads_back() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;

    let one = "create-product";
    let two = "create-rental";
    cleanup(&admin, one).await;
    cleanup(&admin, two).await;

    // --- a product.
    let name = "Probe patch, first";
    let answer = accepted(
        &routes,
        "POST",
        "/api/store/item",
        TestRequest::post("/api/store/item")
            .identity(CALLER, &["chief"])
            .json(&product_body(one, name, CATEGORY_PATCH, 1_750)),
    )
    .await;
    assert_eq!(answer["item"]["name"], name, "{answer}");
    assert_eq!(answer["item"]["kind"], KIND_PRODUCT, "{answer}");
    assert_eq!(answer["item"]["category"], CATEGORY_PATCH, "{answer}");
    assert_eq!(answer["item"]["base_price_cents"], 1_750, "{answer}");
    assert_eq!(answer["item"]["currency"], "cad", "{answer}");
    assert_eq!(answer["item"]["fund_code"], FUND_GENERAL, "{answer}");
    assert_eq!(answer["item"]["active"], true, "{answer}");
    assert_eq!(answer["item"]["created_by"], CALLER, "{answer}");
    let id = answer["item"]["id"]
        .as_i64()
        .expect("the answer names the row it wrote");
    assert!(id > 0, "{answer}");

    // The row itself, and it is the row the answer named.
    let rows = read_back(&admin, one).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.id, id, "the answer and the row agree: {answer}");
    assert_eq!(row.name, name);
    assert_eq!(row.base_price_cents, 1_750);
    assert_eq!(row.kind, KIND_PRODUCT);
    assert_eq!(row.category, CATEGORY_PATCH);
    assert_eq!(row.currency, "cad");
    assert_eq!(row.fund_code, FUND_GENERAL);
    assert_eq!(row.sku.as_deref(), Some(format!("{PROBE_MARK}{one}").as_str()));
    assert_eq!(row.created_by, CALLER);
    assert!(row.active, "a new item is on the shelf");
    assert_eq!(row.equipment_item_id, None, "a product names no equipment");
    assert!(row.fresh, "the row's own timestamps: {row:?}");

    // --- a rental: the equipment item id, and nothing else about it.
    let rental_name = "Probe tent rental";
    let rental = accepted(
        &routes,
        "POST",
        "/api/store/item",
        TestRequest::post("/api/store/item")
            .identity(CALLER, &["chief"])
            .json(&json!({
                "kind": KIND_RENTAL,
                "sku": format!("{PROBE_MARK}{two}"),
                "name": rental_name,
                "category": CATEGORY_GEAR,
                "description": format!("{PROBE_MARK}{two}"),
                "base_price_cents": 2_500,
                "fund_code": FUND_GENERAL,
                "equipment_item_id": 4_242,
            })),
    )
    .await;
    assert_eq!(rental["item"]["kind"], KIND_RENTAL, "{rental}");
    assert_eq!(rental["item"]["equipment_item_id"], 4_242, "{rental}");

    let rentals = read_back(&admin, two).await;
    assert_eq!(rentals.len(), 1, "{rentals:?}");
    assert_eq!(rentals[0].kind, KIND_RENTAL);
    assert_eq!(rentals[0].equipment_item_id, Some(4_242));
    assert_eq!(rentals[0].name, rental_name);
    assert_eq!(rentals[0].base_price_cents, 2_500);
    assert_eq!(rentals[0].fund_code, FUND_GENERAL);

    // --- and the list route returns both, with the prices that were written.
    let list = catalogue(&routes, false).await;
    let product_row = listed(&list, name).unwrap_or_else(|| panic!("{name:?} is not listed: {list}"));
    let rental_row =
        listed(&list, rental_name).unwrap_or_else(|| panic!("{rental_name:?} is not listed: {list}"));
    assert_eq!(product_row["base_price_cents"], 1_750, "{product_row}");
    assert_eq!(rental_row["base_price_cents"], 2_500, "{rental_row}");
    assert_eq!(product_row["id"], id, "{product_row}");

    cleanup(&admin, one).await;
    cleanup(&admin, two).await;
}

// ===========================================================================
// 2. An edit corrects the row; an unknown id is refused and writes nothing
// ===========================================================================

/// **The correction probe.** `PATCH /api/store/item/{id}` is an `UPDATE … RETURNING`
/// whose target carries the alias its column list names — the shape the broken
/// `INSERT` above lacked. This drives it, reads the row back, and proves the one
/// surprising thing the API documents: an **empty string leaves a text field as
/// it is** rather than blanking it. A `PATCH` to an id that does not exist is a
/// `404` naming it, and writes no row.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + a ladder-bootstrapped test database"]
async fn editing_a_catalogue_item_updates_the_row_and_an_unknown_id_is_refused() {
    let _guard = DB.lock().await;
    let admin = admin_pool().await;
    ensure_core_ready(&admin).await;
    let plugin = plugin_pool(&admin).await;
    ensure_plugin_schema(&admin, &plugin).await;
    let ctx = context(&admin, &plugin);
    let routes = routes_of(&ctx).await;

    let mark = "edit";
    cleanup(&admin, mark).await;

    let original = "Probe shirt, before";
    let created = accepted(
        &routes,
        "POST",
        "/api/store/item",
        TestRequest::post("/api/store/item")
            .identity(CALLER, &["chief"])
            .json(&product_body(mark, original, CATEGORY_PATCH, 1_000)),
    )
    .await;
    let id = created["item"]["id"]
        .as_i64()
        .expect("the created row's id");

    // --- a correction, with an empty string standing for "leave it".
    let corrected = "Probe shirt, after";
    let answer = accepted(
        &routes,
        "PATCH",
        "/api/store/item/{id}",
        TestRequest::patch("/api/store/item/{id}")
            .param("id", &id.to_string())
            .identity(CALLER, &["chief"])
            .json(&json!({
                "name": corrected,
                "base_price_cents": 1_250,
                "description": "",
                "active": false,
                // `kind` is immutable and is not even a field of the patch body:
                // a client that sends one is not moving a product into a rental.
                "kind": KIND_RENTAL,
            })),
    )
    .await;
    assert_eq!(answer["item"]["id"], id, "{answer}");
    assert_eq!(answer["item"]["name"], corrected, "{answer}");
    assert_eq!(answer["item"]["base_price_cents"], 1_250, "{answer}");
    assert_eq!(answer["item"]["active"], false, "{answer}");
    assert_eq!(
        answer["item"]["kind"], KIND_PRODUCT,
        "`kind` is immutable: {answer}"
    );

    let rows = read_back(&admin, mark).await;
    assert_eq!(rows.len(), 1, "an edit writes no second row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.name, corrected);
    assert_eq!(row.base_price_cents, 1_250);
    assert!(!row.active, "the row is off the shelf: {row:?}");
    assert_eq!(
        row.kind, KIND_PRODUCT,
        "a patch cannot turn a product into a rental: {row:?}"
    );
    assert_eq!(
        row.description,
        format!("{PROBE_MARK}{mark}"),
        "an empty string leaves a text field as it is: {row:?}"
    );
    assert_eq!(
        row.sku.as_deref(),
        Some(format!("{PROBE_MARK}{mark}").as_str()),
        "and the sku the patch did not mention is untouched: {row:?}"
    );
    assert!(row.fresh, "the row was updated, not rewritten: {row:?}");

    // --- the list shows it only when inactive rows are asked for.
    let hidden = catalogue(&routes, false).await;
    assert!(
        listed(&hidden, corrected).is_none(),
        "a deactivated item is not on the shelf: {hidden}"
    );
    let shown = catalogue(&routes, true).await;
    assert!(
        listed(&shown, corrected).is_some(),
        "but `include_inactive` shows it: {shown}"
    );

    // --- an id that names no item: refused by name, and nothing written.
    let missing = refused(
        &routes,
        "PATCH",
        "/api/store/item/{id}",
        TestRequest::patch("/api/store/item/{id}")
            .param("id", "999999999")
            .identity(CALLER, &["chief"])
            .json(&json!({ "name": "Probe nothing" })),
    )
    .await;
    assert_eq!(missing.status, 404, "{missing:?}");
    assert!(
        missing.reason.contains("no such catalogue item"),
        "the refusal states its own reason: {missing:?}"
    );
    assert_eq!(
        read_back(&admin, mark).await.len(),
        1,
        "an edit to an id that does not exist writes nothing"
    );

    cleanup(&admin, mark).await;
}
