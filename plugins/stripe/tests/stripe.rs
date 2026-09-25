//! Stripe plugin tests: the signature check, the idempotency of a redelivered
//! webhook, and the honest account of how a confirmed payment reaches the
//! ledger (SPEC §7.13, `docs/design/plugin-to-plugin.md`).
//!
//! Handlers are driven through `adjutant_sdk::testing` — no database, no
//! server, no Stripe. The mock replays rows in call order, so each test names
//! the **query order** it arranges around: the first `push_rows` answers the
//! first query.
//!
//! The HTTP tests use a recording `HostHttp` instead of `MockHttp`, because the
//! two properties that matter most here — that a cross-plugin call carries the
//! caller's own credential and nothing else, and that no secret ever leaves the
//! plugin — are properties of the *request*, and `MockHttp` records only
//! `(method, url)`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use adjutant_sdk::async_trait;
use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::{response_json, MockDb, MockEvents, MockIdentity, TestHost, TestRequest};
use adjutant_stripe::{
    body_digest, checkout_form, category_for, form_encode_value, forward_headers, parse_amount_to_cents,
    sign, verify_signature, CheckoutRequest, StripePlugin, CATEGORY_DUES, DEFAULT_BASE_URL,
    FINANCE_FUNDS_PATH, FINANCE_TRANSACTION_PATH, LEDGER_BOOKED, LEDGER_DELEGATED_EVENT,
    LEDGER_INTENT_ENQUEUED, LEDGER_REFUSED, LEDGER_UNBOOKED, MECHANISM_CALLER_FORWARD,
    MECHANISM_OUTBOX, PERM_CHECKOUT, PERM_MANAGE, PERM_READ,
    PERM_READ_ALL, PURPOSE_DONATION, PURPOSE_DUES, PURPOSE_EVENT_FEE, PURPOSES,
    SESSION_COMPLETED, SESSION_FAILED, SIGNATURE_HEADER,
};

/// Not a real key, and distinctive enough that a scan for it cannot miss.
const SECRET_KEY: &str = "sk_test_NOTAREALKEY0000";
/// Not a real signing secret.
const WEBHOOK_SECRET: &str = "whsec_NOTAREALSECRET0000";
/// The Adjutant address the ledger call dials.
const BASE_URL: &str = "http://adjutant.test:8787";

fn config() -> serde_json::Value {
    serde_json::json!({
        "secret_key": SECRET_KEY,
        "webhook_secret": WEBHOOK_SECRET,
        "base_url": BASE_URL,
        "api_base": "https://api.stripe.test",
        "currency": "cad",
        "success_url": "https://troop.test/paid",
        "cancel_url": "https://troop.test/dues",
        "webhook_tolerance_seconds": 300,
        "dues_fund_code": "general",
        "unbooked_after_minutes": 30,
    })
}

fn config_without_secrets() -> serde_json::Value {
    serde_json::json!({ "base_url": BASE_URL })
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Initialise the plugin against a mock host and hand back its routes.
async fn plugin_routes(config: serde_json::Value) -> (TestHost, Vec<RouteDefinition>) {
    let host = TestHost::new().with_config(config);
    let mut plugin = StripePlugin::new();
    plugin.init(host.context("stripe")).await.unwrap();
    let routes = plugin.routes();
    (host, routes)
}

fn route<'a>(routes: &'a [RouteDefinition], method: &str, path: &str) -> &'a RouteDefinition {
    routes
        .iter()
        .find(|r| r.method.as_str() == method && r.path == path)
        .unwrap_or_else(|| panic!("route {method} {path}"))
}

/// Drive a handler and report `(status, body)`. A handler may refuse with an
/// `SdkError` (mapped through `SdkError::status()`) or a `PluginResponse::error`
/// — from a client they are the same answer.
async fn call(handler: &adjutant_sdk::RouteHandler, req: PluginRequest) -> (u16, serde_json::Value) {
    match handler(req).await {
        Ok(resp) => (resp.status, response_json(&resp)),
        Err(e) => (e.status(), serde_json::json!({ "error": e.to_string() })),
    }
}

/// One recorded outbound request, headers and body included.
#[derive(Debug, Clone)]
struct RecordedCall {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

impl RecordedCall {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A `HostHttp` that keeps everything: this is how a test can assert what a
/// cross-plugin call actually carried.
#[derive(Default)]
struct RecordingHttp {
    calls: Mutex<Vec<RecordedCall>>,
    responses: Mutex<VecDeque<(u16, Vec<u8>)>>,
}

impl RecordingHttp {
    fn push_json(&self, status: u16, body: &serde_json::Value) {
        self.responses
            .lock()
            .unwrap()
            .push_back((status, serde_json::to_vec(body).unwrap_or_default()));
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Every byte this plugin sent outward, concatenated — the haystack for the
    /// "no secret ever leaves" scan.
    fn outbound_text(&self) -> String {
        self.calls()
            .iter()
            .map(|call| {
                format!(
                    "{} {} {:?} {}",
                    call.method,
                    call.url,
                    call.headers,
                    call.body.clone().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl HostHttp for RecordingHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        self.calls.lock().unwrap().push(RecordedCall {
            method,
            url,
            headers,
            body: body.map(|(_, bytes)| String::from_utf8_lossy(&bytes).to_string()),
        });
        match self.responses.lock().unwrap().pop_front() {
            Some((status, body)) => Ok(HttpResponse {
                status,
                headers: std::collections::HashMap::new(),
                body,
            }),
            None => Err(SdkError::Internal(
                "RecordingHttp: no queued response (the handler reached the network unexpectedly)"
                    .into(),
            )),
        }
    }
}

/// The mock host, with a recording HTTP client and a real `PluginContext` built
/// the same way `TestHost` builds one.
struct Harness {
    db: Arc<MockDb>,
    events: Arc<MockEvents>,
    http: Arc<RecordingHttp>,
    config: serde_json::Value,
}

impl Harness {
    fn new(config: serde_json::Value) -> Self {
        Self {
            db: Arc::new(MockDb::new()),
            events: Arc::new(MockEvents::new()),
            http: Arc::new(RecordingHttp::default()),
            config,
        }
    }

    fn context(&self) -> PluginContext {
        let db: Arc<dyn HostDb> = self.db.clone();
        PluginContext {
            plugin_id: "stripe".to_string(),
            db: DbHandle::new(db.clone(), "stripe".to_string()),
            config: self.config.clone(),
            events: EventBusHandle::new(self.events.clone(), "stripe".to_string()),
            permissions: PermissionService::new(db.clone()),
            audit: AuditService::new(db, "stripe".to_string()),
            identity: Arc::new(MockIdentity::new()),
            http: self.http.clone(),
        }
    }

    async fn routes(&self) -> Vec<RouteDefinition> {
        let mut plugin = StripePlugin::new();
        plugin.init(self.context()).await.unwrap();
        plugin.routes()
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn payment_row(status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 5,
        "payment_id": "pi_test_1",
        "event_id": "evt_test_1",
        "session_id": "cs_test_1",
        "purpose": PURPOSE_DUES,
        "amount_cents": 2500,
        "currency": "cad",
        "member_id": "42",
        "fund_code": "general",
        "category": CATEGORY_DUES,
        "description": "Troop dues 2026 (member 42)",
        "dues_year": 2026,
        "livemode": false,
        "ledger_status": status,
        "ledger_mechanism": if status == LEDGER_DELEGATED_EVENT { "event:payment.received" } else { "" },
        "ledger_transaction_id": null,
        "ledger_error": null,
        "confirmed_at": "2026-09-25 12:00:00+00",
        "ledger_attempted_at": null,
    })
}

fn session_row() -> serde_json::Value {
    serde_json::json!({
        "id": 7,
        "purpose": PURPOSE_DUES,
        "amount_cents": 2500,
        "currency": "cad",
        "member_id": "42",
        "fund_code": "general",
        "category": CATEGORY_DUES,
        "description": "Troop dues 2026 (member 42)",
        "related_event_id": "",
        "dues_year": 2026,
        "status": "pending",
        "stripe_session_id": null,
        "checkout_url": "",
        "created_by": "bea",
        "created_at": "2026-09-25 11:59:00+00",
        "updated_at": "2026-09-25 11:59:00+00",
    })
}

/// The same session once Stripe has answered for it.
fn settled_session_row() -> serde_json::Value {
    let mut row = session_row();
    row["status"] = serde_json::json!("created");
    row["stripe_session_id"] = serde_json::json!("cs_test_1");
    row["checkout_url"] = serde_json::json!("https://checkout.stripe.test/cs_test_1");
    row
}

/// A `checkout.session.completed` delivery, exactly as Stripe renders one.
fn session_completed_payload() -> serde_json::Value {
    serde_json::json!({
        "id": "evt_test_1",
        "object": "event",
        "type": "checkout.session.completed",
        "api_version": "2024-06-20",
        "created": 1_758_801_600_i64,
        "livemode": false,
        "data": {
            "object": {
                "id": "cs_test_1",
                "object": "checkout.session",
                "payment_status": "paid",
                "payment_intent": "pi_test_1",
                "amount_total": 2500,
                "currency": "cad",
                "client_reference_id": "stripe-session-7",
                "metadata": {
                    "purpose": "dues",
                    "member_id": "42",
                    "fund_code": "general",
                    "category": "dues",
                    "dues_year": "2026",
                    "session_ref": "stripe-session-7",
                    "description": "Troop dues 2026 (member 42)",
                }
            }
        }
    })
}

/// A signed webhook request: the body is serialized once and signed over the
/// same bytes the plugin will verify.
fn signed_webhook(payload: &serde_json::Value, secret: &str, timestamp: i64) -> PluginRequest {
    let body = serde_json::to_vec(payload).unwrap();
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(&body);
    let signature = sign(secret, &message);
    let header = format!("t={timestamp},v1={signature}");
    TestRequest::post("/api/stripe/webhook")
        .json(payload)
        .header(SIGNATURE_HEADER, &header)
        .build()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// SqlValue has no `PartialEq` on purpose (it crosses an FFI-ish boundary), so
// parameter assertions go through these accessors.
// ---------------------------------------------------------------------------

fn text_of(param: &SqlValue) -> Option<String> {
    match param {
        SqlValue::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn int_of(param: &SqlValue) -> Option<i64> {
    match param {
        SqlValue::Int(value) => Some(*value),
        _ => None,
    }
}

fn has_text(params: &[SqlValue], expected: &str) -> bool {
    params.iter().any(|param| text_of(param).as_deref() == Some(expected))
}

fn has_int(params: &[SqlValue], expected: i64) -> bool {
    params.iter().any(|param| int_of(param) == Some(expected))
}

/// The actions recorded in `core.audit_log`, in order. The action is a bind
/// parameter, so `assert_executed` (which greps SQL) cannot see it.
fn audited_actions(db: &MockDb) -> Vec<String> {
    db.executed
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.sql.contains("core.audit_log"))
        .filter_map(|call| call.params.get(1).and_then(text_of))
        .collect()
}

fn assert_audited(db: &MockDb, action: &str) {
    let actions = audited_actions(db);
    assert!(
        actions.iter().any(|a| a == action),
        "no audit entry for {action}; audited: {actions:?}"
    );
}

fn canonical(params: &[SqlValue]) -> Vec<String> {
    params
        .iter()
        .map(|param| match param {
            SqlValue::Text(text) => format!("text:{text}"),
            SqlValue::Int(value) => format!("int:{value}"),
            SqlValue::Null => "null:text".to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The declaration the loader validates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_manifest_satisfies_the_load_rules() {
    let host = TestHost::new().with_config(config());
    let mut plugin = StripePlugin::new();
    plugin.init(host.context("stripe")).await.unwrap();
    let routes = plugin.routes();
    assert_eq!(plugin.id(), "stripe");
    assert_eq!(plugin.name(), "Stripe");

    let permissions: Vec<String> = plugin
        .permissions_granted()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        permissions,
        vec![PERM_READ, PERM_READ_ALL, PERM_CHECKOUT, PERM_MANAGE],
        "the declared set is the documented set"
    );
    for perm in &permissions {
        assert!(
            perm.starts_with("stripe:"),
            "permission {perm} must be namespaced by the plugin id"
        );
    }

    // Every protected route's permission is declared, or the loader rejects the
    // plugin — the one rule that makes the manifest loadable.
    for r in &routes {
        assert!(
            r.path.starts_with("/api/stripe"),
            "route {} escapes the namespace",
            r.path
        );
        match &r.required_permission {
            Some(permission) => {
                assert!(
                    permissions.contains(permission),
                    "route {} requires {permission}, which is not declared in \
                     permissions_granted()",
                    r.path
                );
            }
            None => {
                assert_eq!(
                    r.path, "/api/stripe/webhook",
                    "only the webhook may be open: {} has no permission",
                    r.path
                );
            }
        }
    }
    assert_eq!(routes.len(), 9, "nine routes, as documented");
    assert_eq!(
        routes.iter().filter(|r| r.required_permission.is_none()).count(),
        1,
        "exactly one open route (the webhook, whose credential is the signature)"
    );

    let migrations = plugin.migrations();
    let mut versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    versions.sort_unstable();
    let before = versions.len();
    versions.dedup();
    assert_eq!(before, versions.len(), "migration versions must be unique");
    assert!(versions.iter().all(|v| *v >= 1), "versions start at 1");
    assert_eq!(
        versions,
        vec![1, 2],
        "the outbox intent's status had to be a NEW version: an applied migration is skipped \
         without its SQL being compared, so amending version 1 would be invisible on every \
         database that already ran it"
    );

    let ddl = &migrations[0].sql;
    for table in ["checkout_sessions", "webhook_events", "payments"] {
        assert!(ddl.contains(table), "the schema must create {table}");
    }
    // The vocabulary that reaches a decision is constrained, not conventional.
    assert!(ddl.contains("payments_status_valid"));
    assert!(ddl.contains("checkout_sessions_purpose_valid"));
    assert!(ddl.contains("payment_id TEXT NOT NULL UNIQUE"));

    // Version 1 is byte-identical to what shipped inline: the version and name in
    // `core.schema_migrations` are unchanged, so nothing re-runs on a deployed
    // database and the runner's skip is not asked to notice a difference.
    assert!(
        !ddl.contains("ledger_intent_id"),
        "the intent column belongs to version 2, not to an amended version 1"
    );
    let second = &migrations[1].sql;
    assert!(second.contains("ADD COLUMN IF NOT EXISTS ledger_intent_id"));
    assert!(second.contains("idx_stripe_payments_intent"));
    // The replaced check admits the new status and keeps every old one.
    for status in [
        "unbooked",
        "intent_enqueued",
        "delegated_event",
        "booked",
        "refused",
        "failed",
    ] {
        assert!(
            second.contains(&format!("'{status}'")),
            "version 2's check must name {status}"
        );
    }
    assert_eq!(
        plugin.migrations()[1].name,
        "payment_ledger_intent",
        "the version and name are the migration's identity in core.schema_migrations"
    );

    // This plugin subscribes to the core's outbox outcome events — a
    // **notification** that settles its own `ledger_status`, never the mechanism
    // by which the ledger learns (the intent is). It still does not subscribe to
    // anything it publishes itself (plugin-to-plugin.md §3.4).
    let subscriptions = plugin.subscriptions();
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].filter, adjutant_stripe::OUTBOX_EVENT_PREFIX);
    assert_eq!(subscriptions[0].filter, "core.outbox.");
    assert!(
        subscriptions[0].matches("core.outbox.delivered")
            && subscriptions[0].matches("core.outbox.exhausted"),
        "it must take the relay's terminal outcomes"
    );
    let schedules = plugin.schedules();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].name, "unbooked_sweep");
}

// ---------------------------------------------------------------------------
// Signature verification
// ---------------------------------------------------------------------------

#[test]
fn a_signature_stripe_would_send_verifies() {
    let body = br#"{"id":"evt_1"}"#;
    let timestamp = 1_758_801_600_i64;
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    let header = format!("t={timestamp},v1={}", sign(WEBHOOK_SECRET, &message));

    let stamp = verify_signature(Some(&header), body, WEBHOOK_SECRET, 300, timestamp + 5).unwrap();
    assert_eq!(stamp.timestamp, timestamp);
    assert_eq!(stamp.matched.len(), 64, "a hex SHA-256");
}

#[test]
fn a_tampered_body_a_wrong_secret_and_a_stale_stamp_are_all_refused() {
    let body = br#"{"id":"evt_1","amount":2500}"#;
    let timestamp = 1_758_801_600_i64;
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    let header = format!("t={timestamp},v1={}", sign(WEBHOOK_SECRET, &message));

    // The body changed after signing: what a forged delivery looks like.
    assert!(verify_signature(
        Some(&header),
        br#"{"id":"evt_1","amount":250000}"#,
        WEBHOOK_SECRET,
        300,
        timestamp
    )
    .is_err());
    // The right body, the wrong secret.
    assert!(verify_signature(Some(&header), body, "whsec_other", 300, timestamp).is_err());
    // Right everywhere, outside the replay window.
    assert!(verify_signature(Some(&header), body, WEBHOOK_SECRET, 300, timestamp + 3600).is_err());
    // A delivery dated in the future is as suspect as an old one.
    assert!(verify_signature(Some(&header), body, WEBHOOK_SECRET, 300, timestamp - 3600).is_err());
    // Missing, empty, and malformed headers.
    assert!(verify_signature(None, body, WEBHOOK_SECRET, 300, timestamp).is_err());
    assert!(verify_signature(Some(""), body, WEBHOOK_SECRET, 300, timestamp).is_err());
    assert!(verify_signature(Some("t=abc,v1=00"), body, WEBHOOK_SECRET, 300, timestamp).is_err());
    assert!(verify_signature(Some("nonsense"), body, WEBHOOK_SECRET, 300, timestamp).is_err());
    assert!(verify_signature(Some("v1=00"), body, WEBHOOK_SECRET, 300, timestamp).is_err());
    // A configured-but-blank secret refuses rather than verifying nothing.
    assert!(verify_signature(Some(&header), body, "  ", 300, timestamp).is_err());
}

#[test]
fn a_rotating_secret_is_accepted_when_any_signature_matches() {
    let body = br#"{"id":"evt_1"}"#;
    let timestamp = 1_758_801_600_i64;
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    let header = format!(
        "t={timestamp},v1={},v1={}",
        "0".repeat(64),
        sign(WEBHOOK_SECRET, &message)
    );
    assert!(verify_signature(Some(&header), body, WEBHOOK_SECRET, 300, timestamp).is_ok());
    // The retired scheme is not a fallback: accepting `v0` would be a downgrade.
    let v0 = format!("t={timestamp},v0={}", sign(WEBHOOK_SECRET, &message));
    assert!(verify_signature(Some(&v0), body, WEBHOOK_SECRET, 300, timestamp).is_err());
}

#[test]
fn the_body_digest_is_the_sha256_of_what_arrived() {
    assert_eq!(
        body_digest(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

// ---------------------------------------------------------------------------
// What the plugin sends Stripe
// ---------------------------------------------------------------------------

#[test]
fn the_checkout_form_carries_the_facts_twice() {
    let request = CheckoutRequest {
        row_id: 7,
        purpose: PURPOSE_DUES.to_string(),
        amount_cents: 2500,
        currency: "cad".to_string(),
        label: "Troop dues 2026 (member 42)".to_string(),
        member_id: "42".to_string(),
        fund_code: "general".to_string(),
        category: CATEGORY_DUES.to_string(),
        related_event_id: String::new(),
        dues_year: Some(2026),
        success_url: "https://troop.test/paid".to_string(),
        cancel_url: "https://troop.test/dues".to_string(),
    };
    let form = checkout_form(&request);
    let get = |key: &str| {
        form.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("the form has no {key}"))
    };
    assert_eq!(get("mode"), "payment");
    assert_eq!(get("line_items[0][price_data][unit_amount]"), "2500");
    assert_eq!(get("line_items[0][price_data][currency]"), "cad");
    assert_eq!(get("client_reference_id"), "stripe-session-7");
    assert_eq!(get("success_url"), "https://troop.test/paid");
    // On the session *and* on its PaymentIntent: either event shape can be the
    // one Stripe delivers first.
    assert_eq!(get("metadata[purpose]"), PURPOSE_DUES);
    assert_eq!(get("payment_intent_data[metadata][purpose]"), PURPOSE_DUES);
    assert_eq!(get("metadata[session_ref]"), "stripe-session-7");
    assert_eq!(get("payment_intent_data[metadata][dues_year]"), "2026");
    // A field nobody set is not sent at all.
    assert!(!form.iter().any(|(k, _)| k.contains("related_event_id")));
}

#[test]
fn form_values_cannot_inject_a_field() {
    assert_eq!(form_encode_value("a&b=c"), "a%26b%3Dc");
    assert_eq!(form_encode_value("2+2"), "2%2B2");
    assert_eq!(form_encode_value("fée"), "f%C3%A9e");
    // Bracket paths stay readable, exactly as Stripe's own examples write them.
    assert_eq!(form_encode_value("metadata[x]"), "metadata[x]");
}

#[test]
fn amounts_are_cents_and_never_floats() {
    assert_eq!(parse_amount_to_cents("25").unwrap(), 2500);
    assert_eq!(parse_amount_to_cents("$1,234.56").unwrap(), 123456);
    assert_eq!(parse_amount_to_cents("0.05").unwrap(), 5);
    assert!(parse_amount_to_cents("25.005").is_err());
    assert!(parse_amount_to_cents("-1").is_err());
    assert!(parse_amount_to_cents("1e3").is_err());
}

// ---------------------------------------------------------------------------
// The webhook
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_signed_webhook_records_one_payment_and_hands_it_to_finance() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // 1. the receipt, 2. the payment, 3. the settled row the hand-off returns.
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);
    host.db.push_rows(vec![payment_row("unbooked")]);
    host.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["received"], true);
    assert_eq!(body["duplicate"], false);
    assert_eq!(body["payment"]["payment_id"], "pi_test_1");
    assert_eq!(body["payment"]["amount_cents"], 2500);
    assert_eq!(
        body["payment"]["ledger_status"], LEDGER_DELEGATED_EVENT,
        "the response reports the payment as it now stands, not as it was inserted"
    );

    // The payment is recorded with the ledger hand-off named, not assumed.
    let insert = host
        .db
        .last_query_params("INSERT INTO \"stripe\".\"payments\"")
        .expect("the payment insert ran");
    assert!(has_int(&insert, 2500), "{:?}", canonical(&insert));
    assert!(has_text(&insert, PURPOSE_DUES), "{:?}", canonical(&insert));
    assert!(
        has_text(&insert, CATEGORY_DUES),
        "a dues payment is filed under finance's reserved category: {:?}",
        canonical(&insert)
    );
    assert!(
        has_int(&insert, 2026),
        "the fiscal year rides along: {:?}",
        canonical(&insert)
    );

    // The session it came from is settled by the reference we sent Stripe.
    host.db
        .assert_executed(&["UPDATE \"stripe\".\"checkout_sessions\"", "WHERE id = $1"]);
    let session_settle = host.db.last_execute_params("checkout_sessions").unwrap();
    assert_eq!(
        int_of(&session_settle[0]),
        Some(7),
        "settled by our own row id (the client_reference_id), not by a lookup"
    );
    // The hand-off settles the payment row; the statement returns rows, so on
    // this host it is a query.
    assert!(
        host.db.queried_sql().iter().any(|sql| sql
            .contains("UPDATE \"stripe\".\"payments\"")
            && sql.contains("ledger_status = $2")
            && sql.contains("ledger_attempted_at = now()")),
        "the hand-off must settle the payment row: {:#?}",
        host.db.queried_sql()
    );
    let outcome = host.db.last_query_params("ledger_status = $2").unwrap();
    assert!(
        has_text(&outcome, LEDGER_DELEGATED_EVENT),
        "{:?}",
        canonical(&outcome)
    );

    // The event finance subscribes to, in finance's own field names.
    host.events.assert_published("payment.received");
    let payloads = host.events.payloads("payment.received");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["payment_id"], "pi_test_1");
    assert_eq!(payloads[0]["amount_cents"], 2500);
    assert_eq!(payloads[0]["fund_code"], "general");
    assert_eq!(payloads[0]["category"], CATEGORY_DUES);
    assert_eq!(payloads[0]["member_id"], "42");
    assert_eq!(payloads[0]["fiscal_year"], 2026);
    assert_eq!(
        payloads[0]["occurred_on"], "2026-09-25",
        "the ledger entry is dated when the payment was confirmed"
    );
    host.events.assert_published("stripe.payment.confirmed");

    // And the audited action is on the record.
    assert_audited(&host.db, "stripe.payment.recorded");

    // The ledger block says plainly what was and was not established.
    assert_eq!(body["ledger"]["path"], "event");
    assert_eq!(body["ledger"]["synchronous"], false);
    assert_eq!(body["ledger"]["status"], LEDGER_DELEGATED_EVENT);
    assert!(body["ledger"]["why"]
        .as_str()
        .unwrap()
        .contains("no credential to forward"));
}

#[tokio::test]
async fn a_confirmed_payment_and_its_ledger_intent_are_one_statement() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // 1. the receipt, 2. the payment the statement returns — and, between them,
    // the funds read the enqueue-time resolution makes.
    h.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);
    h.http.push_json(
        200,
        &serde_json::json!({
            "funds": [
                { "id": 3, "code": "general", "name": "General Fund", "active": true },
                { "id": 9, "code": "scholarship", "name": "Scholarship Fund", "active": true },
            ]
        }),
    );
    let mut recorded = payment_row(LEDGER_INTENT_ENQUEUED);
    recorded["ledger_intent_id"] = serde_json::json!(77);
    recorded["ledger_mechanism"] = serde_json::json!(MECHANISM_OUTBOX);
    h.db.push_rows(vec![recorded]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The fund id was resolved AT ENQUEUE TIME over finance's own route, which is
    // the only way the payload can be complete: the relay cannot read at
    // delivery.
    let calls = h.http.calls();
    assert_eq!(
        calls.len(),
        1,
        "one read and no booking call: the relay books it, not this request"
    );
    assert_eq!(calls[0].method, "GET");
    assert!(calls[0]
        .url
        .starts_with(&format!("{BASE_URL}{FINANCE_FUNDS_PATH}")));
    assert!(
        calls[0].headers.is_empty(),
        "a Stripe webhook carries no credential to forward, and none is invented: {:?}",
        calls[0].headers
    );

    // ONE statement recorded the payment and enqueued the intent: the enqueue is
    // an expression in the INSERT's own VALUES list, and its idempotency key is
    // the payment's identifier ($1), so the two commit together or neither does.
    let inserts: Vec<String> = h
        .db
        .queried_sql()
        .into_iter()
        .filter(|sql| sql.contains("INSERT INTO \"stripe\".\"payments\""))
        .collect();
    assert_eq!(inserts.len(), 1, "exactly one payment insert: {inserts:?}");
    let sql = &inserts[0];
    assert!(
        sql.contains(
            "core.outbox_enqueue($15, 'POST', '/api/finance/transaction', $16::jsonb, $1)"
        ),
        "the intent must be enqueued inside the payment's own insert, keyed on the payment \
         id: {sql}"
    );
    assert!(sql.contains("ledger_intent_id"));
    assert!(sql.contains("ON CONFLICT (payment_id) DO NOTHING"));

    let params = h
        .db
        .last_query_params("core.outbox_enqueue")
        .expect("the statement's binds");
    assert_eq!(
        text_of(&params[14]).as_deref(),
        Some(adjutant_stripe::LEDGER_PRINCIPAL),
        "the principal the core declared for this producer, and no other"
    );
    let payload = match &params[15] {
        SqlValue::Json(raw) => serde_json::from_str::<serde_json::Value>(raw).unwrap(),
        other => panic!("the payload must be JSONB: {other:?}"),
    };
    assert_eq!(payload["fund_id"], 3, "resolve at enqueue time, not at delivery");
    assert_eq!(payload["kind"], "income");
    assert_eq!(payload["amount_cents"], 2500, "income is a positive magnitude");
    assert_eq!(payload["category"], CATEGORY_DUES);
    assert_eq!(
        payload["external_ref"], "pi_test_1",
        "finance's unique key is Stripe's payment id, so a redelivery cannot double-book"
    );
    assert_eq!(payload["member_id"], "42");
    assert_eq!(payload["fiscal_year"], 2026);
    assert!(payload["occurred_on"].is_string());

    // The hand-off is the relay, so no event delegation for this payment: two
    // mechanisms for one fact is how a double booking gets written.
    assert!(
        h.events.payloads("payment.received").is_empty(),
        "an enqueued intent replaces the event path"
    );
    let confirmed = h.events.payloads("stripe.payment.confirmed");
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0]["ledger_status"], LEDGER_INTENT_ENQUEUED);
    assert_eq!(confirmed[0]["ledger_intent_id"], 77);

    // And the response states which mechanism carried it, and its intent.
    assert_eq!(body["ledger"]["path"], "outbox");
    assert_eq!(body["ledger"]["intent_id"], 77);
    assert_eq!(
        body["ledger"]["principal"],
        adjutant_stripe::LEDGER_PRINCIPAL
    );
    assert_eq!(body["ledger"]["synchronous"], false);
    assert_audited(&h.db, "stripe.payment.recorded");
}

#[tokio::test]
async fn a_redelivered_webhook_keeps_one_intent() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // A redelivery: the receipt is already there (counted, not inserted), the
    // funds read answers again, and the payment insert conflicts — no row.
    h.db.push_rows(vec![]);
    h.http.push_json(
        200,
        &serde_json::json!({
            "funds": [{ "id": 3, "code": "general", "name": "General Fund", "active": true }]
        }),
    );
    h.db.push_rows(vec![]);
    let mut existing = payment_row(LEDGER_INTENT_ENQUEUED);
    existing["ledger_intent_id"] = serde_json::json!(77);
    existing["ledger_mechanism"] = serde_json::json!(MECHANISM_OUTBOX);
    h.db.push_rows(vec![existing]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate"], true);
    assert_eq!(body["redelivered"], true);

    // One payment, one intent. The statement that ran carries the SAME key the
    // first delivery used — the payment's own id — so `core.outbox_enqueue`
    // returned the intent it already had instead of creating a second.
    let inserts: Vec<String> = h
        .db
        .queried_sql()
        .into_iter()
        .filter(|sql| sql.contains("INSERT INTO \"stripe\".\"payments\""))
        .collect();
    assert_eq!(inserts.len(), 1);
    assert!(
        inserts[0].contains("core.outbox_enqueue($15, 'POST', '/api/finance/transaction', $16::jsonb, $1)"),
        "the key is the payment's own identifier, on every delivery: {}",
        inserts[0]
    );
    assert_eq!(
        body["payment"]["ledger_intent_id"], 77,
        "the payment names the same intent it already had"
    );
    assert_eq!(body["ledger"]["intent_id"], 77);

    // Nothing was told twice: no event, no outcome write.
    assert!(h.events.payloads("payment.received").is_empty());
    assert!(
        !h.db
            .queried_sql()
            .iter()
            .any(|sql| sql.contains("ledger_status = $2")),
        "a duplicate settles nothing: {:?}",
        h.db.queried_sql()
    );
}

#[tokio::test]
async fn without_a_read_credential_the_payment_is_recorded_unbooked_and_delegated() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    h.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);
    // finance refuses the funds read, as its own gate does for a caller with no
    // credential — which is every real webhook delivery.
    h.http.push_json(403, &serde_json::json!({ "error": "authentication required" }));
    h.db.push_rows(vec![payment_row(LEDGER_UNBOOKED)]);
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // No intent was composed, so none was enqueued — an incomplete payload would
    // be refused by finance at delivery, which is worse than saying so. The
    // payment is still recorded, truthfully `unbooked`.
    let inserts: Vec<String> = h
        .db
        .queried_sql()
        .into_iter()
        .filter(|sql| sql.contains("INSERT INTO \"stripe\".\"payments\""))
        .collect();
    assert_eq!(inserts.len(), 1);
    assert!(
        !inserts[0].contains("core.outbox_enqueue"),
        "no intent, so no enqueue expression: {}",
        inserts[0]
    );
    assert!(inserts[0].contains("NULL)"), "ledger_intent_id is null: {}", inserts[0]);
    let refusal = body["ledger"]["intent_refused"].as_str().unwrap_or_default();
    assert!(
        refusal.contains("finance refused the funds read") && refusal.contains("HTTP 403"),
        "the reason is stated, and it is finance's own: {refusal}"
    );

    // The fallback carried it, exactly as before.
    assert_eq!(body["ledger"]["path"], "event");
    assert_eq!(body["payment"]["ledger_status"], LEDGER_DELEGATED_EVENT);
    assert_eq!(h.events.payloads("payment.received").len(), 1);
}

#[tokio::test]
async fn the_worklist_carries_an_in_flight_payments_intent() {
    let (host, routes) = plugin_routes(config()).await;
    let unbooked = route(&routes, "GET", "/api/stripe/unbooked");

    let mut in_flight = payment_row(LEDGER_INTENT_ENQUEUED);
    in_flight["ledger_intent_id"] = serde_json::json!(77);
    in_flight["ledger_mechanism"] = serde_json::json!(MECHANISM_OUTBOX);
    in_flight["intent_state"] = serde_json::json!("attempting");
    in_flight["intent_attempts"] = serde_json::json!(1);
    in_flight["intent_max_attempts"] = serde_json::json!(6);
    in_flight["intent_answer_status"] = serde_json::Value::Null;
    in_flight["intent_last_error"] = serde_json::json!("the ledger is down");
    in_flight["intent_delivered_at"] = serde_json::Value::Null;
    in_flight["total_unbooked"] = serde_json::json!(2);
    // The pre-existing case: recorded before the outbox existed, or with a fund
    // that could not be resolved — no intent, and the worklist's remaining job.
    let mut legacy = payment_row(LEDGER_UNBOOKED);
    legacy["id"] = serde_json::json!(6);
    legacy["payment_id"] = serde_json::json!("pi_test_2");
    legacy["total_unbooked"] = serde_json::json!(2);
    host.db.push_rows(vec![in_flight, legacy]);

    let (status, body) = call(
        &unbooked.handler,
        TestRequest::get("/api/stripe/unbooked").identity("bea", &["treasurer"]).build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The in-flight payment is neither booked nor unbooked, and it is listed
    // explicitly — with its intent id and the intent's own state — rather than
    // counted as either.
    assert_eq!(body["in_flight"], 1);
    assert_eq!(body["by_status"][LEDGER_INTENT_ENQUEUED], 1);
    assert_eq!(body["by_status"][LEDGER_UNBOOKED], 1);
    assert_eq!(body["payments"][0]["ledger_intent_id"], 77);
    assert_eq!(body["payments"][0]["intent_state"], "attempting");
    assert_eq!(body["payments"][0]["intent_last_error"], "the ledger is down");
    // The payment with no intent stays the worklist's job.
    assert!(body["payments"][1]["ledger_intent_id"].is_null());
    assert_eq!(body["payments"][1]["intent_state"], serde_json::Value::Null);

    // The query states the in-flight case in SQL, not prose: the payment's intent
    // is joined from the producer's own view, and an intent that landed is not a
    // worklist item.
    let sql = host.db.queried_sql();
    let worklist = sql
        .iter()
        .find(|sql| sql.contains("total_unbooked"))
        .expect("the worklist query");
    assert!(
        worklist.contains("LEFT JOIN core.outbox_producer_view() v ON v.id = p.ledger_intent_id"),
        "{worklist}"
    );
    assert!(
        worklist.contains("v.state IS DISTINCT FROM 'delivered'"),
        "a delivered intent leaves the list: {worklist}"
    );
}

// ---------------------------------------------------------------------------
// The webhook's refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unsigned_or_badly_signed_body_is_refused_before_any_query() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // No header at all.
    let (status, body) = call(
        &webhook.handler,
        TestRequest::post("/api/stripe/webhook")
            .json(&session_completed_payload())
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        host.db.query_count(),
        0,
        "a refused delivery touches no table — the receipt is not even written"
    );
    host.events.assert_none();

    // A header over a body that was changed afterwards.
    let signed = signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now());
    let mut tampered = session_completed_payload();
    tampered["data"]["object"]["amount_total"] = serde_json::json!(250_000);
    let mut req = TestRequest::post("/api/stripe/webhook").json(&tampered).build();
    req.headers = signed.headers.clone();
    let (status, body) = call(&webhook.handler, req).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(host.db.query_count(), 0);
    host.events.assert_none();

    // The refusal itself is audited — a refused attempt is the most
    // security-relevant row in the log.
    assert_audited(&host.db, "stripe.webhook.refused");
}

#[tokio::test]
async fn a_webhook_without_a_signing_secret_refuses_every_delivery() {
    let (host, routes) = plugin_routes(config_without_secrets()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    let req = signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now());
    let (status, body) = call(&webhook.handler, req).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(host.db.query_count(), 0);
    host.events.assert_none();
}

#[tokio::test]
async fn a_redelivered_webhook_is_not_a_second_payment() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // The receipt already exists → no row; the payment already exists → no row;
    // and the stored payment is already delegated to finance.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate"], true);
    assert_eq!(body["redelivered"], true);
    assert_eq!(body["payment"]["payment_id"], "pi_test_1");
    host.events
        .assert_none(); // a redelivery publishes nothing at all
    host.db
        .assert_executed(&["redeliveries = redeliveries + 1"]);
}

#[tokio::test]
async fn a_redelivery_whose_publish_never_landed_is_self_healing() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");

    // The payment row exists but was never handed to finance: the previous
    // delivery died between the insert and the publish. It is not a duplicate.
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![payment_row("unbooked")]);
    host.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["duplicate"], false,
        "a payment whose ledger hand-off never happened still has work to do"
    );
    host.events.assert_published("payment.received");
    let outcome = host.db.last_query_params("ledger_status = $2").unwrap();
    assert!(
        has_text(&outcome, LEDGER_DELEGATED_EVENT),
        "{:?}",
        canonical(&outcome)
    );
}

#[tokio::test]
async fn a_payment_event_carrying_no_readable_amount_is_recorded_and_audited() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);

    let mut payload = session_completed_payload();
    payload["data"]["object"]["amount_total"] = serde_json::json!(0);
    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&payload, WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "asking Stripe to retry would not make it readable");
    assert_eq!(body["payment"], serde_json::Value::Null);
    assert!(body["unusable"].as_str().unwrap().contains("0 cents"));
    // No bogus payment row is invented, and the event is announced.
    assert_eq!(host.db.query_count(), 1);
    host.events.assert_published("stripe.webhook.unusable");
    assert_audited(&host.db, "stripe.webhook.unusable");
}

#[tokio::test]
async fn a_session_that_was_not_paid_is_not_a_payment() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);

    let mut payload = session_completed_payload();
    payload["data"]["object"]["payment_status"] = serde_json::json!("unpaid");
    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&payload, WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["payment"], serde_json::Value::Null);
    assert_eq!(host.db.query_count(), 1, "only the receipt was written");
    host.events.assert_none();
}

#[tokio::test]
async fn an_event_with_no_id_is_refused_because_it_cannot_be_idempotent() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    let mut payload = session_completed_payload();
    payload.as_object_mut().unwrap().remove("id");

    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&payload, WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn an_unknown_event_type_is_accepted_and_recorded_without_a_payment() {
    let (host, routes) = plugin_routes(config()).await;
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    host.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);

    let mut payload = session_completed_payload();
    payload["type"] = serde_json::json!("invoice.paid");
    let (status, body) = call(
        &webhook.handler,
        signed_webhook(&payload, WEBHOOK_SECRET, now()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["event_type"], "invoice.paid");
    assert_eq!(body["payment"], serde_json::Value::Null);
    assert_audited(&host.db, "stripe.webhook.ignored");
    host.events.assert_none();
}

// ---------------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_checkout_records_the_attempt_then_settles_it_from_stripe() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    h.db.push_rows(vec![session_row()]);
    h.http.push_json(
        200,
        &serde_json::json!({ "id": "cs_test_1", "url": "https://checkout.stripe.test/cs_test_1" }),
    );
    let mut settled = session_row();
    settled["status"] = serde_json::json!("created");
    settled["stripe_session_id"] = serde_json::json!("cs_test_1");
    settled["checkout_url"] = serde_json::json!("https://checkout.stripe.test/cs_test_1");
    h.db.push_rows(vec![settled]);

    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({
                "purpose": "dues",
                "amount_cents": 2500,
                "member_id": "42",
                "dues_year": 2026,
            }))
            .identity("42", &["scout"])
            .build(),
    )
    .await;

    assert_eq!(status, 201, "{body}");
    assert_eq!(body["checkout_url"], "https://checkout.stripe.test/cs_test_1");
    assert_eq!(body["session"]["status"], "created");

    // The row existed before Stripe was called, so the reference Stripe echoes
    // back already names something.
    let insert = h
        .db
        .last_query_params("INSERT INTO \"stripe\".\"checkout_sessions\"")
        .expect("the pending row was written first");
    assert!(has_int(&insert, 2500), "{:?}", canonical(&insert));
    assert!(
        has_text(&insert, "bea") || has_text(&insert, "42"),
        "the opener is recorded: {:?}",
        canonical(&insert)
    );

    // What went to Stripe: the configured api_base, the key, the amount.
    let calls = h.http.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].url, "https://api.stripe.test/v1/checkout/sessions");
    assert_eq!(
        calls[0].header("authorization"),
        Some(format!("Bearer {SECRET_KEY}").as_str())
    );
    let sent = calls[0].body.clone().unwrap();
    assert!(sent.contains("line_items[0][price_data][unit_amount]=2500"), "{sent}");
    assert!(sent.contains("client_reference_id=stripe-session-7"), "{sent}");
    assert!(
        !sent.contains(WEBHOOK_SECRET),
        "the webhook signing secret has no business on an API call"
    );
    h.events.assert_published("stripe.checkout.created");
    assert_audited(&h.db, "stripe.checkout.create");
}

#[tokio::test]
async fn a_checkout_without_a_key_refuses_before_any_http_call() {
    let h = Harness::new(config_without_secrets());
    let routes = h.routes().await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "donation", "amount_cents": 1000 }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(h.http.calls().len(), 0, "nothing was sent");
    assert_eq!(h.db.query_count(), 0, "and nothing was written");
}

#[tokio::test]
async fn a_refused_checkout_is_recorded_as_a_failed_attempt_not_erased() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    h.db.push_rows(vec![session_row()]);
    h.http.push_json(
        401,
        &serde_json::json!({ "error": { "message": "Invalid API Key provided" } }),
    );

    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "dues", "amount_cents": 2500 }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    assert_eq!(status, 401, "finance's own status is passed through: {body}");
    assert!(body["error"].as_str().unwrap().contains("Invalid API Key provided"));
    // The row stays, marked failed, by its own id — an attempt that charged
    // nobody is still a fact.
    let params = h
        .db
        .last_execute_params("checkout_sessions")
        .expect("the failed attempt was written back");
    assert_eq!(int_of(&params[0]), Some(7));
    assert!(has_text(&params, SESSION_FAILED), "{:?}", canonical(&params));
    assert_audited(&h.db, "stripe.checkout.failed");
}

#[tokio::test]
async fn a_donation_needs_no_member_and_defaults_to_the_donation_fund() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    h.db.push_rows(vec![session_row()]);
    h.http.push_json(
        200,
        &serde_json::json!({ "id": "cs_x", "url": "https://checkout.stripe.test/cs_x" }),
    );
    h.db.push_rows(vec![session_row()]);

    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "donation", "amount": "120.00" }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let sent = h.http.calls()[0].body.clone().unwrap();
    assert!(sent.contains("metadata[purpose]=donation"), "{sent}");
    assert!(sent.contains("metadata[category]=donation"), "{sent}");
    assert!(sent.contains("line_items[0][price_data][unit_amount]=12000"), "{sent}");
    assert!(
        sent.contains("product_data][name]=Donation%20to%20the%20troop"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_checkout_for_somebody_else_needs_stripe_manage() {
    let (host, routes) = plugin_routes(config()).await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    // The caller holds stripe:checkout (the route gate's business) but not
    // stripe:manage, and the mock answers the permission query with no rows.
    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({
                "purpose": "dues", "amount_cents": 2500, "member_id": "99"
            }))
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("stripe:manage"));
    assert_eq!(host.db.query_count(), 1, "the permission check, and nothing else");
    assert_eq!(
        host.db.executed_sql().len(),
        0,
        "no session row was written for a caller who may not open one"
    );
}

#[tokio::test]
async fn a_checkout_with_neither_url_configured_nor_supplied_is_refused() {
    let h = Harness::new(serde_json::json!({
        "secret_key": SECRET_KEY,
        "webhook_secret": WEBHOOK_SECRET,
    }));
    let routes = h.routes().await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    let (status, body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "dues", "amount_cents": 2500 }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("success_url"));
    assert_eq!(h.db.query_count(), 0);
}

#[tokio::test]
async fn a_float_amount_is_refused_by_serde_before_any_handler_logic() {
    let (_host, routes) = plugin_routes(config()).await;
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    let (status, _body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "dues", "amount_cents": 2500.5 }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "no amount in this plugin is ever an f64");
}

// ---------------------------------------------------------------------------
// Reading sessions and payments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_caller_without_read_all_is_narrowed_to_their_own_sessions() {
    let (host, routes) = plugin_routes(config()).await;
    let list = route(&routes, "GET", "/api/stripe/sessions");

    // The permission check is a query too: empty rows answer it with "no".
    host.db.push_rows(vec![]);
    host.db.push_rows(vec![session_row()]);
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/stripe/sessions").identity("bea", &["scout"]).build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["narrowed_to_caller"], true);
    let narrowed = host.db.last_query_params("ORDER BY s.id DESC").unwrap();
    assert_eq!(
        text_of(&narrowed[4]).as_deref(),
        Some("bea"),
        "the caller's own id is the narrowing parameter"
    );
    // A troop-wide reader gets a TEXT null there instead: no narrowing.
    let troop = route(&routes, "GET", "/api/stripe/sessions");
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]);
    host.db.push_rows(vec![]);
    let (_status, body) = call(
        &troop.handler,
        TestRequest::get("/api/stripe/sessions").identity("bea", &["treasurer"]).build(),
    )
    .await;
    assert_eq!(body["narrowed_to_caller"], false);
    let unnarrowed = host.db.last_query_params("ORDER BY s.id DESC").unwrap();
    assert!(
        matches!(unnarrowed[4], SqlValue::Null),
        "a troop-wide reader is narrowed by nothing: {:?}",
        canonical(&unnarrowed)
    );
}

#[tokio::test]
async fn somebody_elses_session_is_a_403_that_does_not_leak_it_exists() {
    let (host, routes) = plugin_routes(config()).await;
    let one = route(&routes, "GET", "/api/stripe/session/{id}");
    host.db.push_rows(vec![session_row()]);
    let (status, body) = call(
        &one.handler,
        TestRequest::get("/api/stripe/session/7")
            .param("id", "7")
            .identity("mallory", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(body["error"], "no such checkout session");
}

#[tokio::test]
async fn an_owner_sees_their_session_with_its_ledger_state() {
    let (host, routes) = plugin_routes(config()).await;
    let one = route(&routes, "GET", "/api/stripe/session/{id}");
    host.db.push_rows(vec![settled_session_row()]);
    host.db.push_rows(vec![serde_json::json!({ "n": 1 })]);
    host.db.push_rows(vec![payment_row(LEDGER_BOOKED)]);

    let (status, body) = call(
        &one.handler,
        TestRequest::get("/api/stripe/session/7")
            .param("id", "7")
            .identity("bea", &["scout"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ledger"]["booked"], true);
    assert_eq!(body["ledger"]["status"], LEDGER_BOOKED);
    assert_eq!(body["payment"]["payment_id"], "pi_test_1");
}

#[tokio::test]
async fn a_bad_ledger_status_filter_is_named_rather_than_ignored() {
    let (host, routes) = plugin_routes(config()).await;
    let list = route(&routes, "GET", "/api/stripe/payments");
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/stripe/payments")
            .query_param("ledger_status", "squashed")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(host.db.query_count(), 0);
}

#[tokio::test]
async fn the_unbooked_worklist_separates_a_delegation_from_a_refusal() {
    let (host, routes) = plugin_routes(config()).await;
    let unbooked = route(&routes, "GET", "/api/stripe/unbooked");
    let mut delegated = payment_row(LEDGER_DELEGATED_EVENT);
    let mut refused = payment_row(LEDGER_REFUSED);
    refused["id"] = serde_json::json!(6);
    refused["payment_id"] = serde_json::json!("pi_test_2");
    delegated["total_unbooked"] = serde_json::json!(11);
    refused["total_unbooked"] = serde_json::json!(11);
    host.db.push_rows(vec![delegated, refused]);

    let (status, body) = call(
        &unbooked.handler,
        TestRequest::get("/api/stripe/unbooked").identity("bea", &["treasurer"]).build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 2);
    assert_eq!(
        body["total_unbooked"], 11,
        "the window count answers 'how many', not 'how many on this page'"
    );
    assert_eq!(body["by_status"][LEDGER_DELEGATED_EVENT], 1);
    assert_eq!(body["by_status"][LEDGER_REFUSED], 1);
    assert_eq!(
        body["in_flight"], 0,
        "neither of these payments has an intent, so neither is in flight"
    );
    assert!(body["note"]
        .as_str()
        .unwrap()
        .contains("'unbooked' with no intent is the real job"));
    // The query itself states the in-flight case, in SQL rather than prose: the
    // payment's intent is joined in, a delivered one is gone from the list, and
    // the intent's own state rides with the row.
    let sql = host.db.queried_sql();
    let worklist = sql
        .iter()
        .find(|s| s.contains("total_unbooked"))
        .expect("the worklist query");
    assert!(
        worklist.contains("core.outbox_producer_view() v ON v.id = p.ledger_intent_id"),
        "the worklist reads the payment's intent: {worklist}"
    );
    assert!(
        worklist.contains("v.state IS DISTINCT FROM 'delivered'"),
        "an intent that landed is not a worklist item: {worklist}"
    );
    assert!(worklist.contains("intent_state"));
}

// ---------------------------------------------------------------------------
// Booking the ledger, as the caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn booking_forwards_the_callers_credential_and_never_mints_one() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);
    h.http.push_json(
        200,
        &serde_json::json!({
            "funds": [
                { "id": 3, "code": "general", "name": "General Fund", "active": true },
                { "id": 9, "code": "scholarship", "name": "Scholarship Fund", "active": true },
            ]
        }),
    );
    h.http.push_json(
        201,
        &serde_json::json!({
            "transaction": { "id": 41, "fund_id": 3, "amount_cents": 2500 },
            "balance_cents": 2500,
        }),
    );
    let mut booked = payment_row(LEDGER_BOOKED);
    booked["ledger_transaction_id"] = serde_json::json!("41");
    h.db.push_rows(vec![booked]);

    let (status, body) = call(
        &book.handler,
        TestRequest::post("/api/stripe/payment/5/book")
            .param("id", "5")
            .header("cookie", "adjutant_session=the-treasurers-own-session")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ledger"]["status"], LEDGER_BOOKED);
    assert_eq!(body["ledger"]["booked"], true);
    assert_eq!(body["finance"]["transaction_id"], "41");

    // Two calls, both to finance, both carrying the caller's own cookie — and
    // no credential this plugin made up.
    let calls = h.http.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].url,
        format!("{BASE_URL}{FINANCE_FUNDS_PATH}?include_inactive=1")
    );
    assert_eq!(calls[1].url, format!("{BASE_URL}{FINANCE_TRANSACTION_PATH}"));
    for call in &calls {
        assert_eq!(
            call.header("cookie"),
            Some("adjutant_session=the-treasurers-own-session")
        );
        assert_eq!(call.header("authorization"), None);
        assert!(
            !call.headers.iter().any(|(_, v)| v.contains(SECRET_KEY)),
            "no Stripe credential is ever put on a cross-plugin call: {:?}",
            call.headers
        );
    }
    // The entry finance is asked for: an income in the fund **id** it just told
    // us, keyed on Stripe's payment id so a double-booking is impossible.
    let sent: serde_json::Value = serde_json::from_str(calls[1].body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["fund_id"], 3);
    assert_eq!(sent["kind"], "income");
    assert_eq!(sent["amount_cents"], 2500);
    assert_eq!(sent["category"], CATEGORY_DUES);
    assert_eq!(sent["member_id"], "42");
    assert_eq!(sent["external_ref"], "pi_test_1");
    assert_eq!(sent["occurred_on"], "2026-09-25");

    let booked_params = h.db.last_query_params("ledger_status = $2").unwrap();
    assert_eq!(text_of(&booked_params[1]).as_deref(), Some(LEDGER_BOOKED));
    assert_eq!(
        text_of(&booked_params[2]).as_deref(),
        Some(MECHANISM_CALLER_FORWARD)
    );
    h.events.assert_published("stripe.payment.booked");
    assert_audited(&h.db, "stripe.payment.book");
}

#[tokio::test]
async fn booking_without_a_credential_is_refused_before_anything_is_called() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);

    let (status, body) = call(
        &book.handler,
        TestRequest::post("/api/stripe/payment/5/book")
            .param("id", "5")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;

    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no credential to forward"));
    assert_eq!(h.http.calls().len(), 0);
    assert_eq!(
        h.db.query_count(),
        1,
        "the payment was read; nothing was written and nothing was called"
    );
}

#[tokio::test]
async fn finances_refusal_is_reported_in_its_own_words() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);
    h.http.push_json(
        200,
        &serde_json::json!({ "funds": [{ "id": 3, "code": "general", "active": true }] }),
    );
    h.http.push_json(
        403,
        &serde_json::json!({ "error": "requires finance:write at scope troop" }),
    );
    h.db.push_rows(vec![payment_row(LEDGER_REFUSED)]);

    let (status, body) = call(
        &book.handler,
        TestRequest::post("/api/stripe/payment/5/book")
            .param("id", "5")
            .header("cookie", "adjutant_session=x")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;

    assert_eq!(status, 403, "finance's own answer reaches the caller: {body}");
    assert_eq!(body["ledger"]["status"], LEDGER_REFUSED);
    assert_eq!(body["ledger"]["booked"], false);
    assert_eq!(
        body["finance"]["error"], "requires finance:write at scope troop",
        "the missing authority is named by finance, not paraphrased"
    );
    let refused_params = h.db.last_query_params("ledger_status = $2").unwrap();
    assert_eq!(text_of(&refused_params[1]).as_deref(), Some(LEDGER_REFUSED));
}

#[tokio::test]
async fn a_fund_finance_does_not_have_is_reported_without_booking_anything() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);
    h.http.push_json(
        200,
        &serde_json::json!({ "funds": [{ "id": 3, "code": "scholarship", "active": true }] }),
    );
    h.db.push_rows(vec![payment_row(LEDGER_REFUSED)]);

    let (status, body) = call(
        &book.handler,
        TestRequest::post("/api/stripe/payment/5/book")
            .param("id", "5")
            .header("cookie", "adjutant_session=x")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;

    assert_eq!(status, 400, "{body}");
    assert!(body["finance"]["error"]
        .as_str()
        .unwrap()
        .contains("no fund with code \"general\""));
    assert_eq!(
        h.http.calls().len(),
        1,
        "a fund finance does not have is not a reason to send a transaction"
    );
}

#[tokio::test]
async fn an_inactive_fund_is_refused_by_finances_own_fact() {
    let h = Harness::new(config());
    let routes = h.routes().await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    h.db.push_rows(vec![payment_row(LEDGER_DELEGATED_EVENT)]);
    h.http.push_json(
        200,
        &serde_json::json!({ "funds": [{ "id": 3, "code": "general", "active": false }] }),
    );
    h.db.push_rows(vec![payment_row(LEDGER_REFUSED)]);

    let (status, body) = call(
        &book.handler,
        TestRequest::post("/api/stripe/payment/5/book")
            .param("id", "5")
            .header("cookie", "adjutant_session=x")
            .identity("bea", &["treasurer"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["finance"]["error"].as_str().unwrap().contains("is inactive"));
    assert_eq!(h.http.calls().len(), 1);
}

#[tokio::test]
async fn booking_somebody_elses_payment_without_the_permission_is_refused() {
    // The core gate is outside the handler, so this asserts the *shape* the
    // loader enforces: /book is troop-protected by stripe:manage.
    let (_host, routes) = plugin_routes(config()).await;
    let book = route(&routes, "POST", "/api/stripe/payment/{id}/book");
    assert_eq!(book.required_permission.as_deref(), Some(PERM_MANAGE));
    assert_eq!(book.required_scope, Some(Scope::troop()));
    let unbooked = route(&routes, "GET", "/api/stripe/unbooked");
    assert_eq!(unbooked.required_permission.as_deref(), Some(PERM_READ_ALL));
    let one = route(&routes, "GET", "/api/stripe/session/{id}");
    assert_eq!(one.required_permission.as_deref(), Some(PERM_READ));
    assert_eq!(
        one.required_scope, None,
        "an object route lets the handler check the object"
    );
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    assert_eq!(webhook.required_permission, None);
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_sweep_is_silent_when_the_books_are_settled() {
    let (host, routes) = plugin_routes(config()).await;
    let _ = routes;
    let mut plugin = StripePlugin::new();
    plugin.init(host.context("stripe")).await.unwrap();
    let sweep = &plugin.schedules()[0].handler;
    host.db.push_rows(vec![]);
    sweep().await.unwrap();
    host.events
        .assert_none(); // a silent sweep is a healthy one
}

#[tokio::test]
async fn the_sweep_notices_an_unbooked_payment_without_writing_the_ledger() {
    let (host, routes) = plugin_routes(config()).await;
    let _ = routes;
    let mut plugin = StripePlugin::new();
    plugin.init(host.context("stripe")).await.unwrap();
    let sweep = &plugin.schedules()[0].handler;
    let mut row = payment_row(LEDGER_DELEGATED_EVENT);
    row["total_unbooked"] = serde_json::json!(1);
    host.db.push_rows(vec![row]);
    sweep().await.unwrap();

    host.events.assert_published("stripe.ledger.unbooked");
    let payload = &host.events.payloads("stripe.ledger.unbooked")[0];
    assert_eq!(payload["total_unbooked"], 1);
    assert_eq!(payload["older_than_minutes"], 30);
    assert!(payload["next"].as_str().unwrap().contains("book"));
    // It notices; it does not write. No ledger call, no credential, no write.
    assert_eq!(
        host.db.query_count(),
        1,
        "one SELECT: the sweep reads, and only reads"
    );
    assert_eq!(host.db.executed_sql().len(), 0);
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_secret_ever_reaches_a_response_an_event_or_an_audit_entry() {
    let h = Harness::new(config());
    let routes = h.routes().await;

    // health: presence, never a value
    let (status, health) = call(
        &route(&routes, "GET", "/api/stripe/health").handler,
        TestRequest::get("/api/stripe/health").identity("bea", &["chief"]).build(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(health["config"]["secret_key"], "configured");
    assert_eq!(health["config"]["webhook_secret"], "configured");
    assert_eq!(health["config"]["key_mode"], "test", "one bit, and it is useful");
    let health_text = health.to_string();

    // a checkout, which puts the key on the wire
    let checkout = route(&routes, "POST", "/api/stripe/checkout");
    h.db.push_rows(vec![session_row()]);
    h.http.push_json(
        200,
        &serde_json::json!({ "id": "cs_x", "url": "https://checkout.stripe.test/cs_x" }),
    );
    h.db.push_rows(vec![session_row()]);
    let (_status, checkout_body) = call(
        &checkout.handler,
        TestRequest::post("/api/stripe/checkout")
            .json(&serde_json::json!({ "purpose": "dues", "amount_cents": 2500 }))
            .identity("bea", &["chief"])
            .build(),
    )
    .await;

    // a webhook, which verifies with the signing secret
    let webhook = route(&routes, "POST", "/api/stripe/webhook");
    h.db.push_rows(vec![serde_json::json!({ "id": 1, "redeliveries": 0 })]);
    h.db.push_rows(vec![payment_row("unbooked")]);
    let (_status, webhook_body) = call(
        &webhook.handler,
        signed_webhook(&session_completed_payload(), WEBHOOK_SECRET, now()),
    )
    .await;

    // Nowhere in what this plugin said or sent.
    let said = format!("{health_text}{checkout_body}{webhook_body}");
    assert!(!said.contains(SECRET_KEY), "a response carried the API key");
    assert!(!said.contains(WEBHOOK_SECRET), "a response carried the signing secret");

    let every_event: String = h
        .events
        .published
        .lock()
        .unwrap()
        .iter()
        .map(|p| format!("{} {}", p.event_type, p.payload))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!every_event.contains(SECRET_KEY), "an event payload carried the API key");
    assert!(
        !every_event.contains(WEBHOOK_SECRET),
        "an event payload carried the signing secret"
    );

    let every_db_param = {
        let queried = h.db.queried.lock().unwrap().clone();
        let executed = h.db.executed.lock().unwrap().clone();
        queried
            .iter()
            .chain(executed.iter())
            .map(|call| format!("{} {:?}", call.sql, call.params))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !every_db_param.contains(WEBHOOK_SECRET),
        "an audit detail or a bind parameter carried the signing secret"
    );
    // The API key never reaches the database either — it is only ever a header
    // on an outbound request.
    assert!(!every_db_param.contains(SECRET_KEY));

    // The one place the key belongs: the Authorization header of the call to
    // Stripe. Nowhere else outbound.
    let outbound = h.http.outbound_text();
    assert!(outbound.contains(SECRET_KEY), "the key must reach Stripe");
    assert!(!outbound.contains(WEBHOOK_SECRET));
    assert_eq!(
        outbound.matches(SECRET_KEY).count(),
        1,
        "one Authorization header, once"
    );
}

#[tokio::test]
async fn health_names_the_blocker_it_cannot_fix() {
    let (_host, routes) = plugin_routes(config()).await;
    let (status, body) = call(
        &route(&routes, "GET", "/api/stripe/health").handler,
        TestRequest::get("/api/stripe/health").identity("bea", &["chief"]).build(),
    )
    .await;
    assert_eq!(status, 200);
    // The mechanism that carries a confirmation: an outbox intent the core's
    // relay delivers as the declared service principal — not an event, and not
    // synchronous.
    assert_eq!(body["ledger"]["path"], "outbox");
    assert_eq!(body["ledger"]["synchronous"], false);
    assert_eq!(body["ledger"]["principal"], adjutant_stripe::LEDGER_PRINCIPAL);
    assert_eq!(
        body["ledger"]["target_route"],
        format!("POST {FINANCE_TRANSACTION_PATH}")
    );
    assert!(body["ledger"]["intent"]
        .as_str()
        .unwrap()
        .contains("one statement"));
    assert!(body["ledger"]["why"].as_str().unwrap().contains("no Adjutant caller"));
    // The residual, named: finance takes only a fund id on write paths, and the
    // callerless path holds no read credential — issue #60.
    assert!(body["ledger"]["blocked_on"].as_str().unwrap().contains("#60"));
    assert!(body["ledger"]["fallback"]
        .as_str()
        .unwrap()
        .contains("payment.received"));
    assert_eq!(body["config"]["funds"][PURPOSE_DUES], "general");
    assert_eq!(body["config"]["funds"][PURPOSE_DONATION], "general");
}

// ---------------------------------------------------------------------------
// The credential-forwarding primitive
// ---------------------------------------------------------------------------

#[test]
fn only_the_caller_credentials_are_ever_forwarded() {
    let req = TestRequest::post("/api/stripe/payment/1/book")
        .header("cookie", "adjutant_session=abc")
        .header("authorization", "Bearer xyz")
        .header("x-dev-user", "bea")
        .header("x-dev-role", "chief")
        .header("mcp-session-id", "nope")
        .build();
    assert_eq!(
        forward_headers(&req),
        vec![
            ("authorization".to_string(), "Bearer xyz".to_string()),
            ("cookie".to_string(), "adjutant_session=abc".to_string()),
        ],
        "the dev-header stub is not a credential and is never forwarded"
    );

    let bare = TestRequest::post("/api/stripe/payment/1/book").build();
    assert!(forward_headers(&bare).is_empty());
}

#[test]
fn the_purpose_and_category_vocabulary_is_closed() {
    assert_eq!(PURPOSES.len(), 3);
    assert_eq!(category_for(PURPOSE_EVENT_FEE), "event_fee");
    assert_eq!(DEFAULT_BASE_URL, "http://127.0.0.1:8787");
}

#[test]
fn the_checkout_label_says_what_and_for_whom() {
    assert_eq!(
        adjutant_stripe::checkout_label(PURPOSE_DUES, Some(2026), "42"),
        "Troop dues 2026 (member 42)"
    );
    assert_eq!(
        adjutant_stripe::checkout_label(PURPOSE_EVENT_FEE, None, ""),
        "Event fee"
    );
    assert_eq!(
        adjutant_stripe::checkout_label(PURPOSE_DONATION, None, ""),
        "Donation to the troop"
    );
    assert!(!PURPOSES.contains(&"uniform"));
}

#[tokio::test]
async fn the_session_list_accepts_only_a_status_it_knows() {
    let (host, routes) = plugin_routes(config()).await;
    let list = route(&routes, "GET", "/api/stripe/sessions");
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/stripe/sessions")
            .query_param("status", "halfway")
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(host.db.query_count(), 0);
    // And a known one narrows rather than refuses.
    host.db.push_rows(vec![session_row()]);
    let (status, body) = call(
        &list.handler,
        TestRequest::get("/api/stripe/sessions")
            .query_param("status", SESSION_COMPLETED)
            .identity("bea", &["chief"])
            .build(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        host.db.query_count(),
        2,
        "the permission check and the page — one query each"
    );
}
