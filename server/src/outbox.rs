//! The transactional outbox, the relay that drains it, and the declared service
//! principals a delivery is authorised by (design:
//! `docs/design/plugin-to-plugin.md` §3.2).
//!
//! **Why this is core.** `stripe` and `store` both have to reach `finance` from a
//! confirmation that has **no caller** — a Stripe webhook is a server-to-server
//! POST, and a scholarship draw is applied by the shop rather than by the member.
//! §3.1 refuses the shortcut (a shared secret, a service token, a "trusted
//! internal call"), and §2(b)'s caller-forwarded call is unavailable because
//! there is no caller to forward. What is left is what §3.2 decided: **a
//! transactional outbox with an idempotent consumer, delivery authorised by a
//! declared service principal, reconciliation as the control of last resort.**
//!
//! **The four moving parts.**
//!
//! * **`core.outbox`** — the durable intent. A plugin holds **no grant on the
//!   table**; it enqueues through `core.outbox_enqueue()`, a `SECURITY DEFINER`
//!   function that derives the producer from `session_user` and refuses any
//!   principal that was not declared *for that producer*. The function has no
//!   identity parameter at all, so a plugin cannot assert one: the only principal
//!   it can name is its own. Written as a scalar in the same statement as the
//!   fact it describes, the intent and the fact commit together or neither does.
//! * **The relay** ([`Relay`], [`drain`]) — claims due intents with
//!   `FOR UPDATE SKIP LOCKED`, delivers each to the target plugin's route,
//!   records the target's own answer, and retries a retryable failure with
//!   exponential backoff. What it cannot fix it makes **visible in the data**: an
//!   intent that runs out of attempts lands in `exhausted` carrying its last
//!   error — never a log line and nothing else.
//! * **The service principal** ([`SERVICE_PRINCIPALS`]) — a first-class,
//!   non-human identity: a role in `core.roles` with its own **narrow** grant in
//!   `core.role_permissions`, declared in `core.service_principals` against
//!   exactly one producer, so an operator sees it beside a member's grant and can
//!   revoke it. The relay builds the caller's [`Identity`] from the **intent row**
//!   (never from the payload) and runs the same gate a member's request runs —
//!   [`crate::permissions::authorize`] — so the target still decides for itself
//!   and still refuses.
//! * **Reconciliation** ([`reconciliation`]) — the control of last resort: a pass
//!   that classifies every intent and reports the ones that did not land, with
//!   both sides of the disagreement in the row.
//!
//! **What stays refused, unchanged (§3.1).** The relay holds no token and
//! forwards no credential: a delivered request carries **no headers at all**, and
//! a payload cannot smuggle one in — the identity is a field of the intent row,
//! and the headers of the delivered request are empty by construction. Nothing
//! here can give a plugin an authority its declared principal does not hold, and
//! nothing lets a target skip its own authorization because the caller is
//! internal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlx::{PgPool, Row};
use tokio::task::JoinHandle;

use adjutant_sdk::{
    event_handler, EventSubscription, HostEvents, Identity, PluginRequest, RoleGrant, Scope,
};

use crate::permissions::authorize;
use crate::plugin_runtime::RouteLookup;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// The intent has not been delivered and is due (or waiting for its backoff).
pub const STATE_PENDING: &str = "pending";
/// Claimed by the relay right now. A lease, not a resting state: a row left here
/// by a crash is reclaimed by [`reclaim_stale`].
pub const STATE_ATTEMPTING: &str = "attempting";
/// The target answered 2xx. Terminal: one intent, one delivery.
pub const STATE_DELIVERED: &str = "delivered";
/// The target said no, or its gate refused the principal. Terminal — retrying
/// would change nothing — and named in the data so it cannot be mistaken for a
/// delivery.
pub const STATE_REFUSED: &str = "refused";
/// Every attempt was spent and the intent still did not land. **The money path's
/// visible failure**: the provider's fact is real and the ledger entry is not
/// there.
pub const STATE_EXHAUSTED: &str = "exhausted";

/// Every state, constrained in the database.
pub const STATES: [&str; 5] = [
    STATE_PENDING,
    STATE_ATTEMPTING,
    STATE_DELIVERED,
    STATE_REFUSED,
    STATE_EXHAUSTED,
];

/// Published when an intent lands. A **notifications** channel for the producer
/// (it settles its own `ledger_status` from it); never the mechanism by which the
/// ledger learns, which is the intent itself.
pub const EVENT_DELIVERED: &str = "core.outbox.delivered";
/// Published when the target refused, or its gate refused the principal.
pub const EVENT_REFUSED: &str = "core.outbox.refused";
/// Published when the attempts ran out. The producer's "this will not fix itself"
/// signal, and the operator's.
pub const EVENT_EXHAUSTED: &str = "core.outbox.exhausted";
/// Published by the reconciliation pass when the mismatch set changes and is not
/// empty. Silent while the money path is healthy.
pub const EVENT_MISMATCH: &str = "core.outbox.mismatch";

/// Attempts before an intent is declared exhausted (mirrored by the column
/// default, so an operator can raise it per intent).
pub const DEFAULT_MAX_ATTEMPTS: i32 = 6;
/// Backoff base: attempt 1 waits this long, each further attempt doubles it.
const BACKOFF_BASE_SECS: i64 = 15;
/// Backoff ceiling — an intent is not retried more than hourly.
const BACKOFF_MAX_SECS: i64 = 3600;
/// How long a claimed intent may stay `attempting` before another pass reclaims
/// it (a crash mid-delivery). Never shorter than a delivery can take.
pub const LEASE_SECS: f64 = 120.0;
/// Intents drained per pass.
const BATCH: usize = 32;
/// Relay cadence. Per-intent backoff is what paces a retry; this is only how
/// often the queue is looked at.
const RELAY_INTERVAL_SECS: u64 = 5;
/// Reconciliation cadence, in relay passes.
const RECONCILE_EVERY: u64 = 12;
/// An intent still `pending` after this long did not get delivered — the relay is
/// not draining (not running, wedged, or saturated).
pub const STALE_PENDING_SECS: i64 = 15 * 60;
/// A row stuck in `attempting` longer than this was abandoned (see
/// [`LEASE_SECS`], which must be the shorter of the two).
pub const STALE_ATTEMPTING_SECS: i64 = 15 * 60;
/// The most rows a mismatch report lists.
const MISMATCH_LIMIT: i64 = 200;
/// The answer body is recorded, bounded: enough to see what the target said,
/// not enough to grow a table with a plugin's whole response.
const MAX_ANSWER_BYTES: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Declared service principals
// ---------------------------------------------------------------------------

/// One declared service principal: a first-class, non-human identity with its own
/// narrow grant, and **exactly one** producer allowed to enqueue an intent
/// delivered as it.
///
/// This is the trust anchor for the money path, so it is a core constant rather
/// than a plugin declaration: a plugin must not be able to widen its own machine
/// authority by declaring one (contrast a plugin's `permissions_granted()`, which
/// only says what a *human* grant may be written against).
#[derive(Debug, Clone, Copy)]
pub struct ServicePrincipalDecl {
    /// The role id, in `core.roles` and `core.role_permissions`.
    pub principal: &'static str,
    /// The only plugin whose intents may be delivered as this principal. Enforced
    /// by `core.outbox_enqueue` against `session_user`, not by convention.
    pub producer: &'static str,
    /// How an operator reading `core.roles` sees what this is.
    pub display_name: &'static str,
    /// What it is for, in one sentence, for `core.service_principals`.
    pub description: &'static str,
    /// Its grant. **Narrow on purpose**: the smallest set of permissions that
    /// performs the one operation the producer needs, and no read authority, no
    /// management authority, and never `core:admin`.
    pub grants: &'static [&'static str],
}

/// The service principals this core declares.
///
/// One entry per machine-originated operation that has no caller. Each is a
/// distinct identity with its own grant, so revoking one does not disturb
/// another, and an operator's revocation is never re-granted (see
/// [`seed_service_principals`]).
pub const SERVICE_PRINCIPALS: &[ServicePrincipalDecl] = &[
    ServicePrincipalDecl {
        principal: "svc.stripe.ledger",
        producer: "stripe",
        display_name: "Service principal — Stripe ledger booking",
        description: "Books one confirmed Stripe payment into finance's ledger via \
                      POST /api/finance/transaction. Declared for the stripe plugin only.",
        grants: &["finance:write"],
    },
    ServicePrincipalDecl {
        principal: "svc.store.draw",
        producer: "store",
        display_name: "Service principal - Store scholarship draw",
        description: "Books one store order's scholarship draw into finance's ledger via \
                      POST /api/finance/transfer. Declared for the store plugin only.",
        grants: &["finance:write"],
    },
];

/// The declaration for `principal` if (and only if) `producer` owns it.
///
/// The Rust mirror of the check `core.outbox_enqueue` performs in SQL. Used by
/// tests to state the boundary, and by the relay to decide whether an intent's
/// principal is still declared.
pub fn declared_for(producer: &str, principal: &str) -> Option<&'static ServicePrincipalDecl> {
    SERVICE_PRINCIPALS
        .iter()
        .find(|d| d.principal == principal && d.producer == producer)
}

/// Declare the service principals: the role, the declaring row, and the narrow
/// grant — each `ON CONFLICT DO NOTHING`.
///
/// **Nothing here is ever re-granted.** Revoking a principal is an operator's
/// act (`DELETE FROM core.role_permissions WHERE role_id = …`, or
/// `UPDATE core.service_principals SET revoked_at = now()`), and a boot that
/// put the grant back would make the operator's revocation a lie. Same
/// discipline as `core.scope_owners`.
///
/// A grant is written only for a permission that exists: the `finance:write`
/// permission is registered when the `finance` plugin loads, so a core that boots
/// without `finance` declares the principal and grants it nothing — and then the
/// target's gate refuses, which is the correct answer rather than a crashed
/// boot.
pub async fn seed_service_principals(pool: &PgPool) -> Result<(), sqlx::Error> {
    for d in SERVICE_PRINCIPALS {
        sqlx::query(
            "INSERT INTO core.roles (id, display_name, description) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(d.principal)
        .bind(d.display_name)
        .bind(d.description)
        .execute(pool)
        .await?;
        // `declared_by_core` is what keeps the two ends agreeing: the compiled
        // constant is the declaration and this row is its mirror, and
        // `core.outbox_enqueue` refuses a principal the core never declared, so an
        // intent that could never be delivered cannot be created in the first place.
        // On conflict the flag is re-asserted but the revocation is left alone — a
        // boot must never undo an operator's revocation.
        sqlx::query(
            "INSERT INTO core.service_principals \
                 (principal, producer_plugin, description, declared_by_core) \
             VALUES ($1, $2, $3, true) \
             ON CONFLICT (principal) DO UPDATE \
                SET declared_by_core = true, producer_plugin = EXCLUDED.producer_plugin",
        )
        .bind(d.principal)
        .bind(d.producer)
        .bind(d.description)
        .execute(pool)
        .await?;
        for permission in d.grants {
            let written = sqlx::query(
                "INSERT INTO core.role_permissions (role_id, permission_id) \
                 SELECT $1, id FROM core.permissions WHERE id = $2 \
                 ON CONFLICT DO NOTHING",
            )
            .bind(d.principal)
            .bind(permission)
            .execute(pool)
            .await?
            .rows_affected();
            if written == 0 {
                let already: Option<i64> = sqlx::query_scalar(
                    "SELECT 1::bigint FROM core.role_permissions \
                     WHERE role_id = $1 AND permission_id = $2",
                )
                .bind(d.principal)
                .bind(permission)
                .fetch_optional(pool)
                .await?;
                if already.is_none() {
                    tracing::warn!(
                        principal = d.principal,
                        permission = permission,
                        "service principal declared with no grant: the permission is not \
                         registered (is the plugin loaded?), so the target's gate will refuse \
                         every delivery as this principal"
                    );
                }
            }
        }
        tracing::info!(
            principal = d.principal,
            producer = d.producer,
            grants = ?d.grants,
            "service principal declared"
        );
    }
    Ok(())
}

/// A principal as the database holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PrincipalRow {
    pub producer_plugin: String,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The identity the relay delivers as.
///
/// A **troop-wide grant of the principal's own role**, and nothing else: the
/// target's gate then resolves it against `core.role_permissions` exactly as it
/// resolves a member's grant. `user_id` is namespaced (`service:<principal>`)
/// because it is not a person — it is written to `details.user_id` by the audit
/// service and to `recorded_by` on any ledger row the principal books, so it must
/// be unmistakable in the books.
pub fn identity_for(principal: &str) -> Identity {
    Identity::from_grants(
        format!("service:{principal}"),
        vec![RoleGrant {
            role_id: principal.to_string(),
            scope: Scope::troop(),
        }],
    )
}

// ---------------------------------------------------------------------------
// Seeding order note
// ---------------------------------------------------------------------------

// `core.service_principals.principal` references `core.roles(id)` and a grant
// references `core.permissions(id)`, both of which the core migration creates.
// The *permissions* a principal is granted come from the plugin that owns them
// (the finance plugin registers `finance:write` during load), so `build_app`
// calls `seed_service_principals` **after** `load_all`, beside the `chief`
// bootstrap grant.

// ---------------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------------

/// The relay task's handle. One relay per process.
pub struct Relay {
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Relay {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handle: Mutex::new(None),
        })
    }

    /// Start the drain loop. Holds a [`std::sync::Weak`] to the app state, so the
    /// task cannot keep the process's state alive (no reference cycle) and stops
    /// by itself if the state is dropped.
    pub fn start(&self, state: &Arc<AppState>) {
        let weak = Arc::downgrade(state);
        let handle = tokio::spawn(async move {
            let mut pass: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(RELAY_INTERVAL_SECS)).await;
                let Some(state) = weak.upgrade() else {
                    // The app state is gone; nothing left to serve.
                    break;
                };
                match drain(&state).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(delivered = n, "outbox relay pass"),
                    Err(e) => tracing::error!(error = %e, "outbox relay pass failed"),
                }
                pass += 1;
                if pass.is_multiple_of(RECONCILE_EVERY) {
                    if let Err(e) = reconcile_and_raise(&state).await {
                        tracing::error!(error = %e, "outbox reconciliation pass failed");
                    }
                }
            }
        });
        *self.handle.lock().expect("relay mutex poisoned") = Some(handle);
    }

    /// Stop the relay (graceful shutdown).
    pub fn stop(&self) {
        if let Some(h) = self.handle.lock().expect("relay mutex poisoned").take() {
            h.abort();
        }
    }
}

impl Default for Relay {
    fn default() -> Self {
        Self {
            handle: Mutex::new(None),
        }
    }
}

/// One claimed intent, as the relay needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Intent {
    pub id: i64,
    pub producer_plugin: String,
    pub principal: String,
    pub target_method: String,
    pub target_route: String,
    pub idempotency_key: String,
    pub payload: serde_json::Value,
    pub attempts: i32,
    pub max_attempts: i32,
}

/// What one delivery attempt concluded.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// The state the intent is set to.
    pub state: &'static str,
    /// The target's HTTP status, when the target was reached at all.
    pub http_status: Option<u16>,
    /// The target's answer body (bounded), recorded verbatim.
    pub answer: Option<serde_json::Value>,
    /// Why it did not land. Present whenever the state is not `delivered`.
    pub error: Option<String>,
    /// The event to publish, when the attempt reached a terminal state.
    pub event: Option<&'static str>,
}

impl Delivery {
    fn delivered(status: u16, answer: Option<serde_json::Value>) -> Self {
        Self {
            state: STATE_DELIVERED,
            http_status: Some(status),
            answer,
            error: None,
            event: Some(EVENT_DELIVERED),
        }
    }

    fn refused(status: Option<u16>, answer: Option<serde_json::Value>, error: String) -> Self {
        Self {
            state: STATE_REFUSED,
            http_status: status,
            answer,
            error: Some(error),
            event: Some(EVENT_REFUSED),
        }
    }

    /// A retryable failure. `attempts` is the count **after** the claim that
    /// consumed this attempt, so the last attempt is the one that exhausts.
    fn retry(attempts: i32, max_attempts: i32, status: Option<u16>, error: String) -> Self {
        if attempts >= max_attempts {
            Self {
                state: STATE_EXHAUSTED,
                http_status: status,
                answer: None,
                error: Some(error),
                event: Some(EVENT_EXHAUSTED),
            }
        } else {
            Self {
                state: STATE_PENDING,
                http_status: status,
                answer: None,
                error: Some(error),
                event: None,
            }
        }
    }
}

/// The backoff before attempt `n + 1`, doubling from `BACKOFF_BASE_SECS` (15s)
/// and capped at `BACKOFF_MAX_SECS` (1h).
pub fn backoff_secs(attempts: i32) -> i64 {
    let n = attempts.clamp(1, 32) - 1;
    BACKOFF_BASE_SECS
        .saturating_mul(1i64 << n.min(20))
        .min(BACKOFF_MAX_SECS)
}

/// Return to `pending` any row the relay claimed and then stopped holding — a
/// crash between claim and record. Without this an intent would sit in
/// `attempting` forever and the money path would stop with it.
pub async fn reclaim_stale(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE core.outbox SET state = 'pending', claimed_at = NULL, next_attempt_at = now(), \
             last_error = COALESCE(last_error, 'the delivery was abandoned: the relay stopped \
             while holding this intent') \
         WHERE state = 'attempting' AND claimed_at < now() - make_interval(secs => $1)",
    )
    .bind(LEASE_SECS)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Claim the oldest due intent, if any.
///
/// `FOR UPDATE SKIP LOCKED` so two passes (or two processes) never take the same
/// intent. `attempts` is incremented **at the claim**, so a crash mid-delivery
/// still spends an attempt and an intent cannot be retried forever.
pub async fn claim_next(pool: &PgPool) -> Result<Option<Intent>, sqlx::Error> {
    sqlx::query_as::<_, Intent>(
        "UPDATE core.outbox o \
            SET state = 'attempting', attempts = o.attempts + 1, claimed_at = now(), \
                last_attempt_at = now() \
          WHERE o.id = ( \
                SELECT i.id FROM core.outbox i \
                 WHERE i.state = 'pending' AND i.next_attempt_at <= now() \
                 ORDER BY i.id \
                 FOR UPDATE SKIP LOCKED \
                 LIMIT 1 ) \
      RETURNING o.id, o.producer_plugin, o.principal, o.target_method, o.target_route, \
                o.idempotency_key, o.payload, o.attempts, o.max_attempts",
    )
    .fetch_optional(pool)
    .await
}

/// Write down what an attempt concluded.
pub async fn record(pool: &PgPool, id: i64, attempts: i32, d: &Delivery) -> Result<(), sqlx::Error> {
    let next = if d.state == STATE_PENDING {
        Some(backoff_secs(attempts))
    } else {
        None
    };
    let answer = d.answer.as_ref().map(|a| serde_json::to_string(a).unwrap_or_default());
    sqlx::query(
        "UPDATE core.outbox SET state = $2, next_attempt_at = \
             CASE WHEN $3::bigint IS NULL THEN next_attempt_at \
                  ELSE now() + make_interval(secs => $3::bigint) END, \
             delivered_at = CASE WHEN $2 = 'delivered' THEN now() ELSE delivered_at END, \
             answer_status = $4, answer = $5::jsonb, last_error = $6 \
         WHERE id = $1",
    )
    .bind(id)
    .bind(d.state)
    .bind(next)
    .bind(d.http_status.map(i32::from))
    .bind(answer)
    .bind(d.error.clone())
    .execute(pool)
    .await?;
    Ok(())
}

/// Drain up to `BATCH` (32) due intents. Returns how many were attempted.
pub async fn drain(state: &Arc<AppState>) -> Result<usize, sqlx::Error> {
    reclaim_stale(state.pool.as_ref()).await?;
    let mut attempted = 0;
    while attempted < BATCH {
        let Some(intent) = claim_next(state.pool.as_ref()).await? else {
            break;
        };
        attempted += 1;
        let outcome = deliver(state, &intent).await;
        record(state.pool.as_ref(), intent.id, intent.attempts, &outcome).await?;
        tracing::info!(
            intent = intent.id,
            producer = %intent.producer_plugin,
            principal = %intent.principal,
            route = %intent.target_route,
            attempt = intent.attempts,
            state = outcome.state,
            status = ?outcome.http_status,
            "outbox delivery"
        );
        if let Some(event) = outcome.event {
            raise(state, event, &intent, &outcome).await;
        }
    }
    Ok(attempted)
}

/// Publish a terminal outcome. A failure to publish is logged, never fatal: the
/// notification is how a *producer* learns, and the durable record is the row the
/// producer can read through `core.outbox_producer_view()`.
async fn raise(state: &Arc<AppState>, event: &str, intent: &Intent, d: &Delivery) {
    let host = crate::host::CoreEvents::new(state.pool.clone(), state.bus.sender(), "core".into());
    let payload = serde_json::json!({
        "intent_id": intent.id,
        "producer": intent.producer_plugin,
        "principal": intent.principal,
        "target_route": intent.target_route,
        "idempotency_key": intent.idempotency_key,
        "state": d.state,
        "http_status": d.http_status,
        "error": d.error,
        "answer": d.answer,
    });
    if let Err(e) = host.publish(event.to_string(), payload).await {
        tracing::warn!(intent = intent.id, event, error = %e, "could not publish the intent outcome");
    }
}

/// Deliver one claimed intent.
///
/// The order is the security property: the **principal comes from the row**, the
/// route is resolved against the live registry, **the target's own gate runs**,
/// and only then is the handler called — with an identity the core built and no
/// headers at all.
pub async fn deliver(state: &Arc<AppState>, intent: &Intent) -> Delivery {
    let attempts = intent.attempts.max(1);
    let max_attempts = intent.max_attempts.max(1);

    // 1. The principal, read from the row. Never from the payload, and never
    //    from anything the producer supplied inside it.
    let principal = match load_principal(state.pool.as_ref(), &intent.principal).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Delivery::refused(
                Some(403),
                None,
                format!(
                    "service principal {} is not declared (or names producer {} rather than {}), \
                     so nothing is delivered: the target must not be reached by an identity the \
                     core cannot vouch for",
                    intent.principal, intent.producer_plugin, intent.producer_plugin
                ),
            )
        }
        Err(e) => {
            return Delivery::retry(
                attempts,
                max_attempts,
                None,
                format!("could not read the service principal: {e}"),
            )
        }
    };
    if let Some(revoked) = principal.revoked_at {
        return Delivery::refused(
            Some(403),
            None,
            format!(
                "service principal {} was revoked at {revoked}, so delivery as it is refused \
                 (re-declare it, or re-arm the intent, when the money path should run again)",
                intent.principal
            ),
        );
    }
    if crate::outbox::declared_for(&principal.producer_plugin, &intent.principal).is_none() {
        return Delivery::refused(
            Some(403),
            None,
            format!(
                "principal {} is not declared for producer {} in this core, so no delivery is \
                 attempted",
                intent.principal, intent.producer_plugin
            ),
        );
    }

    // 2. The route, from the live registry (a plugin reload must be visible here).
    let lookup = {
        let reg = state.registry.read().await;
        reg.find(&intent.target_method, &intent.target_route)
    };

    let (plugin_id, required, required_scope, handler, params) = match lookup {
        RouteLookup::Found {
            plugin_id,
            required_permission,
            required_scope,
            handler,
            params,
        } => (plugin_id, required_permission, required_scope, handler, params),
        // Retryable, not terminal: a plugin mid-reload has no routes for a
        // moment, and a *disabled* plugin has none at all (issue #89: it is not
        // loaded, so this is the same answer an absent library gives).
        // Exhaustion is the floor.
        RouteLookup::NotFound => {
            return Delivery::retry(
                attempts,
                max_attempts,
                Some(404),
                format!(
                    "no live route answers {} {} (the target plugin may not be loaded)",
                    intent.target_method, intent.target_route
                ),
            )
        }
    };

    // 3. The target's gate, exactly as a member's request runs it. The principal
    //    is evaluated through `core.role_permissions` like any other role, so a
    //    revoked grant is a 403 here and the operation is refused.
    let identity = identity_for(&intent.principal);
    if let Some(permission) = &required {
        if let Err(status) =
            authorize(Some(&identity), &state.permissions, permission, required_scope.as_ref()).await
        {
            return Delivery::refused(
                Some(status),
                None,
                format!(
                    "the gate of {plugin_id} refused {permission} for principal {}, so the \
                     operation was not performed (HTTP {status})",
                    intent.principal
                ),
            );
        }
    }

    // 4. The request the core builds. No headers: there is no credential to
    //    forward and no credential to mint, and a payload key cannot add one.
    let body = serde_json::to_vec(&intent.payload).unwrap_or_else(|_| b"{}".to_vec());
    let request = PluginRequest {
        method: intent.target_method.clone(),
        path: intent.target_route.clone(),
        params,
        // The core builds the request: no query string, and **no headers at all**.
        query: Vec::new(),
        headers: HashMap::new(),
        body,
        identity: Some(identity),
    };

    // 5. Call it.
    match handler(request).await {
        Ok(resp) => {
            let answer = bounded_answer(&resp.body);
            if (200..300).contains(&resp.status) {
                Delivery::delivered(resp.status, answer)
            } else if (400..500).contains(&resp.status) {
                // Terminal: the request was wrong or the target refused it, and
                // repeating it would say the same thing again.
                Delivery::refused(
                    Some(resp.status),
                    answer,
                    format!(
                        "{} answered {} to the intent: {}",
                        plugin_id,
                        resp.status,
                        resp.body.iter().map(|b| *b as char).collect::<String>()
                    ),
                )
            } else {
                Delivery::retry(
                    attempts,
                    max_attempts,
                    Some(resp.status),
                    format!("{plugin_id} answered {}", resp.status),
                )
            }
        }
        Err(e) => {
            let status = e.status();
            if (400..500).contains(&status) {
                Delivery::refused(Some(status), None, format!("{plugin_id} refused: {e}"))
            } else {
                Delivery::retry(attempts, max_attempts, Some(status), format!("{plugin_id}: {e}"))
            }
        }
    }
}

/// Keep what the target said, bounded.
fn bounded_answer(bytes: &[u8]) -> Option<serde_json::Value> {
    if bytes.is_empty() {
        return None;
    }
    let truncated = bytes.len() > MAX_ANSWER_BYTES;
    let slice = &bytes[..bytes.len().min(MAX_ANSWER_BYTES)];
    let value = serde_json::from_slice::<serde_json::Value>(slice).unwrap_or_else(|_| {
        serde_json::Value::String(String::from_utf8_lossy(slice).into_owned())
    });
    if truncated {
        Some(serde_json::json!({ "truncated": true, "body": value }))
    } else {
        Some(value)
    }
}

/// Read a declared principal.
pub async fn load_principal(
    pool: &PgPool,
    principal: &str,
) -> Result<Option<PrincipalRow>, sqlx::Error> {
    sqlx::query_as::<_, PrincipalRow>(
        "SELECT producer_plugin, revoked_at FROM core.service_principals WHERE principal = $1",
    )
    .bind(principal)
    .fetch_optional(pool)
    .await
}

// ---------------------------------------------------------------------------
// Reconciliation — the control of last resort
// ---------------------------------------------------------------------------

/// The reconciliation pass.
///
/// **What it compares.** Every intent the core holds — which is the core's own
/// record of a provider-confirmed, callerless fact, written atomically with it —
/// against the state and the answer the relay recorded for it. A disagreement is
/// a mismatch, and both sides are in the report: the intent's payload on one
/// side, the target's recorded answer on the other.
///
/// **What it reports.**
///
/// * `unlanded` — `exhausted`: the attempts ran out. The fact is real and the
///   ledger entry is not there. This is the case no relay can fix.
/// * `refused` — the target or its gate said no (a revoked grant, a revoked
///   principal, a payload the target rejects).
/// * `stalled` — `pending` past [`STALE_PENDING_SECS`], or `attempting` past
///   [`STALE_ATTEMPTING_SECS`]: the relay is not draining.
/// * `unanswered` — `delivered` with no recorded 2xx: the row contradicts itself.
///
/// **What it cannot see, stated so nobody assumes otherwise.** A
/// provider-confirmed fact whose producer recorded it *without* an intent — a row
/// written before this mechanism existed, or by a producer that has not adopted
/// it — is invisible here, because only the producer holds it. That is the
/// producer's own worklist (`GET /api/stripe/unbooked`,
/// `GET /api/store/orders/unsettled`), and it is the residual this pass does not
/// close.
pub async fn reconciliation(state: &Arc<AppState>) -> Result<serde_json::Value, sqlx::Error> {
    let pool = state.pool.as_ref();
    let totals = sqlx::query(
        "SELECT \
           COUNT(*) FILTER (WHERE state = 'pending')::bigint AS pending, \
           COUNT(*) FILTER (WHERE state = 'attempting')::bigint AS attempting, \
           COUNT(*) FILTER (WHERE state = 'delivered')::bigint AS delivered, \
           COUNT(*) FILTER (WHERE state = 'refused')::bigint AS refused, \
           COUNT(*) FILTER (WHERE state = 'exhausted')::bigint AS exhausted, \
           COUNT(*)::bigint AS intents \
         FROM core.outbox",
    )
    .fetch_one(pool)
    .await?;

    let mismatches = sqlx::query(
        "SELECT id, producer_plugin, principal, target_route, idempotency_key, state, attempts, \
                max_attempts, answer_status, last_error, created_at::text AS created_at, \
                payload, answer, \
                CASE \
                  WHEN state = 'exhausted' THEN 'unlanded' \
                  WHEN state = 'refused' THEN 'refused' \
                  WHEN state = 'delivered' \
                       AND (answer_status IS NULL OR answer_status >= 300 OR delivered_at IS NULL) \
                    THEN 'unanswered' \
                  ELSE 'stalled' \
                END AS mismatch \
           FROM core.outbox \
          WHERE state IN ('exhausted', 'refused') \
             OR (state = 'attempting' AND claimed_at < now() - make_interval(secs => $2)) \
             OR (state = 'pending' AND created_at < now() - make_interval(secs => $1)) \
             OR (state = 'delivered' \
                 AND (answer_status IS NULL OR answer_status >= 300 OR delivered_at IS NULL)) \
          ORDER BY id \
          LIMIT $3",
    )
    .bind(STALE_PENDING_SECS as f64)
    .bind(STALE_ATTEMPTING_SECS as f64)
    .bind(MISMATCH_LIMIT)
    .fetch_all(pool)
    .await?;

    let rows: Vec<serde_json::Value> = mismatches
        .iter()
        .map(|row| {
            use sqlx::Row;
            serde_json::json!({
                "intent_id": row.try_get::<i64, _>("id").ok(),
                "producer": row.try_get::<String, _>("producer_plugin").ok(),
                "principal": row.try_get::<String, _>("principal").ok(),
                "target_route": row.try_get::<String, _>("target_route").ok(),
                "idempotency_key": row.try_get::<String, _>("idempotency_key").ok(),
                "state": row.try_get::<String, _>("state").ok(),
                "mismatch": row.try_get::<String, _>("mismatch").ok(),
                "attempts": row.try_get::<i32, _>("attempts").ok(),
                "max_attempts": row.try_get::<i32, _>("max_attempts").ok(),
                "answer_status": row.try_get::<Option<i32>, _>("answer_status").ok().flatten(),
                "last_error": row.try_get::<Option<String>, _>("last_error").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").ok(),
                // Both sides of the disagreement, verbatim.
                "intent_payload": row.try_get::<serde_json::Value, _>("payload").ok(),
                "target_answer": row.try_get::<Option<serde_json::Value>, _>("answer").ok().flatten(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "ok": rows.is_empty(),
        "intents": totals.try_get::<i64, _>("intents").unwrap_or(0),
        "by_state": {
            "pending": totals.try_get::<i64, _>("pending").unwrap_or(0),
            "attempting": totals.try_get::<i64, _>("attempting").unwrap_or(0),
            "delivered": totals.try_get::<i64, _>("delivered").unwrap_or(0),
            "refused": totals.try_get::<i64, _>("refused").unwrap_or(0),
            "exhausted": totals.try_get::<i64, _>("exhausted").unwrap_or(0),
        },
        "mismatches": rows,
        "compares": "every intent the core holds (its own record of a provider-confirmed, \
                     callerless fact, written atomically with it) against the state and the \
                     target's recorded answer for it",
        "cannot_see": "a provider-confirmed fact its producer recorded without an intent — a row \
                       written before this mechanism existed, or by a producer that has not \
                       adopted it. Only the producer holds that: its own worklist is the \
                       residual (GET /api/stripe/unbooked, GET /api/store/orders/unsettled)",
        "surfaces": "GET /api/outbox/reconciliation, and a core.outbox.mismatch event published \
                     by the relay only when this set changes",
    }))
}

/// Run [`reconciliation`] and publish a `core.outbox.mismatch` event when the
/// mismatch set changed and is not empty. Silent while the money path is healthy
/// — a schedule that shouts every pass is one nobody reads.
async fn reconcile_and_raise(state: &Arc<AppState>) -> Result<(), sqlx::Error> {
    let report = reconciliation(state).await?;
    let mismatches = report["mismatches"].as_array().map(Vec::len).unwrap_or(0) as i64;
    let changed = {
        let mut last = state
            .outbox_mismatches
            .lock()
            .expect("outbox mismatch counter poisoned");
        let changed = *last != mismatches;
        *last = mismatches;
        changed
    };
    if mismatches > 0 && changed {
        let host =
            crate::host::CoreEvents::new(state.pool.clone(), state.bus.sender(), "core".into());
        if let Err(e) = host.publish(EVENT_MISMATCH.to_string(), report).await {
            tracing::warn!(error = %e, "could not publish the outbox mismatch report");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Core routes
// ---------------------------------------------------------------------------

/// `GET /api/outbox/intents?state=&producer=&limit=` — the queue, and the state
/// every intent is in.
///
/// This is the "visible in the data" half of the relay: an operator (or a
/// monitoring job) reads the states here rather than grepping a log for a
/// `warn!(…)` that nothing acts on. A producer reads *its own* intents through
/// `core.outbox_producer_view()` instead, so neither needs a grant on the table.
pub async fn list_intents(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let params = query_params(req.uri().query());
    let state_filter = params.get("state").cloned();
    if let Some(s) = &state_filter {
        if !STATES.contains(&s.as_str()) {
            return error(400, format!("state must be one of {}", STATES.join(", ")));
        }
    }
    let producer = params.get("producer").cloned();
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 500);

    let rows = sqlx::query(
        "SELECT id, producer_plugin, principal, target_method, target_route, idempotency_key, \
                state, attempts, max_attempts, answer_status, last_error, \
                created_at::text AS created_at, last_attempt_at::text AS last_attempt_at, \
                delivered_at::text AS delivered_at, next_attempt_at::text AS next_attempt_at, \
                payload, answer \
           FROM core.outbox \
          WHERE ($1::text IS NULL OR state = $1) \
            AND ($2::text IS NULL OR producer_plugin = $2) \
          ORDER BY id DESC LIMIT $3",
    )
    .bind(state_filter.as_deref())
    .bind(producer.as_deref())
    .bind(limit)
    .fetch_all(state.pool.as_ref())
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => return internal_error("outbox worklist", &e),
    };
    let counts = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE state = 'pending')::bigint AS pending, \
                COUNT(*) FILTER (WHERE state = 'attempting')::bigint AS attempting, \
                COUNT(*) FILTER (WHERE state = 'delivered')::bigint AS delivered, \
                COUNT(*) FILTER (WHERE state = 'refused')::bigint AS refused, \
                COUNT(*) FILTER (WHERE state = 'exhausted')::bigint AS exhausted \
           FROM core.outbox",
    )
    .fetch_one(state.pool.as_ref())
    .await;
    let counts = match counts {
        Ok(counts) => counts,
        Err(e) => return internal_error("outbox counts", &e),
    };

    let intents: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            use sqlx::Row;
            serde_json::json!({
                "id": row.try_get::<i64, _>("id").ok(),
                "producer": row.try_get::<String, _>("producer_plugin").ok(),
                "principal": row.try_get::<String, _>("principal").ok(),
                "method": row.try_get::<String, _>("target_method").ok(),
                "target_route": row.try_get::<String, _>("target_route").ok(),
                "idempotency_key": row.try_get::<String, _>("idempotency_key").ok(),
                "state": row.try_get::<String, _>("state").ok(),
                "attempts": row.try_get::<i32, _>("attempts").ok(),
                "max_attempts": row.try_get::<i32, _>("max_attempts").ok(),
                "answer_status": row.try_get::<Option<i32>, _>("answer_status").ok().flatten(),
                "last_error": row.try_get::<Option<String>, _>("last_error").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").ok(),
                "next_attempt_at": row.try_get::<String, _>("next_attempt_at").ok(),
                "delivered_at": row.try_get::<String, _>("delivered_at").ok(),
                "payload": row.try_get::<serde_json::Value, _>("payload").ok(),
                "answer": row.try_get::<Option<serde_json::Value>, _>("answer").ok().flatten(),
            })
        })
        .collect();

    axum::Json(serde_json::json!({
        "intents": intents,
        "count": intents.len(),
        "limit": limit,
        "filters": { "state": state_filter, "producer": producer },
        "by_state": {
            "pending": counts.try_get::<i64, _>("pending").unwrap_or(0),
            "attempting": counts.try_get::<i64, _>("attempting").unwrap_or(0),
            "delivered": counts.try_get::<i64, _>("delivered").unwrap_or(0),
            "refused": counts.try_get::<i64, _>("refused").unwrap_or(0),
            "exhausted": counts.try_get::<i64, _>("exhausted").unwrap_or(0),
        },
    }))
    .into_response()
}

/// `POST /api/outbox/intent/{id}/retry` — re-arm a `refused` or `exhausted`
/// intent.
///
/// The operator's hand: fix what was wrong (grant the permission, re-declare the
/// principal), then put the intent back in the queue with its attempt budget
/// restored. The consumer is idempotent on the idempotency key, so re-arming an
/// intent that did in fact land writes nothing a second time.
pub async fn retry_intent(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;
    let row: Option<(String, String)> =
        match sqlx::query_as("SELECT state, producer_plugin FROM core.outbox WHERE id = $1")
            .bind(id)
            .fetch_optional(state.pool.as_ref())
            .await
        {
            Ok(row) => row,
            Err(e) => return internal_error("outbox re-arm", &e),
        };
    let Some((current, producer)) = row else {
        return error(404, "no such intent");
    };
    if !matches!(current.as_str(), STATE_REFUSED | STATE_EXHAUSTED) {
        return error(
            409,
            format!(
                "intent {id} is {current}: only a refused or exhausted intent is re-armed (a \
                 pending one is already queued, and a delivered one is done)"
            ),
        );
    }
    // Audit, then apply (see `server::audit_state_change`): a state change is
    // never applied unless its audit row was written.
    if let Some(resp) = crate::server::audit_state_change(
        &state.audit,
        identity.as_ref(),
        "outbox.retry",
        "outbox_intent",
        &id.to_string(),
        serde_json::json!({ "producer": producer, "from_state": current }),
    )
    .await
    {
        return resp;
    }
    let updated = sqlx::query(
        "UPDATE core.outbox SET state = 'pending', attempts = 0, next_attempt_at = now(), \
             claimed_at = NULL \
         WHERE id = $1 AND state IN ('refused', 'exhausted')",
    )
    .bind(id)
    .execute(state.pool.as_ref())
    .await;
    match updated {
        Ok(res) if res.rows_affected() == 1 => axum::Json(serde_json::json!({
            "intent_id": id,
            "producer": producer,
            "state": STATE_PENDING,
            "from_state": current,
        }))
        .into_response(),
        Ok(_) => error(409, "the intent changed state while re-arming it"),
        Err(e) => internal_error("outbox re-arm", &e),
    }
}

/// `GET /api/outbox/reconciliation` — the control of last resort, on demand.
pub async fn reconciliation_route(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(resp) = state.require_admin(req.headers()).await {
        return resp;
    }
    match reconciliation(&state).await {
        Ok(report) => axum::Json(report).into_response(),
        Err(e) => internal_error("outbox reconciliation", &e),
    }
}

/// The owner id the core's **own** subscription is bound under.
///
/// `core.outbox.` is the first subscription the core owns rather than a plugin,
/// and a reload's sweep clears every subscriber id it finds — so the core's
/// subscription is bound under this id and `server::clear_plugin_subscriptions`
/// skips it, while still clearing every plugin generation. (`core` is a
/// reserved plugin id, so no plugin can collide with it.)
pub const SUBSCRIBER_OWNER: &str = "core";

/// One subscription for the relay's terminal-outcome events, so the core's own
/// audit surface sees them. Producers bind their own (`core.outbox.` prefix).
/// Used by [`crate::server::build_app`]; a producer's own subscription is its
/// business.
pub fn outcome_subscription() -> EventSubscription {
    EventSubscription::new(
        "core.outbox.",
        event_handler(|ev| async move {
            tracing::info!(
                event = %ev.event_type,
                intent = %ev.payload["intent_id"],
                producer = %ev.payload["producer"],
                state = %ev.payload["state"],
                "outbox outcome"
            );
            Ok(())
        }),
    )
}

// ---------------------------------------------------------------------------
// Route helpers
// ---------------------------------------------------------------------------

fn query_params(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|q| {
            q.split('&')
                .filter_map(|pair| {
                    let mut it = pair.splitn(2, '=');
                    let key = it.next()?.to_string();
                    Some((key, it.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn error(status: u16, message: impl Into<String>) -> axum::response::Response {
    crate::server::error_response(
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
        message,
    )
}

fn internal_error(what: &str, err: &dyn std::fmt::Display) -> axum::response::Response {
    crate::server::internal_error(what, err)
}

// ---------------------------------------------------------------------------
// Tests
//
// The four security probes are the argument for this feature; the relay's
// mechanics follow them. Every database-backed probe is `#[ignore]`d (the
// repository's convention, issue #25) so a bare `cargo test --workspace`
// reports it as ignored rather than passed, and under `--ignored` a missing or
// unreachable database is a hard failure, never a skip:
//
// ```text
// ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//   cargo test -p adjutant-server --lib -- --ignored
// ```
//
// The delivery probes act as the **declared** pair the design describes — the
// `stripe` producer and its principal `svc.stripe.ledger`, whose grant is
// `finance:write` — because the relay checks the principal against the core's
// compiled declaration before it delivers anything (see `deliver`, which calls
// `declared_for`). Two fixtures are declared per probe instead where only the
// *enqueue* path is under test (probe 2), where that check does not run.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use adjutant_sdk::{
        async_trait, route_handler, AdjutantPlugin, AuditService, Event, Migration, Permission,
        PermissionService, PluginContext, PluginResponse, RouteDefinition, RouteHandler, SdkError,
    };
    use axum::body::to_bytes;
    use axum::extract::State as AxumState;
    use sqlx::PgPool;

    use crate::config::Config;
    use crate::events::EventBus;
    use crate::host::CoreDb;
    use crate::identity::IdentityHub;
    use crate::plugin_runtime::{LoadedPlugin, PluginInfo, PluginRegistry};
    use crate::scheduler::Scheduler;
    use crate::scope_hierarchy::ScopeHierarchy;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// The producer and principal a delivery probe acts as: the core's own
    /// declared pair (`SERVICE_PRINCIPALS`), so the probe exercises the shape
    /// the money path will actually have.
    const DELIVERY_PRODUCER: &str = "stripe";
    const DELIVERY_PRINCIPAL: &str = "svc.stripe.ledger";
    const DELIVERY_PERMISSION: &str = "finance:write";
    /// The route the principal's declaration describes, protected by
    /// `finance:write` — the gate a delivery must not be able to skip.
    const GATE_ROUTE: &str = "/api/finance/transaction";
    /// A route whose handler always fails, so retry/exhaustion can be driven.
    const BOOM_ROUTE: &str = "/api/probe/boom";
    /// A route whose consumer records one row per idempotency key.
    const LEDGER_ROUTE: &str = "/api/probe/ledger";

    /// Producers used only where the *enqueue* path is under test (probe 2),
    /// declared by the fixture in `core.service_principals`.
    const PROBE_PLUGIN: &str = "outbox_probe";
    const OTHER_PLUGIN: &str = "outbox_probe_other";
    const PROBE_PRINCIPAL: &str = "svc.outbox_probe.ledger";
    /// A principal row that exists in the data but that the core's compiled
    /// declaration does not name: it must not be able to enqueue an intent that
    /// could never be delivered.
    const UNOWNED_PRINCIPAL: &str = "svc.outbox_probe.unowned";
    const OTHER_PRINCIPAL: &str = "svc.outbox_probe.other";

    /// Every intent these probes write carries this key prefix, so cleanup can
    /// name exactly what it owns and nothing else.
    const KEY_PREFIX: &str = "probe-";

    /// Serialises the DB probes: `claim_next` claims the queue's oldest due
    /// intent, so they must not interleave.
    static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn test_database_url() -> String {
        let url = std::env::var("ADJUTANT_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("ADJUTANT_DATABASE_URL"))
            .unwrap_or_else(|_| {
                panic!(
                    "the outbox probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL \
                     (they are #[ignore]d; run with `-- --ignored`)"
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
                "[outbox probe] WARNING: running against {name:?}, which does not end in `_test`"
            );
        }
        url
    }

    async fn pool_from(url: &str) -> Arc<PgPool> {
        Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(5)
                .connect(url)
                .await
                .expect("the database URL is set but unreachable"),
        )
    }

    async fn pool() -> (String, Arc<PgPool>) {
        let url = test_database_url();
        let pool = pool_from(&url).await;
        (url, pool)
    }

    /// Migrate `core`, stand in for what a real boot writes (the finance
    /// plugin's `finance:write` permission, the `chief` bootstrap grant and the
    /// core's declared service principals), and leave the queue holding nothing
    /// but what the probes own.
    async fn provision(pool: &PgPool) {
        crate::db::migrate_core(pool).await.expect("core migrations");
        // The permission the *finance plugin* registers when it loads. A real
        // boot has already written it; asserted here so the probe stands alone.
        sqlx::query(
            "INSERT INTO core.permissions (id, description) \
             VALUES ('finance:write', 'probe fixture (the finance plugin declares this)') \
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("finance:write permission");
        // `build_app` writes this at boot; the operator routes' admin gate needs
        // it to be drivable here.
        sqlx::query(
            "INSERT INTO core.role_permissions (role_id, permission_id) \
             SELECT 'chief', id FROM core.permissions ON CONFLICT DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("chief bootstrap grant");
        seed_service_principals(pool).await.expect("declared principals");
        // The declared pair exists, with the narrow grant the design states —
        // and no admin authority.
        let (producer, granted): (String, i64) = sqlx::query_as(
            "SELECT sp.producer_plugin, \
                    (SELECT count(*)::bigint FROM core.role_permissions rp \
                      WHERE rp.role_id = sp.principal AND rp.permission_id = 'finance:write') \
               FROM core.service_principals sp WHERE sp.principal = $1",
        )
        .bind(DELIVERY_PRINCIPAL)
        .fetch_one(pool)
        .await
        .expect("the core declares stripe's principal");
        assert_eq!(producer, DELIVERY_PRODUCER);
        assert_eq!(granted, 1, "seeded with its narrow grant");

        // Only the probes' own rows are cleared, then the queue must be empty:
        // the claim is global, so anything else queued would break the probes'
        // id assertions. Better to say so than to fail obscurely.
        sqlx::query("DELETE FROM core.outbox WHERE idempotency_key LIKE $1")
            .bind(format!("{KEY_PREFIX}%"))
            .execute(pool)
            .await
            .expect("clear probe intents");
        sqlx::query("DELETE FROM core.outbox WHERE producer_plugin LIKE 'outbox_probe%'")
            .execute(pool)
            .await
            .expect("clear fixture intents");
        let foreign: Option<String> =
            sqlx::query_scalar("SELECT producer_plugin FROM core.outbox LIMIT 1")
                .fetch_optional(pool)
                .await
                .expect("inspect the queue");
        assert!(
            foreign.is_none(),
            "these probes need an otherwise-empty core.outbox (the claim is global); \
             an intent from {foreign:?} is queued"
        );
    }

    /// Declare a principal for a producer, with an optional grant, the way
    /// `core.service_principals` + `core.roles` + `core.role_permissions` hold it.
    async fn declare(pool: &PgPool, principal: &str, producer: &str, grants: &[&str]) {
        sqlx::query(
            "INSERT INTO core.roles (id, display_name, description) \
             VALUES ($1, $1, 'probe fixture') ON CONFLICT (id) DO NOTHING",
        )
        .bind(principal)
        .execute(pool)
        .await
        .expect("probe role");
        // `declared_by_core = true` because this helper stands in for the core's own
        // seeding: the compiled SERVICE_PRINCIPALS is the declaration, and
        // `core.outbox_enqueue` refuses a principal the core never declared. A probe
        // that skipped this flag would be testing a row that can never enqueue.
        sqlx::query(
            "INSERT INTO core.service_principals \
                 (principal, producer_plugin, description, declared_by_core) \
             VALUES ($1, $2, 'probe fixture', true) ON CONFLICT (principal) DO NOTHING",
        )
        .bind(principal)
        .bind(producer)
        .execute(pool)
        .await
        .expect("probe declaration");
        for grant in grants {
            sqlx::query(
                "INSERT INTO core.role_permissions (role_id, permission_id) \
                 SELECT $1, id FROM core.permissions WHERE id = $2 ON CONFLICT DO NOTHING",
            )
            .bind(principal)
            .bind(*grant)
            .execute(pool)
            .await
            .expect("probe grant");
        }
    }

    /// Grant one permission to a role (what an operator does when the money path
    /// should run again).
    async fn grant(pool: &PgPool, principal: &str, permission: &str) {
        sqlx::query(
            "INSERT INTO core.role_permissions (role_id, permission_id) \
             SELECT $1, id FROM core.permissions WHERE id = $2 ON CONFLICT DO NOTHING",
        )
        .bind(principal)
        .bind(permission)
        .execute(pool)
        .await
        .expect("grant");
    }

    /// Revoke a principal's grant, leaving the declaration in place.
    async fn revoke_grant(pool: &PgPool, principal: &str, permission: &str) {
        sqlx::query("DELETE FROM core.role_permissions WHERE role_id = $1 AND permission_id = $2")
            .bind(principal)
            .bind(permission)
            .execute(pool)
            .await
            .expect("revoke");
    }

    /// Remove the fixture's intents, declaration and role. The *declared* pair
    /// is left alone: probe 1 restores it through `seed_service_principals`.
    async fn forget(pool: &PgPool, principal: &str, producer: &str) {
        let _ = sqlx::query("DELETE FROM core.outbox WHERE producer_plugin = $1")
            .bind(producer)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM core.role_permissions WHERE role_id = $1")
            .bind(principal)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM core.service_principals WHERE principal = $1")
            .bind(principal)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM core.roles WHERE id = $1")
            .bind(principal)
            .execute(pool)
            .await;
    }

    /// Remove only the intents these probes wrote (they are named by key
    /// prefix), leaving the **declared** principal and its grant alone.
    async fn drop_probe_intents(pool: &PgPool) {
        let _ = sqlx::query("DELETE FROM core.outbox WHERE idempotency_key LIKE $1")
            .bind(format!("{KEY_PREFIX}%"))
            .execute(pool)
            .await;
    }

    /// Write the intent a producer would have written in its own transaction.
    async fn insert_intent(
        pool: &PgPool,
        producer: &str,
        principal: &str,
        route: &str,
        key: &str,
        max_attempts: i32,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO core.outbox \
               (producer_plugin, principal, target_method, target_route, idempotency_key, \
                payload, max_attempts) \
             VALUES ($1, $2, 'POST', $3, $4, '{\"amount\": 4200}'::jsonb, $5) \
             RETURNING id",
        )
        .bind(producer)
        .bind(principal)
        .bind(route)
        .bind(key)
        .bind(max_attempts)
        .fetch_one(pool)
        .await
        .expect("intent row")
    }

    /// Enqueue as a **plugin role** through the real function. The core's own
    /// pool never calls this: `core.outbox_enqueue` is for `adjutant_plugin_*`
    /// sessions and refuses anything else.
    async fn plugin_enqueue(
        plugin: &PgPool,
        principal: &str,
        route: &str,
        key: &str,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT core.outbox_enqueue($1, 'POST', $2, '{\"amount\": 4200}'::jsonb, $3)",
        )
        .bind(principal)
        .bind(route)
        .bind(key)
        .fetch_one(plugin)
        .await
    }

    async fn intent_state(pool: &PgPool, id: i64) -> (String, i32, Option<String>) {
        sqlx::query_as("SELECT state, attempts, last_error FROM core.outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("intent state")
    }

    // -----------------------------------------------------------------------
    // A fixture plugin: a route the relay can resolve, and a handler the probe
    // observes.
    // -----------------------------------------------------------------------

    struct ProbePlugin;

    #[async_trait]
    impl AdjutantPlugin for ProbePlugin {
        fn id(&self) -> &str {
            PROBE_PLUGIN
        }
        fn name(&self) -> &str {
            "Outbox probe"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        async fn init(&mut self, _ctx: PluginContext) -> Result<(), SdkError> {
            Ok(())
        }
        // The registry resolves against `LoadedPlugin::routes`, not this.
        fn routes(&self) -> Vec<RouteDefinition> {
            Vec::new()
        }
        fn migrations(&self) -> Vec<Migration> {
            Vec::new()
        }
        fn permissions_granted(&self) -> Vec<Permission> {
            Vec::new()
        }
        fn subscriptions(&self) -> Vec<EventSubscription> {
            Vec::new()
        }
    }

    fn probe_state(pool: Arc<PgPool>, routes: Vec<RouteDefinition>) -> Arc<AppState> {
        let config = Config {
            allow_dev_headers: true,
            ..Config::default()
        };
        Arc::new(AppState {
            pool: pool.clone(),
            permissions: PermissionService::new(CoreDb::new(pool.clone())),
            audit: AuditService::new(CoreDb::new(pool.clone()), "core".into()),
            registry: tokio::sync::RwLock::new(PluginRegistry {
                plugins: vec![crate::plugin_runtime::PluginSlot::Live(LoadedPlugin {
                    plugin: Box::new(ProbePlugin),
                    library: None,
                    pool: None,
                    routes,
                    path: std::path::PathBuf::new(),
                    info: PluginInfo {
                        id: PROBE_PLUGIN.into(),
                        name: "Outbox probe".into(),
                        version: "0.0.1".into(),
                        enabled: true,
                        routes: 0,
                        kind: "native".into(),
                        isolated: true,
                        loaded: true,
                        last_error: None,
                        permissions: Vec::new(),
                        schedules: Vec::new(),
                        route_list: Vec::new(),
                    },
                })],
                retired: Vec::new(),
            }),
            bus: EventBus::new(),
            config: Arc::new(config),
            identity: IdentityHub::new(),
            http: crate::host::CoreHttp::new(),
            hierarchy: tokio::sync::RwLock::new(ScopeHierarchy::default()),
            scheduler: Scheduler::new(),
            outbox_mismatches: Mutex::new(0),
            relay: Relay::new(),
            in_flight: crate::server::InFlight::new(),
            lifecycles: crate::server::LifecycleLocks::new(),
        })
    }

    /// A handler that counts its calls, records the identity it was handed, and
    /// answers 200. It also asserts the core forwarded **no headers**: there is
    /// no credential for the relay to forward or mint.
    fn gate_handler(calls: Arc<AtomicUsize>, seen: Arc<Mutex<Option<String>>>) -> RouteHandler {
        route_handler(move |req: PluginRequest| {
            let calls = calls.clone();
            let seen = seen.clone();
            async move {
                assert!(
                    req.headers.is_empty(),
                    "a delivered request carries no headers: {:?}",
                    req.headers
                );
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(identity) = &req.identity {
                    *seen.lock().expect("seen mutex") = Some(identity.user_id.clone());
                }
                PluginResponse::json(200, &serde_json::json!({ "ok": true }))
            }
        })
    }

    /// A handler that always fails the way an unreachable target does.
    fn boom_handler() -> RouteHandler {
        route_handler(|_req: PluginRequest| async move {
            Err::<PluginResponse, _>(SdkError::Internal("the ledger is down".into()))
        })
    }

    /// A handler that is an **idempotent consumer**: one row per idempotency
    /// key, so a redelivery writes nothing a second time. It uses the admin pool
    /// as a fixture convenience; a real consumer writes on its own plugin pool.
    fn ledger_handler(pool: Arc<PgPool>) -> RouteHandler {
        route_handler(move |_req: PluginRequest| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO outbox_probe.ledger (idempotency_key) VALUES ($1) \
                     ON CONFLICT (idempotency_key) DO NOTHING",
                )
                .bind(format!("{KEY_PREFIX}redeliver-1"))
                .execute(pool.as_ref())
                .await
                .map_err(|e| SdkError::Db(e.to_string()))?;
                PluginResponse::json(200, &serde_json::json!({ "booked": true }))
            }
        })
    }

    /// One relay pass for **one** intent, made deterministic: make it due, claim
    /// it (the claim is global, so asserting the id is what keeps the probe
    /// honest), deliver, record.
    async fn attempt_once(state: &Arc<AppState>, id: i64) -> Delivery {
        sqlx::query("UPDATE core.outbox SET next_attempt_at = now() WHERE id = $1")
            .bind(id)
            .execute(state.pool.as_ref())
            .await
            .expect("make due");
        let intent = claim_next(state.pool.as_ref())
            .await
            .expect("claim")
            .expect("an intent to claim");
        assert_eq!(intent.id, id, "the probe's own intent must be the one claimed");
        let outcome = deliver(state, &intent).await;
        record(state.pool.as_ref(), intent.id, intent.attempts, &outcome)
            .await
            .expect("record");
        outcome
    }

    fn admin_request() -> axum::extract::Request {
        axum::http::Request::builder()
            .header("x-dev-user", "christopher")
            .header("x-dev-role", "chief")
            .body(axum::body::Body::empty())
            .expect("request builds")
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    // =======================================================================
    // The four security probes
    // =======================================================================

    /// **Security probe 1.** A principal whose grant does not cover the operation
    /// is refused by the **target's own gate**. The relay runs the same
    /// `authorize` a member's request runs, against `core.role_permissions`; it
    /// does not skip the gate because the caller is internal, and it cannot
    /// widen the principal. The positive control is in the same probe: grant the
    /// permission and the identical delivery lands, so a refusal can only have
    /// been the gate's decision.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn principal_without_the_grant_is_refused_by_the_targets_own_gate() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;
        // The operator's revocation: the declaration stands, the grant is gone.
        revoke_grant(pool.as_ref(), DELIVERY_PRINCIPAL, DELIVERY_PERMISSION).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(None));
        let state = probe_state(
            pool.clone(),
            vec![RouteDefinition::post_protected(
                GATE_ROUTE,
                DELIVERY_PERMISSION,
                gate_handler(calls.clone(), seen.clone()),
            )],
        );

        // --- revoked grant: the target's gate refuses
        let refused_id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            GATE_ROUTE,
            &format!("{KEY_PREFIX}gate-1"),
            2,
        )
        .await;
        let refused = attempt_once(&state, refused_id).await;
        assert_eq!(
            refused.state, STATE_REFUSED,
            "the target's gate must refuse the principal: {refused:?}"
        );
        assert_eq!(refused.http_status, Some(403));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the target's handler must not run when its gate refuses"
        );
        let error = refused.error.clone().unwrap_or_default();
        assert!(
            error.contains("refused finance:write"),
            "the refusal must name the operation and the gate: {error}"
        );
        let (row_state, attempts, last_error) = intent_state(pool.as_ref(), refused_id).await;
        assert_eq!(row_state, STATE_REFUSED, "the refusal is in the data, not only a log");
        assert_eq!(attempts, 1, "the attempt is spent at the claim");
        assert!(last_error.is_some());

        // --- the control: the same principal, granted the permission
        grant(pool.as_ref(), DELIVERY_PRINCIPAL, DELIVERY_PERMISSION).await;
        let delivered_id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            GATE_ROUTE,
            &format!("{KEY_PREFIX}gate-2"),
            2,
        )
        .await;
        let delivered = attempt_once(&state, delivered_id).await;
        assert_eq!(
            delivered.state, STATE_DELIVERED,
            "with the grant the target serves the same delivery: {delivered:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the handler ran exactly once");
        assert_eq!(
            seen.lock().expect("seen mutex").clone(),
            Some(format!("service:{DELIVERY_PRINCIPAL}")),
            "the identity is the core-built service principal, namespaced as such"
        );

        // --- revoking it again is a refusal, never a widening
        revoke_grant(pool.as_ref(), DELIVERY_PRINCIPAL, DELIVERY_PERMISSION).await;
        let revoked_id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            GATE_ROUTE,
            &format!("{KEY_PREFIX}gate-3"),
            2,
        )
        .await;
        let revoked = attempt_once(&state, revoked_id).await;
        assert_eq!(revoked.state, STATE_REFUSED, "a revocation is honoured");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the handler must not run again after the grant was revoked"
        );

        // Leave the database as a boot would: declared, with its narrow grant.
        seed_service_principals(pool.as_ref()).await.expect("restore the declaration");
        drop_probe_intents(pool.as_ref()).await;
    }

    /// **Security probe 2.** A plugin cannot forge or assert a principal: the
    /// enqueue path takes **no identity parameter at all** (the producer is
    /// derived from `session_user`), so the only principal a plugin can name is
    /// its own — and a refused forgery writes nothing. A second real plugin role
    /// cannot read the first producer's intents either.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
    async fn a_plugin_cannot_forge_or_assert_a_principal() {
        let _guard = DB.lock().await;
        let (url, pool) = pool().await;
        provision(pool.as_ref()).await;
        declare(pool.as_ref(), PROBE_PRINCIPAL, PROBE_PLUGIN, &[]).await;
        declare(pool.as_ref(), OTHER_PRINCIPAL, OTHER_PLUGIN, &[]).await;
        // `svc.stripe.ledger` is declared — for the `stripe` plugin only.
        assert!(
            declared_for("stripe", "svc.stripe.ledger").is_some(),
            "the core declares stripe's principal"
        );

        // (a) The signature itself: no parameter names a producer or an actor.
        let (args, secdef): (String, bool) = sqlx::query_as(
            "SELECT pg_get_function_arguments(p.oid), p.prosecdef \
               FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
              WHERE n.nspname = 'core' AND p.proname = 'outbox_enqueue'",
        )
        .fetch_one(pool.as_ref())
        .await
        .expect("outbox_enqueue exists");
        assert!(secdef, "outbox_enqueue is SECURITY DEFINER");
        assert_eq!(
            args.matches(',').count(),
            4,
            "exactly five parameters, none of them an identity: {args}"
        );
        for forbidden in ["producer", "identity", "actor", "session", "role", "user"] {
            assert!(
                !args.contains(forbidden),
                "the enqueue path must take no {forbidden:?} parameter: {args}"
            );
        }
        assert!(args.contains("p_principal") && args.contains("p_idempotency_key"));

        // (b) As a real plugin role.
        let secret = crate::schema::bootstrap_role(pool.as_ref(), PROBE_PLUGIN, None, false)
            .await
            .expect("bootstrap plugin role (needs CREATEROLE/superuser)");
        let plugin = crate::host::plugin_pool(&url, PROBE_PLUGIN, &secret, 2)
            .await
            .expect("plugin pool");

        // A principal declared for another producer: refused, and named.
        let err = plugin_enqueue(&plugin, "svc.stripe.ledger", GATE_ROUTE, &format!("{KEY_PREFIX}forge-1"))
            .await
            .expect_err("a plugin must not enqueue as stripe's principal");
        assert!(
            err.to_string().contains("may not enqueue as principal"),
            "the forgery must be refused by name: {err}"
        );
        // A principal that is not declared at all: refused.
        let err = plugin_enqueue(&plugin, "svc.nobody", GATE_ROUTE, &format!("{KEY_PREFIX}forge-2"))
            .await
            .expect_err("an undeclared principal must be refused");
        assert!(
            err.to_string().contains("no service principal"),
            "the refusal must name the missing declaration: {err}"
        );
        // A row that exists but that the core never declared: the compiled
        // SERVICE_PRINCIPALS is what authorizes a delivery, so this must be refused
        // *at enqueue* rather than becoming an intent that can never be delivered.
        sqlx::query(
            "INSERT INTO core.roles (id, display_name) VALUES ($1, $1) ON CONFLICT (id) DO NOTHING",
        )
        .bind(UNOWNED_PRINCIPAL)
        .execute(pool.as_ref())
        .await
        .expect("unowned role");
        sqlx::query(
            "INSERT INTO core.service_principals \
                 (principal, producer_plugin, description, declared_by_core) \
             VALUES ($1, $2, 'a row the core never declared', false) \
             ON CONFLICT (principal) DO NOTHING",
        )
        .bind(UNOWNED_PRINCIPAL)
        .bind(PROBE_PLUGIN)
        .execute(pool.as_ref())
        .await
        .expect("unowned declaration");
        let err = plugin_enqueue(
            &plugin,
            UNOWNED_PRINCIPAL,
            GATE_ROUTE,
            &format!("{KEY_PREFIX}forge-3"),
        )
        .await
        .expect_err("a principal the core never declared must not enqueue");
        assert!(
            err.to_string().contains("is not declared by this core"),
            "the refusal must say the core does not declare it: {err}"
        );

        let forged: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM core.outbox WHERE producer_plugin = $1")
                .bind(PROBE_PLUGIN)
                .fetch_one(pool.as_ref())
                .await
                .expect("count forged intents");
        assert_eq!(forged, 0, "a refused forgery must write no intent at all");

        // Its own principal enqueues — and the key is idempotent, so a retry of
        // its own write returns the intent it already has rather than a second.
        let first = plugin_enqueue(&plugin, PROBE_PRINCIPAL, GATE_ROUTE, &format!("{KEY_PREFIX}own-1"))
            .await
            .expect("its own declared principal");
        let again = plugin_enqueue(&plugin, PROBE_PRINCIPAL, GATE_ROUTE, &format!("{KEY_PREFIX}own-1"))
            .await
            .expect("the same fact again");
        assert_eq!(first, again, "one fact, one intent: the key is idempotent");
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM core.outbox WHERE producer_plugin = $1")
                .bind(PROBE_PLUGIN)
                .fetch_one(pool.as_ref())
                .await
                .expect("count intents");
        assert_eq!(rows, 1, "no second intent for the same key");

        // (c) Another producer cannot read it, and cannot write as it.
        let other_secret = crate::schema::bootstrap_role(pool.as_ref(), OTHER_PLUGIN, None, false)
            .await
            .expect("bootstrap the second plugin role");
        let other = crate::host::plugin_pool(&url, OTHER_PLUGIN, &other_secret, 2)
            .await
            .expect("second plugin pool");
        let visible: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM core.outbox_producer_view()")
                .fetch_one(other.as_ref())
                .await
                .expect("producer view");
        assert_eq!(visible, 0, "a producer sees only its own intents");
        let mine: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM core.outbox_producer_view()")
                .fetch_one(plugin.as_ref())
                .await
                .expect("producer view");
        assert_eq!(mine, 1, "the producing role sees its own intent");
        let err = plugin_enqueue(&other, PROBE_PRINCIPAL, GATE_ROUTE, &format!("{KEY_PREFIX}other-1"))
            .await
            .expect_err("the second producer may not write as the first's principal");
        assert!(
            err.to_string().contains("may not enqueue as principal"),
            "{err}"
        );

        forget(pool.as_ref(), PROBE_PRINCIPAL, PROBE_PLUGIN).await;
        forget(pool.as_ref(), UNOWNED_PRINCIPAL, PROBE_PLUGIN).await;
        forget(pool.as_ref(), OTHER_PRINCIPAL, OTHER_PLUGIN).await;
    }

    /// **Security probe 3.** A failed delivery is retried, and its exhaustion is
    /// **visible in the data**: the intent lands in `exhausted` carrying its last
    /// error, the reconciliation report names it `unlanded` with both sides of
    /// the disagreement, the operator route lists it, and that route can re-arm
    /// it — while a non-admin cannot read it at all.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn a_failed_delivery_is_retried_and_its_exhaustion_is_visible_in_the_data() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;

        let state = probe_state(
            pool.clone(),
            vec![RouteDefinition::post(BOOM_ROUTE, boom_handler())],
        );
        let id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            BOOM_ROUTE,
            &format!("{KEY_PREFIX}boom-1"),
            2,
        )
        .await;

        // Attempt 1: retryable, so the intent stays queued with a backoff.
        let first = attempt_once(&state, id).await;
        assert_eq!(first.state, STATE_PENDING, "a 5xx is retried: {first:?}");
        assert!(
            first.error.as_deref().unwrap_or_default().contains("ledger is down"),
            "the target's failure is the recorded reason: {first:?}"
        );
        let (row_state, attempts, last_error) = intent_state(pool.as_ref(), id).await;
        assert_eq!(row_state, STATE_PENDING);
        assert_eq!(attempts, 1, "the attempt is spent at the claim");
        assert!(last_error.is_some(), "the failure is recorded on the row");
        let backoff_applied: bool =
            sqlx::query_scalar("SELECT next_attempt_at > now() FROM core.outbox WHERE id = $1")
                .bind(id)
                .fetch_one(pool.as_ref())
                .await
                .expect("backoff");
        assert!(backoff_applied, "a backoff was applied before the next attempt");

        // Attempt 2 (the budget is spent): exhausted, not pending.
        let second = attempt_once(&state, id).await;
        assert_eq!(
            second.state, STATE_EXHAUSTED,
            "the last attempt exhausts: {second:?}"
        );
        assert_eq!(second.event, Some(EVENT_EXHAUSTED));
        let (row_state, attempts, last_error) = intent_state(pool.as_ref(), id).await;
        assert_eq!(row_state, STATE_EXHAUSTED);
        assert_eq!(attempts, 2);
        assert_eq!(
            last_error.as_deref(),
            second.error.as_deref(),
            "the row carries the last error verbatim"
        );
        let answer_status: Option<i32> =
            sqlx::query_scalar("SELECT answer_status FROM core.outbox WHERE id = $1")
                .bind(id)
                .fetch_one(pool.as_ref())
                .await
                .expect("answer_status");
        assert_eq!(answer_status, Some(500), "the target's answer is recorded too");

        // Visible 1: the reconciliation report (the control of last resort).
        let report = reconciliation(&state).await.expect("reconciliation");
        assert_eq!(
            report["ok"],
            serde_json::json!(false),
            "the report says not-ok: {report}"
        );
        let mismatch = report["mismatches"]
            .as_array()
            .expect("mismatches array")
            .iter()
            .find(|m| m["intent_id"] == serde_json::json!(id))
            .expect("the exhausted intent is in the mismatch report");
        assert_eq!(mismatch["mismatch"], serde_json::json!("unlanded"));
        assert_eq!(mismatch["state"], serde_json::json!(STATE_EXHAUSTED));
        assert!(
            mismatch["last_error"].as_str().unwrap_or_default().contains("ledger is down"),
            "the report carries the failure: {mismatch}"
        );
        assert_eq!(
            mismatch["intent_payload"]["amount"],
            serde_json::json!(4200),
            "both sides of the disagreement are in the row"
        );

        // Visible 2: the operator route.
        let resp = list_intents(AxumState(state.clone()), admin_request()).await;
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        let listed = body["intents"]
            .as_array()
            .expect("intents array")
            .iter()
            .find(|i| i["id"] == serde_json::json!(id))
            .expect("the exhausted intent is in the worklist");
        assert_eq!(listed["state"], serde_json::json!(STATE_EXHAUSTED));
        assert_eq!(body["by_state"]["exhausted"], serde_json::json!(1));

        // The operator's hand: re-arm it (with the audit row written first).
        let resp = retry_intent(
            AxumState(state.clone()),
            axum::extract::Path(id),
            admin_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "the operator route re-arms it");
        let (row_state, attempts, _) = intent_state(pool.as_ref(), id).await;
        assert_eq!(row_state, STATE_PENDING);
        assert_eq!(attempts, 0, "the attempt budget is restored");
        // Only a refused or exhausted intent may be re-armed.
        let resp = retry_intent(
            AxumState(state.clone()),
            axum::extract::Path(id),
            admin_request(),
        )
        .await;
        assert_eq!(resp.status(), 409, "a pending intent is already queued");

        // The operator gate is real: no identity is refused.
        let anon = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .expect("request builds");
        let resp = list_intents(AxumState(state.clone()), anon).await;
        assert_eq!(resp.status(), 401, "no identity is refused");

        drop_probe_intents(pool.as_ref()).await;
    }

    /// **Security probe 4.** A redelivery does not double-write: the consumer is
    /// idempotent on the intent's idempotency key, the relay never claims an
    /// intent that reached a terminal state, and re-arming a delivered intent is
    /// refused outright.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn a_redelivery_does_not_double_write() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;
        sqlx::query("CREATE SCHEMA IF NOT EXISTS outbox_probe")
            .execute(pool.as_ref())
            .await
            .expect("fixture schema");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS outbox_probe.ledger (idempotency_key TEXT PRIMARY KEY)",
        )
        .execute(pool.as_ref())
        .await
        .expect("fixture table");
        sqlx::query("DELETE FROM outbox_probe.ledger")
            .execute(pool.as_ref())
            .await
            .expect("clear the consumer's table");

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let inner = ledger_handler(pool.clone());
        let handler = route_handler(move |req: PluginRequest| {
            let inner = inner.clone();
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                inner(req).await
            }
        });
        let state = probe_state(
            pool.clone(),
            vec![RouteDefinition::post(LEDGER_ROUTE, handler)],
        );

        let id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            LEDGER_ROUTE,
            &format!("{KEY_PREFIX}redeliver-1"),
            2,
        )
        .await;
        let first = attempt_once(&state, id).await;
        assert_eq!(first.state, STATE_DELIVERED, "{first:?}");

        // The redelivery: the relay claims only a *pending* intent.
        sqlx::query("UPDATE core.outbox SET next_attempt_at = now() WHERE id = $1")
            .bind(id)
            .execute(pool.as_ref())
            .await
            .expect("make due");
        let again = claim_next(pool.as_ref()).await.expect("claim");
        assert!(
            again.is_none(),
            "a delivered intent is never claimed again (got {:?})",
            again.map(|i| i.id)
        );
        // And re-arming it is refused: a delivered intent is done.
        let resp = retry_intent(
            AxumState(state.clone()),
            axum::extract::Path(id),
            admin_request(),
        )
        .await;
        assert_eq!(resp.status(), 409, "a delivered intent is not re-armed");

        // One fact, one write, on both sides of the wire.
        let written: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM outbox_probe.ledger")
            .fetch_one(pool.as_ref())
            .await
            .expect("consumer rows");
        assert_eq!(written, 1, "the consumer wrote once");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the handler ran once");
        let intents: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM core.outbox WHERE producer_plugin = $1")
                .bind(DELIVERY_PRODUCER)
                .fetch_one(pool.as_ref())
                .await
                .expect("intent rows");
        assert_eq!(intents, 1, "one intent was written for the fact");

        drop_probe_intents(pool.as_ref()).await;
    }

    // =======================================================================
    // Relay mechanics
    // =======================================================================

    /// Two passes — or two processes — cannot take one intent.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn two_passes_cannot_take_one_intent() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;
        let id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            BOOM_ROUTE,
            &format!("{KEY_PREFIX}claim-1"),
            2,
        )
        .await;

        let (a, b) = tokio::join!(claim_next(pool.as_ref()), claim_next(pool.as_ref()));
        let claims = [a.expect("claim a"), b.expect("claim b")];
        let taken: Vec<i64> = claims.iter().flatten().map(|i| i.id).collect();
        assert_eq!(taken, vec![id], "exactly one pass claimed the one intent");
        let (row_state, attempts, _) = intent_state(pool.as_ref(), id).await;
        assert_eq!(row_state, STATE_ATTEMPTING);
        assert_eq!(attempts, 1, "the attempt is spent at the claim, once");

        drop_probe_intents(pool.as_ref()).await;
    }

    /// `FOR UPDATE SKIP LOCKED`: a row another session holds is **skipped**, not
    /// waited for. With a plain `FOR UPDATE` the claim would block here until the
    /// lock is released; the timeout is what makes that a failure rather than a
    /// hang.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn claim_skips_a_row_locked_by_another_session() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;
        let id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            BOOM_ROUTE,
            &format!("{KEY_PREFIX}skip-1"),
            2,
        )
        .await;

        let mut holder = pool.begin().await.expect("holder transaction");
        sqlx::query("SELECT id FROM core.outbox WHERE state = 'pending' ORDER BY id FOR UPDATE")
            .fetch_all(&mut *holder)
            .await
            .expect("lock the row");

        let claimed =
            tokio::time::timeout(std::time::Duration::from_secs(5), claim_next(pool.as_ref()))
                .await
                .expect("the claim must skip the locked row, not block on it")
                .expect("claim");
        assert!(
            claimed.is_none(),
            "the locked row is skipped (got {:?})",
            claimed.map(|i| i.id)
        );

        holder.rollback().await.expect("rollback");
        // Released, it is claimable again.
        let intent = claim_next(pool.as_ref())
            .await
            .expect("claim")
            .expect("claimable once unlocked");
        assert_eq!(intent.id, id);

        drop_probe_intents(pool.as_ref()).await;
    }

    /// An abandoned claim (a crash between claim and record) goes back to
    /// `pending` with its lease cleared — and keeps the attempt it spent, so a
    /// row cannot be retried forever.
    #[tokio::test]
    #[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
    async fn reclaim_stale_returns_an_abandoned_claim_to_pending() {
        let _guard = DB.lock().await;
        let (_url, pool) = pool().await;
        provision(pool.as_ref()).await;
        let id = insert_intent(
            pool.as_ref(),
            DELIVERY_PRODUCER,
            DELIVERY_PRINCIPAL,
            BOOM_ROUTE,
            &format!("{KEY_PREFIX}stale-1"),
            2,
        )
        .await;

        let intent = claim_next(pool.as_ref())
            .await
            .expect("claim")
            .expect("one intent");
        assert_eq!(intent.id, id);
        let fresh = reclaim_stale(pool.as_ref()).await.expect("reclaim");
        assert_eq!(fresh, 0, "a live lease is left alone");

        // The relay died holding it.
        sqlx::query(
            "UPDATE core.outbox SET claimed_at = now() - make_interval(secs => 600) WHERE id = $1",
        )
        .bind(id)
        .execute(pool.as_ref())
        .await
        .expect("age the lease");
        let reclaimed = reclaim_stale(pool.as_ref()).await.expect("reclaim");
        assert_eq!(reclaimed, 1, "the abandoned claim is returned");
        let (row_state, attempts, last_error) = intent_state(pool.as_ref(), id).await;
        assert_eq!(row_state, STATE_PENDING);
        assert_eq!(attempts, 1, "the spent attempt is not refunded");
        assert!(last_error.is_some(), "the abandonment is recorded");
        let claimed_at: Option<String> =
            sqlx::query_scalar("SELECT claimed_at::text FROM core.outbox WHERE id = $1")
                .bind(id)
                .fetch_one(pool.as_ref())
                .await
                .expect("claimed_at");
        assert!(claimed_at.is_none(), "the lease is cleared");
        let due: bool =
            sqlx::query_scalar("SELECT next_attempt_at <= now() FROM core.outbox WHERE id = $1")
                .bind(id)
                .fetch_one(pool.as_ref())
                .await
                .expect("due");
        assert!(due, "it is due again");

        drop_probe_intents(pool.as_ref()).await;
    }

    // =======================================================================
    // The core's own subscription
    // =======================================================================

    /// The hazard this closes: `core.outbox.` is the first subscription the
    /// **core** owns rather than a plugin, and a reload's sweep iterates every
    /// subscriber id it finds. Clearing the core's own subscription would go
    /// unnoticed until an outcome event went unheard — so the sweep skips the
    /// owner id and the core's handler keeps receiving.
    #[tokio::test]
    async fn the_cores_own_subscription_survives_a_reload_sweep() {
        let bus = EventBus::new();
        let core_seen = Arc::new(AtomicUsize::new(0));
        let plugin_seen = Arc::new(AtomicUsize::new(0));
        let counter = |n: Arc<AtomicUsize>| {
            event_handler(move |_ev| {
                let n = n.clone();
                async move {
                    n.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
        };

        assert_eq!(
            outcome_subscription().filter,
            "core.outbox.",
            "the core's subscription is keyed on the outcome events"
        );
        bus.subscribe(
            SUBSCRIBER_OWNER,
            EventSubscription::new("core.outbox.", counter(core_seen.clone())),
        );
        bus.subscribe(
            "hello",
            EventSubscription::new("core.outbox.", counter(plugin_seen.clone())),
        );
        bus.subscribe(
            "membership",
            EventSubscription::new("core.outbox.", counter(plugin_seen.clone())),
        );

        crate::server::clear_plugin_subscriptions(&bus).await;
        let ids = bus.subscriber_ids();
        assert!(
            ids.contains(&SUBSCRIBER_OWNER.to_string()),
            "the core's own subscription must survive the sweep: {ids:?}"
        );
        assert!(
            !ids.contains(&"hello".to_string()) && !ids.contains(&"membership".to_string()),
            "every plugin generation is cleared: {ids:?}"
        );

        let event = Event {
            id: 1,
            event_type: EVENT_DELIVERED.into(),
            payload: serde_json::json!({ "intent_id": 7 }),
            source: "core".into(),
            timestamp: chrono::Utc::now(),
        };
        let _ = bus.sender().send(event);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            core_seen.load(Ordering::SeqCst),
            1,
            "the core's handler still receives its own outcome events"
        );
        assert_eq!(
            plugin_seen.load(Ordering::SeqCst),
            0,
            "no cleared plugin generation receives anything"
        );
        bus.shutdown();
    }

    // =======================================================================
    // Pure boundaries
    // =======================================================================

    /// **The declaration set, stated rather than incidental.** One entry per
    /// machine-originated operation that has no caller, each scoped to exactly one
    /// producer — which is what makes "a plugin cannot widen its own machine
    /// authority" true in the core's mirror of the SQL check.
    #[test]
    fn the_declared_principals_are_the_ones_the_core_ships() {
        let declared: Vec<(&str, &str)> = SERVICE_PRINCIPALS
            .iter()
            .map(|d| (d.principal, d.producer))
            .collect();
        assert_eq!(
            declared,
            vec![
                ("svc.stripe.ledger", "stripe"),
                ("svc.store.draw", "store"),
            ],
            "a new machine-originated operation is a new entry here, and only here"
        );
        for decl in SERVICE_PRINCIPALS {
            assert!(
                !decl.display_name.is_empty() && !decl.description.is_empty(),
                "{} is declared without a name or a description an operator can read",
                decl.principal
            );
            assert!(
                !decl.grants.iter().any(|p| p.contains("admin")),
                "a service principal never holds an admin permission: {:?}",
                decl.grants
            );
            assert!(
                !decl.grants.is_empty(),
                "{} holds no grant, so every delivery as it would be refused",
                decl.principal
            );
        }
    }

    /// The scope is the *pair*: a principal is deliverable by one producer, and a
    /// producer may own any number of principals — but never somebody else's.
    #[test]
    fn a_principal_is_declared_for_exactly_one_producer() {
        let decl = declared_for("stripe", "svc.stripe.ledger").expect("stripe owns it");
        assert_eq!(decl.principal, "svc.stripe.ledger");
        assert_eq!(decl.producer, "stripe");
        assert_eq!(decl.grants, &["finance:write"], "and its grant is narrow");
        assert!(
            declared_for("store", "svc.stripe.ledger").is_none(),
            "another producer may not deliver as it"
        );
        assert!(declared_for("stripe", "svc.nobody").is_none());

        // The store's draw is the second producer on the same rail, with the one
        // grant finance's transfer route gates on.
        let draw = declared_for("store", "svc.store.draw").expect("store owns it");
        assert_eq!(
            draw.display_name,
            "Service principal - Store scholarship draw"
        );
        assert!(
            draw.description.contains("POST /api/finance/transfer"),
            "the description names the one route it may call: {}",
            draw.description
        );
        assert_eq!(draw.grants, &["finance:write"], "and nothing else");
        assert!(
            declared_for("stripe", "svc.store.draw").is_none(),
            "the rail is scoped by producer, not by convention"
        );
    }

    /// The delivered identity is namespaced and carries the principal's own
    /// role at troop scope, and nothing else.
    #[test]
    fn the_delivered_identity_is_the_principal_and_nothing_else() {
        let identity = identity_for("svc.stripe.ledger");
        assert_eq!(identity.user_id, "service:svc.stripe.ledger");
        assert_eq!(identity.roles(), vec!["svc.stripe.ledger".to_string()]);
        assert_eq!(identity.grants.len(), 1);
        assert_eq!(identity.grants[0].scope, Scope::troop());
    }

    /// The backoff grows from the base and caps; the cap is what stops an intent
    /// from being retried sooner than the queue's slowest consumer can bear.
    #[test]
    fn backoff_grows_from_the_base_and_caps() {
        assert_eq!(backoff_secs(1), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(2), BACKOFF_BASE_SECS * 2);
        assert_eq!(backoff_secs(3), BACKOFF_BASE_SECS * 4);
        assert_eq!(backoff_secs(0), BACKOFF_BASE_SECS, "a clamped floor, never zero");
        let mut previous = 0;
        for attempts in 1..=40 {
            let wait = backoff_secs(attempts);
            assert!(
                wait >= previous,
                "monotonic: {attempts} → {wait} after {previous}"
            );
            assert!(
                wait <= BACKOFF_MAX_SECS,
                "capped at {BACKOFF_MAX_SECS}: got {wait}"
            );
            previous = wait;
        }
        assert_eq!(
            backoff_secs(40),
            BACKOFF_MAX_SECS,
            "it reaches the ceiling and stays there"
        );
        assert_eq!(
            DEFAULT_MAX_ATTEMPTS, 6,
            "the attempt budget mirrored by the column default"
        );
    }
}
