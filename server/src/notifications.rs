//! The notification record, and the sentence it must never say (#46, slice 1).
//!
//! Design: `docs/design/notifications.md`. The owner decided (2026-09-25) that
//! the channel is Web Push, device push and an in-app board, and that **the
//! board is the record — everything else is a notification that something is on
//! the board.** This module is the record. It sends nothing anywhere.
//!
//! ## The record is not the delivery
//!
//! ```text
//! read_at          a fact about the person   — "the recipient opened it"
//! delivery_state   a fact about a transport  — "recorded" | "delivered" | "failed"
//! delivered_at     the evidence of a delivery
//! ```
//!
//! **Never report a notification as delivered when it was only recorded.**
//! Marking a notification read writes `read_at` and leaves `delivery_state`
//! alone; slice 1 only ever writes `recorded` because there is no transport to
//! move a row further. The schema refuses the two lies — a `delivered` state
//! without a `delivered_at`, and a `delivered_at` without the state — so the
//! rule holds even for a caller that bypasses this module.
//!
//! ## The channel is a policy, not an assumption
//!
//! [`DELIVERY_CHANNELS`] is the list of transports this core can be responsible
//! for; [`channel_for`] picks one for a new record; the channel is a **column**,
//! not a constant read at the call site. Adding email or web push is a value in
//! that list, a one-line widening of the table's `CHECK`, and a relay that
//! drains `delivery_state = 'recorded'` rows — not a change to how a recipient
//! reads their inbox. [`list_notifications`] and [`mark_notification_read`]
//! never branch on the channel or the delivery state: they report the delivery
//! facts, they do not consult them. See design §4.
//!
//! ## A producer cannot choose the channel, and cannot name itself
//!
//! A plugin's scheduled run executes on its own pool as its own isolation role,
//! so it cannot `INSERT` into `core.notifications` (the table is `REVOKE`d from
//! `PUBLIC`). It writes through the `SECURITY DEFINER` function `core.notify`,
//! which derives `source` from `session_user` — never from a parameter — and
//! fixes the channel itself. A producer therefore states *what happened*, and can
//! only ever state it as itself. [`create`] is the core's own direct write, for
//! the core being driven by something that is not a plugin.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::server::{error_response, internal_error, AppState};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// The in-app board: the surface that *is* the record. The only channel slice 1
/// has a transport for, because the record needs no transport to exist.
pub const CHANNEL_IN_APP: &str = "in_app";

/// The transports this core can be responsible for.
///
/// One entry today. Widening it (with the table's `CHECK`) is what adding email
/// or push *is*: a row may not claim a channel the core has no transport for, or
/// the record would promise a delivery no code can make.
pub const DELIVERY_CHANNELS: &[&str] = &[CHANNEL_IN_APP];

/// Recorded. **This is the only state slice 1 writes**: the row exists, nothing
/// has been sent, and [`create`] says so rather than implying it.
pub const STATE_RECORDED: &str = "recorded";
/// A transport confirmed delivery. Only a transport may write this, and the
/// schema requires `delivered_at` beside it.
pub const STATE_DELIVERED: &str = "delivered";
/// A transport tried and did not deliver.
pub const STATE_FAILED: &str = "failed";

/// The three delivery states, in lifecycle order.
pub const DELIVERY_STATES: &[&str] = &[STATE_RECORDED, STATE_DELIVERED, STATE_FAILED];

/// The core's own source tag: a notification the core recorded for itself,
/// rather than one a plugin recorded through `core.notify`.
pub const SOURCE_CORE: &str = "core";

/// The honest locale when a producer does not know the recipient's language
/// (BCP-47 `und`). The fallback language is an open question
/// (`docs/design/localization.md` §6 Q1) and this module does not answer it.
pub const LOCALE_UNKNOWN: &str = "und";

/// Default and maximum page size of the inbox listing.
pub const DEFAULT_LIMIT: i64 = 50;
/// The listing's ceiling.
pub const MAX_LIMIT: i64 = 200;

/// The sentence every response carries, so a client cannot mistake a record for
/// a delivery. The same posture as `announcements`' `delivery: "deferred…"`.
pub const DELIVERY_NOTE: &str = "recorded is not delivered: a notification is on the board — \
     delivery_state says whether a transport has taken it anywhere, and slice 1 has none";

/// The columns the read path returns, in one place so the list, the mark-read
/// and the re-read cannot drift apart. `::text` on the timestamps because the
/// API speaks ISO-8601 strings (the same cast the outbox operator surface uses).
const NOTIFICATION_FIELDS: &str =
    "id, source, message_code, message_params, locale, delivery_channel, delivery_state, \
     delivered_at::text AS delivered_at, read_at::text AS read_at, created_at::text AS created_at";

// ---------------------------------------------------------------------------
// Pure helpers — validation and vocabulary (no database, no context)
// ---------------------------------------------------------------------------

/// Whether `code` is shaped like a message code: lower-case, dot-separated,
/// namespaced by convention (`bg_check.expiring`).
///
/// The same shape the table's `CHECK` enforces. A code is a *vocabulary* item,
/// never display text (`docs/design/localization.md` §4).
pub fn is_message_code(code: &str) -> bool {
    let mut chars = code.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.'))
}

/// Whether `locale` is shaped like a BCP-47 language tag (`en`, `fr-CA`, `und`).
pub fn is_locale_tag(locale: &str) -> bool {
    let mut parts = locale.split('-');
    match parts.next() {
        Some(language)
            if (2..=8).contains(&language.len())
                && language.chars().all(|c| c.is_ascii_alphabetic()) => {}
        _ => return false,
    }
    parts.all(|part| {
        (1..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// The channel a **new** record is created for: the delivery policy's choice,
/// made here and nowhere else.
///
/// One channel today. When a second transport exists, this is where its
/// eligibility is decided (a recipient's push subscription, an email address),
/// and the caller does not change.
pub fn channel_for() -> &'static str {
    CHANNEL_IN_APP
}

// ---------------------------------------------------------------------------
// The record — create
// ---------------------------------------------------------------------------

/// Record one notification for one user, as the **core**.
///
/// This is the core's own direct write. A plugin (including a plugin's scheduled
/// run) records one through `core.notify` instead, which runs `SECURITY DEFINER`
/// so the plugin needs no grant on `core.notifications` and cannot forge its
/// `source`. Both paths leave `delivery_state = 'recorded'`.
///
/// The `delivery_*` facts are not parameters: the channel is policy
/// ([`channel_for`]) and nothing has been delivered at the moment a record is
/// made. That asymmetry is the hard rule, expressed as a signature.
pub async fn create(
    pool: &PgPool,
    source: &str,
    recipient: &str,
    message_code: &str,
    message_params: Value,
    locale: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO core.notifications \
             (recipient, source, message_code, message_params, locale, delivery_channel) \
         VALUES ($1::uuid, $2, $3, $4, $5, $6) \
         RETURNING id",
    )
    .bind(recipient)
    .bind(source)
    .bind(message_code)
    .bind(message_params)
    .bind(locale)
    .bind(channel_for())
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------------------
// Routes — the recipient's own inbox (an ownership check, not a grant)
// ---------------------------------------------------------------------------

/// `GET /api/notifications` — the caller's own records, newest first.
///
/// **Ownership, not a grant** (SPEC §9.2: "a caller reading their own record is
/// an ownership check, not a grant"). There is no scope at which reading
/// somebody else's notifications is a sensible authority, so the route declares
/// no permission; it requires an authenticated member and filters on
/// `recipient = caller` in the query itself. A manager/oversight list would be a
/// different route with a different permission, and it is not in slice 1.
///
/// `?unread=true` narrows to unread records; `?limit=<1..200>` bounds the page.
pub async fn list_notifications(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let Some(caller) = caller_uuid(&state, req.headers()).await else {
        return error_response(
            axum::http::StatusCode::UNAUTHORIZED,
            "authentication required: an inbox belongs to a member",
        );
    };

    let params = query_params(req.uri().query());
    let unread_only = params
        .get("unread")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    let rows = sqlx::query(&format!(
        "SELECT {NOTIFICATION_FIELDS} FROM core.notifications \
          WHERE recipient = $1::uuid AND ($2::bool = false OR read_at IS NULL) \
          ORDER BY created_at DESC, id DESC LIMIT $3"
    ))
    .bind(&caller)
    .bind(unread_only)
    .bind(limit)
    .fetch_all(state.pool.as_ref())
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => return internal_error("notification inbox", &e),
    };

    // The unread count is its own query: with a `limit` on the page, counting
    // the returned rows would report the page's size, not the badge.
    let unread: Result<i64, sqlx::Error> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM core.notifications \
          WHERE recipient = $1::uuid AND read_at IS NULL",
    )
    .bind(&caller)
    .fetch_one(state.pool.as_ref())
    .await;
    let unread = match unread {
        Ok(count) => count,
        Err(e) => return internal_error("notification unread count", &e),
    };

    let notifications: Vec<Value> = rows.iter().map(notification_json).collect();
    axum::Json(json!({
        "notifications": notifications,
        "count": notifications.len(),
        "unread": unread,
        "filters": { "unread": unread_only, "limit": limit },
        "delivery": {
            "channels": DELIVERY_CHANNELS,
            "states": DELIVERY_STATES,
            "note": DELIVERY_NOTE,
        },
    }))
    .into_response()
}

/// `POST /api/notifications/{id}/read` — mark the caller's own record read.
///
/// Idempotent, like an announcement's receipt: a second mark-read changes
/// nothing and says so (`already_read: true`). A record that is not the
/// caller's updates no row and is reported **404, not 403** — whether somebody
/// else has a notification is not the caller's business.
///
/// **It does not deliver anything.** `read_at` moves; `delivery_state` does not.
/// The response reports both, in separate objects, so a client cannot read a
/// read marker as a delivery.
pub async fn mark_notification_read(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let Some(caller) = caller_uuid(&state, req.headers()).await else {
        return error_response(
            axum::http::StatusCode::UNAUTHORIZED,
            "authentication required: a notification belongs to a member",
        );
    };

    // Read it first: the audit row for a *new* read is written before the write
    // (see `server::audit_state_change`), so a no-op and a 404 must be decided
    // without mutating anything.
    let existing = sqlx::query(&format!(
        "SELECT {NOTIFICATION_FIELDS} FROM core.notifications \
          WHERE id = $1 AND recipient = $2::uuid"
    ))
    .bind(id)
    .bind(&caller)
    .fetch_optional(state.pool.as_ref())
    .await;
    let Some(existing) = (match existing {
        Ok(row) => row,
        Err(e) => return internal_error("notification lookup", &e),
    }) else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            format!("no notification {id} for this member"),
        );
    };
    let already: Option<String> = existing.try_get("read_at").ok().flatten();
    if already.is_some() {
        return axum::Json(json!({
            "notification": notification_json(&existing),
            "is_read": true,
            "already_read": true,
        }))
        .into_response();
    }

    if let Some(resp) = crate::server::audit_state_change(
        &state.audit,
        state.resolve_identity(req.headers()).await.as_ref(),
        "notification.read",
        "notification",
        &id.to_string(),
        json!({ "recipient": caller }),
    )
    .await
    {
        return resp;
    }

    let updated = sqlx::query(&format!(
        "UPDATE core.notifications SET read_at = now() \
          WHERE id = $1 AND recipient = $2::uuid AND read_at IS NULL \
         RETURNING {NOTIFICATION_FIELDS}"
    ))
    .bind(id)
    .bind(&caller)
    .fetch_optional(state.pool.as_ref())
    .await;
    let updated = match updated {
        Ok(Some(row)) => row,
        // Raced with another mark-read between the lookup and here: the record
        // is read either way, and the read that landed is what we report.
        Ok(None) => {
            let reread = sqlx::query(&format!(
                "SELECT {NOTIFICATION_FIELDS} FROM core.notifications WHERE id = $1"
            ))
            .bind(id)
            .fetch_optional(state.pool.as_ref())
            .await;
            match reread {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return error_response(
                        axum::http::StatusCode::NOT_FOUND,
                        format!("no notification {id} for this member"),
                    )
                }
                Err(e) => return internal_error("notification re-read", &e),
            }
        }
        Err(e) => return internal_error("notification mark-read", &e),
    };

    axum::Json(json!({
        "notification": notification_json(&updated),
        "is_read": true,
        "already_read": false,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// The caller's id, or `None` (an unauthenticated request, or an identity that
/// is not a member row).
///
/// A dev-header stub such as `x-dev-user: bea` is not a UUID, and `core.users.id`
/// is one: rather than let a cast fail into a 500, an identity that cannot be a
/// member is treated as no identity at all. The 401 then names the reason.
async fn caller_uuid(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    let identity = state.resolve_identity(headers).await?;
    let id = identity.user_id.trim().to_string();
    uuid::Uuid::parse_str(&id).ok()?;
    Some(id)
}

/// One notification as JSON, with the **read** fact and the **delivery** fact in
/// separate objects. They are never collapsed into one: a reader (or a client
/// badge) that treated them as one would be asserting a delivery nothing made.
fn notification_json(row: &sqlx::postgres::PgRow) -> Value {
    let read_at: Option<String> = row.try_get("read_at").ok().flatten();
    let delivered_at: Option<String> = row.try_get("delivered_at").ok().flatten();
    let state: String = row.try_get("delivery_state").unwrap_or_default();
    json!({
        "id": row.try_get::<i64, _>("id").ok(),
        "source": row.try_get::<String, _>("source").ok(),
        "message_code": row.try_get::<String, _>("message_code").ok(),
        "message_params": row.try_get::<Value, _>("message_params").ok(),
        "locale": row.try_get::<String, _>("locale").ok(),
        "created_at": row.try_get::<String, _>("created_at").ok(),
        // The record's read state …
        "read": { "is_read": read_at.is_some(), "read_at": read_at },
        // … and the transport's delivery state, deliberately not the same object.
        "delivery": {
            "channel": row.try_get::<String, _>("delivery_channel").ok(),
            "state": state,
            "delivered": state == STATE_DELIVERED,
            "delivered_at": delivered_at,
        },
    })
}

/// Percent-decoded query pairs. The same shape `recent_events` parses inline.
fn query_params(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|q| {
            q.split('&')
                .filter_map(|pair| {
                    let mut it = pair.splitn(2, '=');
                    let key = decode(it.next()?);
                    Some((key, decode(it.next().unwrap_or(""))))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `+` is a space and `%XX` is a byte, per the query grammar.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_is_the_one_the_schema_accepts() {
        // The codes the SQL spells literally have to be the codes this module
        // holds, or a CHECK constraint turns a 400 into a 500.
        assert_eq!(DELIVERY_CHANNELS, [CHANNEL_IN_APP]);
        assert_eq!(DELIVERY_STATES, [STATE_RECORDED, STATE_DELIVERED, STATE_FAILED]);
        // The only channel slice 1 can create is the only channel declared.
        assert!(DELIVERY_CHANNELS.contains(&channel_for()));
    }

    #[test]
    fn a_message_code_is_lower_case_and_namespaced_never_a_sentence() {
        for good in [
            "bg_check.expiring",
            "membership.dues.overdue",
            "core.reload_failed",
            "store.order_shipped",
        ] {
            assert!(is_message_code(good), "{good:?} must be a code");
        }
        for bad in [
            "Background check expires",
            "Bg_Check.expiring",
            "bg-check",
            ".expiring",
            "",
            "bg check",
        ] {
            assert!(!is_message_code(bad), "{bad:?} must not be a code");
        }
    }

    #[test]
    fn a_locale_is_a_language_tag_or_und() {
        for good in ["en", "fr", "fr-CA", "en-GB", "und", "zh-Hant-TW"] {
            assert!(is_locale_tag(good), "{good:?} must be a tag");
        }
        for bad in ["", "e", "fr_CA", "fr-", "-CA"] {
            assert!(!is_locale_tag(bad), "{bad:?} must not be a tag");
        }
        // A 5–8 letter primary subtag is grammatical BCP-47 (`5*8ALPHA`), even
        // though no language registers one: the check is the ABNF the table's
        // CHECK enforces, and it must not pretend to be a registry.
        assert!(is_locale_tag("english"));
    }

    #[test]
    fn the_query_decoder_handles_the_grammar() {
        let params = query_params(Some("unread=true&limit=5&note=a+b%2Fc"));
        assert_eq!(params.get("unread").map(String::as_str), Some("true"));
        assert_eq!(params.get("limit").map(String::as_str), Some("5"));
        assert_eq!(params.get("note").map(String::as_str), Some("a b/c"));
        assert!(query_params(None).is_empty());
    }
}
