//! # adjutant-sdk
//!
//! The contract between the Adjutant core and every plugin — first-party and
//! third-party alike. The auth, membership, and missions plugins are built with
//! this exact API. If it's awkward here, it's awkward for everyone: dogfooding
//! is the validation (SPEC §5.2a).
//!
//! ## Host-mediated I/O (the load-bearing design rule)
//!
//! A plugin is a `cdylib` with its **own** copies of every dependency. If plugin
//! code called `sqlx` directly it would look up *its* tokio's thread-local
//! runtime handle — which the server's runtime never set — and panic
//! (`this functionality requires a Tokio context`, aborting the process with
//! `Rust cannot catch foreign exceptions`). Observed live in Milestone 1.
//!
//! So: **nothing that needs a runtime or a connection may be linked into the
//! plugin.** Databases, events, permissions, and audit all cross the boundary as
//! `Arc<dyn Host…>` trait objects whose implementations live in the core. The
//! SDK depends on neither `sqlx` nor `tokio`.
//!
//! ## Plugin anatomy
//!
//! ```rust,ignore
//! pub struct MyPlugin { ctx: OnceLock<PluginContext> };
//!
//! #[async_trait]
//! impl AdjutantPlugin for MyPlugin {
//!     fn id(&self) -> &str { "my-plugin" }
//!     // routes(), migrations(), permissions_granted(), init(), subscriptions()
//! }
//! export_plugin!(MyPlugin);
//! ```
//!
//! Build as `crate-type = ["cdylib", "rlib"]`, drop the `.so` in the server's
//! plugin directory, and the core loads it at boot.
//!
//! ## Native-loading caveat (Milestone 1)
//!
//! Dynamic loading uses `libloading` + a `#[no_mangle] extern "C"` factory, so
//! core and plugin must share a toolchain and dependency versions — fine for
//! first-party plugins, NOT a security boundary. Third-party isolation is the
//! WASM path (wasmtime), deferred per SPEC §14-R1.
//!
//! ## Allocator note
//!
//! `Box<dyn Future>` produced by host calls is freed by plugin code. Both sides
//! use Rust's default (system) allocator, so this is safe — do not install a
//! custom `#[global_allocator]` in either the core or a plugin.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Re-export so plugin authors get `#[async_trait]` from the prelude.
pub use async_trait::async_trait;

/// Boxed future used across the plugin boundary (object-safe async).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// ABI version of the plugin/core boundary.
///
/// Bump this on **any** breaking change to the trait surface, the host traits,
/// or the exported symbols. The core resolves [`ABI_SYMBOL`] *before* it calls
/// the plugin factory and refuses a library whose value differs, so a stale
/// build becomes a clear load error instead of undefined behaviour (the native
/// loading caveat: core and plugin must be built against the same SDK).
pub const SDK_ABI_VERSION: u32 = 4;

/// The SDK crate's SemVer version, for diagnostics and error messages.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Symbol `export_plugin!` emits so the core can check ABI compatibility before
/// calling the factory. Do not rename without bumping [`SDK_ABI_VERSION`].
pub const ABI_SYMBOL: &[u8] = b"adjutant_sdk_abi";

/// Errors that can cross the plugin boundary. Stringly-typed on purpose:
/// concrete error types would pin both sides to identical dependency versions
/// at the type level (they must already match at the ABI level).
///
/// The variants map to HTTP statuses through [`SdkError::status`]; return one
/// from a handler and the core answers with the matching code, so plugins don't
/// hand-build error responses for the common cases.
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("database error: {0}")]
    Db(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl SdkError {
    /// HTTP status the core uses when this error crosses the plugin boundary.
    pub fn status(&self) -> u16 {
        match self {
            SdkError::BadRequest(_) => 400,
            SdkError::Unauthorized(_) => 401,
            SdkError::Forbidden(_) => 403,
            SdkError::NotFound(_) => 404,
            SdkError::Conflict(_) => 409,
            SdkError::Db(_) | SdkError::Internal(_) => 500,
        }
    }
}

/// HTTP, mediated by the core. The M1 host-I/O rule extends here: a plugin
/// that linked its own `reqwest` would resolve *its* tokio reactor
/// thread-local — unset on the core's runtime — and panic exactly like
/// plugin-side `sqlx` did. OIDC discovery/token exchange etc. go through this.
#[async_trait]
pub trait HostHttp: Send + Sync + 'static {
    /// `method` is "GET"/"POST"/…; `body` is (content-type, bytes).
    async fn request(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError>;
}

/// Framework-neutral HTTP response handed back across the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, SdkError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| SdkError::Internal(format!("response decode failed: {e}")))
    }

    /// Case-insensitive header lookup (HTTP/2 lowercases, HTTP/1.1 may not).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A plugin's identity provider, registered during `init` so the core asks
/// *the plugin* "who is this request?" instead of reading dev headers.
/// Replaces the `x-dev-user`/`x-dev-role` stub (SPEC §7.1).
#[async_trait]
pub trait IdentityProvider: Send + Sync + 'static {
    /// `Ok(Some(identity))` = authenticated; `Ok(None)` = no credentials this
    /// provider recognizes (core may fall back); `Err` = invalid credentials
    /// (treated as anonymous — never fall back to spoofable headers).
    async fn identify(&self, headers: &HashMap<String, String>) -> Result<Option<Identity>, SdkError>;
}

/// How a plugin registers its provider with the core (via `PluginContext`).
pub trait IdentityRegistrar: Send + Sync + 'static {
    fn register(&self, owner: &str, provider: Arc<dyn IdentityProvider>);
}

// ---------------------------------------------------------------------------
// Host services — implemented by the core, never by the plugin
// ---------------------------------------------------------------------------

/// A bind parameter. Deliberately a closed enum: passing sqlx types across the
/// boundary would drag sqlx (and a second tokio) into the plugin.
///
/// **A NULL carries a type.** `Null` is a TEXT null; use [`SqlValue::NullInt`],
/// [`SqlValue::NullBool`], or [`SqlValue::NullUuid`] for an optional integer,
/// boolean, or uuid column.
/// PostgreSQL refuses a text null where a bigint is expected
/// (`column "patrol_id" is of type bigint but expression is of type text`),
/// which broke three shipped optional-field routes (member without a patrol,
/// steward without a lodge, patrol without a lodge).
///
/// Do **not** work around it with a cast on a bare parameter —
/// `VALUES ($1, $2::bigint)` makes PostgreSQL infer `$2` as text, so a non-null
/// integer value then arrives as garbage (`invalid byte sequence … 0x00`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    /// A NULL for an integer column (`int2`/`int4`/`int8`).
    NullInt,
    /// A NULL for a boolean column.
    NullBool,
    /// A NULL for a `uuid` column (a text NULL is refused, like the integer case).
    NullUuid,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// A `uuid` value. Bound as `uuid` — no `::uuid` cast on the parameter needed.
    Uuid(String),
    /// `int8[]` — for `= ANY($n)` on an integer array.
    IntArray(Vec<i64>),
    /// `text[]` — for `= ANY($n)`.
    TextArray(Vec<String>),
    /// Pre-serialized JSON. SQL must cast: `$n::jsonb`.
    Json(String),
}

impl From<&str> for SqlValue {
    fn from(s: &str) -> Self {
        SqlValue::Text(s.to_string())
    }
}
impl From<String> for SqlValue {
    fn from(s: String) -> Self {
        SqlValue::Text(s)
    }
}
impl From<Option<String>> for SqlValue {
    /// `None` binds as SQL NULL, `Some` as text — the pattern every plugin
    /// needs for optional form fields (found while dogfooding membership).
    fn from(v: Option<String>) -> Self {
        match v {
            Some(s) => SqlValue::Text(s),
            None => SqlValue::Null,
        }
    }
}
impl From<Option<i64>> for SqlValue {
    /// `None` is a *typed* null: a plain `SqlValue::Null` binds as `Option<String>`
    /// and PostgreSQL refuses it for a bigint column (`column "x" is of type
    /// bigint but expression is of type text`).
    fn from(v: Option<i64>) -> Self {
        match v {
            Some(n) => SqlValue::Int(n),
            None => SqlValue::NullInt,
        }
    }
}
impl From<Option<bool>> for SqlValue {
    /// Same carve-out as `Option<i64>`: a boolean null must not be a text null.
    fn from(v: Option<bool>) -> Self {
        match v {
            Some(b) => SqlValue::Bool(b),
            None => SqlValue::NullBool,
        }
    }
}
impl From<i64> for SqlValue {
    fn from(n: i64) -> Self {
        SqlValue::Int(n)
    }
}
impl From<bool> for SqlValue {
    fn from(b: bool) -> Self {
        SqlValue::Bool(b)
    }
}
impl From<Vec<String>> for SqlValue {
    fn from(v: Vec<String>) -> Self {
        SqlValue::TextArray(v)
    }
}
impl From<Vec<i64>> for SqlValue {
    fn from(v: Vec<i64>) -> Self {
        SqlValue::IntArray(v)
    }
}

/// Database access, mediated by the core. Rows come back as JSON objects keyed
/// by column name. The core decodes: json/jsonb → bool → int8/int4/int2 →
/// float8/float4 → text[] → date/time/timestamp → text. **Every other type
/// (uuid, numeric, bytea, …) must be cast in SQL** (`id::text`); an undecodable
/// column comes back as JSON null and is logged by the core as a warning — it is
/// not a silent NULL, and it is not an error.
#[async_trait]
pub trait HostDb: Send + Sync + 'static {
    /// Run a statement; returns affected row count.
    async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError>;
    /// Run a query; returns rows as JSON objects.
    async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError>;
}

/// Event publishing, mediated by the core: persists to `core.events`, then
/// broadcasts to in-process subscribers. One instance per plugin (owns its
/// `source`).
#[async_trait]
pub trait HostEvents: Send + Sync + 'static {
    async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError>;

    /// Replay persisted events (SPEC §15 M2: event bus with persistence and
    /// replay). Returns rows with `id > since_id`, ascending, capped at
    /// `limit` (the core clamps it).
    async fn replay(&self, since_id: i64, limit: i64) -> Result<Vec<Event>, SdkError>;
}

// ---------------------------------------------------------------------------
// Identity & permissions
// ---------------------------------------------------------------------------

/// The level a role grant applies to (SPEC §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeType {
    /// Troop-wide authority.
    Troop,
    /// Scoped to one lodge.
    Lodge,
    /// Scoped to one patrol.
    Patrol,
}

/// A concrete scope. `scope_id` is `None` only for troop-wide; a non-troop scope
/// carries the owning plugin's opaque id (a bigint, UUID or slug — the core
/// never interprets it, it only compares for equality and asks the owning
/// plugin's check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub scope_type: ScopeType,
    pub scope_id: Option<String>,
}

impl Scope {
    pub fn troop() -> Self {
        Self { scope_type: ScopeType::Troop, scope_id: None }
    }

    pub fn lodge(id: impl Into<String>) -> Self {
        Self { scope_type: ScopeType::Lodge, scope_id: Some(id.into()) }
    }

    pub fn patrol(id: impl Into<String>) -> Self {
        Self { scope_type: ScopeType::Patrol, scope_id: Some(id.into()) }
    }

    /// Does this scope cover `other`? A troop-wide scope covers everything;
    /// otherwise the type and id must match exactly. (The core does not know the
    /// lodge→patrol hierarchy, so coverage is intentionally flat — a lodge grant
    /// does not implicitly cover every patrol in it.)
    pub fn covers(&self, other: &Scope) -> bool {
        if self.scope_type == ScopeType::Troop {
            return true;
        }
        self.scope_type == other.scope_type && self.scope_id == other.scope_id
    }
}

/// A role granted at a particular scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleGrant {
    pub role_id: String,
    pub scope: Scope,
}

/// One `(permission, scope)` pair an identity holds — what a **batched**
/// permission check returns.
///
/// The unit of [`PermissionService::scopes_for`]: one entry per permission per
/// scope the caller's grants resolve to, all in a single round trip. A plugin
/// that must decide "what may this caller see, at every scope they hold?" — an
/// audience — asks once instead of once per (permission, scope) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionScope {
    /// The permission id (`missions:approve`).
    pub permission: String,
    /// The scope at which the caller holds it.
    pub scope: Scope,
}

/// The authenticated caller. Produced by an [`IdentityProvider`] (the auth
/// plugin) or the gated dev-header stub.
///
/// **`grants` is the single source of truth.** [`Identity::roles`] is derived
/// from it, so a role set and a grant set can never disagree. Use
/// [`Identity::new`] for a troop-wide identity (every role at troop scope — the
/// dev-header stub's shape and the reason dev environments are more permissive
/// than production), or [`Identity::from_grants`] for scoped ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub user_id: String,
    /// Scoped role grants (SPEC §9.2). Not `#[serde(default)]`: an identity
    /// payload without grants must fail deserialization loudly rather than
    /// silently holding no scopes.
    pub grants: Vec<RoleGrant>,
}

impl Identity {
    /// A troop-wide identity from a user id and role ids. Every role is granted
    /// at troop scope (the dev-header stub's constructor — more permissive than
    /// a real session).
    pub fn new(user_id: impl Into<String>, roles: Vec<String>) -> Self {
        let grants = roles
            .iter()
            .map(|r| RoleGrant { role_id: r.clone(), scope: Scope::troop() })
            .collect();
        Self { user_id: user_id.into(), grants }
    }

    /// An identity with explicit scoped grants.
    pub fn from_grants(user_id: impl Into<String>, grants: Vec<RoleGrant>) -> Self {
        Self { user_id: user_id.into(), grants }
    }

    /// The flat set of granted role ids, derived from `grants` (sorted,
    /// deduplicated). There is no separate `roles` field to fall out of sync.
    pub fn roles(&self) -> Vec<String> {
        let mut out: Vec<String> = self.grants.iter().map(|g| g.role_id.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Role ids whose grant covers `scope` (sorted, deduplicated).
    pub fn roles_covering(&self, scope: &Scope) -> Vec<String> {
        let mut out: Vec<String> = self
            .grants
            .iter()
            .filter(|g| g.scope.covers(scope))
            .map(|g| g.role_id.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// A permission a plugin defines. Registered into `core.permissions` on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permission {
    pub id: String,
    pub description: String,
}

impl Permission {
    pub fn new(id: &str, description: &str) -> Self {
        Self { id: id.into(), description: description.into() }
    }
}

/// Permission checks against `core.role_permissions`. Plugins normally don't
/// call this directly — the core middleware enforces `required_permission` on
/// every route — but it's available for in-handler checks.
#[derive(Clone)]
pub struct PermissionService {
    db: Arc<dyn HostDb>,
}

impl PermissionService {
    pub fn new(db: Arc<dyn HostDb>) -> Self {
        Self { db }
    }

    /// Check a permission against any role the identity holds, **regardless of
    /// the grant's scope**. This is the core route gate's building block for a
    /// route declared scope-any; plugin authors should not call it — use
    /// [`has_in_scope`](Self::has_in_scope) with an explicit scope, which states
    /// the intent.
    #[doc(hidden)]
    pub async fn has_any_scope(&self, identity: Option<&Identity>, permission: &str) -> bool {
        match identity {
            Some(id) if !id.grants.is_empty() => self.roles_have(id.roles(), permission).await,
            _ => false,
        }
    }

    /// Check a permission against only the roles whose grant **covers** `scope`
    /// (SPEC §9.2). A troop-wide grant covers every scope; a lodge grant covers
    /// only that lodge. Plugins call this in-handler when the action targets a
    /// specific lodge/patrol/person. For the ordinary "troop-wide permission"
    /// check, pass `&Scope::troop()` — which says what you mean.
    pub async fn has_in_scope(
        &self,
        identity: Option<&Identity>,
        permission: &str,
        scope: &Scope,
    ) -> bool {
        let Some(id) = identity else { return false };
        let roles = id.roles_covering(scope);
        if roles.is_empty() {
            return false;
        }
        self.roles_have(roles, permission).await
    }

    /// Same as [`has_in_scope`](Self::has_in_scope) but returns a ready
    /// `SdkError::Forbidden` naming the permission and scope, so the safe path is
    /// the short one:
    ///
    /// ```
    /// # use adjutant_sdk::prelude::*;
    /// # async fn f(ctx: &PluginContext, id: Option<&Identity>, lodge_id: &str)
    /// #     -> Result<(), SdkError> {
    /// ctx.permissions.reach(id, "missions:approve", &Scope::lodge(lodge_id)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn reach(
        &self,
        identity: Option<&Identity>,
        permission: &str,
        scope: &Scope,
    ) -> Result<(), SdkError> {
        if self.has_in_scope(identity, permission, scope).await {
            Ok(())
        } else {
            Err(SdkError::Forbidden(format!(
                "requires {permission} at scope {scope:?}"
            )))
        }
    }

    /// Every `(permission, scope)` pair the identity holds for `permissions`, in
    /// **one** round trip.
    ///
    /// The batched form of [`has_in_scope`](Self::has_in_scope). A plugin that
    /// must decide "what may this caller see, at every scope they hold?" — an
    /// audience — otherwise pays one query per (permission, scope) pair: four
    /// for a scout with a single lodge grant, two dozen or more for someone
    /// holding a grant in every lodge. This asks once.
    ///
    /// ```rust,ignore
    /// let held = ctx
    ///     .permissions
    ///     .scopes_for(req.identity.as_ref(), &["announcement:read", "announcement:manage"])
    ///     .await?;
    /// let reads_somewhere = held.iter().any(|p| p.permission == "announcement:read");
    /// ```
    ///
    /// **Asking cannot widen reach.** The query joins *the grants it was handed*
    /// (`Identity::grants`) against `core.role_permissions`, so the answer is
    /// always a subset of what the caller already holds at the scopes they
    /// already hold it — the same answer `has_in_scope` gives, batched. It runs
    /// on the connection the core injected here, which is the core's own: a
    /// plugin role may not read `core.role_permissions` itself, and does not
    /// need to.
    ///
    /// Scopes **fail closed**, as the auth plugin's grant loader does: a
    /// non-troop grant with no scope id is dropped (with a warning) rather than
    /// widened to the troop, and a scope code that is not
    /// `troop`/`lodge`/`patrol` is dropped rather than guessed at.
    ///
    /// An absent identity, or one with no grants, is answered without a query —
    /// an unauthenticated caller holds nothing, and asking the database to
    /// confirm it is a round trip spent to learn nothing.
    pub async fn scopes_for(
        &self,
        identity: Option<&Identity>,
        permissions: &[&str],
    ) -> Result<Vec<PermissionScope>, SdkError> {
        let Some(identity) = identity else {
            return Ok(Vec::new());
        };
        if identity.grants.is_empty() || permissions.is_empty() {
            return Ok(Vec::new());
        }

        let mut roles: Vec<String> = Vec::with_capacity(identity.grants.len());
        let mut scope_types: Vec<String> = Vec::with_capacity(identity.grants.len());
        let mut scope_ids: Vec<String> = Vec::with_capacity(identity.grants.len());
        let mut dropped = 0usize;
        for grant in &identity.grants {
            let scope_id = grant.scope.scope_id.clone().unwrap_or_default();
            let scope_type = match grant.scope.scope_type {
                ScopeType::Troop => "troop",
                ScopeType::Lodge => "lodge",
                ScopeType::Patrol => "patrol",
            };
            if grant.scope.scope_type != ScopeType::Troop && scope_id.trim().is_empty() {
                // Fail closed rather than widen: a lodge/patrol grant with no id
                // names no place, so it names nowhere the caller may act.
                dropped += 1;
                continue;
            }
            roles.push(grant.role_id.clone());
            scope_types.push(scope_type.to_string());
            scope_ids.push(scope_id);
        }
        if dropped > 0 {
            tracing_warn(&format!(
                "scopes_for dropped {dropped} grant(s) with no scope id (scopes fail closed)"
            ));
        }
        if roles.is_empty() {
            return Ok(Vec::new());
        }

        let wanted: Vec<String> = permissions.iter().map(|p| p.to_string()).collect();
        let rows = self
            .db
            .query(
                "SELECT DISTINCT rp.permission_id AS permission, \
                        g.scope_type AS scope_type, COALESCE(g.scope_id, '') AS scope_id \
                 FROM unnest($1::text[], $2::text[], $3::text[]) AS g(role_id, scope_type, scope_id) \
                 JOIN core.role_permissions rp \
                   ON rp.role_id = g.role_id AND rp.permission_id = ANY($4::text[]) \
                 ORDER BY permission, scope_type, scope_id"
                    .to_string(),
                vec![
                    roles.into(),
                    scope_types.into(),
                    scope_ids.into(),
                    wanted.into(),
                ],
            )
            .await?;

        let mut held = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(permission) = row.get("permission").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(scope_type) = row.get("scope_type").and_then(|v| v.as_str()) else {
                continue;
            };
            let scope_id = row.get("scope_id").and_then(|v| v.as_str()).unwrap_or_default();
            let scope = match scope_type {
                "troop" => Scope::troop(),
                "lodge" if !scope_id.trim().is_empty() => Scope::lodge(scope_id),
                "patrol" if !scope_id.trim().is_empty() => Scope::patrol(scope_id),
                other => {
                    tracing_warn(&format!(
                        "scopes_for dropped a row with scope_type {other:?} (scopes fail closed)"
                    ));
                    continue;
                }
            };
            held.push(PermissionScope { permission: permission.to_string(), scope });
        }
        Ok(held)
    }

    async fn roles_have(&self, roles: Vec<String>, permission: &str) -> bool {
        let rows = self
            .db
            .query(
                "SELECT COUNT(*) AS n FROM core.role_permissions \
                 WHERE role_id = ANY($1) AND permission_id = $2"
                    .to_string(),
                vec![roles.into(), permission.to_string().into()],
            )
            .await;
        match rows {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.get("n"))
                .and_then(|n| n.as_i64())
                .unwrap_or(0)
                > 0,
            Err(e) => {
                tracing_warn(&format!("permission check failed: {e}"));
                false
            }
        }
    }
}

// `tracing` is a no-dependency way to surface warnings from the SDK without
// pulling the core's logging stack into plugins: format the string here and let
// the core's subscriber pick up the plugin's `eprintln` only if needed. Kept
// minimal on purpose.
fn tracing_warn(msg: &str) {
    // Plugins run in-process; stderr reaches the core's captured logs.
    eprintln!("[adjutant-sdk] {msg}");
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An inter-plugin event. Persisted to `core.events` and broadcast in-process.
/// `id` is the `core.events` row id (0 for events that somehow skipped
/// persistence); replayed events carry their real ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub id: i64,
    pub event_type: String,
    pub payload: Value,
    pub source: String,
    pub timestamp: DateTime<Utc>,
}

/// Publish handle handed to plugins via [`PluginContext`].
#[derive(Clone)]
pub struct EventBusHandle {
    host: Arc<dyn HostEvents>,
    source: String,
}

impl EventBusHandle {
    pub fn new(host: Arc<dyn HostEvents>, source: String) -> Self {
        Self { host, source }
    }

    /// The plugin id stamped on events this handle publishes.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub async fn publish(&self, event_type: &str, payload: Value) -> Result<(), SdkError> {
        self.host.publish(event_type.to_string(), payload).await
    }

    /// Replay persisted events with `id > since_id` (ascending, clamped).
    pub async fn replay(&self, since_id: i64, limit: i64) -> Result<Vec<Event>, SdkError> {
        let limit = limit.clamp(1, 500);
        self.host.replay(since_id, limit).await
    }
}

// ---------------------------------------------------------------------------
// Event vocabulary & typed payloads (SDK v0.2)
// ---------------------------------------------------------------------------

/// The event types the core documents (SPEC §5.4), plus the M4 lifecycle events
/// the missions and governance plugins publish.
///
/// The bus deliberately does **not** validate event type names (SPEC §5.4), so
/// these constants are the only thing keeping a publisher and a subscriber
/// spelling `mission.completed` identically. Prefer a constant over a literal;
/// a subscription filter stays a prefix literal (`"mission."`).
pub mod event_type {
    /// `user.registered` — new user created (auth).
    pub const USER_REGISTERED: &str = "user.registered";
    /// `user.role_changed` — a user's role or scope changed (auth).
    pub const USER_ROLE_CHANGED: &str = "user.role_changed";
    /// `mission.created` — new mission proposed (missions).
    pub const MISSION_CREATED: &str = "mission.created";
    /// `mission.approved` — mission approved by the Lodge Commander (missions).
    pub const MISSION_APPROVED: &str = "mission.approved";
    /// `mission.completed` — mission closed out after its debrief and report
    /// (missions). Payload: [`crate::MissionCompleted`].
    pub const MISSION_COMPLETED: &str = "mission.completed";
    /// `motion.proposed` — new motion proposed (governance).
    pub const MOTION_PROPOSED: &str = "motion.proposed";
    /// `motion.passed` — motion approved by vote (governance). Payload:
    /// [`crate::MotionPassed`].
    pub const MOTION_PASSED: &str = "motion.passed";
    /// `motion.failed` — motion rejected by vote (governance). Payload:
    /// [`crate::MotionFailed`].
    pub const MOTION_FAILED: &str = "motion.failed";
    /// `conflict.escalated` — conflict moved to the next stage (conflicts).
    pub const CONFLICT_ESCALATED: &str = "conflict.escalated";
    /// `payment.received` — Stripe payment confirmed (stripe).
    pub const PAYMENT_RECEIVED: &str = "payment.received";
    /// `member.joined` — new member registered (membership).
    pub const MEMBER_JOINED: &str = "member.joined";
    /// `member.left` — member departed (membership).
    pub const MEMBER_LEFT: &str = "member.left";
    /// `event.created` — new calendar event (calendar).
    pub const EVENT_CREATED: &str = "event.created";
}

/// Payload of `mission.completed` (SPEC §5.4) — the contract between missions
/// and every consumer of a closed mission (archive, finance's impact fund,
/// the client's activity feed).
///
/// ```rust,ignore
/// ctx.events
///     .publish_mission_completed(&MissionCompleted {
///         mission_id: 12,
///         title: "Coyote survey".into(),
///         lodge_id: Some("3".into()),
///         stage: "report".into(),
///         completed_at: chrono::Utc::now(),
///         impact: serde_json::json!({ "service_hours": 18.5, "participants": 6 }),
///     })
///     .await?;
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionCompleted {
    pub mission_id: i64,
    pub title: String,
    /// The owning lodge's opaque id (`None` for a troop-wide mission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lodge_id: Option<String>,
    /// The lifecycle stage the mission finished in — `report` for a mission
    /// completed through the six-stage path.
    pub stage: String,
    pub completed_at: DateTime<Utc>,
    /// The impact the mission reported (service hours, participants, goals met).
    #[serde(default)]
    pub impact: Value,
}

/// Payload of `motion.passed` (SPEC §5.4) — the contract between governance and
/// every consumer of a decided motion (archive, the client's meeting view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionPassed {
    pub motion_id: i64,
    pub title: String,
    /// The governing body the motion was decided in — a stable code
    /// (`congress`, `tc`, `lodge`, `committee`), never a display string.
    pub body: String,
    /// The meeting the motion was decided at, when it was tied to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<i64>,
    pub votes_yes: i64,
    pub votes_no: i64,
    pub votes_abstain: i64,
    /// The threshold that was applied: `simple_majority`, `two_thirds`,
    /// `unanimous`.
    pub threshold: String,
    pub passed_at: DateTime<Utc>,
    /// Whether this motion amends the Accords — a passed one is what creates a
    /// new `accords_versions` row.
    #[serde(default)]
    pub amends_accords: bool,
}

/// Payload of `motion.failed` (SPEC §5.4) — the same tally shape as
/// [`MotionPassed`], named for the outcome so a consumer never has to read a
/// `passed_at` field on a motion that failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionFailed {
    pub motion_id: i64,
    pub title: String,
    /// The governing body the motion was decided in (a stable code).
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<i64>,
    pub votes_yes: i64,
    pub votes_no: i64,
    pub votes_abstain: i64,
    pub threshold: String,
    pub failed_at: DateTime<Utc>,
    #[serde(default)]
    pub amends_accords: bool,
}

/// Serialize a typed payload; a value that cannot be serialized is an internal
/// error rather than a silently dropped event.
fn payload<T: Serialize>(value: &T) -> Result<Value, SdkError> {
    serde_json::to_value(value)
        .map_err(|e| SdkError::Internal(format!("event payload serialization failed: {e}")))
}

/// Typed event publishers (SDK v0.2). The generic
/// [`publish`](EventBusHandle::publish) remains the escape hatch for custom
/// event types; these two wrap the M4 contracts so a publisher cannot drift
/// from the documented payload shape.
impl EventBusHandle {
    /// Publish `mission.completed` with the [`MissionCompleted`] payload.
    pub async fn publish_mission_completed(
        &self,
        event: &MissionCompleted,
    ) -> Result<(), SdkError> {
        self.publish(event_type::MISSION_COMPLETED, payload(event)?).await
    }

    /// Publish `motion.passed` with the [`MotionPassed`] payload.
    pub async fn publish_motion_passed(&self, event: &MotionPassed) -> Result<(), SdkError> {
        self.publish(event_type::MOTION_PASSED, payload(event)?).await
    }

    /// Publish `motion.failed` with the [`MotionFailed`] payload.
    pub async fn publish_motion_failed(&self, event: &MotionFailed) -> Result<(), SdkError> {
        self.publish(event_type::MOTION_FAILED, payload(event)?).await
    }
}

/// Plugin-side event handler: `Fn(Event) -> Future<Result<(), SdkError>>`.
pub type EventHandler = Arc<dyn Fn(Event) -> BoxFuture<'static, Result<(), SdkError>> + Send + Sync>;

/// Wrap an async closure into an [`EventHandler`].
pub fn event_handler<F, Fut>(f: F) -> EventHandler
where
    F: Fn(Event) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), SdkError>> + Send + 'static,
{
    Arc::new(move |ev| Box::pin(f(ev)))
}

/// A subscription declaration. The core spawns one task per subscription and
/// dispatches matching events. `filter` matches by prefix — `"hello."` catches
/// `hello.greeted`; `"*"` catches everything. (Declared, never consumed: the
/// core owns the channel, so plugins never see a receiver.)
pub struct EventSubscription {
    pub filter: String,
    pub handler: EventHandler,
}

impl EventSubscription {
    pub fn new(filter: &str, handler: EventHandler) -> Self {
        Self { filter: filter.to_string(), handler }
    }

    pub fn matches(&self, event_type: &str) -> bool {
        self.filter == "*" || event_type.starts_with(&self.filter)
    }
}

// ---------------------------------------------------------------------------
// Scheduled work
// ---------------------------------------------------------------------------

/// Plugin-side scheduled handler: `Fn() -> Future<Result<(), SdkError>>`.
pub type ScheduleHandler = Arc<dyn Fn() -> BoxFuture<'static, Result<(), SdkError>> + Send + Sync>;

/// Wrap an async closure into a [`ScheduleHandler`].
pub fn schedule_handler<F, Fut>(f: F) -> ScheduleHandler
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), SdkError>> + Send + 'static,
{
    Arc::new(move || Box::pin(f()))
}

/// A scheduled job a plugin declares.
///
/// The **core** runs `handler` every `every` on the plugin's own connection pool
/// (its isolation role, exactly like a request), with a per-run timeout, and
/// records each run in `core.scheduled_runs`. This is not a thread the plugin
/// owns: the core starts the schedule when the plugin loads and stops it when the
/// plugin is disabled, uninstalled or reloaded. One attempt per tick — a failure
/// is recorded, not retried; the next tick is the retry.
///
/// Cadence is an **interval**, not cron: `Duration::from_secs(24 * 60 * 60)` is
/// "every 24h from the last completed run". A cron parser is deliberately not in
/// v1.
pub struct Schedule {
    pub name: String,
    pub every: std::time::Duration,
    pub handler: ScheduleHandler,
}

impl Schedule {
    pub fn new(name: &str, every: std::time::Duration, handler: ScheduleHandler) -> Self {
        Self { name: name.to_string(), every, handler }
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// Connection access scoped to the plugin's own schema (SPEC §5.2: each plugin
/// gets `missions.*`, `governance.*`, …). Core runs the plugin's migrations with
/// `search_path` pointed at that schema; qualify tables at runtime with
/// [`DbHandle::table`].
#[derive(Clone)]
pub struct DbHandle {
    db: Arc<dyn HostDb>,
    schema: String,
}

impl DbHandle {
    pub fn new(db: Arc<dyn HostDb>, schema: String) -> Self {
        Self { db, schema }
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Table name qualified with this plugin's schema.
    ///
    /// The schema is core-validated (`[a-z][a-z0-9_]{0,30}`) and `name` is
    /// escaped for a quoted identifier, but a table name is still code: pass a
    /// literal, never a request value. (sqlx prepares a single statement, so a
    /// second statement cannot be appended — a crafted name is limited to
    /// rewriting the one statement, e.g. via UNION.)
    pub fn table(&self, name: &str) -> String {
        format!(
            "\"{}\".\"{}\"",
            self.schema,
            name.replace('"', "\"\"")
        )
    }

    pub async fn execute(
        &self,
        sql: impl Into<String>,
        params: Vec<SqlValue>,
    ) -> Result<u64, SdkError> {
        self.db.execute(sql.into(), params).await
    }

    pub async fn query(
        &self,
        sql: impl Into<String>,
        params: Vec<SqlValue>,
    ) -> Result<Vec<Value>, SdkError> {
        self.db.query(sql.into(), params).await
    }

    // --- query helpers (SDK v0.2) -------------------------------------------

    /// The first row of a query, or `None` when it returns no rows.
    ///
    /// The "fetch one or 404" path is in every plugin; this keeps it one line:
    ///
    /// ```rust
    /// # use adjutant_sdk::prelude::*;
    /// # async fn f(ctx: &PluginContext, id: i64) -> Result<PluginResponse, SdkError> {
    /// let sql = format!("SELECT id, title FROM {} WHERE id = $1", ctx.db.table("missions"));
    /// let row = ctx.db.query_one(sql, vec![SqlValue::Int(id)]).await?;
    /// match row {
    ///     Some(row) => PluginResponse::json(200, &serde_json::json!({ "mission": row })),
    ///     None => PluginResponse::error(404, "no such mission"),
    /// }
    /// # }
    /// ```
    pub async fn query_one(
        &self,
        sql: impl Into<String>,
        params: Vec<SqlValue>,
    ) -> Result<Option<Value>, SdkError> {
        Ok(self.db.query(sql.into(), params).await?.into_iter().next())
    }

    /// Whether a query returns any row (`SELECT 1 … WHERE …`, `COUNT(*) …`).
    pub async fn exists(&self, sql: impl Into<String>, params: Vec<SqlValue>) -> Result<bool, SdkError> {
        Ok(!self.db.query(sql.into(), params).await?.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// Append-only audit log writer (`core.audit_log`).
#[derive(Clone)]
pub struct AuditService {
    db: Arc<dyn HostDb>,
    source: String,
}

impl AuditService {
    pub fn new(db: Arc<dyn HostDb>, source: String) -> Self {
        Self { db, source }
    }

    /// Record an action. A UUID-shaped `identity` is written to
    /// `core.audit_log.user_id` only when the matching `core.users` row actually
    /// exists: `user_id` is a foreign key, so binding a UUID with no row aborts
    /// the whole INSERT and loses the audit entry. When it is absent (the
    /// dev-header stub, a deleted user, or a non-UUID stub id) the FK stays NULL
    /// and the actor is recorded in `details.user_id` instead, so attribution is
    /// never lost. Non-object `details` are coerced to an object for the same
    /// reason.
    pub async fn log(
        &self,
        identity: Option<&Identity>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        details: Value,
    ) -> Result<(), SdkError> {
        // Coerce to an object first: `details.user_id` is the fallback actor, and
        // a JSON null or scalar would otherwise have nowhere to hold it.
        let mut details = match details {
            Value::Object(map) => Value::Object(map),
            other => {
                let mut map = serde_json::Map::new();
                if !other.is_null() {
                    map.insert("details".into(), other);
                }
                Value::Object(map)
            }
        };

        let actor = identity.map(|i| i.user_id.as_str());
        // Does the UUID exist in `core.users`? Binding a missing FK would abort
        // the INSERT, so check first and fall back to details attribution.
        let actor_exists = match actor {
            Some(id) if uuid::Uuid::parse_str(id).is_ok() => {
                let rows = self
                    .db
                    .query(
                        "SELECT id FROM core.users WHERE id = $1".to_string(),
                        vec![SqlValue::Uuid(id.to_string())],
                    )
                    .await?;
                !rows.is_empty()
            }
            _ => false,
        };
        let user_value = if actor_exists {
            SqlValue::Uuid(actor.expect("checked above").to_string())
        } else {
            if let (Some(id), Some(_)) = (actor, identity) {
                details
                    .as_object_mut()
                    .expect("coerced to an object above")
                    .insert("user_id".into(), Value::String(id.to_string()));
            }
            SqlValue::NullUuid
        };
        let details = serde_json::to_string(&details).unwrap_or_else(|_| "{}".into());
        self.db
            .execute(
                "INSERT INTO core.audit_log (user_id, action, resource_type, resource_id, details, source) \
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6)"
                    .to_string(),
                vec![
                    user_value,
                    action.to_string().into(),
                    resource_type.to_string().into(),
                    resource_id.to_string().into(),
                    SqlValue::Json(details),
                    self.source.clone().into(),
                ],
            )
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP surface (SDK-level types — the core adapts these to Axum, so plugin
// code never depends on a specific HTTP framework version)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
        }
    }
}

/// Framework-neutral request handed to plugin handlers.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    pub method: String,
    pub path: String,
    /// Path captures from a templated route (`/api/missions/{id}` → `id`).
    /// Empty for literal routes.
    pub params: HashMap<String, String>,
    pub query: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub identity: Option<Identity>,
}

impl PluginRequest {
    /// One captured path segment (see `params`).
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Deserialize the body as JSON, mapping errors to `SdkError::BadRequest`.
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, SdkError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| SdkError::BadRequest(format!("invalid JSON body: {e}")))
    }

    // --- route helpers (SDK v0.2) -------------------------------------------

    /// A path capture parsed as an integer.
    ///
    /// The capture is delivered by the core, so a missing one is a route
    /// declaration bug (`Internal`), while an unparsable one is the caller's
    /// input (`BadRequest`) — the two cases are kept apart on purpose, so a
    /// handler can't accidentally answer 400 for its own mistake.
    pub fn int_param(&self, key: &str) -> Result<i64, SdkError> {
        let Some(raw) = self.params.get(key) else {
            return Err(SdkError::Internal(format!(
                "route {} has no {{{key}}} capture — declare it in RouteDefinition::path",
                self.path
            )));
        };
        raw.parse::<i64>().map_err(|_| {
            SdkError::BadRequest(format!("path parameter {key} must be a number, got {raw:?}"))
        })
    }

    /// A query parameter that must be present and non-blank.
    pub fn query_required(&self, key: &str) -> Result<&str, SdkError> {
        self.query_param(key)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| SdkError::BadRequest(format!("{key} query parameter is required")))
    }

    /// An integer query parameter; `None` when absent or unparsable.
    ///
    /// Unparsable is `None` rather than an error: query parameters are filters,
    /// and a filter that cannot be read selects nothing. Use
    /// [`query_required`](Self::query_required) plus `str::parse` when a bad
    /// value must be a 400.
    pub fn query_int(&self, key: &str) -> Option<i64> {
        self.query_param(key).and_then(|v| v.trim().parse::<i64>().ok())
    }

    /// A truthy query parameter (`1`, `true`, `yes`, `on`, case-insensitive).
    ///
    /// `?include_inactive=0` and `?include_inactive=false` are both false — a
    /// bare `is_some()` is the trap this closes.
    pub fn query_bool(&self, key: &str) -> bool {
        self.query_param(key).is_some_and(|v| {
            matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        })
    }
}

/// Framework-neutral response produced by plugin handlers.
#[derive(Debug, Clone)]
pub struct PluginResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl PluginResponse {
    pub fn json<T: Serialize>(status: u16, value: &T) -> Result<Self, SdkError> {
        let body = serde_json::to_vec(value)
            .map_err(|e| SdkError::Internal(format!("response serialization failed: {e}")))?;
        Ok(Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        })
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: body.into().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self { status, headers: vec![], body: vec![] }
    }

    /// `204 No Content`.
    pub fn no_content() -> Self {
        Self::empty(204)
    }

    /// `302 Found` redirect to `location` (OIDC login, post-login bounce).
    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("location".to_string(), location.to_string())],
            body: Vec::new(),
        }
    }

    /// `201 Created`, JSON body, and a `location` header.
    pub fn created<T: Serialize>(location: &str, value: &T) -> Result<Self, SdkError> {
        let mut resp = Self::json(201, value)?;
        resp.headers.push(("location".to_string(), location.to_string()));
        Ok(resp)
    }

    /// Add a header, returning the response for chaining.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn error(status: u16, message: impl Into<String>) -> Result<Self, SdkError> {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }
}

/// Plugin-side route handler: `Fn(PluginRequest) -> Future<Result<PluginResponse, SdkError>>`.
pub type RouteHandler =
    Arc<dyn Fn(PluginRequest) -> BoxFuture<'static, Result<PluginResponse, SdkError>> + Send + Sync>;

/// Wrap an async closure into a [`RouteHandler`].
pub fn route_handler<F, Fut>(f: F) -> RouteHandler
where
    F: Fn(PluginRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<PluginResponse, SdkError>> + Send + 'static,
{
    Arc::new(move |req| Box::pin(f(req)))
}

/// A route a plugin registers with the core. `path` MUST live under
/// `/api/{plugin_id}/…` — the core validates this and rejects namespace
/// escapes at load time.
///
/// A segment may be a capture: `/api/missions/{id}` matches
/// `/api/missions/42` and delivers `id = "42"` in `PluginRequest::params`.
/// A capture occupies a whole segment (one path component, never a slash), is
/// **percent-decoded exactly once** before it reaches the handler, and literal
/// routes are matched before templated ones. Two templates of the same shape
/// (`{a}` and `{b}` in the same position) are rejected at load as duplicates.
///
/// ## Declared reach
///
/// `required_scope` says what the core gate demands of the caller's grant:
/// `Some(scope)` — the grant must **cover** that scope (the ordinary
/// `*_protected` constructors pass [`Scope::troop`]); `None` — the caller must
/// hold the permission at *some* scope and the handler must check the object's
/// scope (`*_protected_any_scope`). `delete` always requires a troop-covering
/// grant, even from an `any_scope` constructor.
pub struct RouteDefinition {
    pub method: Method,
    pub path: String,
    pub required_permission: Option<String>,
    pub required_scope: Option<Scope>,
    pub handler: RouteHandler,
}

impl RouteDefinition {
    fn open(method: Method, path: &str, handler: RouteHandler) -> Self {
        Self { method, path: path.into(), required_permission: None, required_scope: None, handler }
    }

    fn protected(method: Method, path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method,
            path: path.into(),
            required_permission: Some(permission.into()),
            required_scope: Some(Scope::troop()),
            handler,
        }
    }

    fn protected_any_scope(
        method: Method,
        path: &str,
        permission: &str,
        handler: RouteHandler,
    ) -> Self {
        // Destructive routes are never available from a scoped grant (§8 #3):
        // an `any_scope` delete still requires troop coverage.
        let required_scope = if method == Method::Delete {
            Some(Scope::troop())
        } else {
            None
        };
        Self {
            method,
            path: path.into(),
            required_permission: Some(permission.into()),
            required_scope,
            handler,
        }
    }

    pub fn get(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Get, path, handler)
    }

    pub fn get_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected(Method::Get, path, permission, handler)
    }

    /// Like [`get_protected`](Self::get_protected) but the caller only needs the
    /// permission at *some* scope; the handler MUST check the object's scope
    /// (with [`PermissionService::has_in_scope`] / `reach`).
    pub fn get_protected_any_scope(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected_any_scope(Method::Get, path, permission, handler)
    }

    pub fn post(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Post, path, handler)
    }

    pub fn post_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected(Method::Post, path, permission, handler)
    }

    /// See [`get_protected_any_scope`](Self::get_protected_any_scope).
    pub fn post_protected_any_scope(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected_any_scope(Method::Post, path, permission, handler)
    }

    pub fn put(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Put, path, handler)
    }

    pub fn put_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected(Method::Put, path, permission, handler)
    }

    /// See [`get_protected_any_scope`](Self::get_protected_any_scope).
    pub fn put_protected_any_scope(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected_any_scope(Method::Put, path, permission, handler)
    }

    pub fn patch(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Patch, path, handler)
    }

    pub fn patch_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected(Method::Patch, path, permission, handler)
    }

    /// See [`get_protected_any_scope`](Self::get_protected_any_scope).
    pub fn patch_protected_any_scope(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected_any_scope(Method::Patch, path, permission, handler)
    }

    pub fn delete(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Delete, path, handler)
    }

    pub fn delete_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected(Method::Delete, path, permission, handler)
    }

    /// A destructive route is **never** scope-any: this exists so a plugin that
    /// reaches for the `any_scope` form gets troop coverage, not a hole (§8 #3).
    pub fn delete_protected_any_scope(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self::protected_any_scope(Method::Delete, path, permission, handler)
    }

    pub fn head(path: &str, handler: RouteHandler) -> Self {
        Self::open(Method::Head, path, handler)
    }
}

/// A database migration. Runs exactly once, in version order, inside the
/// plugin's schema (`search_path` pre-set by the core), recorded in
/// `core.schema_migrations`. Plain data — the core executes the SQL.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    pub name: String,
    pub sql: String,
}

impl Migration {
    pub fn new(version: i64, name: &str, sql: &str) -> Self {
        Self { version, name: name.into(), sql: sql.into() }
    }
}

// ---------------------------------------------------------------------------
// Plugin context & trait
// ---------------------------------------------------------------------------

/// Everything the core hands a plugin at `init`. Cheap to clone; lives as long
/// as the plugin instance. Every service here is a trait object implemented in
/// the core — see the host-mediated I/O note at the top.
#[derive(Clone)]
pub struct PluginContext {
    pub plugin_id: String,
    pub db: DbHandle,
    pub config: Value,
    pub events: EventBusHandle,
    pub permissions: PermissionService,
    pub audit: AuditService,
    /// Register this plugin as the request identity provider (auth plugin).
    pub identity: Arc<dyn IdentityRegistrar>,
    /// Core-mediated HTTP (OIDC discovery, token exchange, …).
    pub http: Arc<dyn HostHttp>,
}

/// The plugin contract. Object-safe so the core can hold `Box<dyn AdjutantPlugin>`.
///
/// Lifecycle (enforced by the core's registry): construct → `init` →
/// `routes` / `migrations` / `permissions_granted` / `subscriptions` are read
/// → serve → `shutdown`.
#[async_trait]
pub trait AdjutantPlugin: Send + Sync {
    /// Stable identifier. Lowercase `[a-z][a-z0-9_]{0,30}` — doubles as the
    /// plugin's PostgreSQL schema and URL namespace (`/api/{id}/…`).
    fn id(&self) -> &str;

    /// Human-readable name for the admin surface.
    fn name(&self) -> &str;

    fn version(&self) -> &str;

    /// Receive core services. Called exactly once, before any other method
    /// (except `id`/`name`/`version`).
    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError>;

    /// HTTP routes. Paths must start with `/api/{id}`.
    fn routes(&self) -> Vec<RouteDefinition>;

    /// SQL migrations, ascending by `version`. Run once on first load.
    fn migrations(&self) -> Vec<Migration> {
        Vec::new()
    }

    /// Permissions this plugin defines. Registered into `core.permissions`.
    fn permissions_granted(&self) -> Vec<Permission> {
        Vec::new()
    }

    /// Event subscriptions. Registered after `init` (handlers may capture the
    /// context stored there).
    fn subscriptions(&self) -> Vec<EventSubscription> {
        Vec::new()
    }

    /// Scheduled work. The core starts each schedule when the plugin loads and
    /// stops it on disable/uninstall/reload; each run is on the plugin's own
    /// pool (isolation role) with a per-run timeout, and is recorded in
    /// `core.scheduled_runs`.
    fn schedules(&self) -> Vec<Schedule> {
        Vec::new()
    }

    /// Called on graceful shutdown, before the plugin instance is dropped.
    async fn shutdown(&mut self) -> Result<(), SdkError> {
        Ok(())
    }
}

/// Entry-point signature the core looks up in a loaded `cdylib`.
///
/// The trait object is intentionally not FFI-safe by C's definition: core and
/// plugin are the same crate graph (same toolchain + versions) — documented in
/// the native-loading caveat above.
#[allow(improper_ctypes_definitions)]
pub type PluginFactory = unsafe extern "C" fn() -> *mut dyn AdjutantPlugin;

/// Symbol the core resolves in every plugin library.
pub const ENTRY_SYMBOL: &[u8] = b"adjutant_plugin_create";

/// Export a plugin type as the library's entry point.
///
/// ```rust,ignore
/// export_plugin!(MyPlugin);   // requires `MyPlugin::new() -> MyPlugin`
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($t:ty) => {
        #[no_mangle]
        pub extern "C" fn adjutant_sdk_abi() -> u32 {
            $crate::SDK_ABI_VERSION
        }

        #[no_mangle]
        #[allow(improper_ctypes_definitions)]
        pub extern "C" fn adjutant_plugin_create() -> *mut dyn $crate::AdjutantPlugin {
            Box::into_raw(Box::new(<$t>::new()))
        }
    };
}

// ---------------------------------------------------------------------------
// Prelude
// ---------------------------------------------------------------------------

/// One import for plugin authors: `use adjutant_sdk::prelude::*;`
pub mod prelude {
    pub use crate::{
        async_trait, event_handler, event_type, export_plugin, route_handler, schedule_handler,
        AdjutantPlugin, AuditService, DbHandle, Event, EventBusHandle, EventSubscription, HostDb,
        HostEvents, HostHttp, HttpResponse, Identity, IdentityProvider, IdentityRegistrar, Method,
        Migration, MissionCompleted, MotionFailed, MotionPassed, Permission, PermissionScope,
        PermissionService, PluginContext, PluginRequest, PluginResponse, RoleGrant, RouteDefinition,
        Schedule, ScheduleHandler, Scope, ScopeType, SdkError, SqlValue,
    };
}

// ---------------------------------------------------------------------------
// Testing support
// ---------------------------------------------------------------------------

/// In-memory host implementations and request builders so a plugin can
/// unit-test its handlers and lifecycle without a database, a server, or a core.
///
/// ```
/// use adjutant_sdk::prelude::*;
/// use adjutant_sdk::testing::*;
///
/// # #[tokio::main] async fn main() {
/// let host = TestHost::new();
/// host.db.push_rows(vec![serde_json::json!({ "id": 1, "message": "hi" })]);
/// let ctx = host.context("greetings");
///
/// let list = route_handler(move |_req: PluginRequest| {
///     let ctx = ctx.clone();
///     async move {
///         let rows = ctx.db.query("SELECT * FROM greetings", vec![]).await?;
///         PluginResponse::json(200, &serde_json::json!({ "greetings": rows }))
///     }
/// });
///
/// let resp = list(TestRequest::get("/api/greetings").build()).await.unwrap();
/// assert_eq!(resp.status, 200);
/// assert_eq!(response_json(&resp)["greetings"][0]["message"], "hi");
/// # }
/// ```
pub mod testing {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    /// A recorded call to [`HostDb`].
    #[derive(Debug, Clone)]
    pub struct DbCall {
        pub sql: String,
        pub params: Vec<SqlValue>,
    }

    impl DbCall {
        /// True when every `needle` appears in the SQL (order-independent).
        pub fn sql_contains(&self, needles: &[&str]) -> bool {
            needles.iter().all(|n| self.sql.contains(n))
        }
    }

    /// Records database calls and replays queued results.
    ///
    /// With no queued result, `execute` returns `Ok(1)` and `query` returns
    /// `Ok(vec![])`, so a handler can be exercised without arranging returns for
    /// calls it makes incidentally.
    #[derive(Default)]
    pub struct MockDb {
        pub executed: Mutex<Vec<DbCall>>,
        pub queried: Mutex<Vec<DbCall>>,
        executes: Mutex<VecDeque<Result<u64, SdkError>>>,
        queries: Mutex<VecDeque<Result<Vec<Value>, SdkError>>>,
    }

    impl MockDb {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue the result of the next `execute` call.
        pub fn push_execute(&self, result: Result<u64, SdkError>) {
            self.executes.lock().unwrap().push_back(result);
        }

        /// Queue the result of the next `query` call.
        pub fn push_query(&self, result: Result<Vec<Value>, SdkError>) {
            self.queries.lock().unwrap().push_back(result);
        }

        /// Convenience: queue a successful `query` returning `rows`.
        pub fn push_rows(&self, rows: Vec<Value>) {
            self.push_query(Ok(rows));
        }

        /// Every `execute` SQL string, in order.
        pub fn executed_sql(&self) -> Vec<String> {
            self.executed.lock().unwrap().iter().map(|c| c.sql.clone()).collect()
        }

        /// Number of `query` calls so far.
        pub fn query_count(&self) -> usize {
            self.queried.lock().unwrap().len()
        }

        /// Every `query` SQL string, in order.
        pub fn queried_sql(&self) -> Vec<String> {
            self.queried.lock().unwrap().iter().map(|c| c.sql.clone()).collect()
        }

        /// Assert some executed statement contains every needle, failing with
        /// the statements that did run — the "the UPDATE never fired" check.
        pub fn assert_executed(&self, needles: &[&str]) {
            let calls = self.executed.lock().unwrap();
            let hit = calls.iter().any(|c| c.sql_contains(needles));
            assert!(
                hit,
                "no executed statement contained {needles:?}; executed: {:#?}",
                calls.iter().map(|c| c.sql.clone()).collect::<Vec<_>>()
            );
        }

        /// The bind parameters of the last `execute` whose SQL contains
        /// `needle` (a shortcut for asserting a typed null or a uuid binding).
        pub fn last_execute_params(&self, needle: &str) -> Option<Vec<SqlValue>> {
            self.executed
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|c| c.sql.contains(needle))
                .map(|c| c.params.clone())
        }

        /// The bind parameters of the last `query` whose SQL contains `needle`.
        ///
        /// `INSERT … RETURNING` is a **query** on this host (it returns rows),
        /// which is exactly the kind of thing a test should not have to
        /// remember: reach for the SQL that matches what the handler does.
        pub fn last_query_params(&self, needle: &str) -> Option<Vec<SqlValue>> {
            self.queried
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|c| c.sql.contains(needle))
                .map(|c| c.params.clone())
        }
    }

    #[async_trait]
    impl HostDb for MockDb {
        async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
            self.executed.lock().unwrap().push(DbCall { sql, params });
            self.executes.lock().unwrap().pop_front().unwrap_or(Ok(1))
        }

        async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
            self.queried.lock().unwrap().push(DbCall { sql, params });
            self.queries.lock().unwrap().pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    /// A recorded event publish.
    #[derive(Debug, Clone)]
    pub struct Published {
        pub event_type: String,
        pub payload: Value,
    }

    /// Records published events and replays queued ones.
    #[derive(Default)]
    pub struct MockEvents {
        pub published: Mutex<Vec<Published>>,
        replay: Mutex<Vec<Event>>,
    }

    impl MockEvents {
        pub fn new() -> Self {
            Self::default()
        }

        /// The `event_type` of every published event, in order.
        pub fn published_types(&self) -> Vec<String> {
            self.published.lock().unwrap().iter().map(|p| p.event_type.clone()).collect()
        }

        /// Queue events for the next `replay` call.
        pub fn push_replay(&self, events: Vec<Event>) {
            self.replay.lock().unwrap().extend(events);
        }

        /// The payload of every event published with `event_type`, in order.
        pub fn payloads(&self, event_type: &str) -> Vec<Value> {
            self.published
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.event_type == event_type)
                .map(|p| p.payload.clone())
                .collect()
        }

        /// Assert at least one `event_type` was published, failing with the
        /// types that *were* published (the usual "why did my event not fire?").
        pub fn assert_published(&self, event_type: &str) {
            assert!(
                !self.payloads(event_type).is_empty(),
                "expected a {event_type:?} event; published: {:?}",
                self.published_types()
            );
        }

        /// Assert nothing was published at all.
        pub fn assert_none(&self) {
            assert!(
                self.published.lock().unwrap().is_empty(),
                "expected no events; published: {:?}",
                self.published_types()
            );
        }
    }

    #[async_trait]
    impl HostEvents for MockEvents {
        async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError> {
            self.published.lock().unwrap().push(Published { event_type, payload });
            Ok(())
        }

        async fn replay(&self, _since_id: i64, _limit: i64) -> Result<Vec<Event>, SdkError> {
            let mut guard = self.replay.lock().unwrap();
            let out = guard.clone();
            guard.clear();
            Ok(out)
        }
    }

    /// A queued HTTP response: status, headers, body.
    type QueuedResponse = (u16, HashMap<String, String>, Vec<u8>);

    /// Records host-mediated HTTP calls and replays queued responses. A call
    /// with no queued response fails (so a handler that unexpectedly reaches the
    /// network is caught, not silently allowed).
    #[derive(Default)]
    pub struct MockHttp {
        pub requests: Mutex<Vec<(String, String)>>,
        responses: Mutex<VecDeque<QueuedResponse>>,
    }

    impl MockHttp {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue a JSON response for the next host HTTP call.
        pub fn push_json(&self, status: u16, body: &Value) {
            let bytes = serde_json::to_vec(body).unwrap_or_default();
            self.responses.lock().unwrap().push_back((status, HashMap::new(), bytes));
        }

        /// Queue a raw response with headers (e.g. a `location` redirect).
        pub fn push_raw(&self, status: u16, headers: &[(&str, &str)], body: Vec<u8>) {
            let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            self.responses.lock().unwrap().push_back((status, h, body));
        }

        /// `(method, url)` of every request, in order.
        pub fn request_urls(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HostHttp for MockHttp {
        async fn request(
            &self,
            method: String,
            url: String,
            _headers: Vec<(String, String)>,
            _body: Option<(String, Vec<u8>)>,
        ) -> Result<HttpResponse, SdkError> {
            self.requests.lock().unwrap().push((method, url));
            match self.responses.lock().unwrap().pop_front() {
                Some((status, headers, body)) => Ok(HttpResponse { status, headers, body }),
                None => Err(SdkError::Internal("MockHttp: no queued response".into())),
            }
        }
    }

    /// Records identity providers registered through `PluginContext::identity`.
    #[derive(Default)]
    pub struct MockIdentity {
        pub providers: Mutex<Vec<(String, Arc<dyn IdentityProvider>)>>,
    }

    impl MockIdentity {
        pub fn new() -> Self {
            Self::default()
        }

        /// Owner keys registered so far.
        pub fn owners(&self) -> Vec<String> {
            self.providers.lock().unwrap().iter().map(|(o, _)| o.clone()).collect()
        }
    }

    impl IdentityRegistrar for MockIdentity {
        fn register(&self, owner: &str, provider: Arc<dyn IdentityProvider>) {
            self.providers.lock().unwrap().push((owner.to_string(), provider));
        }
    }

    /// Bundles the mocks and builds a real [`PluginContext`] around them.
    pub struct TestHost {
        pub db: Arc<MockDb>,
        pub events: Arc<MockEvents>,
        pub http: Arc<MockHttp>,
        pub identity: Arc<MockIdentity>,
        pub config: Value,
    }

    impl TestHost {
        pub fn new() -> Self {
            Self {
                db: Arc::new(MockDb::new()),
                events: Arc::new(MockEvents::new()),
                http: Arc::new(MockHttp::new()),
                identity: Arc::new(MockIdentity::new()),
                config: Value::Null,
            }
        }

        pub fn with_config(mut self, config: Value) -> Self {
            self.config = config;
            self
        }

        /// Build the context a plugin receives from the core at `init`.
        pub fn context(&self, plugin_id: &str) -> PluginContext {
            let db: Arc<dyn HostDb> = self.db.clone();
            PluginContext {
                plugin_id: plugin_id.to_string(),
                db: DbHandle::new(db.clone(), plugin_id.to_string()),
                config: self.config.clone(),
                events: EventBusHandle::new(self.events.clone(), plugin_id.to_string()),
                permissions: PermissionService::new(db.clone()),
                audit: AuditService::new(db, plugin_id.to_string()),
                identity: self.identity.clone(),
                http: self.http.clone(),
            }
        }
    }

    impl Default for TestHost {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Build a [`PluginRequest`] for a handler test.
    pub struct TestRequest {
        req: PluginRequest,
    }

    impl TestRequest {
        pub fn method(method: &str, path: &str) -> Self {
            Self {
                req: PluginRequest {
                    method: method.to_string(),
                    path: path.to_string(),
                    params: HashMap::new(),
                    query: Vec::new(),
                    headers: HashMap::new(),
                    body: Vec::new(),
                    identity: None,
                },
            }
        }

        pub fn get(path: &str) -> Self {
            Self::method("GET", path)
        }
        pub fn post(path: &str) -> Self {
            Self::method("POST", path)
        }
        pub fn put(path: &str) -> Self {
            Self::method("PUT", path)
        }
        pub fn patch(path: &str) -> Self {
            Self::method("PATCH", path)
        }
        pub fn delete(path: &str) -> Self {
            Self::method("DELETE", path)
        }

        /// Set a JSON body and `content-type`.
        pub fn json<T: Serialize>(mut self, value: &T) -> Self {
            self.req.body = serde_json::to_vec(value).unwrap_or_default();
            self.req
                .headers
                .insert("content-type".to_string(), "application/json".to_string());
            self
        }

        /// Add a path capture (what the core extracts from `{name}`).
        pub fn param(mut self, key: &str, value: &str) -> Self {
            self.req.params.insert(key.to_string(), value.to_string());
            self
        }

        /// Add a query parameter.
        pub fn query_param(mut self, key: &str, value: &str) -> Self {
            self.req.query.push((key.to_string(), value.to_string()));
            self
        }

        /// Add a header (name is lowercased, as the core delivers it).
        pub fn header(mut self, key: &str, value: &str) -> Self {
            self.req.headers.insert(key.to_lowercase(), value.to_string());
            self
        }

        /// Attach an authenticated caller (troop-wide roles).
        pub fn identity(mut self, user_id: &str, roles: &[&str]) -> Self {
            self.req.identity = Some(Identity::new(
                user_id,
                roles.iter().map(|r| r.to_string()).collect(),
            ));
            self
        }

        /// Attach an authenticated caller with explicit scoped grants.
        ///
        /// The troop-wide [`identity`](Self::identity) builder cannot express a
        /// lodge- or patrol-scoped grant, which is exactly what an object route's
        /// handler checks — so scoped tests use this:
        ///
        /// ```
        /// # use adjutant_sdk::prelude::*;
        /// # use adjutant_sdk::testing::TestRequest;
        /// let req = TestRequest::post("/api/missions/mission/7/decision")
        ///     .identity_grants(
        ///         "bea",
        ///         vec![RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("3") }],
        ///     )
        ///     .build();
        /// assert_eq!(req.identity.unwrap().roles(), vec!["lodge_commander".to_string()]);
        /// ```
        pub fn identity_grants(mut self, user_id: &str, grants: Vec<RoleGrant>) -> Self {
            self.req.identity = Some(Identity::from_grants(user_id, grants));
            self
        }

        pub fn build(self) -> PluginRequest {
            self.req
        }
    }

    /// Decode a response body as JSON (`Null` when the body is empty or not JSON).
    pub fn response_json(resp: &PluginResponse) -> Value {
        serde_json::from_slice(&resp.body).unwrap_or(Value::Null)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn host_builds_a_context_that_routes_through_the_mocks() {
            let host = TestHost::new();
            host.db.push_rows(vec![serde_json::json!({ "id": 1, "message": "hi" })]);
            let ctx = host.context("greetings");

            let list = crate::route_handler(move |_req: PluginRequest| {
                let ctx = ctx.clone();
                async move {
                    let rows = ctx.db.query("SELECT * FROM greetings", vec![]).await?;
                    PluginResponse::json(200, &serde_json::json!({ "greetings": rows }))
                }
            });

            let resp = list(TestRequest::get("/api/greetings").build()).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(response_json(&resp)["greetings"][0]["message"], "hi");
            assert_eq!(host.db.query_count(), 1);
            assert!(host.db.queried.lock().unwrap()[0].sql_contains(&["SELECT", "greetings"]));
        }

        #[tokio::test]
        async fn mock_http_errors_when_unexpected() {
            let host = TestHost::new();
            let ctx = host.context("auth");
            let err = ctx
                .http
                .request("GET".into(), "https://idp/.well-known/openid-configuration".into(), vec![], None)
                .await
                .unwrap_err();
            assert!(matches!(err, SdkError::Internal(_)));
            assert_eq!(host.http.request_urls().len(), 1);
        }

        #[tokio::test]
        async fn event_publishes_are_observed() {
            let host = TestHost::new();
            let ctx = host.context("hello");
            ctx.events.publish("hello.greeted", serde_json::json!({"m": 1})).await.unwrap();
            assert_eq!(host.events.published_types(), vec!["hello.greeted".to_string()]);
        }

        #[test]
        fn error_statuses_map_consistently() {
            assert_eq!(SdkError::BadRequest("x".into()).status(), 400);
            assert_eq!(SdkError::Unauthorized("x".into()).status(), 401);
            assert_eq!(SdkError::Forbidden("x".into()).status(), 403);
            assert_eq!(SdkError::NotFound("x".into()).status(), 404);
            assert_eq!(SdkError::Conflict("x".into()).status(), 409);
            assert_eq!(SdkError::Internal("x".into()).status(), 500);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stub host: records calls, returns canned rows. Proves plugins can be
    /// exercised without a database or a runtime.
    #[derive(Default)]
    struct StubDb {
        /// `(sql, params)` per call.
        calls: Mutex<Vec<(String, Vec<SqlValue>)>>,
        rows: Vec<Value>,
    }

    #[async_trait]
    impl HostDb for StubDb {
        async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
            self.calls.lock().unwrap().push((sql, params));
            Ok(1)
        }
        async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
            self.calls.lock().unwrap().push((sql, params));
            Ok(self.rows.clone())
        }
    }

    #[derive(Default)]
    struct StubEvents {
        published: Mutex<Vec<(String, Value)>>,
        replayed: Mutex<Vec<(i64, i64)>>,
    }

    #[async_trait]
    impl HostEvents for StubEvents {
        async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError> {
            self.published.lock().unwrap().push((event_type, payload));
            Ok(())
        }

        async fn replay(&self, since_id: i64, limit: i64) -> Result<Vec<Event>, SdkError> {
            self.replayed.lock().unwrap().push((since_id, limit));
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn schedule_handler_wraps_a_closure() {
        let ok = schedule_handler(|| async { Ok(()) });
        assert!((ok)().await.is_ok());
        let fail = schedule_handler(|| async { Err::<(), _>(SdkError::Internal("boom".into())) });
        assert!(matches!((fail)().await, Err(SdkError::Internal(_))));
    }

    #[test]
    fn event_filter_matches_prefix_and_wildcard() {
        let sub = EventSubscription::new("hello.", event_handler(|_| async { Ok(()) }));
        assert!(sub.matches("hello.greeted"));
        assert!(!sub.matches("mission.completed"));

        let all = EventSubscription::new("*", event_handler(|_| async { Ok(()) }));
        assert!(all.matches("anything.at.all"));
    }

    #[test]
    fn plugin_response_json_roundtrip() {
        let resp = PluginResponse::json(200, &serde_json::json!({"ok": true})).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers[0].1, "application/json");
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn db_handle_qualifies_tables() {
        let db = DbHandle::new(Arc::new(StubDb::default()), "missions".into());
        assert_eq!(db.table("items"), "\"missions\".\"items\"");
        assert_eq!(db.schema(), "missions");
    }

    #[test]
    fn sql_value_conversions() {
        assert!(matches!(SqlValue::from("x"), SqlValue::Text(_)));
        assert!(matches!(SqlValue::from(7i64), SqlValue::Int(7)));
        assert!(matches!(SqlValue::from(true), SqlValue::Bool(true)));
        assert!(matches!(
            SqlValue::from(vec!["a".to_string()]),
            SqlValue::TextArray(_)
        ));
    }

    /// Every `Option<T>` conversion must carry the correct *typed* null: a text
    /// null bound to a bigint/bool column is a runtime SQL error, and only the
    /// `Some` path was ever asserted before issue #23.
    #[test]
    fn option_conversions_bind_a_typed_null() {
        assert!(matches!(SqlValue::from(Some("x".to_string())), SqlValue::Text(_)));
        assert!(matches!(SqlValue::from(None::<String>), SqlValue::Null));
        assert!(matches!(SqlValue::from(Some(7i64)), SqlValue::Int(7)));
        assert!(matches!(SqlValue::from(None::<i64>), SqlValue::NullInt));
        assert!(matches!(SqlValue::from(Some(true)), SqlValue::Bool(true)));
        assert!(matches!(SqlValue::from(None::<bool>), SqlValue::NullBool));
    }

    #[tokio::test]
    async fn permission_service_queries_through_host() {
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![serde_json::json!({"n": 1})],
        });
        let svc = PermissionService::new(db.clone());
        let id = Some(Identity::new("chris", vec!["chief".into()]));
        assert!(svc.has_any_scope(id.as_ref(), "hello:read").await);
        // no identity → false without touching the host
        assert!(!svc.has_any_scope(None, "hello:read").await);
        assert_eq!(db.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scopes_for_batches_the_permission_lookup() {
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![
                serde_json::json!({
                    "permission": "missions:approve", "scope_type": "troop", "scope_id": ""
                }),
                serde_json::json!({
                    "permission": "missions:read", "scope_type": "lodge", "scope_id": "l1"
                }),
            ],
        });
        let svc = PermissionService::new(db.clone());
        let id = Identity::from_grants(
            "bea",
            vec![
                RoleGrant { role_id: "chief".into(), scope: Scope::troop() },
                RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("l1") },
            ],
        );

        let held = svc
            .scopes_for(Some(&id), &["missions:read", "missions:approve"])
            .await
            .unwrap();

        assert_eq!(
            held,
            vec![
                PermissionScope {
                    permission: "missions:approve".into(),
                    scope: Scope::troop()
                },
                PermissionScope {
                    permission: "missions:read".into(),
                    scope: Scope::lodge("l1")
                },
            ]
        );

        // One query for two permissions across two grants — the whole point.
        let calls = db.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (sql, params) = &calls[0];
        assert!(sql.contains("core.role_permissions"), "{sql}");
        assert!(sql.contains("ORDER BY"), "the answer is deterministic: {sql}");
        assert_eq!(
            params.len(),
            4,
            "roles, scope types, scope ids, and the permissions asked about"
        );
    }

    #[tokio::test]
    async fn scopes_for_spends_no_query_when_there_is_nothing_to_ask() {
        let db = Arc::new(StubDb { calls: Mutex::new(vec![]), rows: vec![] });
        let svc = PermissionService::new(db.clone());

        // No identity at all.
        assert!(svc.scopes_for(None, &["missions:read"]).await.unwrap().is_empty());
        // An identity that holds nothing.
        let bare = Identity::from_grants("bea", vec![]);
        assert!(svc
            .scopes_for(Some(&bare), &["missions:read"])
            .await
            .unwrap()
            .is_empty());
        // Nothing asked about.
        let id = Identity::new("bea", vec!["chief".into()]);
        assert!(svc.scopes_for(Some(&id), &[]).await.unwrap().is_empty());

        assert_eq!(
            db.calls.lock().unwrap().len(),
            0,
            "an unauthenticated caller holds nothing; confirming that costs a round trip and buys none"
        );
    }

    #[tokio::test]
    async fn scopes_for_fails_closed_on_a_grant_with_no_scope_id() {
        let db = Arc::new(StubDb { calls: Mutex::new(vec![]), rows: vec![] });
        let svc = PermissionService::new(db.clone());
        let id = Identity::from_grants(
            "bea",
            vec![
                // A lodge grant that names no lodge: dropped rather than widened.
                RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("") },
                RoleGrant { role_id: "chief".into(), scope: Scope::troop() },
            ],
        );

        assert!(svc
            .scopes_for(Some(&id), &["missions:read"])
            .await
            .unwrap()
            .is_empty());

        let calls = db.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        match &calls[0].1[0] {
            SqlValue::TextArray(roles) => assert_eq!(
                roles,
                &vec!["chief".to_string()],
                "only the grant that names a scope reached the database"
            ),
            other => panic!("expected the roles as a text array, got {other:?}"),
        }
    }

    #[test]
    fn scope_cover_is_troop_wide_or_exact() {
        assert!(Scope::troop().covers(&Scope::lodge("l1")));
        assert!(Scope::troop().covers(&Scope::troop()));
        assert!(Scope::troop().covers(&Scope::patrol("p1")));
        assert!(Scope::lodge("l1").covers(&Scope::lodge("l1")));
        assert!(!Scope::lodge("l1").covers(&Scope::lodge("l2")));
        assert!(!Scope::lodge("l1").covers(&Scope::troop()));
        assert!(!Scope::patrol("p1").covers(&Scope::lodge("p1")));
    }

    #[test]
    fn roles_covering_filters_by_scope() {
        let id = Identity::from_grants(
            "bea",
            vec![
                RoleGrant { role_id: "chief".into(), scope: Scope::troop() },
                RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("l1") },
            ],
        );
        assert_eq!(id.roles_covering(&Scope::lodge("l2")), vec!["chief"]);
        let mut at_l1 = id.roles_covering(&Scope::lodge("l1"));
        at_l1.sort();
        assert_eq!(at_l1, vec!["chief", "lodge_commander"]);
        // roles is derived from grants, sorted and deduplicated.
        assert_eq!(id.roles(), vec!["chief", "lodge_commander"]);
    }

    /// A patrol grant covers its patrol and no sibling (the matrix's patrol row).
    #[test]
    fn patrol_grant_covers_only_its_patrol() {
        let id = Identity::from_grants(
            "bea",
            vec![RoleGrant { role_id: "pl".into(), scope: Scope::patrol("p1") }],
        );
        assert_eq!(id.roles_covering(&Scope::patrol("p1")), vec!["pl"]);
        assert!(id.roles_covering(&Scope::patrol("p2")).is_empty());
        assert!(
            id.roles_covering(&Scope::lodge("p1")).is_empty(),
            "a patrol grant does not cover a lodge"
        );
    }

    /// `grants` is the source of truth: an identity payload without them fails
    /// loudly instead of silently holding no scopes.
    #[test]
    fn identity_requires_grants_to_deserialize() {
        let ok: Identity = serde_json::from_str(
            r#"{"user_id":"bea","grants":[{"role_id":"scout","scope":{"scope_type":"troop","scope_id":null}}]}"#,
        )
        .expect("grants present");
        assert_eq!(ok.roles(), vec!["scout"]);

        let missing = serde_json::from_str::<Identity>(r#"{"user_id":"bea"}"#);
        assert!(missing.is_err(), "an identity without grants must not deserialize");
    }

    /// `delete` requires troop coverage from every constructor (§8 #3).
    #[test]
    fn delete_routes_always_require_troop() {
        let handler = route_handler(|_: PluginRequest| async {
            PluginResponse::json(200, &serde_json::json!({}))
        });
        let d = RouteDefinition::delete_protected("/api/x/y", "x:write", handler.clone());
        assert_eq!(d.required_scope, Some(Scope::troop()));

        // Even the permissive constructor forces troop for a delete.
        let da = RouteDefinition::delete_protected_any_scope("/api/x/y", "x:write", handler.clone());
        assert_eq!(
            da.required_scope,
            Some(Scope::troop()),
            "a destructive route must never be scope-any"
        );

        // The safe default is troop: a plain protected route requires troop.
        let g = RouteDefinition::get_protected("/api/x/y", "x:read", handler.clone());
        assert_eq!(g.required_scope, Some(Scope::troop()));
        // The permissive form is explicit and greppable.
        let a = RouteDefinition::get_protected_any_scope("/api/x/y", "x:read", handler);
        assert_eq!(a.required_scope, None);
    }

    #[tokio::test]
    async fn scoped_permission_check_uses_covering_roles() {
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![serde_json::json!({"n": 1})],
        });
        let svc = PermissionService::new(db.clone());
        let id = Identity::from_grants(
            "bea",
            vec![RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("l1") }],
        );
        assert!(svc.has_in_scope(Some(&id), "x", &Scope::lodge("l1")).await);
        // A different lodge has no covering role: false without a query.
        let before = db.calls.lock().unwrap().len();
        assert!(!svc.has_in_scope(Some(&id), "x", &Scope::lodge("l2")).await);
        assert_eq!(db.calls.lock().unwrap().len(), before);
    }

    /// `reach` is the safe-path helper: it returns a ready 403 naming the scope.
    #[tokio::test]
    async fn reach_returns_forbidden_naming_the_scope() {
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![serde_json::json!({"n": 1})],
        });
        let svc = PermissionService::new(db);
        let id = Identity::from_grants(
            "bea",
            vec![RoleGrant { role_id: "lc".into(), scope: Scope::lodge("l1") }],
        );
        assert!(svc.reach(Some(&id), "missions:approve", &Scope::lodge("l1")).await.is_ok());
        let err = svc
            .reach(Some(&id), "missions:approve", &Scope::lodge("l2"))
            .await
            .expect_err("another lodge is refused");
        assert!(matches!(err, SdkError::Forbidden(_)));
        assert_eq!(err.status(), 403);
        assert!(err.to_string().contains("missions:approve"), "got: {err}");
    }

    #[tokio::test]
    async fn event_publish_delegates_to_host() {
        let host = Arc::new(StubEvents::default());
        let handle = EventBusHandle::new(host.clone(), "hello".into());
        handle.publish("hello.greeted", serde_json::json!({"m": 1})).await.unwrap();
        let pubd = host.published.lock().unwrap();
        assert_eq!(pubd.len(), 1);
        assert_eq!(pubd[0].0, "hello.greeted");
        assert_eq!(handle.source(), "hello");
    }

    #[tokio::test]
    async fn audit_records_a_real_user_id_in_the_fk_column() {
        let uuid = "11111111-1111-1111-1111-111111111111";
        // The existence check must find the row before the FK is bound.
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![serde_json::json!({ "id": uuid })],
        });
        let audit = AuditService::new(db.clone(), "hello".into());
        let id = Identity::new(uuid, vec!["scout".into()]);
        audit
            .log(Some(&id), "greet", "greeting", "hi", serde_json::json!({}))
            .await
            .unwrap();
        let calls = db.calls.lock().unwrap();
        // One existence query, then the INSERT.
        assert_eq!(calls.len(), 2);
        assert!(calls[0].0.contains("core.users"));
        let (sql, params) = &calls[1];
        assert!(sql.contains("core.audit_log"));
        assert!(
            matches!(params[0], SqlValue::Uuid(ref u) if u.as_str() == uuid),
            "a real user id binds to the FK column"
        );
        // The FK carries the actor, so details must not duplicate it.
        let details = match &params[4] {
            SqlValue::Json(j) => serde_json::from_str::<Value>(j).unwrap(),
            other => panic!("expected JSON details, got {other:?}"),
        };
        assert!(details.get("user_id").is_none());
    }

    #[tokio::test]
    async fn audit_uuid_with_no_users_row_falls_back_to_details() {
        // A UUID-shaped id with no `core.users` row must NOT be bound to the FK —
        // the FK violation would abort the INSERT and lose the whole audit entry.
        // The existence query returns no rows, so attribution moves to details.
        let db = Arc::new(StubDb::default());
        let audit = AuditService::new(db.clone(), "hello".into());
        let uuid = "22222222-2222-2222-2222-222222222222";
        let id = Identity::new(uuid, vec!["chief".into()]);
        audit
            .log(Some(&id), "greet", "greeting", "hi", serde_json::json!({}))
            .await
            .unwrap();
        let calls = db.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "existence query + insert");
        let (_, params) = &calls[1];
        assert!(
            matches!(params[0], SqlValue::NullUuid),
            "a missing core.users row keeps the FK NULL"
        );
        let details = match &params[4] {
            SqlValue::Json(j) => serde_json::from_str::<Value>(j).unwrap(),
            other => panic!("expected JSON details, got {other:?}"),
        };
        assert_eq!(details["user_id"], serde_json::json!(uuid));
    }

    #[tokio::test]
    async fn audit_coerces_non_object_details_so_attribution_survives() {
        // `details = null` (the WASM host default) must not drop the actor: the
        // value is coerced to an object that can carry `user_id`.
        let db = Arc::new(StubDb::default());
        let audit = AuditService::new(db.clone(), "hello".into());
        let id = Identity::new("beatrice", vec!["scout".into()]);
        audit
            .log(Some(&id), "greet", "greeting", "hi", Value::Null)
            .await
            .unwrap();
        let calls = db.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (_, params) = &calls[0];
        let details = match &params[4] {
            SqlValue::Json(j) => serde_json::from_str::<Value>(j).unwrap(),
            other => panic!("expected JSON details, got {other:?}"),
        };
        assert_eq!(details["user_id"], serde_json::json!("beatrice"));
    }

    #[tokio::test]
    async fn audit_keeps_stub_actors_in_details_with_a_null_fk() {
        // The dev-header stub's users don't exist in core.users, so the FK must
        // stay NULL; attribution moves to details instead of being lost.
        let db = Arc::new(StubDb::default());
        let audit = AuditService::new(db.clone(), "hello".into());
        let id = Identity::new("beatrice", vec!["scout".into()]);
        audit
            .log(Some(&id), "greet", "greeting", "hi", serde_json::json!({}))
            .await
            .unwrap();
        let calls = db.calls.lock().unwrap();
        let (_, params) = &calls[0];
        assert!(matches!(params[0], SqlValue::NullUuid), "non-uuid actor → NULL FK");
        let details = match &params[4] {
            SqlValue::Json(j) => serde_json::from_str::<Value>(j).unwrap(),
            other => panic!("expected JSON details, got {other:?}"),
        };
        assert_eq!(details["user_id"], serde_json::json!("beatrice"));
    }

    // --- SDK v0.2 route helpers ---------------------------------------------

    /// A missing capture is the plugin's own declaration bug (500); a bad value
    /// is the caller's (400). Conflating them would hide a broken route behind a
    /// client error.
    #[test]
    fn int_param_separates_a_missing_capture_from_a_bad_value() {
        let req = crate::testing::TestRequest::get("/api/x/7").param("id", "7").build();
        assert_eq!(req.int_param("id").unwrap(), 7);

        let bad = crate::testing::TestRequest::get("/api/x/nope").param("id", "nope").build();
        let err = bad.int_param("id").unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("nope"), "got: {err}");

        let missing = crate::testing::TestRequest::get("/api/x").build();
        let err = missing.int_param("id").unwrap_err();
        assert_eq!(err.status(), 500);
        assert!(err.to_string().contains("capture"), "got: {err}");
    }

    #[test]
    fn query_helpers_read_filters() {
        let req = crate::testing::TestRequest::get("/api/x")
            .query_param("lodge", "windsor")
            .query_param("limit", "12")
            .query_param("include_inactive", "TRUE")
            .query_param("empty", "   ")
            .build();
        assert_eq!(req.query_required("lodge").unwrap(), "windsor");
        assert_eq!(req.query_int("limit"), Some(12));
        assert_eq!(req.query_int("lodge"), None, "a non-numeric filter selects nothing");
        assert!(req.query_bool("include_inactive"));
        assert!(!req.query_bool("empty"), "a blank value is not truthy");
        assert!(!req.query_bool("absent"));
        assert_eq!(req.query_required("empty").unwrap_err().status(), 400);
        assert_eq!(req.query_required("absent").unwrap_err().status(), 400);
    }

    // --- SDK v0.2 event helpers --------------------------------------------

    /// The typed publishers must use the documented event names with the
    /// documented payload shape — that is the whole point of them existing.
    #[tokio::test]
    async fn typed_event_publishers_match_the_documented_contract() {
        let host = crate::testing::TestHost::new();
        let ctx = host.context("missions");

        ctx.events
            .publish_mission_completed(&MissionCompleted {
                mission_id: 12,
                title: "Coyote survey".into(),
                lodge_id: Some("3".into()),
                stage: "report".into(),
                completed_at: Utc::now(),
                impact: serde_json::json!({ "service_hours": 18.5 }),
            })
            .await
            .unwrap();
        ctx.events
            .publish_motion_passed(&MotionPassed {
                motion_id: 4,
                title: "Adopt the 3rd Accords".into(),
                body: "congress".into(),
                meeting_id: Some(2),
                votes_yes: 12,
                votes_no: 3,
                votes_abstain: 1,
                threshold: "two_thirds".into(),
                passed_at: Utc::now(),
                amends_accords: true,
            })
            .await
            .unwrap();

        host.events.assert_published(event_type::MISSION_COMPLETED);
        host.events.assert_published(event_type::MOTION_PASSED);
        let completed = &host.events.payloads("mission.completed")[0];
        assert_eq!(completed["mission_id"], serde_json::json!(12));
        assert_eq!(completed["stage"], serde_json::json!("report"));
        assert_eq!(completed["impact"]["service_hours"], serde_json::json!(18.5));
        // A troop-wide mission omits the lodge rather than publishing null.
        let troop = serde_json::to_value(MissionCompleted {
            mission_id: 1,
            title: "Troop clean-up".into(),
            lodge_id: None,
            stage: "report".into(),
            completed_at: Utc::now(),
            impact: Value::Null,
        })
        .unwrap();
        assert!(troop.get("lodge_id").is_none());

        let passed = &host.events.payloads("motion.passed")[0];
        assert_eq!(passed["threshold"], serde_json::json!("two_thirds"));
        assert_eq!(passed["amends_accords"], serde_json::json!(true));
        let round_trip: MotionPassed =
            serde_json::from_value(passed.clone()).expect("payload deserializes for consumers");
        assert_eq!(round_trip.motion_id, 4);
    }

    // --- SDK v0.2 query helpers --------------------------------------------

    #[tokio::test]
    async fn query_one_and_exists_read_the_host() {
        let host = crate::testing::TestHost::new();
        let ctx = host.context("missions");
        host.db.push_rows(vec![serde_json::json!({ "id": 7, "title": "Survey" })]);
        host.db.push_rows(vec![]);

        let one = ctx.db.query_one("SELECT 1", vec![]).await.unwrap();
        assert_eq!(one.unwrap()["id"], serde_json::json!(7));
        assert!(!ctx.db.exists("SELECT 1", vec![]).await.unwrap());
        assert!(ctx.db.query_one("SELECT 1", vec![]).await.unwrap().is_none());
    }

    // --- SDK v0.2 test-harness assertions ----------------------------------

    #[tokio::test]
    async fn mock_assertions_describe_what_happened() {
        let host = crate::testing::TestHost::new();
        let ctx = host.context("missions");
        ctx.db
            .execute(
                format!("UPDATE {} SET stage = $1 WHERE id = $2", ctx.db.table("missions")),
                vec![SqlValue::Text("execution".into()), SqlValue::Int(3)],
            )
            .await
            .unwrap();
        ctx.events.publish("mission.approved", serde_json::json!({"n": 1})).await.unwrap();

        host.db.assert_executed(&["UPDATE", "missions", "stage"]);
        let params = host.db.last_execute_params("UPDATE").expect("params");
        assert!(matches!(params[0], SqlValue::Text(ref s) if s == "execution"));
        assert_eq!(host.db.queried_sql(), Vec::<String>::new());
        host.events.assert_published("mission.approved");
        assert_eq!(host.events.payloads("mission.approved")[0]["n"], serde_json::json!(1));
        assert!(host.events.payloads("mission.completed").is_empty());
    }

    /// The failure names the event that was expected *and* what was published,
    /// so a wrong event name is one read away instead of a trial-and-error loop.
    #[test]
    #[should_panic(expected = "expected a \"mission.completed\" event")]
    fn assert_published_names_what_was_published_instead() {
        let events = crate::testing::MockEvents::new();
        events.assert_published("mission.completed");
    }
}
