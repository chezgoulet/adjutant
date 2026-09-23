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
pub const SDK_ABI_VERSION: u32 = 1;

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
    fn from(v: Option<i64>) -> Self {
        match v {
            Some(n) => SqlValue::Int(n),
            None => SqlValue::Null,
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

/// The authenticated caller. Produced by the core's auth middleware (Milestone 1
/// uses a `x-dev-user` / `x-dev-role` header stub; the auth plugin replaces it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub user_id: String,
    pub roles: Vec<String>,
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

    pub async fn has(&self, identity: Option<&Identity>, permission: &str) -> bool {
        let Some(id) = identity else { return false };
        if id.roles.is_empty() {
            return false;
        }
        let rows = self
            .db
            .query(
                "SELECT COUNT(*) AS n FROM core.role_permissions \
                 WHERE role_id = ANY($1) AND permission_id = $2"
                    .to_string(),
                vec![id.roles.clone().into(), permission.to_string().into()],
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

    /// `user_id` is recorded inside `details` as a string while the auth plugin
    /// (Milestone 2) is pending — `core.audit_log.user_id` is a nullable FK to
    /// `core.users`, and stub identities don't exist there yet.
    pub async fn log(
        &self,
        identity: Option<&Identity>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        details: Value,
    ) -> Result<(), SdkError> {
        let mut details = details;
        if let (Some(obj), Some(id)) = (details.as_object_mut(), identity) {
            obj.insert("user_id".into(), Value::String(id.user_id.clone()));
        }
        let details = serde_json::to_string(&details).unwrap_or_else(|_| "{}".into());
        self.db
            .execute(
                "INSERT INTO core.audit_log (user_id, action, resource_type, resource_id, details, source) \
                 VALUES (NULL, $1, $2, $3, $4::jsonb, $5)"
                    .to_string(),
                vec![
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
pub struct RouteDefinition {
    pub method: Method,
    pub path: String,
    pub required_permission: Option<String>,
    pub handler: RouteHandler,
}

impl RouteDefinition {
    pub fn get(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Get, path: path.into(), required_permission: None, handler }
    }

    pub fn get_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method: Method::Get,
            path: path.into(),
            required_permission: Some(permission.into()),
            handler,
        }
    }

    pub fn post(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Post, path: path.into(), required_permission: None, handler }
    }

    pub fn post_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method: Method::Post,
            path: path.into(),
            required_permission: Some(permission.into()),
            handler,
        }
    }

    pub fn put(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Put, path: path.into(), required_permission: None, handler }
    }

    pub fn put_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method: Method::Put,
            path: path.into(),
            required_permission: Some(permission.into()),
            handler,
        }
    }

    pub fn patch(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Patch, path: path.into(), required_permission: None, handler }
    }

    pub fn patch_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method: Method::Patch,
            path: path.into(),
            required_permission: Some(permission.into()),
            handler,
        }
    }

    pub fn delete(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Delete, path: path.into(), required_permission: None, handler }
    }

    pub fn delete_protected(path: &str, permission: &str, handler: RouteHandler) -> Self {
        Self {
            method: Method::Delete,
            path: path.into(),
            required_permission: Some(permission.into()),
            handler,
        }
    }

    pub fn head(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Head, path: path.into(), required_permission: None, handler }
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
        async_trait, export_plugin, event_handler, route_handler, AdjutantPlugin, AuditService,
        DbHandle, EventBusHandle, Event, EventSubscription, HostDb, HostEvents, HostHttp,
        HttpResponse, Identity, IdentityProvider, IdentityRegistrar, Method, Migration, Permission,
        PermissionService, PluginContext, PluginRequest, PluginResponse, RouteDefinition, SdkError,
        SqlValue,
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

        /// Attach an authenticated caller.
        pub fn identity(mut self, user_id: &str, roles: &[&str]) -> Self {
            self.req.identity = Some(Identity {
                user_id: user_id.to_string(),
                roles: roles.iter().map(|r| r.to_string()).collect(),
            });
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
        calls: Mutex<Vec<String>>,
        rows: Vec<Value>,
    }

    #[async_trait]
    impl HostDb for StubDb {
        async fn execute(&self, sql: String, _params: Vec<SqlValue>) -> Result<u64, SdkError> {
            self.calls.lock().unwrap().push(sql);
            Ok(1)
        }
        async fn query(&self, sql: String, _params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
            self.calls.lock().unwrap().push(sql);
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

    #[tokio::test]
    async fn permission_service_queries_through_host() {
        let db = Arc::new(StubDb {
            calls: Mutex::new(vec![]),
            rows: vec![serde_json::json!({"n": 1})],
        });
        let svc = PermissionService::new(db.clone());
        let id = Some(Identity { user_id: "chris".into(), roles: vec!["chief".into()] });
        assert!(svc.has(id.as_ref(), "hello:read").await);
        // no identity → false without touching the host
        assert!(!svc.has(None, "hello:read").await);
        assert_eq!(db.calls.lock().unwrap().len(), 1);
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
    async fn audit_log_injects_identity_and_stays_censored() {
        let db = Arc::new(StubDb::default());
        let audit = AuditService::new(db.clone(), "hello".into());
        let id = Identity { user_id: "beatrice".into(), roles: vec!["scout".into()] };
        audit
            .log(Some(&id), "greet", "greeting", "hi", serde_json::json!({}))
            .await
            .unwrap();
        let calls = db.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("core.audit_log"));
        // user_id FK stays NULL; identity lands in details instead
        assert!(calls[0].contains("VALUES (NULL,"));
    }
}
