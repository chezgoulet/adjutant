//! Calendar plugin tests: the recurrence arithmetic, the quorum projection, and
//! the event/RSVP handlers (SPEC §7.7).
//!
//! Handlers are driven through `adjutant_sdk::testing`. `MockDb` replays queued
//! results in call order, so every test states the order its handler arranges —
//! the comment above each `push_*` says which call it answers.

use adjutant_calendar::{
    compute_quorum, expand, is_excluded, parse_exdates, parse_local_timestamp, parse_rrule,
    quorum_met, render_exdates, render_local, season_of, seasonal_prompt, seasonal_tasks_between,
    seasonal_tasks_for_month, CalendarPlugin, CATEGORIES, QUORUM_FIXED, QUORUM_MAJORITY_MEMBERS,
    QUORUM_NONE, QUORUM_ONE_THIRD_REGISTERED, RESPONSES, RESPONSE_GOING, SCOPE_LODGE, SCOPE_TROOP,
    SEASONAL_TASKS,
};
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, TestHost, TestRequest};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde_json::json;

/// The audit log's action is a bind parameter, so a test has to read it back
/// rather than look for it in the statement.
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

async fn plugin() -> (TestHost, CalendarPlugin, Vec<RouteDefinition>) {
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
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
async fn call(
    handler: &adjutant_sdk::RouteHandler,
    req: PluginRequest,
) -> (u16, serde_json::Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), json!({ "error": e.to_string() })),
    }
}

fn local(value: &str) -> NaiveDateTime {
    parse_local_timestamp(value).expect("test timestamp")
}

/// An ordinary troop event row, as `EVENT_FIELDS` renders one.
fn event_row(
    id: i64,
    scope_type: &str,
    scope_id: Option<&str>,
    starts_local: &str,
    rrule: &str,
) -> serde_json::Value {
    json!({
        "id": id,
        "title": "Troop meeting",
        "description": "",
        "category": "meeting",
        "scope_type": scope_type,
        "scope_id": scope_id,
        "body": "",
        "meeting_id": serde_json::Value::Null,
        "location": "Windsor",
        "timezone": "America/New_York",
        "all_day": false,
        "rrule": rrule,
        "exdates": "",
        "starts_at": "2026-10-06 22:00:00+00",
        "starts_local": starts_local,
        "ends_at": serde_json::Value::Null,
        "ends_local": serde_json::Value::Null,
        "now_local": "2026-09-25T09:00:00",
        "status": "scheduled",
        "quorum_basis": QUORUM_NONE,
        "expected_voters": 0,
        "quorum_required": serde_json::Value::Null,
        "source_mission_id": serde_json::Value::Null,
        "cancelled_at": serde_json::Value::Null,
        "cancelled_by": serde_json::Value::Null,
        "created_by": "bea",
        "created_at": "2026-09-01 12:00:00+00",
        "updated_at": "2026-09-01 12:00:00+00"
    })
}

fn rsvp_row(id: i64, member: &str, response: &str) -> serde_json::Value {
    json!({
        "id": id,
        "event_id": 7,
        "member_id": member,
        "response": response,
        "occurrence_at": "-infinity",
        "occurrence_local": "2026-10-06T18:00:00",
        "note": "",
        "responded_by": member,
        "responded_at": "2026-09-25 13:00:00+00",
    })
}

fn counts(going: i64) -> serde_json::Value {
    json!({ "going": going, "not_going": 0, "maybe": 1, "pending": 2, "responded": going + 3 })
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let (_host, plugin, routes) = plugin().await;
    assert_eq!(plugin.id(), "calendar");
    assert_eq!(plugin.name(), "Calendar");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    for perm in &permissions {
        assert!(perm.starts_with("calendar:"), "{perm} must be namespaced");
    }
    // SPEC §9.1's two, plus the M5 additions.
    for expected in [
        "calendar:read",
        "calendar:create",
        "calendar:manage",
        "calendar:rsvp",
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
    for table in ["events", "rsvps"] {
        assert!(
            ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "SPEC §7.7's schema is missing {table}"
        );
    }
    // Vocabulary that reaches a tally is a constraint, not a convention.
    for constraint in [
        "events_scope_type_valid",
        "events_scope_id_present",
        "events_status_valid",
        "events_quorum_basis_valid",
        "events_window_valid",
        "rsvps_response_valid",
    ] {
        assert!(
            ddl.contains(constraint),
            "migration is missing {constraint}"
        );
    }
    assert!(
        ddl.contains("UNIQUE INDEX IF NOT EXISTS idx_events_source_mission"),
        "a replayed mission.completed must not create a second debrief"
    );

    let mut seen: Vec<(String, String)> = Vec::new();
    for r in &routes {
        assert!(
            r.path.starts_with("/api/calendar"),
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
        routes.len() >= 14,
        "the surface the client needs is bigger than this"
    );

    // Destructive is troop-only, and cancelling one occurrence is not the same
    // authority as deleting the record (SPEC §9.2, §8 #3).
    let delete = route(&routes, "DELETE", "/api/calendar/event/{id}");
    assert_eq!(delete.required_scope, Some(Scope::troop()));
    let cancel_occurrence = route(
        &routes,
        "POST",
        "/api/calendar/event/{id}/occurrence/cancel",
    );
    assert_eq!(
        cancel_occurrence.required_scope, None,
        "the handler checks the event's scope"
    );
    let rsvp = route(&routes, "POST", "/api/calendar/event/{id}/rsvp");
    assert_eq!(rsvp.required_permission.as_deref(), Some("calendar:rsvp"));
}

#[test]
fn entry_symbol_is_exported() {
    let raw = adjutant_calendar::adjutant_plugin_create();
    assert!(!raw.is_null());
    let boxed = unsafe { Box::from_raw(raw) };
    assert_eq!(boxed.id(), "calendar");
    assert_eq!(
        adjutant_calendar::adjutant_sdk_abi(),
        adjutant_sdk::SDK_ABI_VERSION
    );
}

// ---------------------------------------------------------------------------
// Recurrence (RFC 5545)
// ---------------------------------------------------------------------------

#[test]
fn weekly_series_lands_on_the_stated_weekdays() {
    let dtstart = local("2026-09-22T18:00:00"); // a Tuesday
    let rule = parse_rrule("FREQ=WEEKLY;BYDAY=TU").unwrap();
    let expansion = expand(&rule, dtstart, dtstart, 3);
    assert_eq!(
        expansion.occurrences,
        vec![
            local("2026-09-22T18:00:00"),
            local("2026-09-29T18:00:00"),
            local("2026-10-06T18:00:00"),
        ]
    );
    // A week start before DTSTART never yields an occurrence before it.
    assert_eq!(expansion.occurrences.iter().min().copied(), Some(dtstart));
}

#[test]
fn weekly_multi_day_and_interval_are_honoured() {
    let dtstart = local("2026-09-22T18:00:00"); // Tuesday
    let rule = parse_rrule("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH").unwrap();
    let expansion = expand(&rule, dtstart, dtstart, 4);
    assert_eq!(
        expansion.occurrences,
        vec![
            local("2026-09-22T18:00:00"), // Tue
            local("2026-09-24T18:00:00"), // Thu
            local("2026-10-06T18:00:00"), // Tue, two weeks later
            local("2026-10-08T18:00:00"), // Thu
        ]
    );
}

#[test]
fn count_is_measured_from_dtstart_and_until_is_inclusive() {
    let dtstart = local("2026-09-22T18:00:00");
    // COUNT counts from DTSTART even when the window starts later, so asking for
    // the second fortnight of a 3-occurrence series returns exactly the third.
    let rule = parse_rrule("FREQ=WEEKLY;BYDAY=TU;COUNT=3").unwrap();
    let expansion = expand(&rule, dtstart, local("2026-10-01T00:00:00"), 5);
    assert_eq!(expansion.occurrences, vec![local("2026-10-06T18:00:00")]);

    let bounded = parse_rrule("FREQ=WEEKLY;BYDAY=TU;UNTIL=20261006T180000Z").unwrap();
    let expansion = expand(&bounded, dtstart, dtstart, 10);
    assert_eq!(
        expansion.occurrences.len(),
        3,
        "UNTIL includes the occurrence on the boundary"
    );
}

#[test]
fn monthly_by_monthday_skips_months_that_are_too_short() {
    let dtstart = local("2026-01-31T19:00:00");
    let rule = parse_rrule("FREQ=MONTHLY;BYMONTHDAY=31").unwrap();
    let expansion = expand(&rule, dtstart, dtstart, 3);
    assert_eq!(
        expansion.occurrences,
        vec![
            local("2026-01-31T19:00:00"),
            local("2026-03-31T19:00:00"),
            local("2026-05-31T19:00:00"),
        ],
        "February, April and June have no 31st"
    );
    assert!(expansion.periods >= 5, "the empty periods are still walked");
}

#[test]
fn monthly_ordinal_weekday_is_the_nth_of_the_month() {
    let dtstart = local("2026-09-25T18:00:00"); // the last Friday of September
    let rule = parse_rrule("FREQ=MONTHLY;BYDAY=-1FR").unwrap();
    let expansion = expand(&rule, dtstart, dtstart, 3);
    assert_eq!(
        expansion.occurrences,
        vec![
            local("2026-09-25T18:00:00"),
            local("2026-10-30T18:00:00"),
            local("2026-11-27T18:00:00"),
        ]
    );

    let first_monday = parse_rrule("FREQ=MONTHLY;BYDAY=1MO").unwrap();
    let expansion = expand(
        &first_monday,
        local("2026-10-01T18:00:00"),
        local("2026-10-01T18:00:00"),
        2,
    );
    assert_eq!(
        expansion.occurrences,
        vec![local("2026-10-05T18:00:00"), local("2026-11-02T18:00:00")]
    );
}

#[test]
fn yearly_rules_use_the_dtstart_month_and_day() {
    let rule = parse_rrule("FREQ=YEARLY").unwrap();
    let expansion = expand(
        &rule,
        local("2026-07-04T09:00:00"),
        local("2026-07-04T09:00:00"),
        3,
    );
    assert_eq!(
        expansion.occurrences,
        vec![
            local("2026-07-04T09:00:00"),
            local("2027-07-04T09:00:00"),
            local("2028-07-04T09:00:00"),
        ]
    );
}

#[test]
fn unsupported_or_incoherent_parts_are_refused_not_ignored() {
    for (rule, needle) in [
        ("FREQ=WEEKLY;BYSETPOS=-1", "BYSETPOS"),
        ("FREQ=HOURLY", "FREQ=HOURLY"),
        ("INTERVAL=2", "missing FREQ"),
        (
            "FREQ=WEEKLY;COUNT=2;UNTIL=20261231T000000Z",
            "both COUNT and UNTIL",
        ),
        ("FREQ=WEEKLY;BYDAY=1MO", "ordinal"),
        ("FREQ=WEEKLY;WKST=SU", "WKST"),
        ("FREQ=MONTHLY;BYDAY=MO;BYMONTHDAY=1", "BYDAY and BYMONTHDAY"),
        ("FREQ=DAILY;BYDAY=MO", "BYDAY with FREQ=DAILY"),
        ("FREQ=MONTHLY;BYMONTHDAY=0", "out of range"),
        ("", "empty"),
    ] {
        let error = parse_rrule(rule).expect_err(&format!("{rule:?} must be refused"));
        assert!(
            error.contains(needle),
            "{rule:?} gave {error:?}, expected {needle:?}"
        );
    }
}

#[test]
fn a_rule_round_trips_through_its_canonical_form() {
    let rule = parse_rrule("rrule:FREQ=weekly;interval=1;byday=th,tu").unwrap();
    assert_eq!(rule.to_ical(), "FREQ=WEEKLY;INTERVAL=1;BYDAY=TU,TH");
    let again = parse_rrule(&rule.to_ical()).unwrap();
    assert_eq!(
        rule, again,
        "the stored form must parse back to the same rule"
    );
    // COUNT=1 is a series of one: stored as non-recurring.
    assert!(!parse_rrule("FREQ=DAILY;COUNT=1").unwrap().recurs());
}

#[test]
fn exdates_remove_one_occurrence_without_ending_the_series() {
    let exdates = parse_exdates("2026-10-13T18:00:00").unwrap();
    let rule = parse_rrule("FREQ=WEEKLY;BYDAY=TU").unwrap();
    let expansion = expand(
        &rule,
        local("2026-10-06T18:00:00"),
        local("2026-10-06T18:00:00"),
        3,
    );
    let kept: Vec<NaiveDateTime> = expansion
        .occurrences
        .iter()
        .copied()
        .filter(|o| !is_excluded(&exdates, *o, false))
        .collect();
    assert_eq!(
        kept,
        vec![local("2026-10-06T18:00:00"), local("2026-10-20T18:00:00")]
    );
    // A date-only entry excludes the whole day, which is how a cancelled meeting
    // is recorded.
    let day = parse_exdates("2026-10-06").unwrap();
    assert!(is_excluded(&day, local("2026-10-06T18:00:00"), false));
    assert_eq!(render_exdates(&day), "2026-10-06T00:00:00");
}

#[test]
fn wall_clock_timestamps_refuse_a_zone_because_the_event_carries_it() {
    assert_eq!(
        parse_local_timestamp("2026-10-06").unwrap(),
        local("2026-10-06T00:00:00")
    );
    assert_eq!(
        parse_local_timestamp("2026-10-06 18:30").unwrap(),
        local("2026-10-06T18:30:00")
    );
    for bad in [
        "2026-10-06T18:00:00Z",
        "2026-10-06T18:00:00-05:00",
        "next tuesday",
        "",
    ] {
        let error = parse_local_timestamp(bad).expect_err(bad);
        assert!(!error.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Quorum (the Accords' arithmetic, one third of registered scouts)
// ---------------------------------------------------------------------------

#[test]
fn quorum_is_governances_arithmetic_over_the_intent_to_attend() {
    // Congress: one-third of registered scouts, rounded up (3rd Congress).
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 27, None), 9);
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 25, None), 9);
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 1, None), 1);
    // Troop Council default.
    assert_eq!(compute_quorum(QUORUM_MAJORITY_MEMBERS, 5, None), 3);
    // Fixed, and no rule at all.
    assert_eq!(compute_quorum(QUORUM_FIXED, 27, Some(12)), 12);
    assert_eq!(compute_quorum(QUORUM_NONE, 27, Some(12)), 0);
    // Fail closed: an event nobody configured is not "in quorum".
    assert_eq!(compute_quorum(QUORUM_ONE_THIRD_REGISTERED, 0, None), 0);
    assert!(!quorum_met(0, 0));
    assert!(!quorum_met(8, 9));
    assert!(quorum_met(9, 9));
}

// ---------------------------------------------------------------------------
// Season
// ---------------------------------------------------------------------------

#[test]
fn every_month_of_the_troop_year_has_a_prompt() {
    for month in 1..=12u32 {
        assert!(
            !seasonal_tasks_for_month(month).is_empty(),
            "month {month} has nothing to surface"
        );
    }
    for task in SEASONAL_TASKS {
        assert!(
            (1..=12).contains(&task.month),
            "{} has a bad month",
            task.key
        );
        assert!(!task.title.is_empty() && !task.detail.is_empty());
    }
    assert_eq!(
        season_of(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
        "fall"
    );
    assert_eq!(
        season_of(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap()),
        "winter"
    );
    assert_eq!(
        season_of(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()),
        "spring"
    );
    assert_eq!(
        season_of(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()),
        "summer"
    );
}

#[test]
fn a_season_window_crosses_the_year_boundary_in_order() {
    let from = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
    let to = NaiveDate::from_ymd_opt(2027, 1, 31).unwrap();
    let tasks = seasonal_tasks_between(from, to);
    let keys: Vec<&str> = tasks.iter().map(|(_, t)| t.key).collect();
    assert!(
        keys.contains(&"year_end_report"),
        "December's prompt is missing"
    );
    assert!(
        keys.contains(&"recharter_prep"),
        "January's prompt is missing"
    );
    let dates: Vec<NaiveDate> = tasks.iter().map(|(d, _)| *d).collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable();
    assert_eq!(dates, sorted, "the window is returned in date order");
}

#[test]
fn the_weekly_reminder_is_empty_in_a_quiet_week_and_full_otherwise() {
    // 1 October 2026: dues (10-05) and winter gear (10-20) are inside 14 days.
    let prompt = seasonal_prompt(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), 14)
        .expect("October has prompts");
    let keys: Vec<&str> = prompt["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"dues_collection"));
    assert_eq!(prompt["window_days"], json!(14));
    // 25 October: the last prompt was 10-20 and the next is 11-15, so nothing is
    // due — a quiet week publishes nothing rather than an empty announcement.
    assert!(seasonal_prompt(NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(), 14).is_none());
}

#[test]
fn the_weekly_schedule_is_declared() {
    // The handler runs on the core's clock, so this pins the declaration and
    // proves the handler is callable; the window logic itself is pure and tested
    // above.
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    futures_lite_block(plugin.init(host.context("calendar"))).unwrap();
    let schedules = plugin.schedules();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].name, "season_reminder");
    assert_eq!(
        schedules[0].every,
        std::time::Duration::from_secs(7 * 24 * 60 * 60)
    );
}

/// Run a future on a throwaway runtime (the schedule declaration test needs an
/// initialised plugin but no async body of its own).
fn futures_lite_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

// ---------------------------------------------------------------------------
// Event creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_event_writes_the_wall_clock_and_the_zone() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/calendar/event");

    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:create at the Lodge
    host.db
        .push_rows(vec![json!({ "name": "America/New_York" })]); // the zone exists
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![counts(0)]); // the RSVP summary of the new event

    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({
                "title": "Lodge 3 meeting",
                "scope_type": SCOPE_LODGE,
                "scope_id": "3",
                "timezone": "America/New_York",
                "starts_at": "2026-10-06T18:00:00",
                "ends_at": "2026-10-06T20:00:00",
                "rrule": "FREQ=WEEKLY;BYDAY=TU",
                "quorum_basis": QUORUM_NONE,
            }))
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["event"]["id"], json!(7));
    assert!(body["quorum"]["kind"] == json!("rsvp_projection"));
    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("events"))
        .expect("an insert ran");
    assert!(
        insert.contains("AT TIME ZONE"),
        "the wall clock is converted by PostgreSQL: {insert}"
    );
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered.iter().any(|p| p.contains("2026-10-06T18:00:00")),
        "the wall clock is stored as given: {rendered:?}"
    );
    assert!(rendered.iter().any(|p| p.contains("America/New_York")));
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("FREQ=WEEKLY") && p.contains("BYDAY=TU")),
        "the stored rule is the canonical content line: {rendered:?}"
    );
    host.events.assert_published(event_type::EVENT_CREATED);
    assert_audited(&host, "event.create");
}

#[tokio::test]
async fn create_event_refuses_a_lodge_event_without_a_lodge_and_a_bad_zone() {
    let (_host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/calendar/event");

    // No scope_id on a Lodge event: rejected before anything is authorized.
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity("bea", &["chief"])
            .json(&json!({
                "title": "Nothing",
                "scope_type": SCOPE_LODGE,
                "starts_at": "2026-10-06T18:00:00",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("scope_id"));

    // A Congress event is the troop's, not a Lodge's (SPEC §7.4's bodies).
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity("bea", &["chief"])
            .json(&json!({
                "title": "Congress",
                "body": "congress",
                "scope_type": SCOPE_LODGE,
                "scope_id": "3",
                "starts_at": "2026-10-06T18:00:00",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("troop-wide"));

    // A UTC designator would move a meeting by hours across a DST boundary.
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity("bea", &["chief"])
            .json(&json!({ "title": "X", "starts_at": "2026-10-06T18:00:00Z" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("wall-clock"));
}

#[tokio::test]
async fn create_event_checks_the_permission_at_the_events_scope_then_the_zone() {
    let (host, _plugin, routes) = plugin().await;
    let create = route(&routes, "POST", "/api/calendar/event");

    // A Lodge-only grant cannot create a troop-wide event: `reach` refuses
    // without a query, because no grant covers the troop.
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "title": "Troop camp", "starts_at": "2026-10-06T18:00:00" }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        host.db.query_count(),
        0,
        "no database work before authorization"
    );

    // Authorized, but the zone is not one PostgreSQL knows: a 400 with advice,
    // not a 500 from the first conversion.
    host.db.push_rows(vec![json!({ "n": 1 })]); // the create grant
    host.db.push_rows(vec![]); // pg_timezone_names finds nothing
    let (status, body) = call(
        &create.handler,
        TestRequest::post("/api/calendar/event")
            .identity("bea", &["chief"])
            .json(&json!({
                "title": "Meeting",
                "timezone": "Mars/Olympus_Mons",
                "starts_at": "2026-10-06T18:00:00",
            }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("IANA"));
}

// ---------------------------------------------------------------------------
// RSVP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rsvp_records_the_answer_and_reports_the_projection() {
    let (host, _plugin, routes) = plugin().await;
    let rsvp = route(&routes, "POST", "/api/calendar/event/{id}/rsvp");

    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:rsvp covers the troop
    host.db.push_rows(vec![rsvp_row(11, "bea", RESPONSE_GOING)]);
    host.db.push_rows(vec![counts(9)]);

    let (status, body) = call(
        &rsvp.handler,
        TestRequest::post("/api/calendar/event/7/rsvp")
            .param("id", "7")
            .identity("bea", &["scout"])
            .json(&json!({ "response": "going" }))
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rsvp"]["response"], json!(RESPONSE_GOING));
    assert_eq!(body["counts"]["going"], json!(9));
    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("ON CONFLICT"))
        .expect("the rsvp is an upsert");
    assert!(
        insert.contains("occurrence_at"),
        "a series-level answer is keyed by the sentinel"
    );
    assert!(
        insert.contains("DO UPDATE"),
        "changing your mind is an update, not a conflict"
    );
    host.events.assert_published("event.rsvp");
    assert_audited(&host, "event.rsvp");
}

#[tokio::test]
async fn an_occurrence_rsvp_must_be_one_of_the_series_occurrences() {
    // A recurring event: the second occurrence is answerable...
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
    let routes = plugin.routes();
    let rsvp = route(&routes, "POST", "/api/calendar/event/{id}/rsvp");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:rsvp
    host.db.push_rows(vec![rsvp_row(11, "bea", RESPONSE_GOING)]);
    host.db.push_rows(vec![counts(1)]);
    let (status, body) = call(
        &rsvp.handler,
        TestRequest::post("/api/calendar/event/7/rsvp")
            .param("id", "7")
            .identity("bea", &["scout"])
            .json(&json!({ "response": "going", "occurrence": "2026-10-13T18:00:00" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // ...and a Wednesday is not.
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:rsvp
    let (status, body) = call(
        &rsvp.handler,
        TestRequest::post("/api/calendar/event/7/rsvp")
            .param("id", "7")
            .identity("bea", &["scout"])
            .json(&json!({ "response": "going", "occurrence": "2026-11-11T18:00:00" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("not an occurrence"));
}

#[tokio::test]
async fn rsvp_refuses_an_unknown_answer_and_an_uncovered_lodge() {
    let (host, _plugin, routes) = plugin().await;
    let rsvp = route(&routes, "POST", "/api/calendar/event/{id}/rsvp");

    let (status, body) = call(
        &rsvp.handler,
        TestRequest::post("/api/calendar/event/7/rsvp")
            .param("id", "7")
            .identity("bea", &["scout"])
            .json(&json!({ "response": "probably" }))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        host.db.query_count(),
        0,
        "the vocabulary is checked before any query"
    );

    // A Lodge 9 grant cannot answer for a Lodge 3 event (SPEC §9.2).
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);
    let (status, body) = call(
        &rsvp.handler,
        TestRequest::post("/api/calendar/event/7/rsvp")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "scout".into(),
                    scope: Scope::lodge("9"),
                }],
            )
            .json(&json!({ "response": RESPONSE_GOING }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
}

#[tokio::test]
async fn recording_another_members_rsvp_needs_manage() {
    let (host, _plugin, routes) = plugin().await;
    let delegated = route(&routes, "POST", "/api/calendar/event/{id}/rsvp/{member}");

    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:manage at Lodge 3
    host.db.push_rows(vec![rsvp_row(12, "carl", "maybe")]);
    host.db.push_rows(vec![counts(0)]);

    let (status, body) = call(
        &delegated.handler,
        TestRequest::post("/api/calendar/event/7/rsvp/carl")
            .param("id", "7")
            .param("member", "carl")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "response": "maybe", "note": "phoned it in" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let params = host.db.last_query_params("ON CONFLICT").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered.iter().any(|p| p.contains("carl")),
        "the answer is recorded for carl"
    );
    assert!(rendered.iter().any(|p| p.contains("maybe")));
}

// ---------------------------------------------------------------------------
// Quorum route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quorum_reports_one_third_of_registered_scouts_and_names_governance() {
    let (host, _plugin, routes) = plugin().await;
    let quorum = route(&routes, "GET", "/api/calendar/event/{id}/quorum");

    let mut congress = event_row(7, SCOPE_TROOP, None, "2026-10-06T18:00:00", "");
    congress["body"] = json!("congress");
    congress["meeting_id"] = json!(12);
    congress["quorum_basis"] = json!(QUORUM_ONE_THIRD_REGISTERED);
    congress["expected_voters"] = json!(27);

    host.db.push_rows(vec![congress.clone()]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // troop-wide calendar:read
    host.db.push_rows(vec![counts(9)]);

    let (status, body) = call(
        &quorum.handler,
        TestRequest::get("/api/calendar/event/7/quorum")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["quorum"]["required"], json!(9));
    assert_eq!(body["quorum"]["going"], json!(9));
    assert_eq!(body["quorum"]["met"], json!(true));
    assert_eq!(body["counted"]["occurrence"], json!("2026-10-06T18:00:00"));
    // The authoritative (attendance-based) number is governance's, and the
    // response says where it is rather than keeping a second tally.
    assert_eq!(
        body["governance_quorum"],
        json!("/api/governance/meeting/12/quorum")
    );

    // Short by two: the projection is not a quorum.
    let mut short = congress.clone();
    short["id"] = json!(8);
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
    let routes = plugin.routes();
    let quorum = route(&routes, "GET", "/api/calendar/event/{id}/quorum");
    host.db.push_rows(vec![short]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![counts(7)]);
    let (status, body) = call(
        &quorum.handler,
        TestRequest::get("/api/calendar/event/8/quorum")
            .param("id", "8")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["quorum"]["met"], json!(false));
    assert_eq!(body["quorum"]["short"], json!(2));
}

#[tokio::test]
async fn an_event_without_a_quorum_rule_fails_closed() {
    let (host, _plugin, routes) = plugin().await;
    let quorum = route(&routes, "GET", "/api/calendar/event/{id}/quorum");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![counts(20)]);

    let (status, body) = call(
        &quorum.handler,
        TestRequest::get("/api/calendar/event/7/quorum")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["quorum"]["basis"], json!(QUORUM_NONE));
    assert_eq!(body["quorum"]["required"], json!(0));
    assert_eq!(
        body["quorum"]["met"],
        json!(false),
        "no rule is not a quorum"
    );
    assert_eq!(body["quorum"]["basis_configured"], json!(false));
}

// ---------------------------------------------------------------------------
// Listing, upcoming, occurrences, season
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_narrows_to_the_callers_scopes() {
    let (host, _plugin, routes) = plugin().await;
    let list = route(&routes, "GET", "/api/calendar/events");
    host.db.push_rows(vec![json!({ "n": 1 })]);
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);

    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/calendar/events")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["scope"], json!("troop"));
    assert_eq!(
        body["status"],
        json!("scheduled"),
        "cancelled events are opt-in"
    );
    let sql = host.db.queried_sql().join("\n");
    assert!(
        sql.contains("scope_id = ANY($2)"),
        "a scoped caller is filtered by lodge"
    );
    assert!(
        sql.contains("created_by = $3"),
        "their own events stay visible"
    );

    // An unknown status is the caller's mistake, not an empty list.
    let (status, _body) = call(
        &list.handler,
        TestRequest::get("/api/calendar/events")
            .query_param("status", "maybe")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn upcoming_returns_occurrences_and_the_seasons_prompts() {
    let (host, _plugin, routes) = plugin().await;
    let upcoming = route(&routes, "GET", "/api/calendar/upcoming");

    let mut weekly = event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    );
    weekly["now_local"] = json!("2026-10-01T09:00:00");
    let mut one_off = event_row(8, SCOPE_LODGE, Some("3"), "2026-10-15T19:00:00", "");
    one_off["title"] = json!("Lodge service day");
    one_off["now_local"] = json!("2026-10-01T09:00:00");

    host.db.push_rows(vec![json!({ "n": 1 })]); // troop-wide read
    host.db.push_rows(vec![weekly, one_off]);

    let (status, body) = call(
        &upcoming.handler,
        TestRequest::get("/api/calendar/upcoming")
            .query_param("days", "21")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    let occurrences = body["occurrences"].as_array().unwrap();
    assert!(
        occurrences.len() >= 4,
        "two Tuesdays plus the one-off: {occurrences:?}"
    );
    assert_eq!(
        occurrences[0]["occurrence_local"],
        json!("2026-10-06T18:00:00")
    );
    assert!(occurrences
        .iter()
        .all(|o| o["occurrence_local"].as_str().unwrap() <= "2026-10-22"));
    assert!(occurrences
        .iter()
        .any(|o| o["title"] == json!("Lodge service day")));
    // Seasonal awareness: October's prompts are inside the same window.
    let keys: Vec<&str> = body["seasonal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"dues_collection"), "{keys:?}");
    assert!(keys.contains(&"winter_gear"), "{keys:?}");
    assert_eq!(body["truncated"], json!(false));
}

#[tokio::test]
async fn occurrences_are_expanded_on_the_events_wall_clock() {
    let (host, _plugin, routes) = plugin().await;
    let occurrences = route(&routes, "GET", "/api/calendar/event/{id}/occurrences");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU;INTERVAL=1",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);

    let (status, body) = call(
        &occurrences.handler,
        TestRequest::get("/api/calendar/event/7/occurrences")
            .param("id", "7")
            .query_param("limit", "3")
            .query_param("from", "2026-10-01T00:00:00")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["occurrences"],
        json!([
            "2026-10-06T18:00:00",
            "2026-10-13T18:00:00",
            "2026-10-20T18:00:00"
        ])
    );
    assert_eq!(body["timezone"], json!("America/New_York"));
}

#[tokio::test]
async fn a_cancelled_occurrence_does_not_use_up_a_slot_in_the_listing() {
    // Found against a real database: the series is expanded before the EXDATEs
    // are dropped, so a `limit` applied at expansion time returns fewer
    // occurrences than asked for. Listing "the next three meetings" after a
    // cancelled week returned two.
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
    let routes = plugin.routes();
    let occurrences = route(&routes, "GET", "/api/calendar/event/{id}/occurrences");

    let mut row = event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    );
    row["exdates"] = json!("2026-10-13T18:00:00");
    host.db.push_rows(vec![row]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // troop-wide calendar:read

    let (status, body) = call(
        &occurrences.handler,
        TestRequest::get("/api/calendar/event/7/occurrences")
            .param("id", "7")
            .query_param("from", "2026-10-01T00:00:00")
            .query_param("limit", "3")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["occurrences"],
        json!([
            "2026-10-06T18:00:00",
            "2026-10-20T18:00:00",
            "2026-10-27T18:00:00"
        ]),
        "{body}"
    );
    assert_eq!(body["exdates"], json!(["2026-10-13T18:00:00"]));
}

#[tokio::test]
async fn season_answers_a_month_or_a_window() {
    let (_host, _plugin, routes) = plugin().await;
    let season = route(&routes, "GET", "/api/calendar/season");

    let (status, body) = call(
        &season.handler,
        TestRequest::get("/api/calendar/season")
            .query_param("on", "2026-10")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["season"], json!("fall"));
    let keys: Vec<&str> = body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec!["dues_collection", "winter_gear"],
        "ascending by day"
    );

    let (status, body) = call(
        &season.handler,
        TestRequest::get("/api/calendar/season")
            .query_param("on", "2026-11-01")
            .query_param("days", "31")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["tasks"].as_array().unwrap().len() >= 2);

    let (status, body) = call(
        &season.handler,
        TestRequest::get("/api/calendar/season")
            .query_param("on", "whenever")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("on"));
}

// ---------------------------------------------------------------------------
// Edit, cancel, occurrence cancel, delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn editing_a_lodge_event_needs_manage_at_that_lodge() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/calendar/event/{id}");

    // A Lodge-only grant on the event's Lodge: the update runs.
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:manage at Lodge 3
    let mut updated = event_row(7, SCOPE_LODGE, Some("3"), "2026-10-07T18:30:00", "");
    updated["location"] = json!("Woodstock");
    host.db.push_rows(vec![updated]);
    host.db.push_rows(vec![counts(0)]);

    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/calendar/event/7")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("3"),
                }],
            )
            .json(&json!({ "location": "Woodstock", "starts_at": "2026-10-07T18:30:00" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["event"]["location"], json!("Woodstock"));
    let update = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("UPDATE"))
        .expect("an update ran");
    assert!(
        update.contains("starts_at = ("),
        "a moved meeting is re-stamped: {update}"
    );
    assert!(update.contains("updated_at = now()"));
    host.events.assert_published("event.updated");

    // A Lodge 9 grant cannot touch the Lodge 3 record.
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
    let routes = plugin.routes();
    let edit = route(&routes, "PATCH", "/api/calendar/event/{id}");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/calendar/event/7")
            .param("id", "7")
            .identity_grants(
                "bea",
                vec![RoleGrant {
                    role_id: "lodge_commander".into(),
                    scope: Scope::lodge("9"),
                }],
            )
            .json(&json!({ "location": "Somewhere else" }))
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
}

#[tokio::test]
async fn an_empty_patch_is_named_as_such() {
    let (host, _plugin, routes) = plugin().await;
    let edit = route(&routes, "PATCH", "/api/calendar/event/{id}");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    let (status, body) = call(
        &edit.handler,
        TestRequest::patch("/api/calendar/event/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({}))
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("no editable field"));
}

#[tokio::test]
async fn cancelling_the_series_and_cancelling_one_occurrence_are_different_acts() {
    let (host, _plugin, routes) = plugin().await;
    let cancel = route(&routes, "POST", "/api/calendar/event/{id}/cancel");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    let mut cancelled = event_row(7, SCOPE_TROOP, None, "2026-10-06T18:00:00", "");
    cancelled["status"] = json!("cancelled");
    host.db.push_rows(vec![cancelled]);

    let (status, body) = call(
        &cancel.handler,
        TestRequest::post("/api/calendar/event/7/cancel")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["event"]["status"], json!("cancelled"));
    assert!(
        host.db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("status = 'cancelled'")),
        "the cancellation is the state change: {:?}",
        host.db.queried_sql()
    );
    host.events.assert_published("event.cancelled");

    // One occurrence of a series: an EXDATE, so the rest of the series stands.
    let host = TestHost::new();
    let mut plugin = CalendarPlugin::new();
    plugin.init(host.context("calendar")).await.unwrap();
    let routes = plugin.routes();
    let cancel_occurrence = route(
        &routes,
        "POST",
        "/api/calendar/event/{id}/occurrence/cancel",
    );
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]);
    let mut exdated = event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "FREQ=WEEKLY;BYDAY=TU",
    );
    exdated["exdates"] = json!("2026-10-13T18:00:00");
    host.db.push_rows(vec![exdated]);

    let (status, body) = call(
        &cancel_occurrence.handler,
        TestRequest::post("/api/calendar/event/7/occurrence/cancel")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({ "occurrence": "2026-10-13T18:00:00", "reason": "holiday week" }))
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["exdates"], json!("2026-10-13T18:00:00"));
    host.events.assert_published("event.occurrence.cancelled");

    // A non-recurring event has no occurrence to cancel.
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_TROOP,
        None,
        "2026-10-06T18:00:00",
        "",
    )]);
    host.db.push_rows(vec![json!({ "n": 1 })]); // calendar:manage
    let (status, body) = call(
        &cancel_occurrence.handler,
        TestRequest::post("/api/calendar/event/7/occurrence/cancel")
            .param("id", "7")
            .identity("bea", &["chief"])
            .json(&json!({ "occurrence": "2026-10-13T18:00:00" }))
            .build(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("does not recur"));
}

#[tokio::test]
async fn deleting_an_event_removes_it_and_says_so() {
    let (host, _plugin, routes) = plugin().await;
    let delete = route(&routes, "DELETE", "/api/calendar/event/{id}");
    host.db.push_rows(vec![event_row(
        7,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-06T18:00:00",
        "",
    )]);

    let (status, body) = call(
        &delete.handler,
        TestRequest::delete("/api/calendar/event/7")
            .param("id", "7")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(7));
    host.db.assert_executed(&["DELETE FROM", "events"]);
    assert_audited(&host, "event.delete");
    host.events.assert_published("event.deleted");
}

// ---------------------------------------------------------------------------
// mission.completed → a debrief event
// ---------------------------------------------------------------------------

fn mission_completed_event() -> Event {
    let payload = MissionCompleted {
        mission_id: 42,
        title: "Coyote survey".into(),
        lodge_id: Some("3".into()),
        stage: "report".into(),
        completed_at: "2026-09-28T20:15:00Z".parse().expect("a timestamp"),
        impact: json!({ "service_hours": 18.5 }),
    };
    Event {
        id: 7,
        event_type: event_type::MISSION_COMPLETED.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        source: "missions".into(),
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn mission_completed_schedules_one_debrief_and_replays_are_no_ops() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].filter, event_type::MISSION_COMPLETED);

    // Nothing scheduled for this mission yet, then the insert.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![event_row(
        9,
        SCOPE_LODGE,
        Some("3"),
        "2026-10-05T18:00:00",
        "",
    )]);

    (subs[0].handler)(mission_completed_event()).await.unwrap();
    host.events.assert_published(event_type::EVENT_CREATED);
    assert_audited(&host, "event.create.debrief");
    let insert = host
        .db
        .queried_sql()
        .into_iter()
        .find(|sql| sql.contains("INSERT INTO") && sql.contains("events"))
        .expect("the debrief was inserted");
    assert!(
        insert.contains("ON CONFLICT (source_mission_id)"),
        "{insert}"
    );
    let params = host.db.last_query_params("INSERT INTO").unwrap();
    let rendered: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
    assert!(
        rendered
            .iter()
            .any(|p| p.contains("Debrief — Coyote survey")),
        "{rendered:?}"
    );
    assert!(
        rendered.iter().any(|p| p.contains("2026-10-05T18:00:00")),
        "a week later, 6pm"
    );
    assert!(
        rendered.iter().any(|p| p.contains("42")),
        "the mission id is the idempotency key"
    );

    // The bus can redeliver: the second delivery finds the debrief and stops.
    host.db.push_rows(vec![json!({ "id": 9 })]);
    let queries_before = host.db.query_count();
    (subs[0].handler)(mission_completed_event()).await.unwrap();
    assert_eq!(
        host.db.query_count(),
        queries_before + 1,
        "only the probe ran"
    );
}

#[tokio::test]
async fn a_malformed_mission_completed_is_reported_rather_than_guessed() {
    let (host, plugin, _routes) = plugin().await;
    let subs = plugin.subscriptions();
    let event = Event {
        id: 8,
        event_type: event_type::MISSION_COMPLETED.to_string(),
        payload: json!({ "nope": true }),
        source: "missions".into(),
        timestamp: Utc::now(),
    };
    let error = (subs[0].handler)(event)
        .await
        .expect_err("a bad payload must not be guessed at");
    assert!(matches!(error, SdkError::BadRequest(_)), "{error}");
    assert_eq!(host.db.query_count(), 0);
}

#[test]
fn the_vocabulary_the_api_accepts_is_the_vocabulary_that_is_constrained() {
    // The database's CHECK constraints and the handlers' lists are the same
    // strings; a drift here would be a 500 instead of a 400.
    assert_eq!(RESPONSES, ["going", "not_going", "maybe", "pending"]);
    assert!(CATEGORIES.contains(&"debrief"));
    assert!(adjutant_calendar::BODY_CODES.contains(&"congress"));
    assert_eq!(
        render_local(local("2026-10-06T18:00:00")),
        "2026-10-06T18:00:00"
    );
}
