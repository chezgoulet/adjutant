//! Finance plugin tests: the manifest the loader validates, the money handlers,
//! and the arithmetic the books depend on (SPEC §7.5).
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results **in call order**, so every test states the order its handler
//! arranges — the comment above each `push_*` says which call it answers, and the
//! doc comment on each route in `src/lib.rs` says how many calls there are.
//!
//! The arithmetic that does not need a database (money parsing and formatting,
//! the sliding scale, the fiscal year, variance, the ledger verdict) is tested in
//! `src/lib.rs` beside the functions themselves; this file tests the handlers and
//! the shape of what they answer.

use adjutant_finance::{
    budget_variance, format_cents, ledger_verdict, parse_dollars_to_cents, parse_percent_to_bps,
    scale_table, FinancePlugin, CATEGORY_DUES, CATEGORY_TRANSFER, DEFAULT_TIER, DIRECTIONS,
    DIRECT_KINDS, DUES_KINDS, DUES_STATUSES, FUND_GENERAL, FUND_KINDS, MAX_SHARE_BPS,
    MINIMUM_DUES_CENTS, STATUS_ASSESSED, STATUS_SELF_REPORTED, STATUS_WAIVED, TIER_CODES,
    TIER_HARDSHIP, TIER_PATRON, TIER_STANDARD, TIER_SUPPORTED,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use chrono::Utc;
use serde_json::{json, Value};

/// The audit log's action is a bind parameter, so a test reads it back rather
/// than looking for it in the statement.
fn assert_audited(host: &TestHost, action: &str) {
    let params = host
        .db
        .last_execute_params("audit_log")
        .expect("an audit write ran");
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered.iter().any(|p| p.contains(action)),
        "no {action} audit: {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn plugin() -> (TestHost, FinancePlugin, Vec<RouteDefinition>) {
    plugin_with_config(json!({ "membership_cost_cents": 60_000 })).await
}

async fn plugin_with_config(config: Value) -> (TestHost, FinancePlugin, Vec<RouteDefinition>) {
    let host = TestHost::new().with_config(config);
    let mut plugin = FinancePlugin::new();
    plugin.init(host.context("finance")).await.unwrap();
    let routes = plugin.routes();
    (host, plugin, routes)
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

/// Drive a handler and report `(status, body)` — an `SdkError` and a
/// `PluginResponse::error` are the same answer to a client.
async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

/// Every SQL statement a handler ran, in call order, as one string — the easy way
/// to assert what happened *and* in what order.
fn statements(host: &TestHost) -> Vec<String> {
    let mut all = host.db.queried_sql();
    all.extend(host.db.executed_sql());
    all
}

fn find_statement(host: &TestHost, needle: &str) -> String {
    statements(host)
        .into_iter()
        .find(|sql| sql.contains(needle))
        .unwrap_or_else(|| panic!("no statement contained {needle:?}"))
}

/// The `Debug` rendering of a call's bind parameters, for asserting a value.
fn params_of(host: &TestHost, needle: &str) -> Vec<String> {
    host.db
        .last_query_params(needle)
        .or_else(|| host.db.last_execute_params(needle))
        .unwrap_or_else(|| panic!("no call contained {needle:?}"))
        .iter()
        .map(|p| format!("{p:?}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Row builders (what the host hands back, shaped like the SQL's columns)
// ---------------------------------------------------------------------------

fn fund_row(id: i64, code: &str, kind: &str, balance_cents: i64) -> Value {
    json!({
        "id": id,
        "code": code,
        "name": format!("{} Fund", code),
        "kind": kind,
        "purpose": "",
        "restricted": kind != FUND_GENERAL,
        "active": true,
        "target_cents": Value::Null,
        "created_by": "finance:seed",
        "created_at": "2026-01-01 00:00:00+00",
        "updated_at": "2026-01-01 00:00:00+00",
        "balance_cents": balance_cents,
        "income_cents": 0,
        "expense_cents": 0,
        "net_transfer_cents": 0,
        "entry_count": 0,
    })
}

fn transaction_row(id: i64, fund_id: i64, amount_cents: i64, kind: &str) -> Value {
    json!({
        "id": id,
        "fund_id": fund_id,
        "amount_cents": amount_cents,
        "kind": kind,
        "transfer_group": Value::Null,
        "counterparty_fund_id": Value::Null,
        "category": "",
        "description": "",
        "member_id": "",
        "fiscal_year": 2026,
        "occurred_on": "2026-02-01",
        "recorded_by": "treasurer",
        "overdraft_authorized": false,
        "external_ref": Value::Null,
        "created_at": "2026-02-01 12:00:00+00",
    })
}

fn budget_row(
    id: i64,
    fund_id: i64,
    direction: &str,
    category: &str,
    planned_cents: i64,
    actual_cents: i64,
    overlaps: bool,
) -> Value {
    json!({
        "id": id,
        "fund_id": fund_id,
        "fund_code": FUND_GENERAL,
        "fund_name": "General Fund",
        "fund_kind": FUND_GENERAL,
        "fiscal_year": 2026,
        "direction": direction,
        "category": category,
        "planned_cents": planned_cents,
        "note": "",
        "created_by": "treasurer",
        "created_at": "2026-01-01 00:00:00+00",
        "updated_at": "2026-01-01 00:00:00+00",
        "actual_cents": actual_cents,
        "overlaps_categories": overlaps,
    })
}

fn dues_row(
    member_id: &str,
    tier: &str,
    base_cents: i64,
    assessed_cents: i64,
    status: &str,
    self_reported: bool,
    paid_cents: i64,
) -> Value {
    json!({
        "id": 1,
        "fiscal_year": 2026,
        "dues_kind": "member",
        "member_id": member_id,
        "lodge_id": "3",
        "tier": tier,
        "share_bps": Value::Null,
        "base_cents": base_cents,
        "assessed_cents": assessed_cents,
        "self_reported": self_reported,
        "status": status,
        "note": "",
        "recorded_by": member_id,
        "assessed_at": "2026-01-02 00:00:00+00",
        "updated_at": "2026-01-02 00:00:00+00",
        "paid_cents": paid_cents,
        "outstanding_cents": (assessed_cents - paid_cents).max(0),
        "settled": paid_cents >= assessed_cents,
    })
}

fn group_row(group: &str, entries: i64, sum_cents: i64) -> Value {
    json!({ "transfer_group": group, "entries": entries, "group_sum_cents": sum_cents })
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "finance");
    assert_eq!(plugin.name(), "Finance");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for perm in &permissions {
        assert!(perm.starts_with("finance:"), "{perm} must be namespaced");
    }
    // SPEC §9.1's three, plus the authorities the real work needs.
    for expected in [
        "finance:read",
        "finance:read_all",
        "finance:write",
        "finance:manage",
        "finance:manage_dues",
        "finance:self_report",
    ] {
        assert!(
            permissions.iter().any(|p| p == expected),
            "missing {expected}"
        );
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");

    let ddl = &migrations[0].sql;
    for table in ["funds", "transactions", "budgets", "dues"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "SPEC §7.5's schema is missing {table}"
        );
    }
    // Vocabulary and arithmetic rules that reach a result are constraints.
    for constraint in [
        "funds_kind_valid",
        "funds_code_shape",
        "transactions_kind_valid",
        "transactions_amount_nonzero",
        "transactions_income_positive",
        "transactions_expense_negative",
        "transactions_transfer_pair",
        "budgets_direction_valid",
        "budgets_planned_positive",
        "dues_kind_valid",
        "dues_subject_present",
        "dues_share_for_lodges",
        "dues_waived_is_zero",
        "UNIQUE (fund_id, fiscal_year, direction, category)",
        "UNIQUE (fiscal_year, dues_kind, member_id, lodge_id)",
    ] {
        assert!(
            ddl.contains(constraint),
            "migration is missing {constraint}"
        );
    }
    // The six funds SPEC §7.5 names are seeded, and seeding twice is a no-op.
    for kind in FUND_KINDS {
        assert!(
            ddl.contains(&format!("('{kind}'")),
            "the {kind} fund is not seeded"
        );
    }
    assert!(ddl.contains("ON CONFLICT (code) DO NOTHING"));
    // The money columns are integers, never floating point.
    assert!(
        !ddl.contains("NUMERIC") && !ddl.contains("DECIMAL") && !ddl.contains("DOUBLE"),
        "money must be integer cents"
    );
    assert!(ddl.contains("amount_cents BIGINT NOT NULL"));

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/finance"),
            "{} escapes the namespace",
            r.path
        );
        let perm = r
            .required_permission
            .as_deref()
            .expect("every route is gated");
        assert!(
            permissions.iter().any(|p| p == perm),
            "{perm} is required but not declared"
        );
        let normalized = r
            .path
            .split('/')
            .map(|s| {
                if s.starts_with('{') {
                    "{}".to_string()
                } else {
                    s.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let key = (r.method.as_str().to_string(), normalized);
        assert!(!seen.contains(&key), "duplicate route {key:?}");
        seen.push(key);
    }
    assert!(
        routes.len() >= 19,
        "the surface SPEC §7.5 needs is bigger than this"
    );

    // Money reads are scoped: balances and the scale are `read`, while the
    // ledger, the dues list and the report need `read_all`.
    for path in ["/api/finance/funds", "/api/finance/sliding-scale"] {
        assert_eq!(
            route(&routes, "GET", path).required_permission.as_deref(),
            Some("finance:read"),
            "{path} is a balance read"
        );
    }
    for path in [
        "/api/finance/transactions",
        "/api/finance/dues",
        "/api/finance/report/annual",
    ] {
        assert_eq!(
            route(&routes, "GET", path).required_permission.as_deref(),
            Some("finance:read_all"),
            "{path} is detailed financial data"
        );
    }
    // A member's own dues are any-scope (their own record is an ownership check),
    // and the Lodge routes check the Lodge in the handler.
    assert_eq!(
        route(&routes, "GET", "/api/finance/dues/member/{member}").required_scope,
        None
    );
    assert_eq!(
        route(&routes, "POST", "/api/finance/dues/lodge").required_scope,
        None
    );
    // Recording money and managing money are different authorities.
    assert_eq!(
        route(&routes, "POST", "/api/finance/transaction")
            .required_permission
            .as_deref(),
        Some("finance:write")
    );
    assert_eq!(
        route(&routes, "POST", "/api/finance/budget")
            .required_permission
            .as_deref(),
        Some("finance:manage")
    );
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_finance::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "finance");
    assert_eq!(
        adjutant_finance::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

// ---------------------------------------------------------------------------
// Funds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_fund_states_its_kind_and_audits_the_act() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/finance/fund");

    // The insert. `{n: 1}` is a placeholder that satisfies "one row returned".
    host.db
        .push_rows(vec![fund_row(7, "lodge_4", FUND_GENERAL, 0)]);

    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/finance/fund")
            .identity("bea", &["chief"])
            .json(&json!({
                "kind": "general",
                "code": "Lodge_4",
                "name": "Lodge 4 gear fund",
                "purpose": "Gear for Lodge 4",
                "restricted": true,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["fund"]["id"], json!(7));
    assert_eq!(body["balance_cents"], json!(0));
    assert_eq!(body["balance_display"], json!("$0.00"));

    let insert = find_statement(&host, "INSERT INTO");
    assert!(insert.contains("ON CONFLICT (code) DO NOTHING"), "{insert}");
    assert!(insert.contains("RETURNING"), "{insert}");
    let params = params_of(&host, "INSERT INTO");
    // A code is stored as the slug the database's CHECK allows.
    assert!(
        params.iter().any(|p| p.contains("\"lodge_4\"")),
        "{params:?}"
    );
    assert!(params.iter().any(|p| p.contains("Lodge 4 gear fund")));
    assert!(params.iter().any(|p| p.contains("Bool(true)")));
    host.events.assert_published("finance.fund.created");
    assert_audited(&host, "fund.create");
}

#[tokio::test]
async fn creating_a_fund_refuses_a_taken_code_and_a_bad_kind() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/finance/fund");

    // The insert returns no row: the code is taken.
    host.db.push_rows(vec![]);
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/finance/fund")
            .identity("bea", &["chief"])
            .json(&json!({ "kind": "general", "code": "general" }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("already exists"));
    assert_eq!(host.db.query_count(), 1, "nothing else was asked of the db");

    // A kind outside the six is the caller's mistake: 400, and no query at all.
    for bad in [
        json!({ "kind": "meshcore" }),
        json!({ "kind": "general", "code": "9bad" }),
        json!({ "kind": "general", "target_cents": -1 }),
    ] {
        let before = host.db.query_count();
        let (status, body) = call(
            &create.handler,
            TestRequest::post("/api/finance/fund")
                .identity("bea", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {body}");
        assert_eq!(host.db.query_count(), before, "rejected before any query");
    }
}

#[tokio::test]
async fn the_fund_list_sums_the_balances_it_was_given() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/finance/funds");

    // The one query: every fund with its derived figures.
    host.db.push_rows(vec![
        fund_row(1, "general", FUND_GENERAL, 60_000),
        fund_row(2, "scholarship", "scholarship", 10_000),
        fund_row(3, "equipment", "equipment", -500),
    ]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/finance/funds")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["funds"].as_array().unwrap().len(), 3);
    assert_eq!(body["total_cents"], json!(69_500));
    assert_eq!(body["total_display"], json!("$695.00"));
    assert_eq!(body["include_inactive"], json!(false));
    assert_eq!(
        host.db.query_count(),
        1,
        "one query, no stored balance read"
    );

    // `include_inactive=0` is false, not "present" — the SDK's trap, closed.
    let (_, body) = call(
        &list.handler,
        TestRequest::get("/api/finance/funds")
            .identity("bea", &["chief"])
            .query_param("include_inactive", "0")
            .build(),
    )
    .await;
    assert_eq!(body["include_inactive"], json!(false));
    let params = params_of(&host, "WHERE ($1::bool");
    assert!(params[0].contains("Bool(false)"), "{params:?}");
}

#[tokio::test]
async fn a_fund_page_shows_its_balance_its_budget_and_its_entries() {
    let (host, _plugin, routes) = plugin().await;
    let detail = route(&routes, "GET", "/api/finance/fund/{id}");

    host.db
        .push_rows(vec![fund_row(1, "general", FUND_GENERAL, 40_000)]);
    host.db.push_rows(vec![budget_row(
        3, 1, "expense", "gear", 20_000, -15_000, false,
    )]);
    host.db
        .push_rows(vec![transaction_row(9, 1, -15_000, "expense")]);

    let (status, body) = call(
        &detail.handler,
        TestRequest::get("/api/finance/fund/{id}")
            .identity("bea", &["chief"])
            .param("id", "1")
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["fund"]["balance_cents"], json!(40_000));
    assert_eq!(body["fiscal_year"], json!(2026));
    assert_eq!(body["entries"][0]["amount_cents"], json!(-15_000));
    // The budget line carries its variance: 20000 planned, 15000 spent.
    assert_eq!(body["budgets"][0]["variance_cents"], json!(5_000));
    assert_eq!(host.db.query_count(), 3);

    // A fund that does not exist is a 404, and no further query runs.
    let (status, _) = call(
        &detail.handler,
        TestRequest::get("/api/finance/fund/{id}")
            .identity("bea", &["chief"])
            .param("id", "99")
            .build(),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn editing_a_fund_sets_only_what_was_given_and_refuses_an_empty_patch() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/finance/fund/{id}");

    host.db
        .push_rows(vec![fund_row(1, "general", FUND_GENERAL, 0)]);

    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/finance/fund/{id}")
            .identity("bea", &["chief"])
            .param("id", "1")
            .json(&json!({ "name": "General (renamed)", "target_cents": 500_000 }))
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    let update = find_statement(&host, "UPDATE");
    assert!(update.contains("name = $2"), "{update}");
    assert!(update.contains("updated_at = now()"), "{update}");
    assert!(
        !update.contains("active ="),
        "an unsupplied field must not be touched: {update}"
    );
    assert!(
        !update.contains("kind ="),
        "an unsupplied field must not be touched: {update}"
    );

    // Nothing supplied is a client bug worth naming, and costs no query.
    host.db
        .push_rows(vec![fund_row(1, "general", FUND_GENERAL, 0)]);
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/finance/fund/{id}")
            .identity("bea", &["chief"])
            .param("id", "1")
            .json(&json!({}))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("no editable field"));

    // `code` is not editable at all: it is what a client addresses the fund by.
    let (status, _) = call(
        &edit.handler,
        TestRequest::patch("/api/finance/fund/{id}")
            .identity("bea", &["chief"])
            .param("id", "1")
            .json(&json!({ "code": "general_2" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// Recording money
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recording_income_takes_the_kinds_sign_and_reports_the_new_balance() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    // 1. the guarded insert. 2. the fund's balance afterwards.
    host.db
        .push_rows(vec![transaction_row(11, 1, 50_000, "income")]);
    host.db.push_rows(vec![json!({ "balance_cents": 110_000 })]);

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1,
                "kind": "income",
                "amount_cents": 50_000,
                "category": CATEGORY_DUES,
                "member_id": "bea",
                "description": "Bea's dues",
                "occurred_on": "2026-02-01",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["transaction"]["id"], json!(11));
    assert_eq!(body["balance_cents"], json!(110_000));
    assert_eq!(body["balance_display"], json!("$1,100.00"));
    assert_eq!(body["overdrawn"], json!(false));

    let insert = find_statement(&host, "INSERT INTO");
    // The fund is the insert's **source**, so the entry's fund is resolved by the
    // same statement that writes it: `$1` an id, `$12` a code.
    assert!(insert.contains("FROM"), "{insert}");
    assert!(insert.contains("f.id = $1"), "{insert}");
    assert!(insert.contains("f.code = $12"), "{insert}");
    assert!(
        insert.contains("+ $2 >= 0"),
        "the statement guards the overdraft: {insert}"
    );
    assert!(insert.contains("ON CONFLICT (external_ref)"), "{insert}");
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(50000)")),
        "income is stored positive: {params:?}"
    );
    assert!(params.iter().any(|p| p.contains("Int(2026)")));
    // The date is bound as text, so the SQL has to cast it.
    assert!(insert.contains("$8::date"), "{insert}");
    host.events.assert_published("finance.transaction.recorded");
    assert_audited(&host, "transaction.record");
}

#[tokio::test]
async fn a_fund_code_is_resolved_inside_the_insert_without_a_read() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    // 1. the guarded insert (which resolved the code itself). 2. the fund's
    // balance afterwards.
    host.db
        .push_rows(vec![transaction_row(13, 1, 25_000, "income")]);
    host.db.push_rows(vec![json!({ "balance_cents": 25_000 })]);

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_code": FUND_GENERAL,
                "kind": "income",
                "amount_cents": 25_000,
                "category": CATEGORY_DUES,
                "member_id": "bea",
                "description": "Bea's dues",
                "occurred_on": "2026-02-01",
                "fiscal_year": 2026,
                "external_ref": "pi_probe_code",
            }))
            .build(),
    )
    .await;

    // 201 — one statement, and **no read of finance's funds first**: a producer
    // that holds only the code can write.
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        host.db.query_count(),
        2,
        "the insert and the balance — nothing else: {:?}",
        host.db.queried_sql()
    );
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params[0].contains("NullInt"),
        "no id was given, so $1 is a typed null: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains(&format!("Text(\"{FUND_GENERAL}\")"))),
        "the code rides as $12: {params:?}"
    );
    // The answer reports the fund the entry landed in — the id finance resolved.
    assert_eq!(body["fund_id"], json!(1));
    assert_eq!(body["transaction"]["fund_id"], json!(1));
    host.events.assert_published("finance.transaction.recorded");
    let published = host.events.payloads("finance.transaction.recorded");
    assert_eq!(
        published[0]["fund_id"],
        json!(1),
        "the event names the resolved id, not the code"
    );
    assert_audited(&host, "transaction.record");
}

#[tokio::test]
async fn naming_a_fund_twice_or_not_at_all_is_refused_without_a_query() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1,
                "fund_code": FUND_GENERAL,
                "kind": "income",
                "amount_cents": 1000,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("either fund_id or fund_code, not both"),
        "{body}"
    );

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({ "kind": "income", "amount_cents": 1000 }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("names no fund"),
        "{body}"
    );
    assert_eq!(
        host.db.query_count(),
        0,
        "a body that cannot name one fund touches nothing"
    );
}

#[tokio::test]
async fn a_code_named_income_entry_is_booked_and_replays_idempotently() {
    // The shape the `stripe` producer composes when it can read nothing (its
    // callerless webhook): `fund_code` instead of `fund_id`, `kind: income`, a
    // positive magnitude, finance's own category, Stripe's payment id as the
    // reference. This test is the finance half of that join — the stripe half is
    // `plugins/stripe/tests/outbox_intent.rs`, which cannot link this crate (two
    // plugins in one binary is a duplicate `adjutant_plugin_create`).
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");
    let body = json!({
        "fund_code": FUND_GENERAL,
        "kind": "income",
        "amount_cents": 2500,
        "category": CATEGORY_DUES,
        "member_id": "42",
        "description": "Stripe dues pi_probe_callerless",
        "occurred_on": "2026-02-01",
        "external_ref": "pi_probe_callerless",
    });

    // 1. the guarded insert (which resolved the code). 2. the fund's balance.
    host.db
        .push_rows(vec![transaction_row(21, 1, 2500, "income")]);
    host.db.push_rows(vec![json!({ "balance_cents": 2500 })]);

    let (status, answer) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("service:svc.stripe.ledger", &["chief"])
            .json(&body)
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{answer}");
    assert_eq!(answer["transaction"]["fund_id"], json!(1));

    let params = params_of(&host, "INSERT INTO");
    assert!(
        params[0].contains("NullInt"),
        "the id is a typed null when the code names the fund: {params:?}"
    );
    assert!(
        params
            .iter()
            .any(|p| p.contains(&format!("Text(\"{FUND_GENERAL}\")"))),
        "the code is bound: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains("pi_probe_callerless")),
        "Stripe's payment id is the reference finance keys on: {params:?}"
    );

    // The replay — the relay retrying, or a redelivery — is a duplicate, not a
    // second entry, because finance keys on the payment id.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![fund_row(1, FUND_GENERAL, FUND_GENERAL, 2500)]);
    host.db
        .push_rows(vec![transaction_row(21, 1, 2500, "income")]);
    let (status, answer) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("service:svc.stripe.ledger", &["chief"])
            .json(&body)
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["duplicate"], json!(true));
    assert_eq!(answer["transaction"]["id"], json!(21));
}

#[tokio::test]
async fn recording_an_expense_stores_it_negative() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    host.db
        .push_rows(vec![transaction_row(12, 3, -15_000, "expense")]);
    host.db.push_rows(vec![json!({ "balance_cents": 5_000 })]);

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 3,
                "kind": "expense",
                "amount_cents": 15_000,
                "category": "gear",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(-15000)")),
        "an expense is the ledger's own negative: {params:?}"
    );
}

#[tokio::test]
async fn a_float_amount_is_refused_rather_than_rounded() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    for (body, needle) in [
        (
            json!({ "fund_id": 1, "kind": "income", "amount_cents": 12.5 }),
            "invalid",
        ),
        (
            json!({ "fund_id": 1, "kind": "income", "amount": 12.5 }),
            "invalid",
        ),
        (
            json!({ "fund_id": 1, "kind": "income", "amount_cents": "12.50" }),
            "integer number of cents",
        ),
        (
            json!({ "fund_id": 1, "kind": "income", "amount": 5000 }),
            "must be a string",
        ),
        (
            json!({ "fund_id": 1, "kind": "income", "amount_cents": 100, "amount": "1.00" }),
            "not both",
        ),
        (json!({ "fund_id": 1, "kind": "income" }), "required"),
        // A float matches neither spelling of an amount: refused by the
        // deserializer, so no rounding logic is ever reached.
        (
            json!({ "fund_id": 1, "kind": "income", "amount_cents": 12.345 }),
            "did not match any variant",
        ),
    ] {
        let (status, response) = call(
            &record.handler,
            TestRequest::post("/api/finance/transaction")
                .identity("treasurer", &["chief"])
                .json(&body)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{body} gave {response}");
        assert!(
            response["error"].as_str().unwrap().contains(needle),
            "{body} gave {response}"
        );
    }
    assert_eq!(host.db.query_count(), 0, "no amount above reached the db");

    // A magnitude is required: the sign is the kind's business.
    for bad in [
        json!({ "fund_id": 1, "kind": "income", "amount_cents": 0 }),
        json!({ "fund_id": 1, "kind": "income", "amount_cents": -100 }),
        json!({ "fund_id": 1, "kind": "expense", "amount_cents": -100 }),
        json!({ "fund_id": 1, "kind": "transfer", "amount_cents": 100 }),
        json!({ "fund_id": 1, "kind": "INCOME", "amount_cents": 100, "occurred_on": "yesterday" }),
        json!({ "fund_id": 1, "kind": "income", "amount_cents": 100, "fiscal_year": 1999 }),
    ] {
        let (status, response) = call(
            &record.handler,
            TestRequest::post("/api/finance/transaction")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
    // Lowercase `income` passes the kind check, so that one only fails the date.
    let (status, _) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1, "kind": "income", "amount_cents": 100, "occurred_on": "yesterday"
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn an_overdrafting_entry_is_refused_with_the_balance_and_can_be_authorised() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    // 1. the guarded insert writes nothing (the guard refused it).
    // 2. the fund and its balance, to say why.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![json!({
        "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true,
        "balance_cents": 1_000
    })]);

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1, "kind": "expense", "amount_cents": 5_000, "fiscal_year": 2026
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 409, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("$10.00"), "the balance is named: {error}");
    assert!(error.contains("$50.00"), "the amount is named: {error}");
    assert!(error.contains("allow_overdraft"), "{error}");
    assert_eq!(host.db.query_count(), 2);

    // With the authorisation, the entry writes and the row records the decision.
    host.db
        .push_rows(vec![transaction_row(13, 1, -5_000, "expense")]);
    host.db.push_rows(vec![json!({ "balance_cents": -4_000 })]);
    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1,
                "kind": "expense",
                "amount_cents": 5_000,
                "allow_overdraft": true,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["overdrawn"], json!(true));
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Bool(true)")),
        "the authorisation is recorded on the entry: {params:?}"
    );
    assert_audited(&host, "transaction.record");
}

#[tokio::test]
async fn an_entry_against_an_unknown_fund_is_a_404() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    host.db.push_rows(vec![]); // the insert wrote nothing
    host.db.push_rows(vec![]); // and the fund is not there either

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 99, "kind": "income", "amount_cents": 500, "fiscal_year": 2026
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert!(body["error"].as_str().unwrap().contains("99"));
}

#[tokio::test]
async fn a_replayed_external_ref_is_a_duplicate_not_a_second_entry() {
    let (host, _plugin, routes) = plugin().await;
    let record = route(&routes, "POST", "/api/finance/transaction");

    // 1. the insert, caught by the unique index on `external_ref` (no row).
    // 2. the fund, which exists.
    // 3. the entry that already holds the reference.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![json!({
        "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true,
        "balance_cents": 60_000
    })]);
    host.db
        .push_rows(vec![transaction_row(3, 1, 25_000, "income")]);

    let (status, body) = call(
        &record.handler,
        TestRequest::post("/api/finance/transaction")
            .identity("stripe", &["chief"])
            .json(&json!({
                "fund_id": 1,
                "kind": "income",
                "amount_cents": 25_000,
                "external_ref": "pi_123",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate"], json!(true));
    assert_eq!(body["recorded"], json!(false));
    assert_eq!(body["transaction"]["id"], json!(3));
    assert!(body["note"].as_str().unwrap().contains("pi_123"));
    assert_eq!(
        host.events.published_types(),
        Vec::<String>::new(),
        "nothing was recorded, so nothing is announced"
    );
}

#[tokio::test]
async fn the_ledger_page_reports_its_filters_and_the_total_they_select() {
    let (host, _plugin, routes) = plugin().await;
    let ledger = route(&routes, "GET", "/api/finance/transactions");

    // 1. the page: two rows for a limit of one, which is how `has_more` is known.
    // 2. the filtered total.
    host.db.push_rows(vec![
        transaction_row(9, 1, -15_000, "expense"),
        transaction_row(8, 1, 50_000, "income"),
    ]);
    host.db.push_rows(vec![json!({
        "total_cents": 35_000, "entries": 2
    })]);

    let (status, body) = call(
        &ledger.handler,
        TestRequest::get("/api/finance/transactions")
            .identity("treasurer", &["chief"])
            .query_param("fund_id", "1")
            .query_param("fiscal_year", "2026")
            .query_param("limit", "1")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], json!(1));
    assert_eq!(body["has_more"], json!(true));
    assert_eq!(body["next_before_id"], json!(9));
    assert_eq!(body["page_total_cents"], json!(-15_000));
    assert_eq!(body["filtered_total_cents"], json!(35_000));
    assert_eq!(body["filters"]["fund_id"], json!(1));
    assert_eq!(body["filters"]["limit"], json!(1));
    assert_eq!(host.db.query_count(), 2);

    // The page and the total share one filter: a second call cannot disagree.
    let page = find_statement(&host, "ORDER BY t.id DESC LIMIT $10");
    assert!(
        page.contains("($1::bigint IS NULL OR t.fund_id = $1)"),
        "{page}"
    );
    assert!(
        page.contains("($7::date IS NULL OR t.occurred_on >= $7::date)"),
        "{page}"
    );

    // An unknown kind in a filter is a 400 rather than a silently empty page.
    let (status, _) = call(
        &ledger.handler,
        TestRequest::get("/api/finance/transactions")
            .identity("treasurer", &["chief"])
            .query_param("kind", "refund")
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// Transfers: two entries, one statement, sum preserved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_transfer_writes_both_legs_in_one_statement() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    // 1. the one statement that writes both legs (it returns two rows).
    // 2. the two funds' balances.
    host.db.push_rows(vec![
        json!({
            "id": 5, "fund_id": 1, "amount_cents": -10_000, "kind": "transfer",
            "transfer_group": "4f2a8c1e-0000-4000-8000-000000000001",
            "counterparty_fund_id": 2, "category": CATEGORY_TRANSFER, "description": "",
            "member_id": "", "fiscal_year": 2026, "occurred_on": "2026-03-01",
            "recorded_by": "treasurer", "overdraft_authorized": false,
            "external_ref": Value::Null, "created_at": "2026-03-01 12:00:00+00",
        }),
        json!({
            "id": 6, "fund_id": 2, "amount_cents": 10_000, "kind": "transfer",
            "transfer_group": "4f2a8c1e-0000-4000-8000-000000000001",
            "counterparty_fund_id": 1, "category": CATEGORY_TRANSFER, "description": "",
            "member_id": "", "fiscal_year": 2026, "occurred_on": "2026-03-01",
            "recorded_by": "treasurer", "overdraft_authorized": false,
            "external_ref": Value::Null, "created_at": "2026-03-01 12:00:00+00",
        }),
    ]);
    host.db.push_rows(vec![
        json!({ "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true, "balance_cents": 50_000 }),
        json!({ "id": 2, "code": "scholarship", "name": "Scholarship Fund", "active": true, "balance_cents": 10_000 }),
    ]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_id": 1,
                "to_fund_id": 2,
                "amount_cents": 10_000,
                "description": "Scholarship contribution",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);
    assert_eq!(body["amount_cents"], json!(10_000));
    // Conservation is a property of the two legs, and the response says so.
    assert_eq!(body["sum_cents"], json!(0));
    let legs: i64 = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["amount_cents"].as_i64().unwrap())
        .sum();
    assert_eq!(legs, 0, "the legs cancel: {body}");
    let funds: Vec<i64> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["fund_id"].as_i64().unwrap())
        .collect();
    assert_eq!(funds, vec![1, 2], "one leg per fund");

    // Both legs, one statement, one group — that is what makes it atomic.
    assert_eq!(host.db.query_count(), 2, "the transfer, then the balances");
    let sql = find_statement(&host, "CROSS JOIN LATERAL unnest");
    assert_eq!(
        host.db
            .queried_sql()
            .iter()
            .filter(|s| s.contains("CROSS JOIN LATERAL unnest"))
            .count(),
        1,
        "a transfer is ONE statement, never two writes"
    );
    assert!(sql.contains("gen_random_uuid()"), "{sql}");
    // Both references are resolved **inside** the statement (`WITH ref AS …`), so
    // an id is never required from the caller and the ids that land in
    // `transactions.fund_id` are the statement's own.
    assert!(sql.contains("WITH ref AS ("), "{sql}");
    assert!(
        sql.contains("f.code = $3") || sql.contains("f.code = $4"),
        "a reference by code is resolved against the funds table in this statement: {sql}"
    );
    assert!(
        sql.contains("r.from_id IS NOT NULL") && sql.contains("r.to_id IS NOT NULL"),
        "both references must resolve or neither leg is written: {sql}"
    );
    assert!(
        sql.contains("r.from_id <> r.to_id"),
        "a transfer between one fund and itself is refused by the statement: {sql}"
    );
    assert!(
        sql.contains("$5[1]"),
        "the out-leg's amount is the guarded one: {sql}"
    );
    // The legs are the same validated number, signed once in Rust.
    let params = params_of(&host, "CROSS JOIN LATERAL unnest");
    assert!(
        params
            .iter()
            .any(|p| p.contains("IntArray([-10000, 10000])")),
        "the legs are -a and +a: {params:?}"
    );
    // Each reference is an id **or** a code: one non-null per leg.
    assert!(params.iter().any(|p| p.contains("Int(1)")), "{params:?}");
    assert!(params.iter().any(|p| p.contains("Int(2)")), "{params:?}");
    assert!(params.iter().any(|p| p.contains("Null")), "{params:?}");
    host.events.assert_published("finance.transfer.recorded");
    assert_audited(&host, "transaction.transfer");
}

#[tokio::test]
async fn a_transfer_that_would_overdraw_is_refused_and_writes_nothing() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    // 1. the statement writes nothing (the guard refused it: it returns one row
    //    or none, and anything but two is a refusal).
    // 2. both funds, so the refusal can be explained with the balance.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![
        json!({ "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true, "balance_cents": 5_000 }),
        json!({ "id": 2, "code": "scholarship", "name": "Scholarship Fund", "active": true, "balance_cents": 0 }),
    ]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_id": 1, "to_fund_id": 2, "amount_cents": 90_000, "fiscal_year": 2026
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 409, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("$50.00"), "{error}");
    assert!(
        error.contains("-85000") || error.contains("-$850.00"),
        "{error}"
    );
    // Nothing was written and nothing was announced.
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("INSERT")),
        "no leg was written"
    );
    host.events.assert_none();
}

#[tokio::test]
async fn a_transfer_to_an_unknown_fund_is_refused_whole() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    host.db.push_rows(vec![]); // the statement wrote nothing
    host.db
        .push_rows(vec![json!({ "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true, "balance_cents": 5_000 })]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_id": 1, "to_fund_id": 99, "amount_cents": 100, "fiscal_year": 2026
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 404, "{body}");
    assert!(body["error"].as_str().unwrap().contains("99"));
}

#[tokio::test]
async fn a_transfer_needs_two_funds_and_a_positive_amount() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    for bad in [
        json!({ "from_fund_id": 1, "to_fund_id": 1, "amount_cents": 100 }),
        json!({ "from_fund_id": 1, "to_fund_id": 2, "amount_cents": 0 }),
        json!({ "from_fund_id": 1, "to_fund_id": 2, "amount_cents": -100 }),
        json!({ "from_fund_id": 1, "to_fund_id": 2 }),
    ] {
        let (status, response) = call(
            &transfer.handler,
            TestRequest::post("/api/finance/transfer")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
    assert_eq!(host.db.query_count(), 0);
}

// ---------------------------------------------------------------------------
// Transfers named by code: the resolution happens inside the statement
// ---------------------------------------------------------------------------

/// A caller that holds only the funds' **codes** (the reference
/// `plugin-to-plugin.md` §3.5 asks for) writes a transfer without reading
/// finance first: both legs are resolved by the statement's own `WITH ref`, and
/// the ids that land in `transactions.fund_id` are the ones it resolved.
#[tokio::test]
async fn a_code_named_transfer_resolves_both_legs_inside_the_statement() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    // 1. the one statement that writes both legs (it returns two rows, with the
    //    ids the resolution produced: scholarship is fund 2, general is fund 1).
    // 2. the two funds' balances.
    host.db.push_rows(vec![
        json!({
            "id": 7, "fund_id": 2, "amount_cents": -8_000, "kind": "transfer",
            "transfer_group": "4f2a8c1e-0000-4000-8000-000000000002",
            "counterparty_fund_id": 1, "category": CATEGORY_TRANSFER, "description": "",
            "member_id": "", "fiscal_year": 2026, "occurred_on": "2026-03-01",
            "recorded_by": "", "overdraft_authorized": false,
            "external_ref": Value::Null, "created_at": "2026-03-01 12:00:00+00",
        }),
        json!({
            "id": 8, "fund_id": 1, "amount_cents": 8_000, "kind": "transfer",
            "transfer_group": "4f2a8c1e-0000-4000-8000-000000000002",
            "counterparty_fund_id": 2, "category": CATEGORY_TRANSFER, "description": "",
            "member_id": "", "fiscal_year": 2026, "occurred_on": "2026-03-01",
            "recorded_by": "", "overdraft_authorized": false,
            "external_ref": Value::Null, "created_at": "2026-03-01 12:00:00+00",
        }),
    ]);
    host.db.push_rows(vec![
        json!({ "id": 2, "code": "scholarship", "name": "Scholarship Fund", "active": true, "balance_cents": 20_000 }),
        json!({ "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true, "balance_cents": 5_000 }),
    ]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_code": "scholarship",
                "to_fund_code": FUND_GENERAL,
                "amount_cents": 8_000,
                "description": "Store order 12 scholarship draw",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["sum_cents"], json!(0));
    let funds: Vec<i64> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["fund_id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        funds,
        vec![2, 1],
        "the ids are the ones the statement resolved, not the ones nobody sent: {body}"
    );
    // One statement, and it carried the codes rather than ids.
    assert_eq!(host.db.query_count(), 2, "the transfer, then the balances");
    let sql = find_statement(&host, "CROSS JOIN LATERAL unnest");
    assert!(sql.contains("$2::text") && sql.contains("$4::text"), "{sql}");
    let params = params_of(&host, "CROSS JOIN LATERAL unnest");
    assert!(
        params.iter().any(|p| p == "Text(\"scholarship\")"),
        "the origin is named by code: {params:?}"
    );
    assert!(
        params.iter().any(|p| *p == format!("Text({FUND_GENERAL:?})")),
        "{params:?}"
    );
    assert!(
        params.iter().filter(|p| p == &"NullInt").count() == 2,
        "and no id was sent for either leg: {params:?}"
    );
    assert_audited(&host, "transaction.transfer");
}

/// Two codes that name **one** fund are the same refusal as two equal ids: a
/// transfer to itself is not a transfer. It is caught before the statement, so
/// nothing is read and nothing is written.
#[tokio::test]
async fn a_transfer_naming_one_fund_by_code_twice_is_refused() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_code": "scholarship",
                "to_fund_code": "scholarship",
                "amount_cents": 100,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("two different funds"),
        "{body}"
    );
    assert_eq!(host.db.query_count(), 0, "nothing was read");
}

/// A code-named transfer that would overdraw is refused by the **same guard**,
/// and explained in the same words: the resolution did not change which fund is
/// the guarded one.
#[tokio::test]
async fn a_code_named_transfer_that_would_overdraw_is_refused_with_the_balance() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    host.db.push_rows(vec![]); // the guarded statement wrote nothing
    host.db.push_rows(vec![
        json!({ "id": 2, "code": "scholarship", "name": "Scholarship Fund", "active": true, "balance_cents": 5_000 }),
        json!({ "id": 1, "code": FUND_GENERAL, "name": "General Fund", "active": true, "balance_cents": 0 }),
    ]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_code": "scholarship",
                "to_fund_code": FUND_GENERAL,
                "amount_cents": 90_000,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("$50.00"), "{error}");
    assert!(
        error.contains("-85000") || error.contains("-$850.00"),
        "{error}"
    );
    assert!(
        !host.db.executed_sql().iter().any(|s| s.contains("INSERT")),
        "no leg was written"
    );
    host.events.assert_none();
}

/// A code finance does not have is a `404` naming the reference — and neither leg
/// is written, exactly as for an unknown id.
#[tokio::test]
async fn a_transfer_to_an_unknown_code_is_refused_whole() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    host.db.push_rows(vec![]); // the statement wrote nothing
    host.db.push_rows(vec![json!({
        "id": 2, "code": "scholarship", "name": "Scholarship Fund", "active": true, "balance_cents": 5_000
    })]);

    let (status, body) = call(
        &transfer.handler,
        TestRequest::post("/api/finance/transfer")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "from_fund_code": "scholarship",
                "to_fund_code": "no-such-fund",
                "amount_cents": 100,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("no-such-fund"),
        "the code is named back: {body}"
    );
}

/// Exactly one reference per leg. Both together, or neither, is a `400` in the
/// same vocabulary `POST /api/finance/transaction` uses for the same question.
#[tokio::test]
async fn a_transfer_needs_exactly_one_reference_per_leg() {
    let (host, _plugin, routes) = plugin().await;
    let transfer = route(&routes, "POST", "/api/finance/transfer");

    for bad in [
        json!({ "from_fund_id": 1, "from_fund_code": FUND_GENERAL, "to_fund_id": 2, "amount_cents": 100 }),
        json!({ "from_fund_id": 1, "to_fund_id": 2, "to_fund_code": "scholarship", "amount_cents": 100 }),
        json!({ "to_fund_id": 2, "amount_cents": 100 }),
        json!({ "from_fund_id": 1, "amount_cents": 100 }),
    ] {
        let (status, response) = call(
            &transfer.handler,
            TestRequest::post("/api/finance/transfer")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
    assert_eq!(host.db.query_count(), 0, "nothing was read");
}

// ---------------------------------------------------------------------------
// Budgets against actuals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn budget_lines_carry_a_favourable_positive_variance() {
    let (host, _plugin, routes) = plugin().await;
    let budgets = route(&routes, "GET", "/api/finance/budgets");

    // One query: the lines with their actuals from the ledger.
    host.db.push_rows(vec![
        budget_row(1, 1, "income", "", 100_000, 85_000, false),
        budget_row(2, 1, "expense", "", 40_000, -15_000, true),
        budget_row(3, 1, "expense", "gear", 20_000, -25_000, false),
    ]);

    let (status, body) = call(
        &budgets.handler,
        TestRequest::get("/api/finance/budgets")
            .identity("bea", &["chief"])
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    let lines = body["lines"].as_array().unwrap();
    // Income behind plan: adverse, and stated negatively.
    assert_eq!(lines[0]["variance_cents"], json!(-15_000));
    assert_eq!(lines[0]["adverse"], json!(true));
    assert_eq!(lines[0]["variance_display"], json!("-$150.00"));
    // Expense under plan: favourable.
    assert_eq!(lines[1]["variance_cents"], json!(25_000));
    assert_eq!(lines[1]["adverse"], json!(false));
    assert_eq!(lines[1]["actual_cents"], json!(-15_000));
    // The whole-fund envelope sits beside a category line, and says so.
    assert_eq!(lines[1]["overlaps_categories"], json!(true));
    // Expense over plan: adverse.
    assert_eq!(lines[2]["variance_cents"], json!(-5_000));
    assert_eq!(lines[2]["adverse"], json!(true));

    let totals = &body["totals"];
    assert_eq!(totals["planned_expense_cents"], json!(60_000));
    assert_eq!(totals["actual_expense_cents"], json!(-40_000));
    assert_eq!(totals["expense_variance_cents"], json!(20_000));
    assert_eq!(totals["adverse_lines"], json!(2));
    assert_eq!(totals["lines_overlapping_categories"], json!(1));

    // The actual is a sum over the ledger, so nothing needs recomputing.
    let sql = find_statement(&host, "LEFT JOIN LATERAL");
    assert!(sql.contains("SUM(t.amount_cents)"), "{sql}");
    assert!(sql.contains("EXISTS (SELECT 1 FROM"), "{sql}");
    assert!(
        !sql.contains("remaining"),
        "there is no stored remainder to drift: {sql}"
    );
}

#[tokio::test]
async fn setting_a_budget_line_upserts_and_checks_the_fund() {
    let (host, _plugin, routes) = plugin().await;
    let set = route(&routes, "POST", "/api/finance/budget");

    host.db
        .push_rows(vec![budget_row(4, 1, "expense", "gear", 25_000, 0, false)]);

    let (status, body) = call(
        &set.handler,
        TestRequest::post("/api/finance/budget")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "fund_id": 1,
                "direction": "expense",
                "amount": "250.00",
                "category": "gear",
                "fiscal_year": 2026,
                "note": "Revised after the meeting",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["budget"]["id"], json!(4));
    assert_eq!(body["planned_display"], json!("$250.00"));
    let sql = find_statement(&host, "INSERT INTO");
    assert!(
        sql.contains("ON CONFLICT (fund_id, fiscal_year, direction, category) DO UPDATE"),
        "a budget line is revised in place: {sql}"
    );
    assert!(sql.contains("WHERE EXISTS (SELECT 1 FROM"), "{sql}");
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(25000)")),
        "the dollar string became exact cents: {params:?}"
    );
    host.events.assert_published("finance.budget.set");
    assert_audited(&host, "budget.set");

    // No such fund: the guarded upsert returns nothing.
    host.db.push_rows(vec![]);
    let (status, body) = call(
        &set.handler,
        TestRequest::post("/api/finance/budget")
            .identity("treasurer", &["chief"])
            .json(&json!({ "fund_id": 99, "direction": "income", "amount_cents": 100 }))
            .build(),
    )
    .await;
    assert_eq!(status, 404, "{body}");

    for bad in [
        json!({ "fund_id": 1, "direction": "transfer", "amount_cents": 100 }),
        json!({ "fund_id": 1, "direction": "income", "amount_cents": 0 }),
        json!({ "fund_id": 1, "direction": "income", "amount_cents": -5 }),
    ] {
        let (status, response) = call(
            &set.handler,
            TestRequest::post("/api/finance/budget")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
}

// ---------------------------------------------------------------------------
// The sliding scale
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_sliding_scale_needs_no_database_and_shows_every_tier() {
    let (host, _plugin, routes) = plugin().await;
    let scale = route(&routes, "GET", "/api/finance/sliding-scale");

    let (status, body) = call(
        &scale.handler,
        TestRequest::get("/api/finance/sliding-scale")
            .identity("bea", &["member"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        host.db.query_count(),
        0,
        "the scale is a constant table and exact arithmetic, not a query"
    );
    assert_eq!(body["base_cents"], json!(60_000), "from the plugin config");
    assert_eq!(body["minimum_cents"], json!(0));
    assert_eq!(body["honor_system"], json!(true));

    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), TIER_CODES.len());
    let assessed = |code: &str| -> i64 {
        tiers
            .iter()
            .find(|t| t["tier"] == json!(code))
            .unwrap_or_else(|| panic!("{code} is on the scale"))["assessed_cents"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(assessed(TIER_PATRON), 120_000); // double
    assert_eq!(assessed(TIER_STANDARD), 60_000); // the full cost
    assert_eq!(assessed(TIER_SUPPORTED), 30_000); // half
    assert_eq!(assessed(TIER_HARDSHIP), 0); // nothing at all
                                            // Nobody is asked for anything that is not on the scale.
    assert!(tiers
        .iter()
        .all(|tier| tier["self_reportable"] == json!(true)));

    // A caller may ask about another membership cost, and an unconfigured troop
    // sees zeroes rather than a made-up number.
    let (_, body) = call(
        &scale.handler,
        TestRequest::get("/api/finance/sliding-scale")
            .identity("bea", &["member"])
            .query_param("base_cents", "10000")
            .build(),
    )
    .await;
    assert_eq!(body["base_cents"], json!(10_000));
    let table = scale_table(10_000);
    assert_eq!(table[2]["assessed_cents"], json!(5_000));

    let (_, body) = call(
        &scale.handler,
        TestRequest::get("/api/finance/sliding-scale")
            .identity("bea", &["member"])
            .query_param("base_cents", "-5")
            .build(),
    )
    .await;
    assert_eq!(body["base_cents"], json!(0));
    // A troop that has configured no membership cost sees zeroes, not a guess.
    let (host2, _plugin2, routes2) = plugin_with_config(json!({})).await;
    let scale2 = route(&routes2, "GET", "/api/finance/sliding-scale");
    let (status, body) = call(
        &scale2.handler,
        TestRequest::get("/api/finance/sliding-scale")
            .identity("bea", &["member"])
            .build(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["base_cents"], json!(0));
    assert_eq!(body["base_configured"], json!(false));
    assert_eq!(body["tiers"][1]["assessed_cents"], json!(0));
    assert_eq!(
        host2.db.query_count(),
        0,
        "still no database, even unconfigured"
    );
    assert_eq!(
        host.db.query_count(),
        0,
        "three calls to the scale, and not one statement"
    );
}

#[test]
fn an_unconfigured_scale_is_zero_and_says_so() {
    // The scale's arithmetic is exact and pure; a troop that has configured no
    // membership cost sees a scale of zeroes rather than a guessed number.
    for tier in scale_table(0) {
        assert_eq!(tier["assessed_cents"], json!(0));
        assert_eq!(tier["assessed_display"], json!("$0.00"));
    }
}

// ---------------------------------------------------------------------------
// Dues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assessing_dues_applies_the_scale_and_a_waiver_is_always_zero() {
    let (host, _plugin, routes) = plugin().await;
    let assess = route(&routes, "POST", "/api/finance/dues/assess");

    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_STANDARD,
        60_000,
        60_000,
        STATUS_ASSESSED,
        false,
        0,
    )]);

    let (status, body) = call(
        &assess.handler,
        TestRequest::post("/api/finance/dues/assess")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "member_id": "bea",
                "tier": TIER_STANDARD,
                "lodge_id": "3",
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["assessed_display"], json!("$600.00"));
    assert_eq!(body["dues"]["assessed_cents"], json!(60_000));
    // The whole scale travels with the assessment, so a client can show the
    // alternatives a scout might choose.
    assert_eq!(body["scale"].as_array().unwrap().len(), 4);

    let sql = find_statement(&host, "INSERT INTO");
    assert!(
        sql.contains("ON CONFLICT (fiscal_year, dues_kind, member_id, lodge_id) DO UPDATE"),
        "one assessment per member per year: {sql}"
    );
    let params = params_of(&host, "INSERT INTO");
    assert!(params.iter().any(|p| p.contains("Int(60000)")));
    assert!(params.iter().any(|p| p.contains("\"member\"")));
    assert!(params.iter().any(|p| p.contains("Bool(false)")));
    host.events.assert_published("finance.dues.assessed");
    assert_audited(&host, "dues.assess");

    // A waiver assesses nothing, whatever tier is named — and the database
    // insists on the same rule.
    host.db.push_rows(vec![dues_row(
        "carl",
        TIER_STANDARD,
        60_000,
        0,
        STATUS_WAIVED,
        false,
        0,
    )]);
    let (status, body) = call(
        &assess.handler,
        TestRequest::post("/api/finance/dues/assess")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "member_id": "carl",
                "tier": TIER_STANDARD,
                "status": STATUS_WAIVED,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["assessed_display"], json!("$0.00"));
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(0)")),
        "a waiver assesses nothing: {params:?}"
    );

    // The hardship tier is $0 with no waiver needed at all.
    host.db.push_rows(vec![dues_row(
        "dana",
        TIER_HARDSHIP,
        60_000,
        0,
        STATUS_SELF_REPORTED,
        true,
        0,
    )]);
    let (status, body) = call(
        &assess.handler,
        TestRequest::post("/api/finance/dues/assess")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "member_id": "dana",
                "tier": TIER_HARDSHIP,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["assessed_display"], json!("$0.00"));
    assert_eq!(MINIMUM_DUES_CENTS, 0, "the mandatory minimum is $0");
}

#[tokio::test]
async fn assessing_dues_validates_the_member_the_tier_and_the_base() {
    let (host, _plugin, routes) = plugin().await;
    let assess = route(&routes, "POST", "/api/finance/dues/assess");

    for bad in [
        json!({ "member_id": "  ", "tier": TIER_STANDARD }),
        json!({ "member_id": "bea", "tier": "free" }),
        json!({ "member_id": "bea", "tier": TIER_STANDARD, "status": "paid" }),
        json!({ "member_id": "bea", "tier": TIER_STANDARD, "base_cents": -1 }),
        json!({ "member_id": "bea", "tier": TIER_STANDARD, "fiscal_year": 1800 }),
    ] {
        let (status, response) = call(
            &assess.handler,
            TestRequest::post("/api/finance/dues/assess")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn a_lodge_levy_is_a_fraction_of_the_membership_cost() {
    let (host, _plugin, routes) = plugin().await;
    let set = route(&routes, "POST", "/api/finance/dues/lodge");

    // 1. the permission check at the Lodge's scope (a Lodge 3 grant covers it).
    // 2. the upsert.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({
        "id": 4, "fiscal_year": 2026, "dues_kind": "lodge", "member_id": "",
        "lodge_id": "3", "tier": "", "share_bps": 1_000, "base_cents": 60_000,
        "assessed_cents": 6_000, "self_reported": false, "status": "assessed",
        "note": "", "recorded_by": "lodge_commander",
        "assessed_at": "2026-01-02 00:00:00+00", "updated_at": "2026-01-02 00:00:00+00",
        "paid_cents": 0, "outstanding_cents": 6_000, "settled": false,
    })]);

    let (status, body) = call(
        &set.handler,
        TestRequest::post("/api/finance/dues/lodge")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({
                "lodge_id": "3",
                "share_percent": "10",
                "base_cents": 60_000,
                "fiscal_year": 2026,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["share_percent"], json!("10%"));
    assert_eq!(body["dues"]["assessed_cents"], json!(6_000));
    assert_eq!(body["assessed_display"], json!("$60.00"));
    assert_eq!(body["arithmetic"], json!("10% of $600.00 = $60.00"));
    assert_audited(&host, "dues.assess.lodge");

    // A percentage finer than a basis point, both spellings, and an absurd
    // share are all refused before any query.
    for bad in [
        json!({ "lodge_id": "3", "share_percent": "10.001" }),
        json!({ "lodge_id": "3", "share_percent": "10", "share_bps": 1000 }),
        json!({ "lodge_id": "3" }),
        json!({ "lodge_id": "", "share_bps": 1000 }),
        json!({ "lodge_id": "3", "share_bps": MAX_SHARE_BPS + 1 }),
        json!({ "lodge_id": "3", "share_bps": -1 }),
    ] {
        let before = host.db.query_count();
        let (status, response) = call(
            &set.handler,
            TestRequest::post("/api/finance/dues/lodge")
                .identity("bea", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
        assert_eq!(host.db.query_count(), before);
    }

    // A Lodge grant does not reach another Lodge: the permission check refuses.
    host.db.push_rows(vec![]);
    let (status, _) = call(
        &set.handler,
        TestRequest::post("/api/finance/dues/lodge")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("4"),
                }],
            )
            .json(&json!({ "lodge_id": "3", "share_bps": 1000, "fiscal_year": 2026 }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "a Lodge 4 grant cannot levy Lodge 3");
}

#[tokio::test]
async fn the_lodge_view_shows_its_levy_and_its_members_standing() {
    let (host, _plugin, routes) = plugin().await;
    let view = route(&routes, "GET", "/api/finance/dues/lodge/{lodge}");

    // 1. the permission check (a Lodge 3 read grant).
    // 2. the Lodge's levy row.
    // 3. the Lodge's members with what the ledger shows.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![json!({
        "id": 4, "fiscal_year": 2026, "dues_kind": "lodge", "member_id": "",
        "lodge_id": "3", "tier": "", "share_bps": 1_000, "base_cents": 60_000,
        "assessed_cents": 6_000, "self_reported": false, "status": "assessed",
        "note": "", "recorded_by": "treasurer",
        "assessed_at": "2026-01-02 00:00:00+00", "updated_at": "2026-01-02 00:00:00+00",
        "paid_cents": 0, "outstanding_cents": 6_000, "settled": false,
    })]);
    host.db.push_rows(vec![
        dues_row(
            "bea",
            TIER_STANDARD,
            60_000,
            60_000,
            STATUS_SELF_REPORTED,
            true,
            20_000,
        ),
        dues_row("carl", TIER_HARDSHIP, 60_000, 0, STATUS_WAIVED, false, 0),
    ]);

    let (status, body) = call(
        &view.handler,
        TestRequest::get("/api/finance/dues/lodge/{lodge}")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .param("lodge", "3")
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["levy"]["share_bps"], json!(1_000));
    assert_eq!(body["per_member_levy_cents"], json!(6_000));
    assert_eq!(body["members"].as_array().unwrap().len(), 2);
    assert_eq!(body["totals"]["assessed_cents"], json!(60_000));
    assert_eq!(body["totals"]["collected_cents"], json!(20_000));
    assert_eq!(body["totals"]["outstanding_cents"], json!(40_000));
    assert_eq!(body["totals"]["at_no_cost"], json!(1));
    assert_eq!(host.db.query_count(), 3);
}

#[tokio::test]
async fn the_dues_list_totals_what_the_ledger_shows() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/finance/dues");

    host.db.push_rows(vec![
        dues_row(
            "bea",
            TIER_STANDARD,
            60_000,
            60_000,
            STATUS_SELF_REPORTED,
            true,
            60_000,
        ),
        dues_row("carl", TIER_HARDSHIP, 60_000, 0, STATUS_WAIVED, false, 0),
        dues_row(
            "dana",
            TIER_SUPPORTED,
            60_000,
            30_000,
            STATUS_SELF_REPORTED,
            true,
            10_000,
        ),
    ]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/finance/dues")
            .identity("treasurer", &["chief"])
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], json!(3));
    assert_eq!(body["truncated"], json!(false));
    assert_eq!(body["totals"]["assessed_cents"], json!(90_000));
    assert_eq!(body["totals"]["assessed_display"], json!("$900.00"));
    assert_eq!(body["totals"]["collected_cents"], json!(70_000));
    assert_eq!(body["totals"]["outstanding_cents"], json!(20_000));
    assert_eq!(body["totals"]["at_no_cost"], json!(1));
    assert_eq!(body["totals"]["waived"], json!(1));
    assert_eq!(body["totals"]["settled"], json!(2));
    assert_eq!(body["honor_system"], json!(true));

    // Grouped by tier, in the scale's own order, with unknown tiers absent.
    let tiers = body["by_tier"].as_array().unwrap();
    assert_eq!(tiers.len(), 3);
    assert_eq!(tiers[0]["tier"], json!(TIER_STANDARD));
    assert_eq!(tiers[1]["tier"], json!(TIER_SUPPORTED));
    assert_eq!(tiers[2]["tier"], json!(TIER_HARDSHIP));
    assert_eq!(tiers[2]["at_no_cost"], json!(1));

    // The filters are part of one query: year, Lodge, tier, status.
    let sql = find_statement(&host, "dues_kind = 'member' AND d.fiscal_year = $1");
    assert!(
        sql.contains("($2::text IS NULL OR d.lodge_id = $2)"),
        "{sql}"
    );
    assert!(sql.contains("($3::text IS NULL OR d.tier = $3)"), "{sql}");
    assert!(
        sql.contains("t.category = 'dues'"),
        "the standing comes from the ledger: {sql}"
    );
    assert_eq!(
        host.db.query_count(),
        1,
        "one query answers the list and its totals"
    );

    // A bad tier filter is the caller's mistake.
    let (status, _) = call(
        &list.handler,
        TestRequest::get("/api/finance/dues")
            .identity("treasurer", &["chief"])
            .query_param("tier", "free")
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn a_member_reads_their_own_dues_without_read_all() {
    let (host, _plugin, routes) = plugin().await;
    let view = route(&routes, "GET", "/api/finance/dues/member/{member}");

    // Owning the record needs no permission query at all — it is an ownership
    // check, not a grant (SPEC §9.2). Just the standing and the payments.
    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_HARDSHIP,
        60_000,
        0,
        STATUS_SELF_REPORTED,
        true,
        0,
    )]);
    host.db
        .push_rows(vec![transaction_row(3, 1, 60_000, "income")]);

    let (status, body) = call(
        &view.handler,
        TestRequest::get("/api/finance/dues/member/{member}")
            .identity("bea", &["member"])
            .param("member", "bea")
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["member_id"], json!("bea"));
    assert_eq!(body["dues"]["tier"], json!(TIER_HARDSHIP));
    assert_eq!(body["dues"]["assessed_cents"], json!(0));
    assert_eq!(body["payments"].as_array().unwrap().len(), 1);
    assert_eq!(host.db.query_count(), 2, "no permission query for your own");

    // Somebody else's needs `finance:read_all` covering the troop. A Lodge grant
    // covers no troop scope at all, so this is refused before any query runs.
    let (status, body) = call(
        &view.handler,
        TestRequest::get("/api/finance/dues/member/{member}")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "member".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .param("member", "carl")
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");

    // With the grant, another member's record opens.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![dues_row(
        "carl",
        TIER_STANDARD,
        60_000,
        60_000,
        STATUS_ASSESSED,
        false,
        0,
    )]);
    host.db.push_rows(vec![]);
    let (status, body) = call(
        &view.handler,
        TestRequest::get("/api/finance/dues/member/{member}")
            .identity("treasurer", &["chief"])
            .param("member", "carl")
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dues"]["member_id"], json!("carl"));

    // A member with no assessment yet is told how to open one rather than 404ed.
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![]); // no dues row
    host.db.push_rows(vec![]); // no payments
    let (status, body) = call(
        &view.handler,
        TestRequest::get("/api/finance/dues/member/{member}")
            .identity("treasurer", &["chief"])
            .param("member", "eve")
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dues"], Value::Null);
    assert!(body["next"].as_str().unwrap().contains("self-report"));
}

#[tokio::test]
async fn a_self_report_chooses_a_tier_and_never_a_price() {
    let (host, _plugin, routes) = plugin().await;
    let report = route(&routes, "POST", "/api/finance/dues/self-report");

    // 1. the existing assessment, whose *base* is what the scale is a fraction
    //    of — the treasurer's 60000, never the scout's.
    // 2. the upsert.
    // 3. the standing the scout actually cares about.
    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_STANDARD,
        60_000,
        60_000,
        STATUS_ASSESSED,
        false,
        20_000,
    )]);
    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_SUPPORTED,
        60_000,
        30_000,
        STATUS_SELF_REPORTED,
        true,
        20_000,
    )]);
    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_SUPPORTED,
        60_000,
        30_000,
        STATUS_SELF_REPORTED,
        true,
        20_000,
    )]);

    let (status, body) = call(
        &report.handler,
        TestRequest::post("/api/finance/dues/self-report")
            .identity("bea", &["member"])
            .json(&json!({ "tier": TIER_SUPPORTED, "fiscal_year": 2026 }))
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dues"]["tier"], json!(TIER_SUPPORTED));
    assert_eq!(body["assessed_display"], json!("$300.00"));
    assert_eq!(body["standing"]["paid_cents"], json!(20_000));
    assert_eq!(body["standing"]["outstanding_cents"], json!(10_000));
    assert_eq!(body["honor_system"], json!(true));

    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(60000)")),
        "the base stays the treasurer's number: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains("Bool(true)")),
        "the row records that the scout reported it: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains("self_reported")),
        "{params:?}"
    );
    host.events.assert_published("finance.dues.self_reported");
    assert_audited(&host, "dues.self_report");

    // No assessment and no configured cost: refused, with the next step named —
    // a self-report must never invent its own base.
    let (host2, _plugin2, routes2) = plugin_with_config(json!({})).await;
    let report2 = route(&routes2, "POST", "/api/finance/dues/self-report");
    host2.db.push_rows(vec![]); // no existing assessment
    let (status, body) = call(
        &report2.handler,
        TestRequest::post("/api/finance/dues/self-report")
            .identity("bea", &["member"])
            .json(&json!({ "tier": TIER_SUPPORTED, "fiscal_year": 2026 }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("treasurer"));

    // Reporting for somebody else needs `finance:manage_dues`.
    host.db.push_rows(vec![]); // the manage_dues check: no covering role
    let (status, _) = call(
        &report.handler,
        TestRequest::post("/api/finance/dues/self-report")
            .identity("bea", &["member"])
            .json(&json!({ "tier": TIER_SUPPORTED, "member_id": "carl" }))
            .build(),
    )
    .await;
    assert_eq!(status, 403);

    // An anonymous caller cannot self-report at all.
    let (status, _) = call(
        &report.handler,
        TestRequest::post("/api/finance/dues/self-report")
            .json(&json!({ "tier": TIER_SUPPORTED }))
            .build(),
    )
    .await;
    assert_eq!(status, 401);

    for bad in [
        json!({ "tier": "free" }),
        json!({ "tier": TIER_STANDARD, "fiscal_year": 1800 }),
    ] {
        let (status, response) = call(
            &report.handler,
            TestRequest::post("/api/finance/dues/self-report")
                .identity("bea", &["member"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
}

#[tokio::test]
async fn a_dues_payment_is_income_tagged_to_the_member() {
    let (host, _plugin, routes) = plugin().await;
    let pay = route(&routes, "POST", "/api/finance/dues/payment");

    // 1. the fund: no fund_id given, so the configured dues code (General).
    // 2. the entry.
    // 3. the member's standing, which the new payment has already moved.
    host.db.push_rows(vec![json!({
        "id": 1, "code": FUND_GENERAL, "name": "General Fund"
    })]);
    host.db
        .push_rows(vec![transaction_row(7, 1, 30_000, "income")]);
    host.db.push_rows(vec![dues_row(
        "bea",
        TIER_STANDARD,
        60_000,
        60_000,
        STATUS_SELF_REPORTED,
        true,
        60_000,
    )]);

    let (status, body) = call(
        &pay.handler,
        TestRequest::post("/api/finance/dues/payment")
            .identity("treasurer", &["chief"])
            .json(&json!({
                "member_id": "bea",
                "amount": "300.00",
                "fiscal_year": 2026,
                "occurred_on": "2026-02-01",
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["payment"]["amount_cents"], json!(30_000));
    assert_eq!(body["fund"]["code"], json!(FUND_GENERAL));
    assert_eq!(body["settled"], json!(true));
    assert_eq!(body["standing"]["outstanding_cents"], json!(0));

    let insert = find_statement(&host, "INSERT INTO");
    let params = params_of(&host, "INSERT INTO");
    assert!(
        params.iter().any(|p| p.contains("Int(30000)")),
        "a payment is income: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains(CATEGORY_DUES)),
        "tagged so the standing finds it: {params:?}"
    );
    assert!(
        params.iter().any(|p| p.contains("\"bea\"")),
        "and tagged to the member: {params:?}"
    );
    assert!(insert.contains("ON CONFLICT (external_ref)"));
    assert_audited(&host, "dues.payment");

    // A fund that is not there (by id or by code) is a 404 naming the config.
    host.db.push_rows(vec![]);
    let (status, body) = call(
        &pay.handler,
        TestRequest::post("/api/finance/dues/payment")
            .identity("treasurer", &["chief"])
            .json(&json!({ "member_id": "bea", "amount_cents": 100 }))
            .build(),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert!(body["error"].as_str().unwrap().contains("dues_fund_code"));

    for bad in [
        json!({ "member_id": " ", "amount_cents": 100 }),
        json!({ "member_id": "bea", "amount_cents": 0 }),
        json!({ "member_id": "bea", "amount_cents": -100 }),
        json!({ "member_id": "bea" }),
    ] {
        let (status, response) = call(
            &pay.handler,
            TestRequest::post("/api/finance/dues/payment")
                .identity("treasurer", &["chief"])
                .json(&bad)
                .build(),
        )
        .await;
        assert_eq!(status, 400, "{bad} gave {response}");
    }
}

// ---------------------------------------------------------------------------
// The Annual Financial Report and the ledger's health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_annual_report_states_the_year_and_the_money_moving_through_it() {
    let (host, _plugin, routes) = plugin().await;
    let report = route(&routes, "GET", "/api/finance/report/annual");

    // 1. per-fund figures. 2. budget lines. 3. dues by tier. 4. dues collected.
    // 5-7. the integrity check (ledger totals, fund total, transfer groups).
    host.db.push_rows(vec![
        json!({
            "id": 1, "code": FUND_GENERAL, "name": "General Fund", "kind": FUND_GENERAL,
            "restricted": false, "active": true, "target_cents": Value::Null,
            "opening_cents": 10_000, "income_cents": 85_000, "expense_cents": -15_000,
            "transfers_in_cents": 0, "transfers_out_cents": -10_000, "closing_cents": 70_000,
        }),
        json!({
            "id": 2, "code": "scholarship", "name": "Scholarship Fund", "kind": "scholarship",
            "restricted": true, "active": true, "target_cents": 100_000,
            "opening_cents": 0, "income_cents": 0, "expense_cents": 0,
            "transfers_in_cents": 10_000, "transfers_out_cents": 0, "closing_cents": 10_000,
        }),
    ]);
    host.db
        .push_rows(vec![budget_row(1, 1, "income", "", 100_000, 85_000, false)]);
    host.db.push_rows(vec![
        json!({ "tier": TIER_STANDARD, "members": 2, "assessed_cents": 120_000, "at_no_cost": 0, "self_reported": 2, "waived": 0 }),
        json!({ "tier": TIER_HARDSHIP, "members": 1, "assessed_cents": 0, "at_no_cost": 1, "self_reported": 1, "waived": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "collected_cents": 85_000 })]);
    // 5. the year's receipts — only the ones nothing supersedes are summed.
    host.db.push_rows(vec![json!({
        "issued": 2, "live": 1, "total_cents": 2_500, "corrections": 1
    })]);
    host.db.push_rows(vec![
        json!({ "ledger_total_cents": 80_000, "entries": 4, "unpaired_transfers": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "fund_total_cents": 80_000 })]);
    host.db.push_rows(vec![group_row(
        "4f2a8c1e-0000-4000-8000-000000000001",
        2,
        0,
    )]);

    let (status, body) = call(
        &report.handler,
        TestRequest::get("/api/finance/report/annual")
            .identity("treasurer", &["chief"])
            .query_param("fiscal_year", "2026")
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["fiscal_year"], json!(2026));
    assert_eq!(body["period"]["start"], json!("2026-01-01"));
    assert_eq!(body["period"]["end"], json!("2026-12-31"));
    assert!(body["generated_at"].is_string());

    // The troop's figures, summed from the funds'.
    let totals = &body["totals"];
    assert_eq!(totals["opening_cents"], json!(10_000));
    assert_eq!(totals["income_cents"], json!(85_000));
    assert_eq!(totals["expense_cents"], json!(-15_000));
    assert_eq!(totals["net_movement_cents"], json!(70_000));
    assert_eq!(totals["transfers_in_cents"], json!(10_000));
    assert_eq!(totals["transfers_out_cents"], json!(-10_000));
    assert_eq!(
        totals["net_transfers_cents"],
        json!(0),
        "transfers net to zero"
    );
    assert_eq!(totals["closing_cents"], json!(80_000));
    assert_eq!(totals["closing_display"], json!("$800.00"));
    assert_eq!(totals["fund_count"], json!(2));

    // Each fund's own arithmetic has to add up, or the report is lying.
    for fund in body["funds"].as_array().unwrap() {
        let opening = fund["opening_cents"].as_i64().unwrap();
        let closing = fund["closing_cents"].as_i64().unwrap();
        let movement = fund["income_cents"].as_i64().unwrap()
            + fund["expense_cents"].as_i64().unwrap()
            + fund["transfers_in_cents"].as_i64().unwrap()
            + fund["transfers_out_cents"].as_i64().unwrap();
        assert_eq!(
            opening + movement,
            closing,
            "{} does not add up: {fund}",
            fund["code"]
        );
    }

    assert_eq!(body["budgets"]["lines"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["budgets"]["totals"]["income_variance_cents"],
        json!(-15_000)
    );
    assert_eq!(body["dues"]["assessed_cents"], json!(120_000));
    assert_eq!(body["dues"]["collected_cents"], json!(85_000));
    assert_eq!(body["dues"]["outstanding_cents"], json!(35_000));
    assert_eq!(body["dues"]["members_assessed"], json!(3));
    assert_eq!(body["dues"]["at_no_cost"], json!(1));
    assert_eq!(body["dues"]["honor_system"], json!(true));
    assert_eq!(body["dues"]["by_tier"].as_array().unwrap().len(), 2);

    // The year's receipts: every one issued in the year, and the money only the
    // ones nothing supersedes stand for — a correction restates, it never adds.
    assert_eq!(body["receipts"]["fiscal_year"], json!(2026));
    assert_eq!(body["receipts"]["issued"], json!(2));
    assert_eq!(body["receipts"]["live"], json!(1));
    assert_eq!(body["receipts"]["corrections"], json!(1));
    assert_eq!(body["receipts"]["total_cents"], json!(2_500));
    assert_eq!(body["receipts"]["total_display"], json!("$25.00"));
    assert_eq!(
        body["receipts"]["tax_statement_declared"],
        json!(false),
        "the troop declared no status, so no receipt claims one"
    );

    // The report carries the ledger's own verdict: it is the document handed to
    // an outside body.
    assert_eq!(body["integrity"]["balanced"], json!(true));
    assert_eq!(body["integrity"]["ledger_total_cents"], json!(80_000));
    assert_eq!(body["integrity"]["transfer_groups"], json!(1));
    assert_eq!(body["ledger"]["entries"], json!(4));

    assert_eq!(host.db.query_count(), 8);
    assert_audited(&host, "report.annual");

    // A nonsense year is the caller's mistake.
    let (status, _) = call(
        &report.handler,
        TestRequest::get("/api/finance/report/annual")
            .identity("treasurer", &["chief"])
            .query_param("fiscal_year", "1700")
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn the_health_check_fails_closed_on_a_broken_transfer() {
    let (host, _plugin, routes) = plugin().await;
    let health = route(&routes, "GET", "/api/finance/health");

    // 1. ledger totals. 2. fund total. 3. transfer groups.
    host.db.push_rows(vec![
        json!({ "ledger_total_cents": 70_000, "entries": 5, "unpaired_transfers": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "fund_total_cents": 70_000 })]);
    host.db.push_rows(vec![
        group_row("a", 2, 0),
        group_row("b", 2, 100),
        group_row("c", 1, -500),
    ]);

    let (status, body) = call(
        &health.handler,
        TestRequest::get("/api/finance/health")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["integrity"]["balanced"], json!(false));
    assert_eq!(body["integrity"]["transfer_groups"], json!(3));
    assert_eq!(
        body["integrity"]["imbalanced_groups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        body["integrity"]["imbalanced_groups"][0]["transfer_group"],
        json!("b")
    );
    assert_eq!(
        body["integrity"]["imbalanced_groups"][0]["problem"],
        json!("the legs do not cancel")
    );
    assert_eq!(
        body["integrity"]["imbalanced_groups"][1]["problem"],
        json!("a transfer is two entries")
    );

    // A funds total that disagrees with the ledger is the other failure.
    host.db.push_rows(vec![
        json!({ "ledger_total_cents": 70_000, "entries": 5, "unpaired_transfers": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "fund_total_cents": 69_999 })]);
    host.db.push_rows(vec![group_row("a", 2, 0)]);
    let (status, body) = call(
        &health.handler,
        TestRequest::get("/api/finance/health")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["integrity"]["balanced"], json!(false));
    assert_eq!(body["integrity"]["difference_cents"], json!(1));
    assert_eq!(
        body["integrity"]["imbalanced_groups"],
        json!([]),
        "the groups are fine; the totals are not"
    );
}

// ---------------------------------------------------------------------------
// payment.received → a booked entry
// ---------------------------------------------------------------------------

fn payment_event(payload: Value) -> Event {
    Event {
        id: 1,
        event_type: event_type::PAYMENT_RECEIVED.to_string(),
        payload,
        source: "stripe".into(),
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn a_payment_received_books_income_and_a_replay_is_a_no_op() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].filter, event_type::PAYMENT_RECEIVED);

    // 1. the idempotency probe: nothing holds this payment id yet.
    // 2. the fund the payment names.
    // 3. the entry.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![json!({
        "id": 2, "code": "scholarship", "name": "Scholarship Fund"
    })]);
    host.db
        .push_rows(vec![transaction_row(21, 2, 15_000, "income")]);

    (subs[0].handler)(payment_event(json!({
        "payment_id": "pi_999",
        "amount_cents": 15_000,
        "fund_code": "scholarship",
        "member_id": "bea",
        "description": "Gift",
        "occurred_on": "2026-02-01",
        "fiscal_year": 2026,
    })))
    .await
    .unwrap();

    let insert = find_statement(&host, "INSERT INTO");
    let params = params_of(&host, "INSERT INTO");
    assert!(insert.contains("ON CONFLICT (external_ref) DO NOTHING"));
    assert!(
        params.iter().any(|p| p.contains("pi_999")),
        "the provider's id is the idempotency key: {params:?}"
    );
    assert!(params.iter().any(|p| p.contains("Int(15000)")));
    // The fund is named by code and bound by id — the resolve ran first.
    let fund_lookup = params_of(&host, "WHERE f.code = $1");
    assert!(
        fund_lookup.iter().any(|p| p.contains("scholarship")),
        "{fund_lookup:?}"
    );
    assert!(params.iter().any(|p| p.contains("Int(2)")), "{params:?}");
    host.events.assert_published("finance.payment.recorded");
    assert_audited(&host, "payment.received");
    assert_eq!(host.db.query_count(), 3);

    // A replay finds the reference and stops: one query, no second deposit.
    host.db.push_rows(vec![json!({ "id": 21 })]);
    let before = host.db.query_count();
    (subs[0].handler)(payment_event(json!({
        "payment_id": "pi_999",
        "amount_cents": 15_000,
        "fund_code": "scholarship",
    })))
    .await
    .unwrap();
    assert_eq!(
        host.db.query_count(),
        before + 1,
        "only the probe ran — a replayed payment is not a second deposit"
    );
}

#[tokio::test]
async fn a_payment_that_cannot_be_believed_is_reported_rather_than_guessed() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();

    // No payment id: without it a replay cannot be told from a second payment.
    let error = (subs[0].handler)(payment_event(json!({ "amount_cents": 500 })))
        .await
        .expect_err("a payload with no payment_id must not be guessed at");
    assert!(matches!(error, SdkError::BadRequest(_)), "{error}");

    let error = (subs[0].handler)(payment_event(json!({
        "payment_id": "pi_0", "amount_cents": 0
    })))
    .await
    .expect_err("a zero payment is not a deposit");
    assert!(matches!(error, SdkError::BadRequest(_)), "{error}");

    let error = (subs[0].handler)(payment_event(json!({
        "payment_id": "pi_neg", "amount_cents": -500
    })))
    .await
    .expect_err("a negative payment is not a deposit");
    assert!(matches!(error, SdkError::BadRequest(_)), "{error}");

    // A payload that is not a payment at all.
    let error = (subs[0].handler)(payment_event(json!({ "nope": true })))
        .await
        .expect_err("a malformed payload must not be guessed at");
    assert!(matches!(error, SdkError::BadRequest(_)), "{error}");

    assert_eq!(host.db.query_count(), 0, "none of these reached the db");

    // A payment naming a fund that does not exist is reported, not silently lost.
    host.db.push_rows(vec![]); // the probe: not already recorded
    host.db.push_rows(vec![]); // the fund: not there
    let error = (subs[0].handler)(payment_event(json!({
        "payment_id": "pi_1", "amount_cents": 500, "fund_code": "nope"
    })))
    .await
    .expect_err("an unknown fund is a configuration error");
    assert!(error.to_string().contains("nope"), "{error}");
}

// ---------------------------------------------------------------------------
// The scheduled ledger audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ledger_audit_publishes_only_when_the_books_do_not_add_up() {
    let host = TestHost::new();
    let mut plugin = FinancePlugin::new();
    plugin.init(host.context("finance")).await.unwrap();

    let schedules = plugin.schedules();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].name, "ledger_audit");
    assert_eq!(
        schedules[0].every,
        std::time::Duration::from_secs(24 * 60 * 60)
    );

    // A healthy ledger: the schedule stays silent (a silent watch is a happy one).
    host.db.push_rows(vec![
        json!({ "ledger_total_cents": 70_000, "entries": 5, "unpaired_transfers": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "fund_total_cents": 70_000 })]);
    host.db.push_rows(vec![group_row("a", 2, 0)]);
    (schedules[0].handler)().await.unwrap();
    host.events.assert_none();

    // A broken one: it says so, with the evidence.
    host.db.push_rows(vec![
        json!({ "ledger_total_cents": 70_000, "entries": 5, "unpaired_transfers": 0 }),
    ]);
    host.db
        .push_rows(vec![json!({ "fund_total_cents": 69_000 })]);
    host.db.push_rows(vec![group_row("a", 2, 0)]);
    (schedules[0].handler)().await.unwrap();
    host.events.assert_published("finance.ledger.imbalanced");
    let payload = host
        .events
        .payloads("finance.ledger.imbalanced")
        .pop()
        .expect("the verdict travels with the alarm");
    assert_eq!(payload["balanced"], json!(false));
    assert_eq!(payload["difference_cents"], json!(1_000));
}

// ---------------------------------------------------------------------------
// The vocabulary the API and the database agree on
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The vocabulary the API and the database agree on
// ---------------------------------------------------------------------------

#[test]
fn the_vocabulary_the_migration_constrains_is_the_vocabulary_the_api_accepts() {
    // The DDL and the Rust constants are the same strings: a code the handlers
    // accept but the database refuses would be a 500 on the first real request,
    // and a code the database allows but no handler produces is dead weight.
    let plugin = FinancePlugin::new();
    let migrations = plugin.migrations();
    let ddl = &migrations[0].sql;

    for code in FUND_KINDS {
        assert!(
            ddl.contains(&format!("'{code}'")),
            "funds_kind_valid: {code}"
        );
    }
    for code in TIER_CODES {
        assert!(
            !ddl.contains(&format!("tier IN ('{code}'")),
            "the tiers are the API's vocabulary, not a database CHECK: {code} must not be \
             constrained in DDL (a troop may extend the scale)"
        );
    }
    for code in DUES_STATUSES {
        assert!(
            ddl.contains(&format!("'{code}'")),
            "dues_status_valid: {code}"
        );
    }
    for code in DUES_KINDS {
        assert!(
            ddl.contains(&format!("'{code}'")),
            "dues_kind_valid: {code}"
        );
    }
    assert!(
        ddl.contains("direction IN ('income', 'expense')"),
        "budgets_direction_valid must be the API's two directions"
    );
    assert!(
        ddl.contains("kind IN ('income', 'expense', 'transfer')"),
        "transactions_kind_valid must be the API's three kinds"
    );
    assert_eq!(DIRECT_KINDS, ["income", "expense"]);
    assert_eq!(
        DIRECTIONS, DIRECT_KINDS,
        "a budget plans exactly the directions a caller may record"
    );
    // A Lodge's share is a percentage, parsed to basis points exactly.
    assert_eq!(parse_percent_to_bps("10"), Ok(1_000));
    assert_eq!(parse_percent_to_bps("12.5"), Ok(1_250));

    // The reserved category codes the dues arithmetic keys on are the ones the
    // handlers write: `dues` for a payment, `transfer` for a transfer leg.
    assert_eq!(CATEGORY_DUES, "dues");
    assert_eq!(CATEGORY_TRANSFER, "transfer");
    assert_eq!(DEFAULT_TIER, TIER_STANDARD);

    // And the arithmetic the report leans on, stated once more where a reader
    // comparing the two files will see it.
    assert_eq!(parse_dollars_to_cents("600.00"), Ok(60_000));
    assert_eq!(format_cents(60_000), "$600.00");
    assert_eq!(budget_variance("expense", 20_000, -25_000), (-5_000, true));
    assert_eq!(ledger_verdict(0, 0, &[])["balanced"], json!(true));
    assert_eq!(scale_table(60_000)[1]["tier"], json!(TIER_STANDARD));
}
