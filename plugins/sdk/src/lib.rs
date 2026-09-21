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

/// Errors that can cross the plugin boundary. Stringly-typed on purpose:
/// concrete error types would pin both sides to identical dependency versions
/// at the type level (they must already match at the ABI level).
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("database error: {0}")]
    Db(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Host services — implemented by the core, never by the plugin
// ---------------------------------------------------------------------------

/// A bind parameter. Deliberately a closed enum: passing sqlx types across the
/// boundary would drag sqlx (and a second tokio) into the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
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

/// Database access, mediated by the core. Rows come back as JSON objects keyed
/// by column name — decode order in the core is jsonb → bool → i64 → f64 →
/// text → NULL, so cast exotic types (`timestamptz`) with `::text` in your SQL.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
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

    /// Table name qualified with this plugin's schema, safe to interpolate (the
    /// schema name is validated by the core: `[a-z][a-z0-9_]{0,30}`).
    pub fn table(&self, name: &str) -> String {
        format!("\"{}\".\"{}\"", self.schema, name)
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
    Delete,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// Framework-neutral request handed to plugin handlers.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub identity: Option<Identity>,
}

impl PluginRequest {
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

    pub fn delete(path: &str, handler: RouteHandler) -> Self {
        Self { method: Method::Delete, path: path.into(), required_permission: None, handler }
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
        DbHandle, EventBusHandle, Event, EventSubscription, HostDb, HostEvents, Identity, Method,
        Migration, Permission, PermissionService, PluginContext, PluginRequest, PluginResponse,
        RouteDefinition, SdkError, SqlValue,
    };
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
    }

    #[async_trait]
    impl HostEvents for StubEvents {
        async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError> {
            self.published.lock().unwrap().push((event_type, payload));
            Ok(())
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
