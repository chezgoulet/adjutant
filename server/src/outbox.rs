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

use sqlx::PgPool;
use tokio::task::JoinHandle;

use adjutant_sdk::{
    event_handler, EventSubscription, HostEvents, Identity, PluginRequest, RoleGrant, Scope, SqlValue,
};

use crate::permissions::authorize;
use crate::plugin_runtime::{PluginRegistry, RouteLookup};
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
pub const SERVICE_PRINCIPALS: &[ServicePrincipalDecl] = &[ServicePrincipalDecl {
    principal: "svc.stripe.ledger",
    producer: "stripe",
    display_name: "Service principal — Stripe ledger booking",
    description: "Books one confirmed Stripe payment into finance's ledger via \
                  POST /api/finance/transaction. Declared for the stripe plugin only.",
    grants: &["finance:write"],
}];

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
        sqlx::query(
            "INSERT INTO core.service_principals (principal, producer_plugin, description) \
             VALUES ($1, $2, $3) ON CONFLICT (principal) DO NOTHING",
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
                if pass % RECONCILE_EVERY == 0 {
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

/// The backoff before attempt `n + 1`, doubling from [`BACKOFF_BASE_SECS`] and
/// capped at [`BACKOFF_MAX_SECS`].
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

/// Drain up to [`BATCH`] due intents. Returns how many were attempted.
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
    if !crate::outbox::declared_for(&principal.producer_plugin, &intent.principal).is_some() {
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
        // Retryable, not terminal: a disabled plugin may be enabled again, and a
        // plugin mid-reload has no routes for a moment. Exhaustion is the floor.
        RouteLookup::Disabled { plugin_id } => {
            return Delivery::retry(
                attempts,
                max_attempts,
                None,
                format!("target plugin {plugin_id} is disabled, so its route does not serve"),
            )
        }
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
        query: HashMap::new(),
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
                intent = ev.payload["intent_id"],
                producer = ev.payload["producer"],
                state = ev.payload["state"],
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
