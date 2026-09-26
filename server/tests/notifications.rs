//! DB-backed probes for the notification record (#46, slice 1).
//!
//! Design: `docs/design/notifications.md`. These prove, against a real
//! PostgreSQL and through the **real route handlers**, the acceptance criteria
//! the issue states:
//!
//! - a scheduled run records a notification for a user (through `core.notify`,
//!   the function a plugin's [`Schedule`](adjutant_sdk::Schedule) handler calls on
//!   its own pool — the same seam the background-check sweep will use), and the
//!   core itself can record one directly;
//! - the recipient lists it and marks it read;
//! - a **different** member can neither read it nor mark it read, and cannot tell
//!   it exists;
//! - the delivery state is **not** silently reported as delivered when the record
//!   was only recorded: the two facts are distinguishable in the row, in the API
//!   response and in the schema, and the schema refuses the lie outright.
//!
//! These tests are `#[ignore]`d on purpose (issue #25). A bare
//! `cargo test --workspace` reports them as **ignored**, never as passed, so a
//! green local run cannot hide a database test that did not execute. CI runs them
//! explicitly against a throwaway `_test` database:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//!   cargo test -p adjutant-server --test notifications -- --ignored --nocapture
//! ```
//!
//! Under `--ignored` a missing, empty, or unreachable
//! `ADJUTANT_TEST_DATABASE_URL` is a hard failure — never a skip.
//!
//! The fixture plugin role is bootstrapped by the probes themselves (they need a
//! role that can `CREATEROLE`, i.e. a throwaway superuser container, exactly like
//! `host_db.rs`'s confinement probes). It is named `bg_check` because the first
//! real consumer is the background-check sweep (`docs/design/bg.md` §4) and the
//! notification's `source` is derived from that role's name — the probe would be
//! less honest under a made-up name.

use std::sync::Arc;

use adjutant_sdk::{AuditService, PermissionService};
use adjutant_server::config::Config;
use adjutant_server::server::AppState;
use adjutant_server::{
    db, host, notifications, outbox, plugin_runtime::PluginRegistry, scope_hierarchy::ScopeHierarchy,
    scheduler::Scheduler,
};
use axum::extract::{Path, State as AxumState};
use axum::http::Request;
use serde_json::{json, Value};
use sqlx::PgPool;

/// Serializes the probes. `CREATE SCHEMA/TABLE IF NOT EXISTS` and `CREATE ROLE`
/// still race between sessions, so concurrent setup would both try to migrate
/// `core` and bootstrap the fixture role — and the probes also assert on the
/// **shared** per-user rows they create, so the guard is held for the whole
/// probe, not only its setup. A probe that let go early would see another
/// probe's rows in its inbox.
static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to run the DB-gated notification \
         probes (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );
    url
}

/// Migrate `core` on the test database (idempotent) and hand back the admin pool.
async fn admin_pool() -> Arc<PgPool> {
    let cfg = Config {
        database_url: base_url(),
        ..Config::default()
    };
    db::connect_and_migrate(&cfg)
        .await
        .unwrap_or_else(|e| panic!("ADJUTANT_TEST_DATABASE_URL is set but unusable: {e}"))
}

/// A pool authenticated as `adjutant_plugin_<id>` — exactly the connection a
/// plugin's `Schedule` handler runs on (the scheduler runs the handler on the
/// plugin's own pool, bound by its isolation role).
async fn plugin_pool(admin: &PgPool, id: &str) -> Arc<PgPool> {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{id}\" CASCADE"))
        .execute(admin)
        .await;
    let secret = adjutant_server::schema::bootstrap_role(admin, id, None, false)
        .await
        .expect("bootstrap the fixture plugin role (needs CREATEROLE/superuser)");
    host::plugin_pool(&base_url(), id, &secret, 2)
        .await
        .expect("the fixture plugin's own pool")
}

/// The core's `AppState`, minimal but real: the real pool, the real permission
/// and audit services, an empty plugin registry (these are core routes, so
/// dispatch does not consult it), and the dev-header stub ON so a probe can hold
/// a member identity without an OIDC provider.
fn probe_state(pool: Arc<PgPool>) -> Arc<AppState> {
    Arc::new(AppState {
        pool: pool.clone(),
        permissions: PermissionService::new(host::CoreDb::new(pool.clone())),
        audit: AuditService::new(host::CoreDb::new(pool.clone()), "core".into()),
        registry: tokio::sync::RwLock::new(PluginRegistry::new(Vec::new())),
        bus: adjutant_server::events::EventBus::new(),
        config: Arc::new(Config {
            database_url: base_url(),
            allow_dev_headers: true,
            ..Config::default()
        }),
        identity: adjutant_server::identity::IdentityHub::new(),
        http: host::CoreHttp::new(),
        hierarchy: tokio::sync::RwLock::new(ScopeHierarchy::default()),
        scheduler: Scheduler::new(),
        outbox_mismatches: std::sync::Mutex::new(0),
        relay: outbox::Relay::new(),
        in_flight: adjutant_server::server::InFlight::new(),
        lifecycles: adjutant_server::server::LifecycleLocks::new(),
    })
}

const BEA: &str = "11111111-1111-4111-8111-111111111111";
const CAL: &str = "22222222-2222-4222-8222-222222222222";

/// A member, created the way the roster would create one.
async fn make_user(pool: &PgPool, id: &str, name: &str) {
    sqlx::query("INSERT INTO core.users (id, display_name) VALUES ($1::uuid, $2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("a member row");
}

/// A request carrying the caller's identity, the way a session does.
fn request(user: &str) -> Request<axum::body::Body> {
    Request::builder()
        .header("x-dev-user", user)
        .body(axum::body::Body::empty())
        .expect("request builds")
}

/// The response body as JSON. A response the probe cannot read is a failed
/// probe, not a null.
async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .expect("a response body");
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// What a plugin's scheduled sweep does: record a notification for one member.
/// It runs on the plugin's own pool, as the plugin's own role — so it goes
/// through `core.notify`, which derives `source` from `session_user`.
async fn scheduled_sweep(
    plugin: &PgPool,
    recipient: &str,
    code: &str,
    params: Value,
    locale: &str,
) -> i64 {
    sqlx::query_scalar("SELECT core.notify($1::uuid, $2, $3::jsonb, $4)")
        .bind(recipient)
        .bind(code)
        .bind(params.to_string())
        .bind(locale)
        .fetch_one(plugin)
        .await
        .expect("a scheduled run records a notification through core.notify")
}

// ---------------------------------------------------------------------------
// 1. A scheduled run records one; its recipient reads it and marks it read.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL (+ CREATEROLE); run with `-- --ignored`"]
async fn probe_a_scheduled_run_records_and_the_recipient_reads_it() {
    let _guard = SETUP.lock().await;
    let admin = admin_pool().await;
    make_user(admin.as_ref(), BEA, "Bea").await;
    sqlx::query("DELETE FROM core.notifications WHERE recipient = $1::uuid")
        .bind(BEA)
        .execute(admin.as_ref())
        .await
        .expect("clear the probe's records");
    let plugin = plugin_pool(admin.as_ref(), "bg_check").await;
    let state = probe_state(admin.clone());

    // (a) The scheduled run's path: as the plugin's own role, through core.notify.
    let sweep_id = scheduled_sweep(
        plugin.as_ref(),
        BEA,
        "bg_check.expiring",
        json!({ "expires_on": "2026-11-01", "days_left": 36 }),
        "fr-CA",
    )
    .await;
    // (b) The core's own direct path (the "core can be driven directly" case).
    let core_id = notifications::create(
        admin.as_ref(),
        notifications::SOURCE_CORE,
        BEA,
        "core.dues_overdue",
        json!({ "amount_cents": 4200 }),
        notifications::LOCALE_UNKNOWN,
    )
    .await
    .expect("the core records a notification directly");

    // The record names its producer and its message, and stores NO display text.
    let (source, code, locale, params): (String, String, String, Value) = sqlx::query_as(
        "SELECT source, message_code, locale, message_params FROM core.notifications WHERE id = $1",
    )
    .bind(sweep_id)
    .fetch_one(admin.as_ref())
    .await
    .expect("the scheduled run's row");
    assert_eq!(source, "bg_check", "source is the plugin role, never a parameter");
    assert_eq!(code, "bg_check.expiring");
    assert_eq!(locale, "fr-CA", "the recipient's language travels with the message");
    assert_eq!(params["days_left"], json!(36));

    // The recipient lists their inbox — through the real route handler.
    let listed = notifications::list_notifications(AxumState(state.clone()), request(BEA)).await;
    assert_eq!(listed.status(), 200, "a member may read their own inbox");
    let body = json_body(listed).await;
    assert_eq!(body["count"], json!(2), "both records are in the inbox: {body}");
    assert_eq!(body["unread"], json!(2));
    let ids: Vec<i64> = body["notifications"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|n| n["id"].as_i64())
        .collect();
    assert!(ids.contains(&sweep_id) && ids.contains(&core_id), "got {ids:?}");

    // The unread filter is honoured.
    let unread = notifications::list_notifications(
        AxumState(state.clone()),
        Request::builder()
            .header("x-dev-user", BEA)
            .uri("/api/notifications?unread=true")
            .body(axum::body::Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(json_body(unread).await["count"], json!(2));

    // Mark it read, through the real route handler.
    let marked = notifications::mark_notification_read(
        AxumState(state.clone()),
        Path(sweep_id),
        request(BEA),
    )
    .await;
    assert_eq!(marked.status(), 200);
    let marked = json_body(marked).await;
    assert_eq!(marked["is_read"], json!(true));
    assert_eq!(marked["already_read"], json!(false));

    // **The two facts, distinguishable.** The read marker moved; the delivery
    // state did not, and the response says so in a different object.
    let read = &marked["notification"]["read"];
    let delivery = &marked["notification"]["delivery"];
    assert_eq!(read["is_read"], json!(true));
    assert!(read["read_at"].is_string(), "the read fact is recorded");
    assert_eq!(delivery["state"], json!("recorded"), "nothing has been delivered");
    assert_eq!(delivery["delivered"], json!(false));
    assert_eq!(delivery["delivered_at"], Value::Null);
    assert_eq!(delivery["channel"], json!("in_app"));

    // And the same two facts in the row itself.
    let (read_at, state_col, delivered_at): (Option<String>, String, Option<String>) =
        sqlx::query_as(
            "SELECT read_at::text, delivery_state, delivered_at::text \
             FROM core.notifications WHERE id = $1",
        )
        .bind(sweep_id)
        .fetch_one(admin.as_ref())
        .await
        .expect("the row after mark-read");
    assert!(read_at.is_some(), "read_at is set");
    assert_eq!(state_col, "recorded", "marking read did NOT deliver anything");
    assert!(delivered_at.is_none(), "and nothing claims a delivery time");

    // A second mark-read changes nothing and says so (idempotent, like a receipt).
    let again = notifications::mark_notification_read(
        AxumState(state.clone()),
        Path(sweep_id),
        request(BEA),
    )
    .await;
    assert_eq!(again.status(), 200);
    let again = json_body(again).await;
    assert_eq!(again["already_read"], json!(true));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM core.notifications WHERE recipient = $1::uuid AND id = $2",
    )
    .bind(BEA)
    .bind(sweep_id)
    .fetch_one(admin.as_ref())
    .await
    .expect("count");
    assert_eq!(count, 1, "a second mark-read creates no second record");

    // The badge follows: one of the two is read.
    let after = notifications::list_notifications(AxumState(state.clone()), request(BEA)).await;
    assert_eq!(json_body(after).await["unread"], json!(1));
}

// ---------------------------------------------------------------------------
// 2. Another member cannot read it, mark it read, or learn that it exists.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL (+ CREATEROLE); run with `-- --ignored`"]
async fn probe_another_member_cannot_read_or_mark_anothers_notification() {
    let _guard = SETUP.lock().await;
    let admin = admin_pool().await;
    make_user(admin.as_ref(), BEA, "Bea").await;
    make_user(admin.as_ref(), CAL, "Cal").await;
    sqlx::query("DELETE FROM core.notifications WHERE recipient IN ($1::uuid, $2::uuid)")
        .bind(BEA)
        .bind(CAL)
        .execute(admin.as_ref())
        .await
        .expect("clear the probes' records");
    let state = probe_state(admin.clone());

    let id = notifications::create(
        admin.as_ref(),
        notifications::SOURCE_CORE,
        BEA,
        "core.dues_overdue",
        json!({}),
        "en",
    )
    .await
    .expect("a record for Bea");

    // Cal's inbox is Cal's: Bea's record is not in it.
    let cal_list = notifications::list_notifications(AxumState(state.clone()), request(CAL)).await;
    assert_eq!(cal_list.status(), 200);
    let cal_list = json_body(cal_list).await;
    assert_eq!(cal_list["count"], json!(0), "Cal's inbox holds Cal's records: {cal_list}");

    // Cal cannot mark Bea's record read — and is told it does not exist, because
    // whether somebody else has a notification is not Cal's business (404, not 403).
    let cal_mark =
        notifications::mark_notification_read(AxumState(state.clone()), Path(id), request(CAL)).await;
    assert_eq!(cal_mark.status(), 404, "another member's record is not found, not forbidden");

    // Nothing moved.
    let (read_at, state_col): (Option<String>, String) = sqlx::query_as(
        "SELECT read_at::text, delivery_state FROM core.notifications WHERE id = $1",
    )
    .bind(id)
    .fetch_one(admin.as_ref())
    .await
    .expect("the row is untouched");
    assert!(read_at.is_none(), "Cal's attempt marked nothing read");
    assert_eq!(state_col, "recorded");

    // The owner still can.
    let bea_mark =
        notifications::mark_notification_read(AxumState(state.clone()), Path(id), request(BEA)).await;
    assert_eq!(bea_mark.status(), 200);

    // An unauthenticated caller gets 401, never an empty list that looks like a
    // quiet answer.
    let anon = notifications::list_notifications(
        AxumState(state.clone()),
        Request::builder()
            .body(axum::body::Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(anon.status(), 401);
}

// ---------------------------------------------------------------------------
// 3. "Recorded" is never reported as "delivered", and the schema refuses the lie.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL (+ CREATEROLE); run with `-- --ignored`"]
async fn probe_recorded_is_never_reported_as_delivered() {
    let _guard = SETUP.lock().await;
    let admin = admin_pool().await;
    make_user(admin.as_ref(), BEA, "Bea").await;
    sqlx::query("DELETE FROM core.notifications WHERE recipient = $1::uuid")
        .bind(BEA)
        .execute(admin.as_ref())
        .await
        .expect("clear the probe's records");
    let plugin = plugin_pool(admin.as_ref(), "bg_check").await;
    let state = probe_state(admin.clone());

    let id = scheduled_sweep(
        plugin.as_ref(),
        BEA,
        "bg_check.expiring",
        json!({ "days_left": 30 }),
        "en",
    )
    .await;

    // A freshly recorded notification is recorded, and says so in the API.
    let listed = notifications::list_notifications(AxumState(state.clone()), request(BEA)).await;
    let listed = json_body(listed).await;
    let row = listed["notifications"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|n| n["id"].as_i64() == Some(id))
        .expect("the probe's own record");
    assert_eq!(row["delivery"]["state"], json!("recorded"));
    assert_eq!(row["delivery"]["delivered"], json!(false));
    assert_eq!(row["delivery"]["delivered_at"], Value::Null);
    assert_eq!(row["read"]["is_read"], json!(false));
    assert_eq!(listed["delivery"]["channels"], json!(["in_app"]));
    assert!(
        listed["delivery"]["note"]
            .as_str()
            .unwrap_or_default()
            .contains("recorded is not delivered"),
        "the response carries the hard rule's sentence"
    );

    // The lie is **unstorable**, not merely unwritten: a `delivered` state with
    // no evidence is refused by the schema.
    let lie = sqlx::query("UPDATE core.notifications SET delivery_state = 'delivered' WHERE id = $1")
        .bind(id)
        .execute(admin.as_ref())
        .await;
    assert!(
        lie.is_err(),
        "a notification must not be storable as delivered with no delivered_at"
    );
    // …and the evidence alone, with no state, is refused too.
    let evidence_without_state =
        sqlx::query("UPDATE core.notifications SET delivered_at = now() WHERE id = $1")
            .bind(id)
            .execute(admin.as_ref())
            .await;
    assert!(
        evidence_without_state.is_err(),
        "a delivery time must not be storable without the delivered state"
    );

    // A delivery that IS real is storable — the constraint forbids the lie, not
    // the transport.
    sqlx::query(
        "UPDATE core.notifications SET delivery_state = 'delivered', delivered_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(admin.as_ref())
    .await
    .expect("a transport's real delivery is storable");

    // The producer seam is not open to the core's own connection either: a
    // notification is created by a *plugin's* scheduled run (as itself) or by the
    // core directly — never by whoever happens to hold a connection.
    let refused = sqlx::query_scalar::<_, i64>("SELECT core.notify($1::uuid, 'x.y', '{}'::jsonb, 'en')")
        .bind(BEA)
        .fetch_one(admin.as_ref())
        .await;
    assert!(
        refused.is_err(),
        "core.notify is for plugin roles; the core's own connection uses notifications::create"
    );

    // And a plugin role cannot reach the table directly — the record is read
    // through the core's routes, not by a plugin's SELECT.
    let denied = sqlx::query("SELECT id FROM core.notifications LIMIT 1")
        .fetch_all(plugin.as_ref())
        .await;
    assert!(
        denied.is_err(),
        "core.notifications is not a plugin-readable table: {denied:?}"
    );
}
