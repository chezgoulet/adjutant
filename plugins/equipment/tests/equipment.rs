//! Equipment plugin tests: the checkout state machine, condition attribution,
//! availability, maintenance, and replacement flagging (SPEC §7.6).
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results **in call order**, so every test states the order its handler arranges
//! — the comment above each `push_*` says which call it answers. A handler that
//! makes fewer calls than the test queued is fine; one that makes more than were
//! queued sees an empty result (the mock's default), which is why the queue
//! counts here are asserted rather than assumed.
//!
//! Dates are computed from `today()` rather than written down, so the arithmetic
//! these tests pin stays the arithmetic that runs tomorrow.

use adjutant_equipment::{
    age_years, checkout_end, checkout_window, condition_rank, is_downgrade, is_overdue,
    maintenance_report, overlaps, parse_date, parse_optional_date, partition_availability,
    replacement_candidates, replacement_reasons, today, EquipmentPlugin, CATEGORIES, CONDITIONS,
    CONDITION_GOOD, CONDITION_NEW, CONDITION_POOR, CONDITION_UNSERVICEABLE, DEFAULT_AVAILABILITY_DAYS,
    ITEM_STATUSES, POOR_CONDITIONS, STATUS_AVAILABLE, STATUS_MAINTENANCE, STATUS_RETIRED, Thresholds,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use chrono::{Datelike, Duration, NaiveDate};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn plugin() -> (TestHost, EquipmentPlugin, Vec<RouteDefinition>) {
    build(TestHost::new()).await
}

async fn build(host: TestHost) -> (TestHost, EquipmentPlugin, Vec<RouteDefinition>) {
    let mut plugin = EquipmentPlugin::new();
    plugin.init(host.context("equipment")).await.unwrap();
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

fn set(mut value: Value, key: &str, field: Value) -> Value {
    value[key] = field;
    value
}

/// An item row as `ITEM_FIELDS` renders one.
fn item_row(id: i64, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "asset_tag": format!("TAG-{id}"),
        "category": "tent",
        "description": "",
        "condition": CONDITION_GOOD,
        "location": "Quartermaster shed",
        "acquired_on": Value::Null,
        "source": "",
        "service_count": 0,
        "next_service_on": Value::Null,
        "status": STATUS_AVAILABLE,
        "maintenance_since": Value::Null,
        "maintenance_until": Value::Null,
        "maintenance_note": "",
        "replacement_flagged": false,
        "replacement_note": "",
        "retired_at": Value::Null,
        "retired_reason": "",
        "created_by": "bea",
        "created_at": "2026-01-01 12:00:00+00",
        "updated_at": "2026-01-01 12:00:00+00"
    })
}

fn in_maintenance(id: i64, name: &str) -> Value {
    set(
        set(
            set(item_row(id, name), "status", json!(STATUS_MAINTENANCE)),
            "maintenance_since",
            json!("2026-09-01"),
        ),
        "maintenance_note",
        json!("split seam"),
    )
}

fn retired(id: i64, name: &str) -> Value {
    set(
        set(item_row(id, name), "status", json!(STATUS_RETIRED)),
        "retired_at",
        json!("2026-08-01 12:00:00+00"),
    )
}

/// A checkout row as `CHECKOUT_FIELDS` renders one. `closed_on` `None` is an
/// **open** checkout — the state every mutating handler asks about.
fn checkout_row(
    id: i64,
    item_id: i64,
    out_on: &str,
    closed_on: Option<&str>,
    due_on: Option<&str>,
) -> Value {
    let closed = closed_on.is_some();
    json!({
        "id": id,
        "item_id": item_id,
        "item_name": format!("Item {item_id}"),
        "asset_tag": format!("TAG-{item_id}"),
        "category": "tent",
        "checked_out_by": "bea",
        "checked_out_at": format!("{out_on} 12:00:00+00"),
        "checked_out_on": out_on,
        "due_on": due_on,
        "purpose": "Coyote survey",
        "mission_id": 12,
        "destination": "Windsor",
        "condition_out": CONDITION_GOOD,
        "note_out": "",
        "checked_in_at": closed_on.map(|d| json!(format!("{d} 12:00:00+00"))).unwrap_or(Value::Null),
        "checked_in_on": closed_on,
        "checked_in_by": if closed { json!("bea") } else { Value::Null },
        "condition_in": if closed { json!(CONDITION_GOOD) } else { Value::Null },
        "note_in": "",
        "damaged": false,
        "open": !closed,
    })
}

fn days(n: i64) -> NaiveDate {
    today() + Duration::days(n)
}

/// A calendar date `n` whole years back — a leap day must not turn "six years
/// old" into "five years old" in a threshold test.
fn years_ago(n: i32) -> String {
    NaiveDate::from_ymd_opt(today().year() - n, 1, 1)
        .expect("a valid date")
        .to_string()
}

fn body_json(resp: &PluginResponse) -> Value {
    response_json(resp)
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "equipment");
    assert_eq!(plugin.name(), "Equipment");
    assert_eq!(plugin.version(), env!("CARGO_PKG_VERSION"));

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for perm in &permissions {
        assert!(perm.starts_with("equipment:"), "{perm} must be namespaced");
    }
    for expected in [
        "equipment:read",
        "equipment:write",
        "equipment:checkout",
        "equipment:manage",
    ] {
        assert!(permissions.iter().any(|p| p == expected), "missing {expected}");
    }

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    let ddl = &migrations[0].sql;
    for table in ["items", "checkouts"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "SPEC §7.6's schema is missing {table}"
        );
    }
    // The database enforces the two rules the state machine rests on.
    assert!(
        ddl.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_checkouts_open_item")
            && ddl.contains("WHERE checked_in_at IS NULL"),
        "the partial unique index is what makes 'checked out twice' impossible"
    );
    assert!(
        ddl.contains("REFERENCES items(id) ON DELETE CASCADE"),
        "a checkout names an actual item"
    );
    for constraint in [
        "items_condition_valid",
        "items_status_valid",
        "items_maintenance_consistent",
        "items_retired_consistent",
        "checkouts_condition_out_valid",
        "checkouts_returned_consistent",
    ] {
        assert!(ddl.contains(constraint), "migration is missing {constraint}");
    }
    // An item's vocabulary has to agree with the codes the handlers use.
    for grade in CONDITIONS {
        assert!(ddl.contains(&format!("'{grade}'")), "DDL is missing grade {grade}");
    }
    for status in ITEM_STATUSES {
        assert!(ddl.contains(&format!("'{status}'")), "DDL is missing status {status}");
    }

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/equipment"),
            "{} escapes the namespace",
            r.path
        );
        let perm = r.required_permission.as_deref().expect("every route is gated");
        assert!(
            permissions.iter().any(|p| p == perm),
            "{perm} is required but not declared"
        );
        assert_eq!(
            r.required_scope,
            Some(Scope::troop()),
            "{} must be troop-scoped: SPEC §7.6 gives equipment no Lodge authority",
            r.path
        );
        let normalized = r
            .path
            .split('/')
            .map(|s| if s.starts_with('{') { "{}".to_string() } else { s.to_string() })
            .collect::<Vec<_>>()
            .join("/");
        let key = (r.method.as_str().to_string(), normalized);
        assert!(!seen.contains(&key), "duplicate route {key:?}");
        seen.push(key);
    }
    // The five SPEC §7.6 responsibilities each have an address.
    for (method, path) in [
        ("POST", "/api/equipment/item"),
        ("GET", "/api/equipment/availability"),
        ("GET", "/api/equipment/maintenance"),
        ("GET", "/api/equipment/replacements"),
        ("POST", "/api/equipment/item/{id}/checkout"),
        ("POST", "/api/equipment/item/{id}/checkin"),
    ] {
        route(&routes, method, path);
    }
    assert_eq!(routes.len(), 16, "declared route count");
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_equipment::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "equipment");
    assert_eq!(
        adjutant_equipment::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

// ---------------------------------------------------------------------------
// Pure arithmetic
// ---------------------------------------------------------------------------

#[test]
fn condition_ranks_run_best_to_worst_and_unknown_is_worst() {
    assert!(condition_rank(CONDITION_NEW) < condition_rank(CONDITION_GOOD));
    assert!(condition_rank(CONDITION_GOOD) < condition_rank("fair"));
    assert!(condition_rank("fair") < condition_rank(CONDITION_POOR));
    assert!(condition_rank(CONDITION_POOR) < condition_rank(CONDITION_UNSERVICEABLE));
    // A grade that cannot be read must not be read as "no damage".
    assert_eq!(
        condition_rank("mangled"),
        condition_rank(CONDITION_UNSERVICEABLE)
    );
    assert!(is_downgrade(CONDITION_GOOD, CONDITION_POOR));
    assert!(!is_downgrade(CONDITION_POOR, CONDITION_GOOD));
    assert!(!is_downgrade(CONDITION_GOOD, CONDITION_GOOD));
}

#[test]
fn age_counts_completed_years() {
    let acquired = parse_date("2016-05-01").unwrap();
    assert_eq!(age_years(acquired, parse_date("2026-04-30").unwrap()), 9);
    assert_eq!(age_years(acquired, parse_date("2026-05-01").unwrap()), 10);
    assert_eq!(age_years(acquired, parse_date("2026-06-01").unwrap()), 10);
    assert_eq!(age_years(acquired, parse_date("2016-04-01").unwrap()), 0);
}

#[test]
fn date_parsing_is_strict_on_the_wall_clock() {
    assert_eq!(parse_date("2026-10-06").unwrap(), parse_date("2026-10-06").unwrap());
    assert!(parse_date("2026-10-06T18:00").is_err());
    assert!(parse_date("06/10/2026").is_err());
    assert!(parse_date("").is_err());
    assert_eq!(parse_optional_date(Some("  ")).unwrap(), None);
    assert_eq!(parse_optional_date(None).unwrap(), None);
    assert!(parse_optional_date(Some("nonsense")).is_err());
}

#[test]
fn overlap_is_an_inclusive_day_range() {
    let from = parse_date("2026-10-03").unwrap();
    let to = parse_date("2026-10-05").unwrap();
    // Touching at either end counts: the item is wanted on the 3rd and the 5th.
    assert!(overlaps(from, to, from, Some(to)));
    assert!(overlaps(from, to, parse_date("2026-10-05").unwrap(), Some(parse_date("2026-10-09").unwrap())));
    assert!(overlaps(from, to, parse_date("2026-09-01").unwrap(), Some(from)));
    assert!(!overlaps(from, to, parse_date("2026-10-06").unwrap(), Some(parse_date("2026-10-09").unwrap())));
    assert!(!overlaps(from, to, parse_date("2026-09-01").unwrap(), Some(parse_date("2026-10-02").unwrap())));
    // An open-ended checkout blocks every window that starts after it went out.
    assert!(overlaps(from, to, parse_date("2026-01-01").unwrap(), None));
    assert!(!overlaps(from, to, parse_date("2026-11-01").unwrap(), None));
}

#[test]
fn checkout_end_respects_the_promise_of_a_due_date() {
    let on = today();
    let future = days(3);
    let past = days(-3);
    // Closed: it ends the day it came back.
    assert_eq!(checkout_end(false, Some(past), Some(future), on), Some(past));
    // Open with a due date still ahead: the promise stands.
    assert_eq!(checkout_end(true, None, Some(future), on), Some(future));
    // Open and overdue, or open with no due date: no reliable end.
    assert_eq!(checkout_end(true, None, Some(past), on), None);
    assert_eq!(checkout_end(true, None, None, on), None);
    assert!(is_overdue(true, Some(past), on));
    assert!(!is_overdue(true, Some(future), on));
    assert!(!is_overdue(false, Some(past), on), "a returned item is not overdue");
}

#[test]
fn checkout_window_reads_the_row_columns() {
    let on = today();
    let row = checkout_row(7, 1, &days(-2).to_string(), None, Some(&days(4).to_string()));
    let (start, end) = checkout_window(&row, on).expect("a parsable window");
    assert_eq!(start, days(-2));
    assert_eq!(end, Some(days(4)));
    let closed = checkout_row(8, 1, &days(-9).to_string(), Some(&days(-5).to_string()), None);
    let (start, end) = checkout_window(&closed, on).expect("a parsable window");
    assert_eq!(start, days(-9));
    assert_eq!(end, Some(days(-5)));
}

// ---------------------------------------------------------------------------
// Thresholds and replacement flagging
// ---------------------------------------------------------------------------

#[test]
fn thresholds_come_from_config_and_are_clamped() {
    let defaults = Thresholds::from_config(&Value::Null);
    assert_eq!(defaults.replacement_service_count, 40);
    assert_eq!(defaults.replacement_conditions, POOR_CONDITIONS.to_vec());

    let configured = Thresholds::from_config(&json!({
        "replacement_service_count": 5,
        "replacement_age_years": 3,
        "maintenance_lead_days": 30,
        "replacement_conditions": ["poor", "nonsense", "poor"]
    }));
    assert_eq!(configured.replacement_service_count, 5);
    assert_eq!(configured.replacement_age_years, 3);
    assert_eq!(configured.maintenance_lead_days, 30);
    assert_eq!(configured.replacement_conditions, vec!["poor".to_string()]);

    // A typo cannot flag the whole catalogue, nor quiet it forever.
    let clamped = Thresholds::from_config(&json!({
        "replacement_service_count": 0,
        "replacement_age_years": 100_000,
        "replacement_conditions": []
    }));
    assert_eq!(clamped.replacement_service_count, 1);
    assert_eq!(clamped.replacement_age_years, 200);
    assert_eq!(clamped.replacement_conditions, POOR_CONDITIONS.to_vec());
}

#[test]
fn replacement_reasons_name_each_rule() {
    let on = today();
    let t = Thresholds {
        replacement_service_count: 10,
        replacement_age_years: 5,
        ..Thresholds::default()
    };

    let fine = item_row(1, "Good tent");
    assert!(replacement_reasons(&fine, on, &t).is_empty());

    let poor = set(item_row(2, "Split tent"), "condition", json!(CONDITION_POOR));
    assert_eq!(replacement_reasons(&poor, on, &t), vec!["condition_poor".to_string()]);

    let worn = set(item_row(3, "Worn stove"), "service_count", json!(12));
    assert_eq!(replacement_reasons(&worn, on, &t), vec!["service_count".to_string()]);

    let old = set(item_row(4, "Old lantern"), "acquired_on", json!(years_ago(6)));
    assert_eq!(replacement_reasons(&old, on, &t), vec!["age".to_string()]);

    let flagged = set(item_row(5, "Radio"), "replacement_flagged", json!(true));
    assert_eq!(replacement_reasons(&flagged, on, &t), vec!["flagged".to_string()]);

    // Unserviceable and worn and old: every rule that fires is named, so a
    // reader sees the whole case rather than the first reason.
    let hopeless = set(
        set(
            set(item_row(6, "Dead stove"), "condition", json!(CONDITION_UNSERVICEABLE)),
            "service_count",
            json!(50),
        ),
        "acquired_on",
        json!(years_ago(30)),
    );
    assert_eq!(
        replacement_reasons(&hopeless, on, &t),
        vec![
            "condition_unserviceable".to_string(),
            "service_count".to_string(),
            "age".to_string()
        ]
    );

    // A raised age threshold takes the age reason away: the rules are config.
    let patient = Thresholds {
        replacement_age_years: 50,
        ..t.clone()
    };
    assert_eq!(replacement_reasons(&old, on, &patient), Vec::<String>::new());
}

#[test]
fn candidates_are_ordered_worst_first_and_exclude_the_fine() {
    let on = today();
    let t = Thresholds {
        replacement_service_count: 10,
        ..Thresholds::default()
    };
    let items = vec![
        item_row(1, "Fine"),
        set(item_row(2, "Poor"), "condition", json!(CONDITION_POOR)),
        set(
            set(item_row(3, "Worse"), "condition", json!(CONDITION_UNSERVICEABLE)),
            "service_count",
            json!(11),
        ),
    ];
    let candidates = replacement_candidates(&items, on, &t);
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["item"]["id"], 3, "two reasons leads");
    assert_eq!(candidates[1]["item"]["id"], 2);
    assert_eq!(candidates[0]["reasons"].as_array().unwrap().len(), 2);
    assert!(candidates[0]["age_years"].is_null(), "no acquisition date, no age");
}

// ---------------------------------------------------------------------------
// Availability (pure)
// ---------------------------------------------------------------------------

#[test]
fn availability_subtracts_items_on_overlapping_checkouts() {
    let on = today();
    let from = days(1);
    let to = days(5);
    let items = vec![
        item_row(1, "Free tent"),
        item_row(2, "Tent out now"),
        item_row(3, "Tent back last week"),
        in_maintenance(4, "Stove in service"),
        set(item_row(5, "Dead radio"), "condition", json!(CONDITION_UNSERVICEABLE)),
        retired(6, "Old rope"),
    ];

    let from_str = |d: NaiveDate| d.to_string();
    let checkouts = vec![
        // Open, due after the window: blocks it.
        checkout_row(100, 2, &from_str(days(-1)), None, Some(&from_str(days(9)))),
        // Returned before the window opened: does not block.
        checkout_row(101, 3, &from_str(days(-20)), Some(&from_str(days(-2))), None),
    ];

    let pool = partition_availability(&items, &checkouts, from, to, on);
    let available: Vec<i64> = pool.available.iter().filter_map(|i| i["id"].as_i64()).collect();
    assert_eq!(available, vec![1, 3]);

    let reasons: Vec<(i64, Vec<String>)> = pool
        .unavailable
        .iter()
        .map(|entry| {
            (
                entry["item"]["id"].as_i64().unwrap_or_default(),
                entry["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            (2, vec!["checked_out".to_string()]),
            (4, vec!["in_maintenance".to_string()]),
            (5, vec!["unserviceable".to_string()]),
            (6, vec!["retired".to_string()]),
        ]
    );

    // The refusal names who has it and when it is due — a planner's next call.
    let held = &pool.unavailable[0];
    assert_eq!(held["blocking"][0]["held_by"], "bea");
    assert_eq!(held["blocking"][0]["due_on"], days(9).to_string());
    assert_eq!(held["blocking"][0]["overdue"], false);

    let counts = pool.reason_counts();
    assert_eq!(counts["checked_out"], 1);
    assert_eq!(counts["in_maintenance"], 1);
}

#[test]
fn an_overdue_open_checkout_keeps_its_item_out_of_the_pool() {
    let on = today();
    let from = days(1);
    let to = days(5);
    // Due three days ago and never returned: the item is gone until somebody
    // records what happened to it, however old the promise is.
    let checkouts = vec![checkout_row(
        100,
        1,
        &days(-30).to_string(),
        None,
        Some(&days(-3).to_string()),
    )];
    let pool = partition_availability(&[item_row(1, "Tent")], &checkouts, from, to, on);
    assert!(pool.available.is_empty());
    assert_eq!(pool.unavailable[0]["blocking"][0]["overdue"], true);
}

#[test]
fn a_checkout_touching_the_window_edge_still_blocks() {
    let on = today();
    let items = vec![item_row(1, "Tent")];
    // Back on the first day of the window.
    let back_on_the_edge = vec![checkout_row(
        100,
        1,
        &days(-10).to_string(),
        Some(&days(1).to_string()),
        None,
    )];
    let pool = partition_availability(&items, &back_on_the_edge, days(1), days(5), on);
    assert!(pool.available.is_empty(), "the item is out on the first day asked for");

    // Back the day before it.
    let back_before = vec![checkout_row(
        101,
        1,
        &days(-10).to_string(),
        Some(&days(0).to_string()),
        None,
    )];
    let pool = partition_availability(&items, &back_before, days(1), days(5), on);
    assert_eq!(pool.available.len(), 1);
}

// ---------------------------------------------------------------------------
// Maintenance (pure)
// ---------------------------------------------------------------------------

#[test]
fn maintenance_buckets_due_overdue_and_unscheduled() {
    let on = today();
    let t = Thresholds {
        maintenance_lead_days: 14,
        ..Thresholds::default()
    };
    let items = vec![
        in_maintenance(1, "Stove"),
        set(item_row(2, "Tent"), "next_service_on", json!(days(-2).to_string())),
        set(item_row(3, "Lantern"), "next_service_on", json!(days(3).to_string())),
        set(item_row(4, "Rope"), "next_service_on", json!(days(60).to_string())),
        set(item_row(5, "Split pack"), "condition", json!(CONDITION_POOR)),
        item_row(6, "Fine stove"),
    ];
    let report = maintenance_report(&items, on, &t);
    assert_eq!(report.in_service.len(), 1);
    assert_eq!(report.overdue.len(), 1);
    assert_eq!(report.due.len(), 1);
    assert_eq!(report.scheduled.len(), 1);
    assert_eq!(report.needs_schedule.len(), 1);
    assert_eq!(report.needs_schedule[0]["id"], 5);
    assert!(report.has_due_work());
    assert_eq!(report.due_item_ids(), vec![1, 2, 3]);

    let quiet = maintenance_report(&[item_row(6, "Fine stove")], on, &t);
    assert!(!quiet.has_due_work(), "nothing due means no reminder at all");
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_item_records_and_announces_it() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item").handler;
    host.db.push_rows(Vec::new()); // the asset_tag check: not taken
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the INSERT … RETURNING

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item")
            .param("id", "1")
            .json(&json!({
                "name": "  Dome tent  ",
                "asset_tag": "TENT-001",
                "category": "tent",
                "condition": "new",
                "location": "Shed"
            }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["item"]["name"], "Dome tent");
    assert_eq!(
        host.db.query_count(),
        2,
        "the asset_tag check, then the insert"
    );
    let insert = &host.db.queried_sql()[1];
    assert!(insert.contains("INSERT INTO"), "{insert}");
    assert!(insert.contains("\"condition\""), "the reserved word is quoted: {insert}");
    assert_audited(&host, "item.create");
    host.events.assert_published("equipment.item.created");
}

#[tokio::test]
async fn a_duplicate_asset_tag_is_refused_without_writing() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item").handler;
    host.db.push_rows(vec![json!({ "taken": 1 })]); // the asset_tag existence check

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item")
            .json(&json!({ "name": "Tent", "asset_tag": "TENT-001" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("TENT-001"));
    assert_eq!(host.db.query_count(), 1, "the insert never ran");
    host.events.assert_none();
}

#[tokio::test]
async fn an_unknown_category_or_condition_is_a_bad_request() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item").handler;
    // No database call: validation comes first.
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item")
            .json(&json!({ "name": "Tent", "category": "spaceship" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains(CATEGORIES[0]));
    assert_eq!(host.db.query_count(), 0);

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item")
            .json(&json!({ "name": "Tent", "condition": "pristine" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains(CONDITION_GOOD));
}

#[tokio::test]
async fn list_items_narrows_to_the_filters_asked_for() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/items").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]);

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/items")
            .query_param("category", "tent")
            .query_param("q", "dome")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["count"], 1);
    let sql = &host.db.queried_sql()[0];
    assert!(sql.contains("ILIKE"), "the free-text filter: {sql}");
    assert!(sql.contains("i.status <> 'retired'"), "retired is hidden by default: {sql}");

    let (status, _body) = call(
        handler,
        TestRequest::get("/api/equipment/items")
            .query_param("status", "exploded")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn item_detail_reports_the_open_checkout_and_the_flags() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/item/{id}").handler;
    let worn = set(
        set(item_row(4, "Old stove"), "acquired_on", json!(years_ago(12))),
        "service_count",
        json!(41),
    );
    host.db.push_rows(vec![worn.clone()]); // the item
    host.db.push_rows(vec![checkout_row(9, 4, &days(-3).to_string(), None, Some(&days(2).to_string()))]); // open checkout
    host.db.push_rows(vec![checkout_row(9, 4, &days(-3).to_string(), None, Some(&days(2).to_string()))]); // history

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/item/4")
            .param("id", "4")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["open_checkout"]["checked_out_by"], "bea");
    assert_eq!(body["checkouts"].as_array().unwrap().len(), 1);
    assert_eq!(body["flags"]["in_pool"], true);
    assert_eq!(body["flags"]["replacement"]["candidate"], true);
    let reasons = body["flags"]["replacement"]["reasons"].as_array().unwrap();
    assert!(reasons.iter().any(|r| r == "age"), "{reasons:?}");
    assert!(reasons.iter().any(|r| r == "service_count"), "{reasons:?}");
    assert_eq!(body["flags"]["replacement"]["age_years"], 12);
    assert_eq!(host.db.query_count(), 3);
}

// ---------------------------------------------------------------------------
// Checkout / checkin — the state machine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkout_records_the_holder_the_condition_and_the_promise() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkout").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the item
    host.db.push_rows(Vec::new()); // no open checkout
    host.db.push_rows(vec![checkout_row(20, 1, &days(0).to_string(), None, Some(&days(4).to_string()))]); // the insert

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkout")
            .param("id", "1")
            .json(&json!({
                "checked_out_by": "bea",
                "due_on": days(4).to_string(),
                "purpose": "Coyote survey",
                "mission_id": 12,
                "condition": CONDITION_GOOD
            }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["checkout"]["checked_out_by"], "bea");
    assert_eq!(host.db.query_count(), 3);
    let insert = &host.db.queried_sql()[2];
    assert!(insert.contains("INSERT INTO"), "{insert}");
    assert!(insert.contains("($3::text)::date"), "dates bind as text and cast: {insert}");
    assert_audited(&host, "item.checkout");
    let published = host.events.payloads("equipment.checked_out");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["condition_out"], CONDITION_GOOD);
    assert_eq!(published[0]["checked_out_by"], "bea");
    assert_eq!(published[0]["mission_id"], 12);
}

#[tokio::test]
async fn checkout_defaults_the_holder_to_the_caller() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkout").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]);
    host.db.push_rows(Vec::new());
    host.db.push_rows(vec![checkout_row(21, 1, &days(0).to_string(), None, None)]);

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkout")
            .param("id", "1")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let params = host.db.last_query_params("INSERT INTO").expect("the insert");
    assert!(
        matches!(&params[1], SqlValue::Text(who) if who == "bea"),
        "the holder defaults to the caller: {params:?}"
    );
    assert!(
        matches!(&params[2], SqlValue::Null),
        "no due date must be a text null that SQL casts to date: {params:?}"
    );
}

#[tokio::test]
async fn an_item_cannot_be_checked_out_twice() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkout").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the item
    host.db.push_rows(vec![checkout_row(20, 1, &days(-2).to_string(), None, Some(&days(1).to_string()))]); // already open

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkout")
            .param("id", "1")
            .json(&json!({ "checked_out_by": "cal" }))
            .identity("cal", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 409);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("already checked out to bea"), "{error}");
    assert!(error.contains("check it in first"), "{error}");
    assert_eq!(host.db.query_count(), 2, "the insert never ran");
    assert!(
        host.db.queried_sql().iter().all(|sql| !sql.contains("INSERT INTO")),
        "a refused checkout writes nothing"
    );
    host.events.assert_none();
}

#[tokio::test]
async fn checkout_refuses_an_item_that_is_not_in_the_pool() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkout").handler;

    // In maintenance: out of the pool, and the answer says how to get it back.
    host.db.push_rows(vec![in_maintenance(3, "Stove")]);
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/3/checkout")
            .param("id", "3")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("return-to-service"));
    assert_eq!(host.db.query_count(), 1);

    host.db.push_rows(vec![retired(4, "Old rope")]);
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/4/checkout")
            .param("id", "4")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("retired"));

    host.db.push_rows(vec![set(
        item_row(5, "Dead radio"),
        "condition",
        json!(CONDITION_UNSERVICEABLE),
    )]);
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/5/checkout")
            .param("id", "5")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("unserviceable"));
}

#[tokio::test]
async fn checkout_requires_an_identifiable_holder() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkout").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]);
    host.db.push_rows(Vec::new());

    // No body holder and no identity to fall back on.
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkout")
            .param("id", "1")
            .json(&json!({}))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("checked_out_by"));
    assert_eq!(host.db.query_count(), 2, "nothing was written");
}

#[tokio::test]
async fn checkin_records_the_condition_and_attributes_the_damage() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkin").handler;
    let open = checkout_row(20, 1, &days(-5).to_string(), None, Some(&days(1).to_string()));
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the item, good
    host.db.push_rows(vec![open.clone()]); // the open checkout, condition_out good
    host.db.push_rows(vec![set(  // the closed checkout
        set(open, "checked_in_on", json!(days(0))),
        "damaged",
        json!(true),
    )]);
    let returned = set(
        set(item_row(1, "Dome tent"), "condition", json!(CONDITION_POOR)),
        "service_count",
        json!(1),
    );
    host.db.push_rows(vec![returned]); // the item update

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkin")
            .param("id", "1")
            .json(&json!({ "condition": "poor", "note": "seam split" }))
            .identity("cal", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["damaged"], true, "good out, poor back is the damage question");
    assert_eq!(body["service_count"], 1);
    assert_eq!(body["item"]["condition"], CONDITION_POOR);
    assert_eq!(body["new_replacement_candidate"], true);
    assert_eq!(body["flags"]["replacement"]["candidate"], true);
    assert_eq!(host.db.query_count(), 4);

    let close = &host.db.queried_sql()[2];
    assert!(close.contains("AND c.checked_in_at IS NULL"), "only an open one closes: {close}");
    let update = &host.db.queried_sql()[3];
    assert!(update.contains("service_count = i.service_count + 1"), "{update}");

    assert_audited(&host, "item.checkin");
    let published = host.events.payloads("equipment.checked_in");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["condition_out"], CONDITION_GOOD);
    assert_eq!(published[0]["condition_in"], CONDITION_POOR);
    assert_eq!(published[0]["damaged"], true);
    // Becoming a candidate is announced once, as a transition.
    host.events.assert_published("equipment.replacement.flagged");
    let flagged = host.events.payloads("equipment.replacement.flagged");
    assert_eq!(flagged[0]["by"], "checkin");
    assert_eq!(flagged[0]["reasons"][0], "condition_poor");
    assert!(body["item"]["maintenance_hint"].as_str().unwrap().contains("maintenance"));
}

#[tokio::test]
async fn a_clean_checkin_announces_no_damage_and_no_candidate() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkin").handler;
    let open = checkout_row(20, 1, &days(-2).to_string(), None, None);
    host.db.push_rows(vec![item_row(1, "Dome tent")]);
    host.db.push_rows(vec![open.clone()]);
    host.db.push_rows(vec![set(open, "checked_in_on", json!(days(0)))]);
    host.db.push_rows(vec![set(item_row(1, "Dome tent"), "service_count", json!(1))]);

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkin")
            .param("id", "1")
            .json(&json!({ "condition": "good" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["damaged"], false);
    assert_eq!(body["new_replacement_candidate"], false);
    assert_eq!(
        host.events.published_types(),
        vec!["equipment.checked_in".to_string()],
        "a clean return is one event: checked in"
    );
}

#[tokio::test]
async fn checkin_without_an_open_checkout_is_refused() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkin").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the item
    host.db.push_rows(Vec::new()); // no open checkout

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkin")
            .param("id", "1")
            .json(&json!({ "condition": "good" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("no open checkout"));
    assert_eq!(host.db.query_count(), 2, "nothing was closed");
    assert!(host.db.executed_sql().is_empty(), "and nothing was audited as if it had");
    host.events.assert_none();
}

#[tokio::test]
async fn a_checkin_must_state_the_condition() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/checkin").handler;
    // No database call at all: the condition is validated first.
    let (status, _body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkin")
            .param("id", "1")
            .json(&json!({}))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/checkin")
            .param("id", "1")
            .json(&json!({ "condition": "mangled" }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains(CONDITION_NEW));
    assert_eq!(host.db.query_count(), 0);
}

// ---------------------------------------------------------------------------
// Maintenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flagging_maintenance_takes_the_item_out_of_the_pool() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/maintenance").handler;
    host.db.push_rows(vec![item_row(2, "Two-burner stove")]); // the item
    host.db.push_rows(Vec::new()); // not checked out
    host.db.push_rows(vec![in_maintenance(2, "Two-burner stove")]); // the update

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/2/maintenance")
            .param("id", "2")
            .json(&json!({ "reason": "seized valve", "until": days(21).to_string() }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["status"], STATUS_MAINTENANCE);
    assert_eq!(body["flags"]["in_pool"], false);
    assert_eq!(body["flags"]["maintenance"]["in_service"], true);
    let update = &host.db.queried_sql()[2];
    assert!(update.contains("status = 'maintenance'"), "{update}");
    assert!(
        update.contains("AND i.status = 'available'"),
        "the transition is guarded, not assumed: {update}"
    );
    assert_audited(&host, "item.maintenance.flag");
    host.events.assert_published("equipment.maintenance.flagged");
}

#[tokio::test]
async fn maintenance_needs_a_reason_and_an_item_that_is_present() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/maintenance").handler;

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/2/maintenance")
            .param("id", "2")
            .json(&json!({ "reason": "   " }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("reason"));
    assert_eq!(host.db.query_count(), 0);

    // Already in maintenance: the state machine says so rather than re-flagging.
    host.db.push_rows(vec![in_maintenance(2, "Stove")]);
    let (status, _body) = call(
        handler,
        TestRequest::post("/api/equipment/item/2/maintenance")
            .param("id", "2")
            .json(&json!({ "reason": "again" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(host.db.query_count(), 1);

    // Retired: already out of the pool.
    host.db.push_rows(vec![retired(3, "Old rope")]);
    let (status, _body) = call(
        handler,
        TestRequest::post("/api/equipment/item/3/maintenance")
            .param("id", "3")
            .json(&json!({ "reason": "why not" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn an_item_that_is_somewhere_else_cannot_be_flagged_for_service() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/maintenance").handler;
    host.db.push_rows(vec![item_row(2, "Stove")]);
    host.db.push_rows(vec![checkout_row(30, 2, &days(-1).to_string(), None, Some(&days(3).to_string()))]);

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/2/maintenance")
            .param("id", "2")
            .json(&json!({ "reason": "seized valve" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 409);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("checked out to bea"), "{error}");
    assert!(error.contains("check it in"), "{error}");
    assert_eq!(host.db.query_count(), 2);
}

#[tokio::test]
async fn return_to_service_puts_the_item_back_in_the_pool() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/return-to-service").handler;
    host.db.push_rows(vec![in_maintenance(2, "Stove")]); // the item, in service
    let back = set(
        set(item_row(2, "Stove"), "condition", json!("good")),
        "maintenance_note",
        json!(""),
    );
    host.db.push_rows(vec![back]); // the update

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/2/return-to-service")
            .param("id", "2")
            .json(&json!({ "condition": "good", "note": "valve replaced" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["status"], STATUS_AVAILABLE);
    assert_eq!(body["flags"]["in_pool"], true);
    let update = &host.db.queried_sql()[1];
    assert!(update.contains("maintenance_since = NULL"), "{update}");
    assert!(update.contains("retired_at = NULL"), "a retirement is undoable in the same write");
    assert!(update.contains("($3::text)::date"), "dates bind as text and cast: {update}");
    assert_audited(&host, "item.maintenance.clear");
    host.events.assert_published("equipment.maintenance.cleared");

    // An item that is already in the pool has nothing to clear.
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/return-to-service").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]);
    let (status, _body) = call(
        handler,
        TestRequest::post("/api/equipment/item/1/return-to-service")
            .param("id", "1")
            .json(&json!({}))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(host.db.query_count(), 1);
}

#[tokio::test]
async fn service_can_be_scheduled_without_leaving_the_pool() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/schedule-service").handler;
    host.db.push_rows(vec![item_row(3, "Lantern")]); // the item
    let scheduled = set(
        item_row(3, "Lantern"),
        "next_service_on",
        json!(days(5).to_string()),
    );
    host.db.push_rows(vec![scheduled]); // the update

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/3/schedule-service")
            .param("id", "3")
            .json(&json!({ "on": days(5).to_string(), "note": "re-wick" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["due_in_days"], 5);
    assert_eq!(body["due"], true, "5 days is inside the default 14-day lead");
    assert_eq!(body["item"]["status"], STATUS_AVAILABLE, "still in the pool");
    assert_audited(&host, "item.maintenance.schedule");
    host.events.assert_published("equipment.maintenance.scheduled");

    // A date in the past is overdue, not scheduled.
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/schedule-service").handler;
    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/3/schedule-service")
            .param("id", "3")
            .json(&json!({ "on": days(-1).to_string() }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("past"));
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn retiring_removes_the_item_and_can_be_undone() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "POST", "/api/equipment/item/{id}/retire").handler;
    host.db.push_rows(vec![in_maintenance(5, "Dead stove")]); // retiring from maintenance
    host.db.push_rows(Vec::new()); // not checked out
    host.db.push_rows(vec![retired(5, "Dead stove")]); // the update

    let (status, body) = call(
        handler,
        TestRequest::post("/api/equipment/item/5/retire")
            .param("id", "5")
            .json(&json!({ "reason": "scrapped after 12 years" }))
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["status"], STATUS_RETIRED);
    assert_eq!(body["flags"]["in_pool"], false);
    let update = &host.db.queried_sql()[2];
    assert!(
        update.contains("maintenance_since = NULL"),
        "retiring from maintenance has to clear it: {update}"
    );
    assert_audited(&host, "item.retire");
    host.events.assert_published("equipment.item.retired");
}

#[tokio::test]
async fn delete_refuses_an_item_with_a_history() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "DELETE", "/api/equipment/item/{id}").handler;
    host.db.push_rows(vec![item_row(1, "Dome tent")]); // the item
    host.db.push_rows(vec![json!({ "id": 20 })]); // it has been checked out

    let (status, body) = call(
        handler,
        TestRequest::delete("/api/equipment/item/1")
            .param("id", "1")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 409);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("checkout history"), "{error}");
    assert!(error.contains("Retire it instead"), "{error}");
    assert!(host.db.executed_sql().is_empty(), "no delete ran");
    host.events.assert_none();
}

#[tokio::test]
async fn delete_removes_a_mistake_that_was_never_used() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "DELETE", "/api/equipment/item/{id}").handler;
    host.db.push_rows(vec![item_row(1, "Duplicate entry")]); // the item
    host.db.push_rows(Vec::new()); // no history

    let (status, body) = call(
        handler,
        TestRequest::delete("/api/equipment/item/1")
            .param("id", "1")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], 1);
    host.db.assert_executed(&["DELETE FROM", "items"]);
    assert_audited(&host, "item.delete");
    host.events.assert_published("equipment.item.deleted");
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[tokio::test]
async fn availability_answers_what_can_i_take_on_these_dates() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/availability").handler;
    let from = days(1);
    let to = days(5);
    host.db.push_rows(vec![
        item_row(1, "Free tent"),
        item_row(2, "Tent out now"),
        in_maintenance(3, "Stove"),
    ]); // the pool
    host.db.push_rows(vec![checkout_row(
        40,
        2,
        &days(-1).to_string(),
        None,
        Some(&days(3).to_string()),
    )]); // checkouts touching the window

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/availability")
            .query_param("from", &from.to_string())
            .query_param("to", &to.to_string())
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["from"], from.to_string());
    assert_eq!(body["to"], to.to_string());
    assert_eq!(body["days"], 4);
    let available: Vec<i64> = body["available"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_i64())
        .collect();
    assert_eq!(available, vec![1]);
    assert_eq!(body["counts"]["available"], 1);
    assert_eq!(body["counts"]["unavailable"], 2);
    assert_eq!(body["counts"]["by_reason"]["checked_out"], 1);
    assert_eq!(body["counts"]["by_reason"]["in_maintenance"], 1);
    assert_eq!(body["unavailable"][0]["blocking"][0]["held_by"], "bea");
    assert_eq!(body["out"].as_array().unwrap().len(), 1);
    assert_eq!(host.db.query_count(), 2);

    // A window that runs backwards is a caller mistake, not an empty answer.
    let (status, _body) = call(
        handler,
        TestRequest::get("/api/equipment/availability")
            .query_param("from", &to.to_string())
            .query_param("to", &from.to_string())
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn availability_defaults_to_the_next_week() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/availability").handler;
    host.db.push_rows(vec![item_row(1, "Free tent")]);
    host.db.push_rows(Vec::new());

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/availability")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["from"], today().to_string());
    assert_eq!(body["days"], DEFAULT_AVAILABILITY_DAYS);
    assert_eq!(
        body["to"].as_str().unwrap(),
        days(DEFAULT_AVAILABILITY_DAYS).to_string()
    );
}

#[tokio::test]
async fn replacements_lists_the_candidates_with_their_reasons() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/replacements").handler;
    host.db.push_rows(vec![
        item_row(1, "Fine tent"),
        set(item_row(2, "Split tent"), "condition", json!(CONDITION_POOR)),
    ]);

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/replacements")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 1);
    assert_eq!(body["examined"], 2);
    assert_eq!(body["candidates"][0]["item"]["id"], 2);
    assert_eq!(body["candidates"][0]["reasons"][0], "condition_poor");
    assert_eq!(body["thresholds"]["replacement_service_count"], 40);
}

#[tokio::test]
async fn the_maintenance_view_buckets_the_work() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/maintenance").handler;
    host.db.push_rows(vec![
        in_maintenance(1, "Stove"),
        set(item_row(2, "Tent"), "next_service_on", json!(days(-1).to_string())),
        set(item_row(3, "Lantern"), "next_service_on", json!(days(3).to_string())),
        set(item_row(4, "Rope"), "next_service_on", json!(days(90).to_string())),
    ]);

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/maintenance")
            .identity("bea", &["equipment_manager"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["counts"]["in_service"], 1);
    assert_eq!(body["counts"]["overdue"], 1);
    assert_eq!(body["counts"]["due"], 1);
    assert_eq!(body["counts"]["scheduled"], 1);
    let sql = &host.db.queried_sql()[0];
    assert!(sql.contains("ANY($1)"), "the poor/unserviceable set is a parameter: {sql}");
}

#[tokio::test]
async fn the_checkout_log_filters_by_state_holder_and_mission() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/checkouts").handler;
    host.db.push_rows(vec![
        checkout_row(50, 1, &days(-25).to_string(), None, Some(&days(-10).to_string())),
        checkout_row(51, 2, &days(-2).to_string(), Some(&days(-1).to_string()), None),
    ]);

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/checkouts")
            .query_param("state", "open")
            .query_param("member", "bea")
            .query_param("mission_id", "12")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "open");
    assert_eq!(body["count"], 2);
    assert_eq!(body["open"], 1);
    let sql = &host.db.queried_sql()[0];
    assert!(sql.contains("c.checked_in_at IS NULL"), "{sql}");
    assert!(sql.contains("c.due_on <"), "the overdue filter is a SQL comparison: {sql}");

    let (status, _body) = call(
        handler,
        TestRequest::get("/api/equipment/checkouts")
            .query_param("state", "somewhere")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn a_checkout_can_be_traced_to_the_actual_item_it_names() {
    let (host, _plugin, routes) = plugin().await;
    let handler = &route(&routes, "GET", "/api/equipment/checkouts").handler;
    host.db.push_rows(vec![checkout_row(60, 7, &days(-1).to_string(), None, Some(&days(2).to_string()))]);

    let (status, body) = call(
        handler,
        TestRequest::get("/api/equipment/checkouts")
            .query_param("overdue", "true")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200);
    // A checkout row carries the item it names: the row is not just a count.
    assert_eq!(body["checkouts"][0]["item_id"], 7);
    assert_eq!(body["checkouts"][0]["asset_tag"], "TAG-7");
    assert_eq!(body["checkouts"][0]["open"], true);
}

// ---------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_daily_schedule_speaks_only_when_something_is_due() {
    let (host, equipment, _routes) = plugin().await;
    let schedules = equipment.schedules();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].name, "maintenance_due");
    assert_eq!(schedules[0].every.as_secs(), 24 * 60 * 60);

    // One item in service, one overdue: the run speaks.
    host.db.push_rows(vec![
        in_maintenance(1, "Stove"),
        set(item_row(2, "Tent"), "next_service_on", json!(days(-1).to_string())),
    ]);
    (schedules[0].handler)().await.unwrap();
    let published = host.events.payloads("equipment.maintenance.due");
    assert_eq!(published.len(), 1, "one event, not one per item");
    assert_eq!(published[0]["in_service"], 1);
    assert_eq!(published[0]["overdue"], 1);
    assert_eq!(published[0]["item_ids"], json!([1, 2]));

    // Nothing due: the run is silent.
    let (host, equipment, _routes) = plugin().await;
    let schedules = equipment.schedules();
    host.db.push_rows(vec![item_row(3, "Fine lantern")]);
    (schedules[0].handler)().await.unwrap();
    host.events.assert_none();
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

#[test]
fn response_helper_decodes_json_bodies() {
    // Guards the harness itself: every status/body assertion above goes through
    // `response_json`, so a body that decoded as Null would pass for "no error".
    let resp = PluginResponse::json(200, &json!({ "ok": true })).unwrap();
    assert_eq!(body_json(&resp)["ok"], true);
}
