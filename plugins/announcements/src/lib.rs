//! # adjutant-announcements — troop communication (SPEC §7.14)
//!
//! SPEC §7.14 gives this plugin four responsibilities: announcement creation and
//! distribution, read receipts, categorisation (urgent, informational, event),
//! and push notifications. The first three are here; the fourth is deliberately
//! **not**, and the shape of that decision is the most important thing in this
//! file.
//!
//! ## Delivery is deferred, and the seam is an event
//!
//! No push provider is wired anywhere in Adjutant, and the core has no delivery
//! channel (`docs/design/bg.md` §4 names this as the missing core capability).
//! So this plugin does not pretend: it records the announcement, its category,
//! its scope and every read receipt, publishes `announcement.published`
//! ([`EVENT_PUBLISHED`]) with everything a sender would need, and says
//! `"delivery": "deferred…"` in the responses it returns. A future
//! notification-delivery plugin subscribes to that event; until one exists, the
//! honest statement is "recorded, not delivered" — never "sent".
//!
//! ## Urgent is hard to abuse
//!
//! A category that can be cried wolf is worthless, so `urgent` is not just a
//! label: publishing one needs **two** permissions at the announcement's scope —
//! [`PERM_WRITE`] *and* [`PERM_PUBLISH_URGENT`] (`require_publish_authority`).
//! The emergency authority is an addition to the ordinary one, never a substitute
//! for it, and the gate is re-checked when a *published* announcement is edited
//! **into** the urgent category (the other way to cry wolf). Drafting an urgent
//! announcement without the permission is allowed — a draft reaches nobody — and
//! any manager can retract an urgent announcement; putting a fire out is not the
//! act that needs the sharp permission.
//!
//! ## Two scope rules, on purpose: authority vs audience
//!
//! The scoped permission model (SPEC §9.2, `docs/design/scoped-permissions.md`)
//! answers "may this caller do this here?" with **coverage**: a troop-wide grant
//! covers every Lodge. That is the rule for *authority* — writing, editing,
//! retracting, reading the receipt list — and every such check goes through
//! `ctx.permissions.reach(…, &scope)`.
//!
//! The rule for the **audience** is stricter, because an announcement is
//! addressed to a scope and must not leak across scopes it was not sent to:
//! *the caller must be addressed by the announcement's own scope*. A troop-wide
//! announcement reaches readers who hold [`PERM_READ`] at troop scope; a Lodge 3
//! announcement reaches readers who hold it at Lodge 3. Neither sees the other.
//! The rejected alternative was the coverage rule (`has_in_scope(read,
//! announcement_scope)`), which lets a troop-wide reader see every Lodge's
//! notice — a quiet widening of the audience that the scope on the row exists to
//! prevent. Oversight is not lost: a `manage` grant at a **covering** scope sees
//! everything below it, including drafts and retracted notices, which is exactly
//! the "I need to see all of it" authority.
//!
//! A caller also always sees their own drafts (`created_by = caller`), so a
//! scribe who may write in a scope is not blind to what they wrote.
//!
//! The audience is resolved in **one** query: the caller's grants are asked of
//! the core's permission service in a single batched lookup
//! (`PermissionService::scopes_for`) for this plugin's read/manage permissions,
//! so a caller with twelve lodge grants costs one round trip, not twelve
//! (`resolve_audience`). The resulting predicate is one function
//! (`Audience::sql`) spliced into every visibility query, so the list, the
//! detail, the receipt write and the unread count cannot drift apart.
//!
//! ## Read receipts are idempotent, and the unread count is not a scan
//!
//! `POST …/{id}/read` is a single statement:
//!
//! ```sql
//! WITH inserted AS (
//!   INSERT INTO receipts (announcement_id, member_id, read_via)
//!   VALUES ($1, $2, $3) ON CONFLICT (announcement_id, member_id) DO NOTHING
//!   RETURNING …, true AS created
//! )
//! SELECT … FROM inserted UNION ALL SELECT …, false AS created FROM receipts …
//! ```
//!
//! `DO NOTHING` means a second mark-read writes **no** row (no upsert churn, no
//! second receipt) and the second branch of the union reports the receipt that
//! already existed — so marking read twice is a 200 that changes nothing, and
//! `announcement.receipt` is emitted only for the receipt that was actually
//! created. The unique key `(announcement_id, member_id)` is the invariant;
//! the handler is only the polite path to it.
//!
//! Unread counts never load a member's receipts. One query computes the badge
//! from the indexed side:
//!
//! ```sql
//! SELECT COUNT(*) AS visible,
//!        COUNT(*) FILTER (WHERE r.id IS NULL) AS unread, …
//! FROM announcements a
//! LEFT JOIN receipts r ON r.announcement_id = a.id AND r.member_id = $6
//! WHERE a.status = 'published' AND (a.expires_at IS NULL OR a.expires_at > now())
//!   AND <audience predicate>
//! ```
//!
//! The join is an index probe into the unique key for each candidate
//! announcement, and the candidate set is the caller's *visible published*
//! announcements — so the cost is "how many announcements could this member
//! read", not "how many receipts does this troop have". The same query yields
//! the per-category breakdown with `FILTER`, so the badge and its urgent dot cost
//! one round trip together.
//!
//! ## Schema
//!
//! `announcements` (title, body, category, scope, status, expiry, retraction) and
//! `receipts` (one row per member per announcement). Vocabulary that reaches a
//! reader is constrained in the database — `category`, `scope_type`, `status` —
//! because a category the troop does not know is a category nobody reads.
//! `related_event_id` is calendar's event id when the announcement is *about*
//! one; it is an opaque link, not a cross-schema read (a plugin owns its schema
//! only).
//!
//! ## What is deliberately not here
//!
//! * **Push/email delivery** — see above; the event is the seam.
//! * **A roster** — "who has *not* read this" needs membership's roster, and a
//!   plugin role cannot read another schema. The client intersects the receipt
//!   list with the roster it already has; that is why `/receipts` returns who
//!   read rather than a completion percentage.
//! * **Patrol audiences** — SPEC §7.14 says troop or Lodge; a patrol notice is a
//!   Lodge notice with the patrol in the body. Adding a third audience would need
//!   a third addressee rule and nobody has asked for one.
//! * **Recording somebody else's read receipt** — a receipt means "this member
//!   opened it". Writing one on their behalf would make the number a fiction;
//!   calendar has a phoned-in RSVP, an announcement has no equivalent.
//! * **Deleting a member's receipts when they leave** — the record of who was
//!   told is deliberately kept (SPEC §2's audit posture).

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Vocabulary — stable codes, never display strings
// ---------------------------------------------------------------------------

/// The category that interrupts. Publishing one needs [`PERM_PUBLISH_URGENT`].
pub const CATEGORY_URGENT: &str = "urgent";
/// The ordinary notice.
pub const CATEGORY_INFORMATIONAL: &str = "informational";
/// A notice about something on the calendar.
pub const CATEGORY_EVENT: &str = "event";

/// The troop's own three categories (SPEC §7.14), in the order a client
/// should show them: the one that interrupts first.
pub const CATEGORIES: [&str; 3] = [CATEGORY_URGENT, CATEGORY_INFORMATIONAL, CATEGORY_EVENT];

/// What a category means and what it costs to use — the reference data behind
/// `GET /api/announcements/categories`.
pub const CATEGORY_MEANINGS: [(&str, &str, &str); 3] = [
    (
        CATEGORY_URGENT,
        "Urgent",
        "Something the troop must know now (a cancelled camp, a road closure). \
         Publishing one needs announcements:write AND announcements:publish_urgent at the \
         announcement's scope.",
    ),
    (
        CATEGORY_INFORMATIONAL,
        "Informational",
        "The ordinary notice: a schedule, a reminder, a result. Needs announcements:write.",
    ),
    (
        CATEGORY_EVENT,
        "Event",
        "A notice about something on the calendar (a meeting, a service day). Needs \
         announcements:write; related_event_id links it to the event.",
    ),
];

/// A draft nobody but its author (and managers) can see.
pub const STATUS_DRAFT: &str = "draft";
/// Sent. Visible to the scope it was addressed to.
pub const STATUS_PUBLISHED: &str = "published";
/// Withdrawn after being sent: out of the inbox and the badge, but the record
/// that it was sent stays visible to the audience it was sent to.
pub const STATUS_RETRACTED: &str = "retracted";

/// The three states, in lifecycle order.
pub const STATUSES: [&str; 3] = [STATUS_DRAFT, STATUS_PUBLISHED, STATUS_RETRACTED];

/// Troop-wide audience.
pub const SCOPE_TROOP: &str = "troop";
/// One Lodge's audience.
pub const SCOPE_LODGE: &str = "lodge";

/// The two audiences this plugin addresses (SPEC §7.14: "troop-wide or a Lodge").
pub const SCOPE_TYPES: [&str; 2] = [SCOPE_TROOP, SCOPE_LODGE];

// --- permissions -----------------------------------------------------------

/// See the announcements addressed to a scope you hold.
pub const PERM_READ: &str = "announcements:read";
/// Draft and publish an announcement at a scope you hold.
pub const PERM_WRITE: &str = "announcements:write";
/// Publish an *urgent* announcement — the category that interrupts.
pub const PERM_PUBLISH_URGENT: &str = "announcements:publish_urgent";
/// Edit, retract or delete an announcement, and read who has read it.
pub const PERM_MANAGE: &str = "announcements:manage";

// --- events ----------------------------------------------------------------

/// New announcement recorded — published immediately or left a draft.
pub const EVENT_CREATED: &str = "announcement.created";
/// **The delivery seam.** Emitted once, when an announcement becomes visible to
/// its audience: a future notification-delivery plugin subscribes to this
/// (SPEC §7.14's "push notifications", which no provider is wired for).
pub const EVENT_PUBLISHED: &str = "announcement.published";
/// Title, body, category, scope or expiry changed after publishing.
pub const EVENT_UPDATED: &str = "announcement.updated";
/// Withdrawn after being sent.
pub const EVENT_RETRACTED: &str = "announcement.retracted";
/// A receipt was *created* — a repeat mark-read publishes nothing.
pub const EVENT_RECEIPT: &str = "announcement.receipt";
/// A member cleared their own receipt (mark-unread).
pub const EVENT_UNREAD: &str = "announcement.unread";
/// Deleted outright (the receipts go with it).
pub const EVENT_DELETED: &str = "announcement.deleted";

// --- limits and constants --------------------------------------------------

/// Longest accepted title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest accepted body, in characters.
pub const MAX_BODY_CHARS: usize = 20_000;
/// Longest accepted `via` tag on a receipt.
pub const MAX_VIA_CHARS: usize = 32;
/// How much of the body travels in the `announcement.published` payload: enough
/// for a notification to be useful without a second fetch.
pub const EVENT_PREVIEW_CHARS: usize = 160;
/// Default and maximum page size of the inbox listing.
pub const DEFAULT_LIMIT: i64 = 50;
/// The listing's ceiling.
pub const MAX_LIMIT: i64 = 200;

/// The sentence every write response carries, so a client (and a future
/// delivery plugin) can never mistake "recorded" for "delivered".
pub const DELIVERY_DEFERRED: &str = "deferred: no push provider is wired — \
     announcement.published carries what a delivery plugin needs";

/// The permissions resolved into an [`Audience`] — the read/addressee side of
/// the model.
const AUDIENCE_PERMISSIONS: [&str; 2] = [PERM_READ, PERM_MANAGE];

// ---------------------------------------------------------------------------
// Pure helpers — validation and vocabulary (no database, no context)
// ---------------------------------------------------------------------------

/// A title that is present and short enough to be a title.
pub fn require_title(value: &str) -> Result<String, SdkError> {
    let title = value.trim();
    if title.is_empty() {
        return Err(SdkError::BadRequest("title is required".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(SdkError::BadRequest(format!(
            "title must be at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title.to_string())
}

/// The announcement's body: trimmed, length-checked, allowed to be empty (a
/// title is a valid announcement — "Camp is cancelled").
pub fn require_body(value: Option<&str>) -> Result<String, SdkError> {
    let body = value.unwrap_or_default().trim();
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(SdkError::BadRequest(format!(
            "body must be at most {MAX_BODY_CHARS} characters"
        )));
    }
    Ok(body.to_string())
}

/// Normalise a category to the troop's vocabulary, defaulting to the ordinary
/// notice. An unknown category is the caller's mistake, not a new category.
pub fn normalize_category(value: Option<&str>) -> Result<String, SdkError> {
    let category = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(CATEGORY_INFORMATIONAL)
        .to_ascii_lowercase();
    if !CATEGORIES.contains(&category.as_str()) {
        return Err(SdkError::BadRequest(format!(
            "category must be one of {}",
            CATEGORIES.join(", ")
        )));
    }
    Ok(category)
}

/// Normalise the audience: a Lodge needs its id, and a troop-wide announcement
/// must not carry one (the database refuses both shapes too).
pub fn normalize_scope(
    scope_type: Option<&str>,
    scope_id: Option<&str>,
) -> Result<(String, Option<String>), SdkError> {
    let scope_type = scope_type
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(SCOPE_TROOP)
        .to_ascii_lowercase();
    if !SCOPE_TYPES.contains(&scope_type.as_str()) {
        return Err(SdkError::BadRequest(format!(
            "scope_type must be one of {}",
            SCOPE_TYPES.join(", ")
        )));
    }
    let scope_id = scope_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    match (scope_type.as_str(), scope_id) {
        (SCOPE_LODGE, None) => Err(SdkError::BadRequest(
            "a Lodge announcement needs scope_id (the Lodge it is sent to)".into(),
        )),
        (SCOPE_TROOP, Some(_)) => Err(SdkError::BadRequest(
            "a troop-wide announcement carries no scope_id (scope_id is for Lodge announcements)"
                .into(),
        )),
        (_, scope_id) => Ok((scope_type, scope_id)),
    }
}

/// The scope an announcement's authority — and its audience — lives at.
///
/// A Lodge announcement whose `scope_id` is missing is **troop-wide**, not
/// "lodge with no id": the database refuses that shape on write, and a read that
/// meets one must not silently widen or narrow the check.
pub fn announcement_scope(scope_type: &str, scope_id: Option<&str>) -> Scope {
    match (scope_type, scope_id) {
        (SCOPE_LODGE, Some(id)) if !id.trim().is_empty() => Scope::lodge(id),
        _ => Scope::troop(),
    }
}

/// Parse an expiry: RFC 3339 (`2026-10-01T12:00:00Z`), a date
/// (`2026-10-01`, which expires at midnight UTC — the announcement is stale from
/// that day onward), or the form PostgreSQL renders a `timestamptz::text` as
/// (`2026-10-01 00:00:00+00`), which is what a stored row reads back through
/// this same function. Blank (or absent) means no expiry.
pub fn parse_expiry(value: Option<&str>) -> Result<Option<DateTime<Utc>>, SdkError> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(timestamp.with_timezone(&Utc)));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"] {
        if let Ok(timestamp) = DateTime::parse_from_str(raw, format) {
            return Ok(Some(timestamp.with_timezone(&Utc)));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(Some(
            date.and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc(),
        ));
    }
    Err(SdkError::BadRequest(format!(
        "expires_at {raw:?} is not an RFC 3339 timestamp (2026-10-01T12:00:00Z) or a date \
         (2026-10-01)"
    )))
}

/// The `via` a receipt records: a short tag naming the surface that opened it.
pub fn normalize_via(value: Option<&str>) -> Result<String, SdkError> {
    let via = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("api")
        .to_ascii_lowercase();
    if via.chars().count() > MAX_VIA_CHARS {
        return Err(SdkError::BadRequest(format!(
            "via must be at most {MAX_VIA_CHARS} characters"
        )));
    }
    Ok(via)
}

/// The first `max` characters of a body, whitespace collapsed — what travels in
/// the published event so a notification can be useful without a second fetch.
pub fn preview(body: &str, max: usize) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

/// Push a bind parameter and return its placeholder number.
///
/// The number and the bind order are the same push, so a query cannot name a
/// placeholder that holds the wrong value — the trap every dynamic `SET`/filter
/// list has.
fn bind(params: &mut Vec<SqlValue>, value: SqlValue) -> usize {
    params.push(value);
    params.len()
}

/// `column = $n`, for a dynamic `SET` list.
fn set_clause(sets: &mut Vec<String>, params: &mut Vec<SqlValue>, column: &str, value: SqlValue) {
    let placeholder = bind(params, value);
    sets.push(format!("{column} = ${placeholder}"));
}

/// A `SET` whose right-hand side is an expression over one bound value
/// (`{placeholder}` is replaced by that value's number), e.g. a `timestamptz`
/// cast.
fn set_expression(
    sets: &mut Vec<String>,
    params: &mut Vec<SqlValue>,
    expression: &str,
    value: SqlValue,
) {
    let placeholder = bind(params, value);
    sets.push(expression.replace("{placeholder}", &format!("${placeholder}")));
}

/// The caller's member id, when the request carries one (the core's gate has
/// already refused unauthenticated calls, so this is belt-and-braces).
fn caller_of(req: &PluginRequest) -> Option<&str> {
    req.identity
        .as_ref()
        .map(|identity| identity.user_id.as_str())
        .filter(|user_id| !user_id.trim().is_empty())
}

/// The caller's member id, or 401 — for the routes that *are* about the caller
/// (reading an announcement, clearing a receipt).
fn require_caller(req: &PluginRequest) -> Result<String, SdkError> {
    caller_of(req)
        .map(str::to_string)
        .ok_or_else(|| {
            SdkError::Unauthorized(
                "this needs an authenticated member (a read receipt is per member)".into(),
            )
        })
}

// ---------------------------------------------------------------------------
// Audience — who is addressed by what (the read side of the scope model)
// ---------------------------------------------------------------------------

/// The scopes one caller is *addressed* by, per permission.
///
/// Deliberately not the coverage model: this records the scope each of the
/// caller's grants names, so a troop-wide announcement is read by troop-addressed
/// callers and a Lodge's notice by that Lodge's — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Audience {
    /// The caller's member id (bound into every visibility query).
    caller: String,
    /// Holds [`PERM_READ`] from a troop-scoped grant.
    troop_read: bool,
    /// Lodges the caller holds [`PERM_READ`] in.
    read_lodges: Vec<String>,
    /// Holds [`PERM_MANAGE`] from a troop-scoped grant: overseer of everything.
    troop_manage: bool,
    /// Lodges the caller holds [`PERM_MANAGE`] in (drafts and retractions
    /// included).
    manage_lodges: Vec<String>,
}

impl Audience {
    /// The visibility predicate. Every read path splices this in, so the inbox,
    /// the detail, the receipt write and the unread count cannot disagree about
    /// who may see what.
    ///
    /// `a` is the announcement alias.
    fn sql(&self) -> String {
        "((a.status IN ('published', 'retracted') \
           AND ((a.scope_type = 'troop' AND $1::bool) \
                OR (a.scope_type = 'lodge' AND a.scope_id = ANY($2)))) \
          OR a.created_by = $3 \
          OR $4::bool \
          OR (a.scope_type = 'lodge' AND a.scope_id = ANY($5)))"
            .to_string()
    }

    /// The predicate's parameters, in placeholder order: troop-addressed flag,
    /// addressed lodges, the caller, the troop-manage flag, managed lodges.
    fn params(&self) -> Vec<SqlValue> {
        vec![
            SqlValue::Bool(self.troop_read),
            SqlValue::TextArray(self.read_lodges.clone()),
            SqlValue::Text(self.caller.clone()),
            SqlValue::Bool(self.troop_manage),
            SqlValue::TextArray(self.manage_lodges.clone()),
        ]
    }

    /// Does this reader see every scope? (A troop-covering `manage` grant.)
    fn oversees(&self) -> bool {
        self.troop_manage
    }

    /// A compact statement of what the caller is addressed by, for responses.
    fn summary(&self) -> Value {
        json!({
            "troop": self.troop_read,
            "lodges": self.read_lodges,
            "oversees": self.oversees(),
            "manage_lodges": self.manage_lodges,
        })
    }
}

/// Resolve the caller's addressing in **one** query: the permissions they hold,
/// at every scope they hold them, asked of the core's permission service
/// ([`PermissionService::scopes_for`]).
///
/// One round trip regardless of how many grants the caller has, and no query at
/// all for an anonymous caller or one with no grants (the core's gate has
/// already refused those, and a query that cannot match anything is not worth a
/// round trip).
///
/// This **asks** rather than reads: a plugin role has no privileges on
/// `core.role_permissions`, and does not need any, because the service answers
/// from the grants it was handed — on the core's connection, and only ever as a
/// subset of what the caller already holds.
async fn resolve_audience(
    c: &PluginContext,
    identity: Option<&Identity>,
) -> Result<Audience, SdkError> {
    let mut audience = Audience::default();
    let Some(identity) = identity else {
        return Ok(audience);
    };
    audience.caller = identity.user_id.clone();
    if identity.grants.is_empty() {
        return Ok(audience);
    }

    let held = c
        .permissions
        .scopes_for(Some(identity), &AUDIENCE_PERMISSIONS)
        .await?;
    for entry in held {
        match (entry.permission.as_str(), entry.scope.scope_type) {
            (PERM_READ, ScopeType::Troop) => audience.troop_read = true,
            (PERM_READ, ScopeType::Lodge) => {
                if let Some(lodge) = entry.scope.scope_id {
                    audience.read_lodges.push(lodge);
                }
            }
            (PERM_MANAGE, ScopeType::Troop) => audience.troop_manage = true,
            (PERM_MANAGE, ScopeType::Lodge) => {
                if let Some(lodge) = entry.scope.scope_id {
                    audience.manage_lodges.push(lodge);
                }
            }
            _ => {}
        }
    }
    audience.read_lodges.sort();
    audience.read_lodges.dedup();
    audience.manage_lodges.sort();
    audience.manage_lodges.dedup();
    Ok(audience)
}

// ---------------------------------------------------------------------------
// Row shape and reads
// ---------------------------------------------------------------------------

/// Every column the API states, with the conversions the host needs
/// (`timestamptz` comes back as `::text`, absolute, with offset).
const ANNOUNCEMENT_FIELDS: &str = r#"
    a.id, a.title, a.body, a.category, a.scope_type, a.scope_id, a.status,
    a.related_event_id,
    a.published_at::text AS published_at, a.published_by,
    a.expires_at::text AS expires_at,
    a.retracted_at::text AS retracted_at, a.retracted_by,
    a.created_by, a.created_at::text AS created_at, a.updated_at::text AS updated_at
"#;

/// Fetch one announcement by id, with no visibility filter — for the management
/// routes, which gate on the object's scope themselves (so they can tell 404
/// from 403).
async fn fetch_announcement(c: &PluginContext, id: i64) -> Result<Option<Value>, SdkError> {
    c.db.query_one(
        format!(
            "SELECT {ANNOUNCEMENT_FIELDS} FROM {} a WHERE a.id = $1",
            c.db.table("announcements")
        ),
        vec![SqlValue::Int(id)],
    )
    .await
}

/// Fetch one announcement **only if the caller is addressed by it**, together
/// with this caller's receipt and the receipt count.
///
/// The filter is the shared [`Audience::sql`] predicate, so "not visible" and
/// "does not exist" are the same answer here (which is the point).
async fn fetch_visible(
    c: &PluginContext,
    audience: &Audience,
    id: i64,
) -> Result<Option<Value>, SdkError> {
    let receipts = c.db.table("receipts");
    let mut params = audience.params();
    let member = bind(&mut params, SqlValue::Text(audience.caller.clone()));
    let target = bind(&mut params, SqlValue::Int(id));
    c.db.query_one(
        format!(
            "SELECT {ANNOUNCEMENT_FIELDS}, \
                    r.id AS receipt_id, r.read_via AS receipt_via, \
                    r.read_at::text AS read_at, (r.id IS NOT NULL) AS is_read, \
                    (SELECT COUNT(*) FROM {receipts} rr WHERE rr.announcement_id = a.id) \
                        AS read_count \
             FROM {announcements} a \
             LEFT JOIN {receipts} r ON r.announcement_id = a.id AND r.member_id = ${member} \
             WHERE {audience} AND a.id = ${target}",
            announcements = c.db.table("announcements"),
            audience = audience.sql()
        ),
        params,
    )
    .await
}

/// The caller's badge: how many visible, how many unread, and by category.
///
/// One query, indexed side in, receipts never loaded — see the module docs.
async fn unread_rows(c: &PluginContext, audience: &Audience) -> Result<Option<Value>, SdkError> {
    let mut params = audience.params();
    let member = bind(&mut params, SqlValue::Text(audience.caller.clone()));
    c.db.query_one(
        format!(
            "SELECT COUNT(*) AS visible, \
                    COUNT(*) FILTER (WHERE r.id IS NULL) AS unread, \
                    COUNT(*) FILTER (WHERE r.id IS NULL AND a.category = 'urgent') \
                        AS unread_urgent, \
                    COUNT(*) FILTER (WHERE r.id IS NULL AND a.category = 'informational') \
                        AS unread_informational, \
                    COUNT(*) FILTER (WHERE r.id IS NULL AND a.category = 'event') \
                        AS unread_event, \
                    COUNT(*) FILTER (WHERE r.id IS NOT NULL) AS read \
             FROM {announcements} a \
             LEFT JOIN {receipts} r ON r.announcement_id = a.id AND r.member_id = ${member} \
             WHERE a.status = '{published}' \
               AND (a.expires_at IS NULL OR a.expires_at > now()) \
               AND {audience}",
            announcements = c.db.table("announcements"),
            receipts = c.db.table("receipts"),
            published = STATUS_PUBLISHED,
            audience = audience.sql()
        ),
        params,
    )
    .await
}

/// The unread report a client renders as a badge.
fn unread_report(audience: &Audience, row: Option<&Value>) -> Value {
    let count = |key: &str| {
        row.and_then(|r| r.get(key))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let mut by_category = Map::new();
    by_category.insert(CATEGORY_URGENT.to_string(), json!(count("unread_urgent")));
    by_category.insert(
        CATEGORY_INFORMATIONAL.to_string(),
        json!(count("unread_informational")),
    );
    by_category.insert(CATEGORY_EVENT.to_string(), json!(count("unread_event")));
    json!({
        "member_id": audience.caller,
        "visible": count("visible"),
        "unread": count("unread"),
        "read": count("read"),
        "urgent_unread": count("unread_urgent"),
        "has_urgent": count("unread_urgent") > 0,
        "unread_by_category": Value::Object(by_category),
        "addressed": audience.summary(),
    })
}

/// The `announcement.published` payload — the whole contract a delivery plugin
/// needs, and nothing it does not.
fn published_payload(row: &Value) -> Value {
    json!({
        "announcement_id": row["id"],
        "title": row["title"],
        "preview": preview(row["body"].as_str().unwrap_or_default(), EVENT_PREVIEW_CHARS),
        "category": row["category"],
        "urgent": row["category"].as_str() == Some(CATEGORY_URGENT),
        "scope_type": row["scope_type"],
        "scope_id": row["scope_id"],
        "published_at": row["published_at"],
        "published_by": row["published_by"],
        "expires_at": row["expires_at"],
        "related_event_id": row["related_event_id"],
    })
}

/// The receipt as the API states it (the `created` flag is reported beside it,
/// not inside it — it is about this call, not about the receipt).
fn receipt_of(row: &Value) -> Value {
    json!({
        "id": row["id"],
        "announcement_id": row["announcement_id"],
        "member_id": row["member_id"],
        "read_via": row["read_via"],
        "read_at": row["read_at"],
    })
}

/// Publishing an *urgent* announcement needs the sharper permission as well.
///
/// `announcements:write` says "you may write here"; `announcements:publish_urgent`
/// says "you may interrupt". Requiring both means the emergency authority is an
/// addition, never a substitute — and a category that can be cried wolf is
/// worthless.
async fn require_publish_authority(
    c: &PluginContext,
    identity: Option<&Identity>,
    category: &str,
    scope: &Scope,
) -> Result<(), SdkError> {
    if category == CATEGORY_URGENT {
        c.permissions.reach(identity, PERM_PUBLISH_URGENT, scope).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnnouncementBody {
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope_type: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    /// Send it now instead of leaving it a draft.
    #[serde(default)]
    publish: Option<bool>,
    #[serde(default)]
    expires_at: Option<String>,
    /// Calendar's event id, when this announcement is about an event.
    #[serde(default)]
    related_event_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AnnouncementEditBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope_type: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    related_event_id: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct ReceiptBody {
    /// The surface that opened it (`api`, `app`, `digest`).
    #[serde(default)]
    via: Option<String>,
}

/// A body that is absent is a valid `{}` for the receipt routes: marking read
/// is one tap, and requiring a JSON object for it would be ceremony.
fn optional_body<T: for<'de> Deserialize<'de> + Default>(req: &PluginRequest) -> Result<T, SdkError> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        Ok(T::default())
    } else {
        req.json()
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct AnnouncementsPlugin {
    ctx: OnceLock<PluginContext>,
}

impl AnnouncementsPlugin {
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
        }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()")
    }
}

impl Default for AnnouncementsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Wire one handler over a cloned context.
///
/// Every handler below has the shape `(&PluginContext, PluginRequest)`, so this
/// keeps `routes()` a manifest of paths and permissions rather than a wall of
/// captured closures.
fn route_over<F, Fut>(ctx: PluginContext, f: F) -> adjutant_sdk::RouteHandler
where
    F: Fn(PluginContext, PluginRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<PluginResponse, SdkError>> + Send + 'static,
{
    route_handler(move |req| f(ctx.clone(), req))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/announcements/announcement` — write an announcement, and send it
/// at once when `publish` is true.
///
/// Object-shaped: the caller needs [`PERM_WRITE`] at *some* scope and the
/// handler checks the announcement's own scope (a troop grant covers every
/// Lodge; a Lodge grant covers that Lodge's announcements only).
async fn create_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let body: AnnouncementBody = req.json()?;

    // Everything that can be validated without a database first: a bad request
    // must not cost a query, let alone a permission check.
    let title = require_title(&body.title)?;
    let text = require_body(body.body.as_deref())?;
    let category = normalize_category(body.category.as_deref())?;
    let (scope_type, scope_id) =
        normalize_scope(body.scope_type.as_deref(), body.scope_id.as_deref())?;
    let expires_at = parse_expiry(body.expires_at.as_deref())?;
    let related_event_id = match body.related_event_id {
        Some(id) if id <= 0 => {
            return Err(SdkError::BadRequest(
                "related_event_id must be a positive event id".into(),
            ))
        }
        other => other,
    };
    let publish = body.publish.unwrap_or(false);
    let scope = announcement_scope(&scope_type, scope_id.as_deref());
    // An announcement that expires before it is sent reaches nobody: that is a
    // request-shape mistake, so it is refused before it costs a permission check.
    if publish && expires_at.is_some_and(|at| at <= Utc::now()) {
        return Err(SdkError::BadRequest(
            "expires_at is already in the past — an announcement that expires before it is sent \
             reaches nobody"
                .into(),
        ));
    }

    // Authority: write here.
    c.permissions
        .reach(req.identity.as_ref(), PERM_WRITE, &scope)
        .await?;
    // Sending an urgent one is a further act still.
    if publish {
        require_publish_authority(&c, req.identity.as_ref(), &category, &scope).await?;
    }
    let caller = require_caller(&req)?;

    let mut params: Vec<SqlValue> = Vec::new();
    let title_p = bind(&mut params, SqlValue::Text(title.clone()));
    let body_p = bind(&mut params, SqlValue::Text(text));
    let category_p = bind(&mut params, SqlValue::Text(category.clone()));
    let scope_type_p = bind(&mut params, SqlValue::Text(scope_type.clone()));
    let scope_id_p = bind(&mut params, scope_id.clone().into());
    let status_p = bind(
        &mut params,
        SqlValue::Text(if publish { STATUS_PUBLISHED } else { STATUS_DRAFT }.to_string()),
    );
    let event_p = bind(
        &mut params,
        related_event_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
    );
    let publish_p = bind(&mut params, SqlValue::Bool(publish));
    let publisher_p = bind(&mut params, SqlValue::Text(caller.clone()));
    let expires_p = bind(
        &mut params,
        expires_at
            .map(|at| SqlValue::Text(at.to_rfc3339()))
            .unwrap_or(SqlValue::Null),
    );
    let creator_p = bind(&mut params, SqlValue::Text(caller.clone()));

    let row = c
        .db
        .query_one(
            format!(
                "INSERT INTO {announcements} AS a \
                   (title, body, category, scope_type, scope_id, status, related_event_id, \
                    published_at, published_by, expires_at, created_by) \
                 VALUES (${title_p}, ${body_p}, ${category_p}, ${scope_type_p}, ${scope_id_p}, \
                         ${status_p}, ${event_p}, \
                         CASE WHEN ${publish_p}::bool THEN now() END, \
                         CASE WHEN ${publish_p}::bool THEN ${publisher_p}::text END, \
                         ${expires_p}::timestamptz, ${creator_p}) \
                 RETURNING {ANNOUNCEMENT_FIELDS}",
                announcements = c.db.table("announcements")
            ),
            params,
        )
        .await?
        .ok_or_else(|| SdkError::Internal("insert returned no row".into()))?;
    let id = row["id"].as_i64().unwrap_or_default();

    c.audit
        .log(
            req.identity.as_ref(),
            "announcement.create",
            "announcement",
            &id.to_string(),
            json!({
                "title": title,
                "category": category,
                "scope_type": scope_type,
                "scope_id": scope_id,
                "published": publish,
                "expires_at": row["expires_at"],
            }),
        )
        .await?;
    c.events
        .publish(
            EVENT_CREATED,
            json!({
                "announcement_id": id,
                "title": row["title"],
                "category": row["category"],
                "scope_type": row["scope_type"],
                "scope_id": row["scope_id"],
                "published": publish,
                "created_by": caller,
            }),
        )
        .await?;
    if publish {
        // The delivery seam, emitted exactly once, at the moment the
        // announcement becomes visible to its audience.
        c.events.publish(EVENT_PUBLISHED, published_payload(&row)).await?;
    }

    PluginResponse::created(
        &format!("/api/announcements/announcement/{id}"),
        &json!({
            "announcement": row,
            "published": publish,
            "delivery": DELIVERY_DEFERRED,
            "next": if publish {
                "a reader marks it read at POST /api/announcements/announcement/{id}/read"
            } else {
                "send it at POST /api/announcements/announcement/{id}/publish"
            },
        }),
    )
}

/// `POST /api/announcements/announcement/{id}/publish` — send a draft.
///
/// The author may send what they wrote with [`PERM_WRITE`]; anybody else needs
/// [`PERM_MANAGE`] at the scope. The `UPDATE … WHERE status = 'draft'` is what
/// makes a double publish impossible: the second attempt changes no row, and so
/// `announcement.published` cannot fire twice for the same announcement.
async fn publish_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(row) = fetch_announcement(&c, id).await? else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };
    let scope = announcement_scope(
        row["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        row["scope_id"].as_str(),
    );
    let caller = require_caller(&req)?;

    let status = row["status"].as_str().unwrap_or(STATUS_DRAFT);
    if status != STATUS_DRAFT {
        return Err(SdkError::Conflict(format!(
            "announcement {id} is {status}, not a draft — publishing it again would tell the \
             troop the same thing twice"
        )));
    }
    // Authority: sending your own draft is writing; sending somebody else's is
    // managing.
    let permission = if row["created_by"].as_str() == Some(caller.as_str()) {
        PERM_WRITE
    } else {
        PERM_MANAGE
    };
    c.permissions.reach(req.identity.as_ref(), permission, &scope).await?;
    let category = row["category"].as_str().unwrap_or(CATEGORY_INFORMATIONAL);
    require_publish_authority(&c, req.identity.as_ref(), category, &scope).await?;
    if let Some(at) = parse_expiry(row["expires_at"].as_str())? {
        if at <= Utc::now() {
            return Err(SdkError::Conflict(
                "this announcement's expiry has passed — move it forward (PATCH) before sending \
                 it, or nobody will ever see it"
                    .into(),
            ));
        }
    }

    let rows = c
        .db
        .query(
            format!(
                "UPDATE {announcements} AS a \
                    SET status = 'published', published_at = now(), published_by = $2, \
                        updated_at = now() \
                  WHERE a.id = $1 AND a.status = 'draft' \
                 RETURNING {ANNOUNCEMENT_FIELDS}",
                announcements = c.db.table("announcements")
            ),
            vec![SqlValue::Int(id), SqlValue::Text(caller.clone())],
        )
        .await?;
    let Some(sent) = rows.first().cloned() else {
        // The draft went out between the read and the write.
        return Err(SdkError::Conflict(format!(
            "announcement {id} was published by somebody else a moment ago"
        )));
    };

    c.audit
        .log(
            req.identity.as_ref(),
            "announcement.publish",
            "announcement",
            &id.to_string(),
            json!({
                "title": sent["title"],
                "category": sent["category"],
                "scope_type": sent["scope_type"],
                "scope_id": sent["scope_id"],
                "by": caller,
            }),
        )
        .await?;
    c.events.publish(EVENT_PUBLISHED, published_payload(&sent)).await?;

    PluginResponse::json(
        200,
        &json!({
            "announcement": sent,
            "delivery": DELIVERY_DEFERRED,
        }),
    )
}

/// `GET /api/announcements/announcements` — the inbox.
async fn list_announcements(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    // Filters are validated before the audience is resolved: a typo in a query
    // parameter must not cost a round trip (and must not be silently ignored).
    let status = match req.query_param("status") {
        None => Some(STATUS_PUBLISHED.to_string()),
        Some("all") => None,
        Some(other) => {
            let value = other.trim().to_ascii_lowercase();
            if !STATUSES.contains(&value.as_str()) {
                return Err(SdkError::BadRequest(format!(
                    "status must be one of {}, or 'all'",
                    STATUSES.join(", ")
                )));
            }
            Some(value)
        }
    };
    let category = match req.query_param("category") {
        None => None,
        Some(other) => Some(normalize_category(Some(other))?),
    };
    let scope_type = match req.query_param("scope_type") {
        None => None,
        Some(other) => {
            let value = other.trim().to_ascii_lowercase();
            if !SCOPE_TYPES.contains(&value.as_str()) {
                return Err(SdkError::BadRequest(format!(
                    "scope_type must be one of {}",
                    SCOPE_TYPES.join(", ")
                )));
            }
            Some(value)
        }
    };
    let scope_id = req
        .query_param("scope_id")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let include_expired = req.query_bool("include_expired");
    let unread_only = req.query_bool("unread");
    let limit = req.query_int("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = req.query_int("offset").unwrap_or(0).max(0);

    let audience = resolve_audience(&c, req.identity.as_ref()).await?;
    let receipts = c.db.table("receipts");

    let mut params = audience.params();
    let member = bind(&mut params, SqlValue::Text(audience.caller.clone()));
    let status_p = bind(&mut params, status.clone().into());
    let category_p = bind(&mut params, category.clone().into());
    let scope_type_p = bind(&mut params, scope_type.clone().into());
    let scope_id_p = bind(&mut params, scope_id.clone().into());
    let include_expired_p = bind(&mut params, SqlValue::Bool(include_expired));
    let unread_p = bind(&mut params, SqlValue::Bool(unread_only));
    let limit_p = bind(&mut params, SqlValue::Int(limit));
    let offset_p = bind(&mut params, SqlValue::Int(offset));

    let rows = c
        .db
        .query(
            format!(
                "SELECT {ANNOUNCEMENT_FIELDS}, r.id AS receipt_id, r.read_via AS receipt_via, \
                        r.read_at::text AS read_at, (r.id IS NOT NULL) AS is_read \
                 FROM {announcements} a \
                 LEFT JOIN {receipts} r ON r.announcement_id = a.id AND r.member_id = ${member} \
                 WHERE {audience} \
                   AND (${status_p}::text IS NULL OR a.status = ${status_p}) \
                   AND (${category_p}::text IS NULL OR a.category = ${category_p}) \
                   AND (${scope_type_p}::text IS NULL OR a.scope_type = ${scope_type_p}) \
                   AND (${scope_id_p}::text IS NULL OR a.scope_id = ${scope_id_p}) \
                   AND (${include_expired_p}::bool OR a.expires_at IS NULL OR a.expires_at > now()) \
                   AND (${unread_p}::bool = false OR r.id IS NULL) \
                 ORDER BY (a.category = 'urgent') DESC, a.published_at DESC NULLS LAST, a.id DESC \
                 LIMIT ${limit_p} OFFSET ${offset_p}",
                announcements = c.db.table("announcements"),
                audience = audience.sql()
            ),
            params,
        )
        .await?;

    // The badge is the caller's whole unread count, not the filtered page's:
    // a filter must not change what the badge says.
    let badge = unread_report(&audience, unread_rows(&c, &audience).await?.as_ref());

    PluginResponse::json(
        200,
        &json!({
            "announcements": rows,
            "count": rows.len(),
            "limit": limit,
            "offset": offset,
            "status": status.unwrap_or_else(|| "all".to_string()),
            "filters": {
                "category": category,
                "scope_type": scope_type,
                "scope_id": scope_id,
                "include_expired": include_expired,
                "unread": unread_only,
            },
            "unread": badge,
            "delivery": DELIVERY_DEFERRED,
        }),
    )
}

/// `GET /api/announcements/unread` — the badge on its own.
async fn unread_route(c: PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let audience = resolve_audience(&c, req.identity.as_ref()).await?;
    let row = unread_rows(&c, &audience).await?;
    PluginResponse::json(200, &unread_report(&audience, row.as_ref()))
}

/// `GET /api/announcements/categories` — the troop's vocabulary, with what each
/// category costs to use.
async fn categories_route(_c: PluginContext, _req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let categories: Vec<Value> = CATEGORY_MEANINGS
        .iter()
        .map(|(code, label, description)| {
            json!({
                "code": code,
                "label": label,
                "description": description,
                "requires_permission": if *code == CATEGORY_URGENT {
                    json!([PERM_WRITE, PERM_PUBLISH_URGENT])
                } else {
                    json!([PERM_WRITE])
                },
            })
        })
        .collect();
    PluginResponse::json(
        200,
        &json!({
            "categories": categories,
            "statuses": STATUSES,
            "scopes": SCOPE_TYPES,
            "note": "urgent needs a second permission on purpose: a category that can be cried \
                     wolf is worthless",
            "delivery": DELIVERY_DEFERRED,
        }),
    )
}

/// `GET /api/announcements/announcement/{id}` — one announcement, with the
/// caller's own receipt and how many members have read it.
async fn get_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let audience = resolve_audience(&c, req.identity.as_ref()).await?;
    let Some(row) = fetch_visible(&c, &audience, id).await? else {
        // Absent and not-addressed-to-you are the same answer: an announcement
        // sent somewhere else is one you do not know exists.
        return Err(SdkError::Forbidden(format!(
            "no announcement {id}, or it was not sent to a scope you hold"
        )));
    };
    let is_read = row["is_read"].as_bool().unwrap_or(false);
    let my_receipt = if is_read {
        json!({
            "id": row["receipt_id"],
            "announcement_id": row["id"],
            "member_id": audience.caller,
            "read_via": row["receipt_via"],
            "read_at": row["read_at"],
        })
    } else {
        Value::Null
    };
    PluginResponse::json(
        200,
        &json!({
            "announcement": row,
            "is_read": is_read,
            "my_receipt": my_receipt,
            "read_count": row["read_count"],
            "delivery": DELIVERY_DEFERRED,
        }),
    )
}

/// `POST /api/announcements/announcement/{id}/read` — and, with
/// [`mark_unread`], its inverse. Both are idempotent, and both are about the
/// caller: a receipt says *this member* opened it.
async fn set_receipt(
    c: PluginContext,
    req: PluginRequest,
    unread: bool,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let caller = require_caller(&req)?;
    let body: ReceiptBody = optional_body(&req)?;
    let via = normalize_via(body.via.as_deref())?;

    let audience = resolve_audience(&c, req.identity.as_ref()).await?;
    let Some(row) = fetch_visible(&c, &audience, id).await? else {
        return Err(SdkError::Forbidden(format!(
            "no announcement {id}, or it was not sent to a scope you hold"
        )));
    };

    if unread {
        let receipts = c.db.table("receipts");
        let removed = c
            .db
            .execute(
                format!(
                    "DELETE FROM {receipts} WHERE announcement_id = $1 AND member_id = $2"
                ),
                vec![SqlValue::Int(id), SqlValue::Text(caller.clone())],
            )
            .await?;
        // Forgetting twice is not an error, and nothing is published when there
        // was nothing to forget.
        if removed > 0 {
            c.audit
                .log(
                    req.identity.as_ref(),
                    "announcement.unread",
                    "announcement",
                    &id.to_string(),
                    json!({ "member_id": caller }),
                )
                .await?;
            c.events
                .publish(
                    EVENT_UNREAD,
                    json!({ "announcement_id": id, "member_id": caller }),
                )
                .await?;
        }
        let badge = unread_report(&audience, unread_rows(&c, &audience).await?.as_ref());
        return PluginResponse::json(
            200,
            &json!({
                "receipt": Value::Null,
                "is_read": false,
                "forgotten": removed > 0,
                "unread": badge,
            }),
        );
    }

    let status = row["status"].as_str().unwrap_or_default();
    if status != STATUS_PUBLISHED {
        return Err(SdkError::Conflict(format!(
            "announcement {id} is {status}, not published — a draft has no readers to record"
        )));
    }

    let receipts = c.db.table("receipts");
    let mut params: Vec<SqlValue> = Vec::new();
    let announcement_p = bind(&mut params, SqlValue::Int(id));
    let member_p = bind(&mut params, SqlValue::Text(caller.clone()));
    let via_p = bind(&mut params, SqlValue::Text(via.clone()));
    let receipt = c
        .db
        .query_one(
            format!(
                "WITH inserted AS ( \
                   INSERT INTO {receipts} (announcement_id, member_id, read_via) \
                   VALUES (${announcement_p}, ${member_p}, ${via_p}) \
                   ON CONFLICT (announcement_id, member_id) DO NOTHING \
                   RETURNING id, announcement_id, member_id, read_via, \
                             read_at::text AS read_at, true AS created \
                 ) \
                 SELECT id, announcement_id, member_id, read_via, read_at, created FROM inserted \
                 UNION ALL \
                 SELECT r.id, r.announcement_id, r.member_id, r.read_via, \
                        r.read_at::text AS read_at, false AS created \
                   FROM {receipts} r \
                  WHERE r.announcement_id = ${announcement_p} AND r.member_id = ${member_p} \
                    AND NOT EXISTS (SELECT 1 FROM inserted) \
                 LIMIT 1"
            ),
            params,
        )
        .await?
        .ok_or_else(|| SdkError::Internal("receipt insert returned no row".into()))?;
    let created = receipt["created"].as_bool().unwrap_or(false);

    // Marking read twice changes nothing: no second row, no second event.
    if created {
        c.audit
            .log(
                req.identity.as_ref(),
                "announcement.read",
                "announcement",
                &id.to_string(),
                json!({
                    "member_id": caller,
                    "category": row["category"],
                    "scope_type": row["scope_type"],
                    "scope_id": row["scope_id"],
                    "via": via,
                }),
            )
            .await?;
        c.events
            .publish(
                EVENT_RECEIPT,
                json!({
                    "announcement_id": id,
                    "member_id": caller,
                    "category": row["category"],
                    "scope_type": row["scope_type"],
                    "scope_id": row["scope_id"],
                    "via": via,
                }),
            )
            .await?;
    }

    let badge = unread_report(&audience, unread_rows(&c, &audience).await?.as_ref());
    PluginResponse::json(
        200,
        &json!({
            "receipt": receipt_of(&receipt),
            "is_read": true,
            "already_read": !created,
            "unread": badge,
        }),
    )
}

/// `GET /api/announcements/announcement/{id}/receipts` — who has read it.
///
/// A `manage` operation: the list of who opened what is oversight data. It
/// returns *who read*, never *who has not* — the roster that answer needs
/// belongs to membership, and a plugin role reads its own schema only.
async fn list_receipts(c: PluginContext, req: PluginRequest) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(row) = fetch_announcement(&c, id).await? else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };
    let scope = announcement_scope(
        row["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        row["scope_id"].as_str(),
    );
    c.permissions
        .reach(req.identity.as_ref(), PERM_MANAGE, &scope)
        .await?;

    let receipts = c
        .db
        .query(
            format!(
                "SELECT id, announcement_id, member_id, read_via, read_at::text AS read_at \
                 FROM {} WHERE announcement_id = $1 ORDER BY read_at DESC, member_id",
                c.db.table("receipts")
            ),
            vec![SqlValue::Int(id)],
        )
        .await?;
    PluginResponse::json(
        200,
        &json!({
            "announcement_id": id,
            "scope": { "scope_type": row["scope_type"], "scope_id": row["scope_id"] },
            "status": row["status"],
            "receipts": receipts,
            "count": receipts.len(),
            "read_count": row["read_count"],
            "note": "receipts record who read it; a member who heard it in the group chat did \
                     not, and the person who has not read it is a question for the roster",
        }),
    )
}

/// `PATCH /api/announcements/announcement/{id}` — fix a draft, or a sent
/// announcement (a correction to something already sent is a `manage` act).
///
/// Declared with the *weaker* permission on purpose: the handler picks between
/// them — the author may edit their own draft with [`PERM_WRITE`], everything
/// else needs [`PERM_MANAGE`] at the object's scope — and a refusal names the
/// permission that was actually required.
async fn edit_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let body: AnnouncementEditBody = req.json()?;
    let Some(row) = fetch_announcement(&c, id).await? else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };
    let status = row["status"].as_str().unwrap_or(STATUS_DRAFT);
    let scope = announcement_scope(
        row["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        row["scope_id"].as_str(),
    );
    let caller = caller_of(&req).map(str::to_string);

    let own_draft = status == STATUS_DRAFT && row["created_by"].as_str() == caller.as_deref();
    let permission = if own_draft { PERM_WRITE } else { PERM_MANAGE };
    c.permissions.reach(req.identity.as_ref(), permission, &scope).await?;

    // Effective values: what the patch supplies, otherwise what the row already
    // holds — so a partial patch is validated against the whole announcement.
    let title = match body.title.as_deref() {
        Some(given) => require_title(given)?,
        None => row["title"].as_str().unwrap_or_default().to_string(),
    };
    let text = match body.body.as_deref() {
        Some(given) => require_body(Some(given))?,
        None => row["body"].as_str().unwrap_or_default().to_string(),
    };
    let category = match body.category.as_deref() {
        Some(given) => normalize_category(Some(given))?,
        None => row["category"]
            .as_str()
            .unwrap_or(CATEGORY_INFORMATIONAL)
            .to_string(),
    };
    let (scope_type, scope_id) = match (&body.scope_type, &body.scope_id) {
        (None, None) => (
            row["scope_type"].as_str().unwrap_or(SCOPE_TROOP).to_string(),
            row["scope_id"].as_str().map(str::to_string),
        ),
        (given_type, given_id) => normalize_scope(
            given_type.as_deref().or(row["scope_type"].as_str()),
            given_id.as_deref(),
        )?,
    };
    // Moving a notice to another audience is re-addressing it, and the caller
    // must hold the authority at the *new* scope — not only the old one.
    let target_scope = announcement_scope(&scope_type, scope_id.as_deref());
    if target_scope != scope {
        c.permissions
            .reach(req.identity.as_ref(), PERM_WRITE, &target_scope)
            .await?;
    }
    let expires_at = match body.expires_at.as_deref() {
        Some(given) => parse_expiry(Some(given))?,
        None => parse_expiry(row["expires_at"].as_str())?,
    };
    let related_event_id = match body.related_event_id {
        Some(given) if given <= 0 => {
            return Err(SdkError::BadRequest(
                "related_event_id must be a positive event id".into(),
            ))
        }
        Some(given) => Some(given),
        None => row["related_event_id"].as_i64(),
    };
    // Editing a *sent* announcement into `urgent` is the other way to cry wolf,
    // so the sharper permission is required for the transition. An announcement
    // that is already published as urgent does not need it again to be fixed.
    let was_urgent = row["category"].as_str() == Some(CATEGORY_URGENT);
    if status != STATUS_DRAFT && category == CATEGORY_URGENT && !was_urgent {
        require_publish_authority(&c, req.identity.as_ref(), &category, &scope).await?;
    }

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    if body.title.is_some() {
        set_clause(&mut sets, &mut params, "title", SqlValue::Text(title.clone()));
    }
    if body.body.is_some() {
        set_clause(&mut sets, &mut params, "body", SqlValue::Text(text));
    }
    if body.category.is_some() {
        set_clause(
            &mut sets,
            &mut params,
            "category",
            SqlValue::Text(category.clone()),
        );
    }
    if body.scope_type.is_some() || body.scope_id.is_some() {
        set_clause(
            &mut sets,
            &mut params,
            "scope_type",
            SqlValue::Text(scope_type.clone()),
        );
        set_clause(
            &mut sets,
            &mut params,
            "scope_id",
            scope_id.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null),
        );
    }
    if body.expires_at.is_some() {
        set_expression(
            &mut sets,
            &mut params,
            "expires_at = {placeholder}::timestamptz",
            expires_at
                .map(|at| SqlValue::Text(at.to_rfc3339()))
                .unwrap_or(SqlValue::Null),
        );
    }
    if body.related_event_id.is_some() {
        set_clause(
            &mut sets,
            &mut params,
            "related_event_id",
            related_event_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
        );
    }
    if sets.is_empty() {
        return Err(SdkError::BadRequest(
            "no editable field was supplied (title, body, category, scope, expires_at, \
             related_event_id)"
                .into(),
        ));
    }
    let id_p = bind(&mut params, SqlValue::Int(id));
    let rows = c
        .db
        .query(
            format!(
                "UPDATE {announcements} AS a SET {sets}, updated_at = now() \
                  WHERE a.id = ${id_p} RETURNING {ANNOUNCEMENT_FIELDS}",
                announcements = c.db.table("announcements"),
                sets = sets.join(", ")
            ),
            params,
        )
        .await?;
    let Some(updated) = rows.first().cloned() else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };

    c.audit
        .log(
            req.identity.as_ref(),
            "announcement.update",
            "announcement",
            &id.to_string(),
            json!({
                "fields": sets.len(),
                "category": updated["category"],
                "scope_type": updated["scope_type"],
                "scope_id": updated["scope_id"],
                "status": updated["status"],
            }),
        )
        .await?;
    c.events
        .publish(
            EVENT_UPDATED,
            json!({
                "announcement_id": id,
                "title": updated["title"],
                "category": updated["category"],
                "scope_type": updated["scope_type"],
                "scope_id": updated["scope_id"],
                "status": updated["status"],
            }),
        )
        .await?;

    PluginResponse::json(
        200,
        &json!({
            "announcement": updated,
            "delivery": DELIVERY_DEFERRED,
        }),
    )
}

/// `POST /api/announcements/announcement/{id}/retract` — withdraw something
/// already sent.
///
/// Retracting is the *ordinary* authority ([`PERM_MANAGE`]) even for an urgent
/// announcement: putting a fire out is not the act that needs the sharp
/// permission. The record stays visible to its audience, marked retracted — a
/// notice that silently vanishes teaches the troop not to trust the next one.
async fn retract_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(row) = fetch_announcement(&c, id).await? else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };
    let scope = announcement_scope(
        row["scope_type"].as_str().unwrap_or(SCOPE_TROOP),
        row["scope_id"].as_str(),
    );
    c.permissions
        .reach(req.identity.as_ref(), PERM_MANAGE, &scope)
        .await?;
    let status = row["status"].as_str().unwrap_or_default();
    if status != STATUS_PUBLISHED {
        return Err(SdkError::Conflict(format!(
            "announcement {id} is {status} — only a published announcement can be retracted"
        )));
    }
    let caller = caller_of(&req).unwrap_or_default().to_string();
    let rows = c
        .db
        .query(
            format!(
                "UPDATE {announcements} AS a \
                    SET status = 'retracted', retracted_at = now(), retracted_by = $2, \
                        updated_at = now() \
                  WHERE a.id = $1 AND a.status = 'published' \
                 RETURNING {ANNOUNCEMENT_FIELDS}",
                announcements = c.db.table("announcements")
            ),
            vec![SqlValue::Int(id), SqlValue::Text(caller.clone())],
        )
        .await?;
    let Some(retracted) = rows.first().cloned() else {
        return Err(SdkError::Conflict(format!(
            "announcement {id} was retracted or published by somebody else a moment ago"
        )));
    };
    c.audit
        .log(
            req.identity.as_ref(),
            "announcement.retract",
            "announcement",
            &id.to_string(),
            json!({
                "title": retracted["title"],
                "category": retracted["category"],
                "scope_type": retracted["scope_type"],
                "scope_id": retracted["scope_id"],
                "by": caller,
            }),
        )
        .await?;
    c.events
        .publish(
            EVENT_RETRACTED,
            json!({
                "announcement_id": id,
                "title": retracted["title"],
                "category": retracted["category"],
                "scope_type": retracted["scope_type"],
                "scope_id": retracted["scope_id"],
                "retracted_by": caller,
                "read_count": retracted["read_count"],
            }),
        )
        .await?;
    PluginResponse::json(200, &json!({ "announcement": retracted }))
}

/// `DELETE /api/announcements/announcement/{id}` — erase it and its receipts.
///
/// Destructive and therefore troop-covered by the SDK's rule: a Lodge-scoped
/// grant may retract its Lodge's notice but may not erase the record. Retracting
/// is the scoped operation; this is the "it should never have existed" one.
async fn delete_announcement(
    c: PluginContext,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let id = req.int_param("id")?;
    let Some(row) = fetch_announcement(&c, id).await? else {
        return Err(SdkError::NotFound(format!("no announcement {id}")));
    };
    c.db.execute(
        format!("DELETE FROM {} WHERE id = $1", c.db.table("announcements")),
        vec![SqlValue::Int(id)],
    )
    .await?;
    c.audit
        .log(
            req.identity.as_ref(),
            "announcement.delete",
            "announcement",
            &id.to_string(),
            json!({
                "title": row["title"],
                "category": row["category"],
                "status": row["status"],
                "scope_type": row["scope_type"],
                "scope_id": row["scope_id"],
            }),
        )
        .await?;
    c.events
        .publish(
            EVENT_DELETED,
            json!({
                "announcement_id": id,
                "title": row["title"],
                "scope_type": row["scope_type"],
                "scope_id": row["scope_id"],
            }),
        )
        .await?;
    PluginResponse::json(200, &json!({ "deleted": id }))
}

// ---------------------------------------------------------------------------
// Plugin declaration
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for AnnouncementsPlugin {
    fn id(&self) -> &str {
        "announcements"
    }

    fn name(&self) -> &str {
        "Announcements"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new(
                PERM_READ,
                "View announcements addressed to a scope you hold",
            ),
            Permission::new(
                PERM_WRITE,
                "Draft and publish an announcement at a scope you hold",
            ),
            Permission::new(
                PERM_PUBLISH_URGENT,
                "Publish an urgent announcement (the category that interrupts)",
            ),
            Permission::new(
                PERM_MANAGE,
                "Edit, retract or delete announcements, and see who has read them",
            ),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "announcements_schema",
            "CREATE TABLE IF NOT EXISTS announcements (\
                 id BIGSERIAL PRIMARY KEY, \
                 title TEXT NOT NULL, \
                 body TEXT NOT NULL DEFAULT '', \
                 category TEXT NOT NULL DEFAULT 'informational', \
                 scope_type TEXT NOT NULL DEFAULT 'troop', \
                 scope_id TEXT, \
                 status TEXT NOT NULL DEFAULT 'draft', \
                 related_event_id BIGINT, \
                 published_at TIMESTAMPTZ, \
                 published_by TEXT, \
                 expires_at TIMESTAMPTZ, \
                 retracted_at TIMESTAMPTZ, \
                 retracted_by TEXT, \
                 created_by TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT announcements_title_present CHECK (btrim(title) <> ''), \
                 CONSTRAINT announcements_category_valid CHECK (\
                     category IN ('urgent', 'informational', 'event')), \
                 CONSTRAINT announcements_scope_type_valid CHECK (\
                     scope_type IN ('troop', 'lodge')), \
                 CONSTRAINT announcements_scope_id_present CHECK ( \
                     (scope_type = 'troop' AND scope_id IS NULL) OR \
                     (scope_type = 'lodge' AND scope_id IS NOT NULL AND scope_id <> '') \
                 ), \
                 CONSTRAINT announcements_status_valid CHECK (\
                     status IN ('draft', 'published', 'retracted')), \
                 CONSTRAINT announcements_publication_consistent CHECK ( \
                     (status = 'draft' AND published_at IS NULL) OR \
                     (status <> 'draft' AND published_at IS NOT NULL) \
                 ), \
                 CONSTRAINT announcements_retraction_consistent CHECK ( \
                     (status = 'retracted') = (retracted_at IS NOT NULL) \
                 ) \
             );\
             CREATE INDEX IF NOT EXISTS idx_announcements_scope \
               ON announcements(scope_type, scope_id);\
             CREATE INDEX IF NOT EXISTS idx_announcements_status \
               ON announcements(status, published_at DESC);\
             CREATE INDEX IF NOT EXISTS idx_announcements_category ON announcements(category);\
             CREATE INDEX IF NOT EXISTS idx_announcements_expiry \
               ON announcements(expires_at) WHERE expires_at IS NOT NULL;\
             CREATE TABLE IF NOT EXISTS receipts (\
                 id BIGSERIAL PRIMARY KEY, \
                 announcement_id BIGINT NOT NULL \
                     REFERENCES announcements(id) ON DELETE CASCADE, \
                 member_id TEXT NOT NULL, \
                 read_via TEXT NOT NULL DEFAULT 'api', \
                 read_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 CONSTRAINT receipts_member_present CHECK (btrim(member_id) <> ''), \
                 UNIQUE (announcement_id, member_id) \
             );\
             CREATE INDEX IF NOT EXISTS idx_receipts_member ON receipts(member_id, read_at DESC);",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx().clone();

        // --- write ----------------------------------------------------------
        let create = RouteDefinition::post_protected_any_scope(
            "/api/announcements/announcement",
            PERM_WRITE,
            route_over(ctx.clone(), create_announcement),
        );

        // --- read -----------------------------------------------------------
        // Object-shaped throughout: the caller needs the permission at *some*
        // scope and the handler applies the addressing rule.
        let list = RouteDefinition::get_protected_any_scope(
            "/api/announcements/announcements",
            PERM_READ,
            route_over(ctx.clone(), list_announcements),
        );
        let unread = RouteDefinition::get_protected_any_scope(
            "/api/announcements/unread",
            PERM_READ,
            route_over(ctx.clone(), unread_route),
        );
        let detail = RouteDefinition::get_protected_any_scope(
            "/api/announcements/announcement/{id}",
            PERM_READ,
            route_over(ctx.clone(), get_announcement),
        );

        // --- reference data -------------------------------------------------
        let categories = RouteDefinition::get_protected(
            "/api/announcements/categories",
            PERM_READ,
            route_over(ctx.clone(), categories_route),
        );

        // --- lifecycle ------------------------------------------------------
        let publish = RouteDefinition::post_protected_any_scope(
            "/api/announcements/announcement/{id}/publish",
            PERM_WRITE,
            route_over(ctx.clone(), publish_announcement),
        );
        let edit = RouteDefinition::patch_protected_any_scope(
            "/api/announcements/announcement/{id}",
            PERM_WRITE,
            route_over(ctx.clone(), edit_announcement),
        );
        let retract = RouteDefinition::post_protected_any_scope(
            "/api/announcements/announcement/{id}/retract",
            PERM_MANAGE,
            route_over(ctx.clone(), retract_announcement),
        );
        // Destructive: troop coverage is the SDK's rule for a DELETE, even from
        // the `any_scope` constructor, so a Lodge grant cannot erase the record.
        let delete = RouteDefinition::delete_protected(
            "/api/announcements/announcement/{id}",
            PERM_MANAGE,
            route_over(ctx.clone(), delete_announcement),
        );

        // --- receipts -------------------------------------------------------
        let read = RouteDefinition::post_protected_any_scope(
            "/api/announcements/announcement/{id}/read",
            PERM_READ,
            route_over(ctx.clone(), |c, req| set_receipt(c, req, false)),
        );
        let unread_again = RouteDefinition::post_protected_any_scope(
            "/api/announcements/announcement/{id}/unread",
            PERM_READ,
            route_over(ctx.clone(), |c, req| set_receipt(c, req, true)),
        );
        let receipts = RouteDefinition::get_protected_any_scope(
            "/api/announcements/announcement/{id}/receipts",
            PERM_MANAGE,
            route_over(ctx, list_receipts),
        );

        vec![
            create, list, unread, detail, categories, publish, edit, retract, delete, read,
            unread_again, receipts,
        ]
    }

    // No subscriptions and no schedules: this plugin publishes (the delivery
    // seam is its output, not its input) and has nothing periodic to do. A
    // digest that sweeps unread announcements belongs to the delivery plugin
    // that does not exist yet.
}

export_plugin!(AnnouncementsPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_is_the_one_the_helpers_accept() {
        // The codes the SQL spells literally have to be the codes the handlers
        // accept, or a CHECK constraint turns a 400 into a 500.
        assert_eq!(CATEGORIES, [CATEGORY_URGENT, CATEGORY_INFORMATIONAL, CATEGORY_EVENT]);
        assert_eq!(STATUSES, [STATUS_DRAFT, STATUS_PUBLISHED, STATUS_RETRACTED]);
        assert_eq!(SCOPE_TYPES, [SCOPE_TROOP, SCOPE_LODGE]);
        assert_eq!(normalize_category(None).unwrap(), CATEGORY_INFORMATIONAL);
        assert_eq!(normalize_category(Some(" URGENT ")).unwrap(), CATEGORY_URGENT);
        assert!(normalize_category(Some("emergency")).is_err());
    }

    #[test]
    fn an_expiry_is_a_timestamp_or_a_date_and_never_a_guess() {
        assert_eq!(parse_expiry(None).unwrap(), None);
        assert_eq!(parse_expiry(Some("  ")).unwrap(), None);
        assert_eq!(
            parse_expiry(Some("2026-10-01T12:00:00Z")).unwrap(),
            Some("2026-10-01T12:00:00Z".parse().expect("a timestamp"))
        );
        // A date expires at its midnight: stale from that day onward.
        assert_eq!(
            parse_expiry(Some("2026-10-01")).unwrap(),
            Some("2026-10-01T00:00:00Z".parse().expect("a timestamp"))
        );
        for bad in ["whenever", "01/10/2026", "2026-13-45"] {
            assert!(parse_expiry(Some(bad)).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_scope_is_a_troop_or_a_lodge_with_an_id() {
        assert_eq!(normalize_scope(None, None).unwrap(), (SCOPE_TROOP.to_string(), None));
        assert_eq!(
            normalize_scope(Some("LODGE"), Some(" 3 ")).unwrap(),
            (SCOPE_LODGE.to_string(), Some("3".to_string()))
        );
        assert!(normalize_scope(Some(SCOPE_LODGE), None).is_err());
        assert!(normalize_scope(Some(SCOPE_TROOP), Some("3")).is_err());
        assert!(normalize_scope(Some("patrol"), Some("7")).is_err());
        assert_eq!(
            announcement_scope(SCOPE_LODGE, Some("3")),
            Scope::lodge("3")
        );
        // A Lodge row with no id is troop-wide, never "lodge with nothing".
        assert_eq!(announcement_scope(SCOPE_LODGE, None), Scope::troop());
    }

    #[test]
    fn a_preview_is_flat_and_bounded() {
        assert_eq!(preview("  Camp   is\ncancelled  ", 40), "Camp is cancelled");
        let long = preview(&"a".repeat(200), 10);
        assert_eq!(long, format!("{}…", "a".repeat(10)));
        assert_eq!(preview("", 10), "");
    }

    #[test]
    fn the_audience_predicate_binds_five_parameters_in_order() {
        let audience = Audience {
            caller: "bea".into(),
            troop_read: true,
            read_lodges: vec!["3".into()],
            troop_manage: false,
            manage_lodges: vec!["9".into()],
        };
        let sql = audience.sql();
        for placeholder in ["$1", "$2", "$3", "$4", "$5"] {
            assert!(sql.contains(placeholder), "{placeholder} missing from {sql}");
        }
        assert_eq!(
            sql.matches('$').count(),
            audience.params().len(),
            "every placeholder must have exactly one bound value"
        );
        assert!(sql.contains("a.created_by = $3"), "the author sees their own");
        assert!(!audience.oversees());
        assert_eq!(audience.summary()["lodges"], json!(["3"]));
    }

    #[test]
    fn entry_symbol_is_exported() {
        // The core resolves this symbol via libloading; a name typo would only
        // show up at load time, so pin it with a direct call.
        let raw = adjutant_plugin_create();
        assert!(!raw.is_null());
        let boxed = unsafe { Box::from_raw(raw) };
        assert_eq!(boxed.id(), "announcements");
        assert_eq!(adjutant_sdk_abi(), adjutant_sdk::SDK_ABI_VERSION);
    }
}
