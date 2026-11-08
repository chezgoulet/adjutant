//! Host implementations of the SDK's `HostDb` / `HostEvents` traits.
//!
//! This is the only place in the system where sqlx runs on behalf of a plugin.
//! The core links sqlx/tokio once; plugins link neither (see the host-mediated
//! I/O note in the SDK). Everything here executes inside core tasks, on the
//! core's runtime — which is exactly why `Handle::current()` resolves.

use std::sync::Arc;

use adjutant_sdk::{
    canonical_created_at, canonical_payload, chain_key, AuditRow, Event, HostDb, HostEvents,
    HostHttp, HttpResponse, SdkError, SqlValue, AUDIT_CHAIN_ADVISORY_LOCK, AUDIT_ZERO_HASH,
};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{Column, Row, TypeInfo};
use sqlx::ValueRef as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::sync::broadcast;

/// Core-backed database access. The **pool** decides the principal:
/// core services use the deployment pool, a plugin uses the pool authenticated
/// as its own `adjutant_plugin_<id>` role (see [`plugin_pool`]). There is no
/// per-call `SET ROLE`/`search_path` — the connection identity is the boundary
/// (`docs/design/plugin-isolation.md`).
pub struct CoreDb {
    pool: Arc<sqlx::PgPool>,
}

impl CoreDb {
    pub fn new(pool: Arc<sqlx::PgPool>) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

/// Build a plugin's pool, authenticated as `adjutant_plugin_<id>` with the
/// stored secret. Connects eagerly so a missing/rotated credential fails the
/// load loudly instead of surfacing as a 500 on the first request.
///
/// `max_connections` is small on purpose (design §3.3): the budget is
/// `plugins × max_connections`; see `docs/deployment.md`.
pub async fn plugin_pool(
    base_url: &str,
    plugin_id: &str,
    secret: &str,
    max_connections: u32,
) -> Result<Arc<sqlx::PgPool>, sqlx::Error> {
    use std::str::FromStr;
    let opts = sqlx::postgres::PgConnectOptions::from_str(base_url)?
        .username(&crate::schema::role_for(plugin_id))
        .password(secret);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await?;
    Ok(Arc::new(pool))
}

/// Decode one column into JSON.
///
/// sqlx requires an **exact** type match, so this is a whitelist: json/jsonb,
/// bool, int8/int4/int2, float8/float4, text[], the chrono date/time family, and
/// text. Anything else (uuid, numeric, bytea, inet, …) must be cast in SQL
/// (`id::text`) — and if it is not, the fallback below logs it rather than
/// returning a silent NULL, which is how a DATE column once came back null from a
/// NOT NULL column in a shipped route.
fn decode_value(row: &PgRow, idx: usize) -> Value {
    // A genuine SQL NULL is data, not a decode failure: return it without
    // attempting the type-specific paths below or warning about them.
    if let Ok(raw) = row.try_get_raw(idx) {
        if raw.is_null() {
            return Value::Null;
        }
    }
    if let Ok(v) = row.try_get::<Value, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return Value::Bool(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<i32, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<i16, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f32, _>(idx) {
        return Value::from(v as f64);
    }
    // text[] — needed for any array column (e.g. core.user_roles roles).
    // Without this, TEXT[] matched none of the above and fell to Null,
    // which silently emptied role lists and broke permission checks.
    if let Ok(v) = row.try_get::<Vec<String>, _>(idx) {
        return Value::Array(v.into_iter().map(Value::String).collect());
    }
    // chrono family (sqlx `chrono` feature). DATE/TIMESTAMP/TIMESTAMPTZ used to
    // fall through to Null unless every query remembered `::text`.
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return Value::String(v.to_rfc3339());
    }
    // uuid — a very common primary/foreign key; decoded to its canonical string
    // form so plugins don't have to remember `id::text` on every query.
    if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
        return Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    if let Some(col) = row.columns().get(idx) {
        tracing::warn!(
            column = %col.name(),
            pg_type = %col.type_info().name(),
            "host cannot represent this column as JSON; returning null (cast it in SQL, e.g. ::text)"
        );
    }
    Value::Null
}

fn bind_params<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    params: Vec<SqlValue>,
) -> Result<sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, SdkError> {
    for p in params {
        q = match p {
            SqlValue::Null => q.bind(Option::<String>::None),
            // Typed nulls: a text NULL cannot be assigned to a bigint/bool/uuid
            // column, and casting a bare parameter would make Postgres infer it
            // as text.
            SqlValue::NullInt => q.bind(Option::<i64>::None),
            SqlValue::NullBool => q.bind(Option::<bool>::None),
            SqlValue::NullUuid => q.bind(Option::<uuid::Uuid>::None),
            SqlValue::Bool(b) => q.bind(b),
            SqlValue::Int(n) => q.bind(n),
            SqlValue::Float(f) => q.bind(f),
            SqlValue::Text(s) => q.bind(s),
            // Parse at the boundary: a malformed uuid is the caller's fault (400),
            // not a 500 from a failed SQL bind.
            SqlValue::Uuid(s) => {
                let u = uuid::Uuid::parse_str(&s)
                    .map_err(|e| SdkError::BadRequest(format!("invalid uuid {s:?}: {e}")))?;
                q.bind(u)
            }
            SqlValue::IntArray(v) => q.bind(v),
            SqlValue::TextArray(v) => q.bind(v),
            SqlValue::Json(j) => q.bind(j),
        };
    }
    Ok(q)
}

#[async_trait]
impl HostDb for CoreDb {
    async fn execute(&self, sql: String, params: Vec<SqlValue>) -> Result<u64, SdkError> {
        let res = bind_params(sqlx::query(&sql), params)?
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;
        Ok(res.rows_affected())
    }

    async fn query(&self, sql: String, params: Vec<SqlValue>) -> Result<Vec<Value>, SdkError> {
        let rows = bind_params(sqlx::query(&sql), params)?
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| SdkError::Db(format!("{e} (sql: {sql})")))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let cols = row.columns();
            let mut obj = serde_json::Map::with_capacity(cols.len());
            for (i, col) in cols.iter().enumerate() {
                obj.insert(col.name().to_string(), decode_value(row, i));
            }
            out.push(Value::Object(obj));
        }
        Ok(out)
    }

    /// Appending an audit row is the one place the SDK needs several statements
    /// to agree. Without a key it is the plain INSERT. With a key it is one
    /// transaction:
    ///
    /// 1. take the same advisory lock the SQL trigger takes (`CHAIN_ADVISORY_LOCK`);
    /// 2. read the previous `entry_hash`;
    /// 3. allocate the id the row will carry;
    /// 4. make PostgreSQL render `details` as `jsonb::text` (the exact bytes the
    ///    trigger will hash) and pick a microsecond-precision timestamp;
    /// 5. compute `HMAC(key, canonical_payload)` in Rust — the key is never sent
    ///    to PostgreSQL, not even as a bind parameter;
    /// 6. INSERT the row with `id`, `created_at` and `keyed_hash` already set.
    ///
    /// The trigger fills `prev_hash`/`entry_hash` from the same lock and sees
    /// the same id and timestamp, so both hashes describe the same row. There is
    /// no UPDATE: the append-only trigger would (correctly) refuse it.
    async fn append_audit(&self, row: AuditRow) -> Result<(), SdkError> {
        let Some(key) = chain_key() else {
            let (sql, params) = row.unkeyed_insert();
            return self.execute(sql, params).await.map(|_| ());
        };

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SdkError::Db(format!("audit transaction: {e}")))?;

        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(AUDIT_CHAIN_ADVISORY_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(|e| SdkError::Db(format!("audit chain lock: {e}")))?;

        let prev: Option<String> = sqlx::query_scalar(
            "SELECT entry_hash FROM core.audit_log ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| SdkError::Db(format!("audit previous hash: {e}")))?
        .flatten();
        let prev = prev.unwrap_or_else(|| AUDIT_ZERO_HASH.to_string());

        let id: i64 = sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence('core.audit_log', 'id'))",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| SdkError::Db(format!("audit id allocation: {e}")))?;

        // jsonb normalises key order and whitespace. The trigger hashes
        // `NEW.details::text` *after* the cast, so the HMAC must use that same
        // rendering, not the caller's JSON string.
        let details_text: String = sqlx::query_scalar("SELECT ($1::jsonb)::text")
            .bind(&row.details)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| SdkError::Db(format!("audit details canonicalisation: {e}")))?;

        let created_at = canonical_created_at(chrono::Utc::now());
        let user_text = match &row.user_id {
            SqlValue::Uuid(u) => Some(
                uuid::Uuid::parse_str(u)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| u.clone()),
            ),
            _ => None,
        };
        let payload = canonical_payload(
            &prev,
            id,
            &row.action,
            &row.resource_type,
            &row.resource_id,
            &details_text,
            &row.source,
            user_text.as_deref(),
            &created_at,
        );
        let keyed_hash = key.keyed_digest(&payload);

        let insert = bind_params(
            sqlx::query(
                "INSERT INTO core.audit_log \
                 (id, user_id, action, resource_type, resource_id, details, source, created_at, keyed_hash) \
                 VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8::timestamptz, $9)",
            ),
            vec![
                SqlValue::Int(id),
                row.user_id.clone(),
                SqlValue::Text(row.action.clone()),
                SqlValue::Text(row.resource_type.clone()),
                SqlValue::Text(row.resource_id.clone()),
                SqlValue::Text(details_text.clone()),
                SqlValue::Text(row.source.clone()),
                SqlValue::Text(created_at.clone()),
                SqlValue::Text(keyed_hash.clone()),
            ],
        )?;
        insert
            .execute(&mut *tx)
            .await
            .map_err(|e| SdkError::Db(format!("audit insert: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| SdkError::Db(format!("audit commit: {e}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Outbound HTTP policy (#155)
// ---------------------------------------------------------------------------

/// One declared scheme+host a guest may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpAllow {
    scheme: String,
    host: String,
    /// Effective port, so `https://api.example.com` (443) does not silently
    /// authorise `https://api.example.com:8443`.
    port: u16,
}

/// What a plugin's `ctx.http` may reach.
///
/// Two layers, both enforced before a request is sent:
///
/// 1. **Never** loopback, link-local, private, carrier-grade-NAT, documentation
///    or otherwise non-public addresses, and never a configured deny host.
///    Resolution is filtered too (`PublicOnlyResolver`), so a public hostname
///    that resolves to `127.0.0.1` cannot be used as a bypass.
/// 2. **Sandboxed WASM guests** may reach only the scheme+host entries their
///    manifest declares (`http_allow`), with no entries meaning no outbound
///    HTTP at all. Native plugins keep their historical public reach because
///    they are trusted code, but the same range/deny checks apply.
#[derive(Debug, Clone)]
pub struct HttpPolicy {
    /// Scheme+host entries a sandboxed guest declared. Ignored when
    /// `allow_public` is set.
    allow: Vec<HttpAllow>,
    /// Native (trusted) plugins may reach any public host.
    allow_public: bool,
    /// Operator deny list, exact host or any subdomain of it.
    deny: Vec<String>,
}

impl HttpPolicy {
    /// Native/trusted policy: any public host except the operator deny list.
    pub fn public_only(deny_hosts: Vec<String>) -> Self {
        Self {
            allow: Vec::new(),
            allow_public: true,
            deny: normalize_deny_list(deny_hosts),
        }
    }

    /// Sandboxed policy: only the declared scheme+host entries, default deny.
    /// Entries are validated here so a malformed manifest fails at load, not at
    /// the first request.
    pub fn untrusted(allow: &[String], deny_hosts: Vec<String>) -> Result<Self, String> {
        let mut parsed = Vec::with_capacity(allow.len());
        for entry in allow {
            let url = reqwest::Url::parse(entry.trim())
                .map_err(|e| format!("http_allow entry {entry:?} is not a URL: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!(
                    "http_allow entry {entry:?} must use http or https, not {:?}",
                    url.scheme()
                ));
            }
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return Err(format!(
                    "http_allow entry {entry:?} must name a scheme and host only \
                     (no path, query or fragment)"
                ));
            }
            let Some(host) = url_host_key(&url) else {
                return Err(format!("http_allow entry {entry:?} has no host"));
            };
            let Some(port) = url.port_or_known_default() else {
                return Err(format!(
                    "http_allow entry {entry:?} has no known port for {:?}",
                    url.scheme()
                ));
            };
            parsed.push(HttpAllow {
                scheme: url.scheme().to_string(),
                host,
                port,
            });
        }
        Ok(Self {
            allow: parsed,
            allow_public: false,
            deny: normalize_deny_list(deny_hosts),
        })
    }

    /// Validate a manifest allowlist without constructing a policy.
    pub fn validate_allowlist(allow: &[String]) -> Result<(), String> {
        Self::untrusted(allow, Vec::new()).map(|_| ())
    }

    /// Enforce the policy for one URL. Returns the parsed URL so the caller need
    /// not parse it again.
    pub fn check_url(&self, url: &str) -> Result<reqwest::Url, SdkError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| SdkError::BadRequest(format!("invalid http URL {url:?}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(SdkError::Forbidden(format!(
                "http.request refuses scheme {:?}; only http and https are reachable",
                parsed.scheme()
            )));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(SdkError::Forbidden(
                "http.request refuses URLs with embedded credentials".into(),
            ));
        }
        let host = url_host_key(&parsed).ok_or_else(|| {
            SdkError::BadRequest(format!("http URL {url:?} has no host"))
        })?;

        if let Ok(ip) = host.parse::<IpAddr>() {
            if !is_public_ip(ip) {
                return Err(SdkError::Forbidden(format!(
                    "http.request refuses non-public address {ip}"
                )));
            }
        }
        if is_internal_name(&host) || self.denied(&host) {
            return Err(SdkError::Forbidden(format!(
                "http.request refuses host {host:?} (loopback/link-local/private or deny list)"
            )));
        }
        let port = parsed.port_or_known_default().ok_or_else(|| {
            SdkError::BadRequest(format!("http URL {url:?} has no known port"))
        })?;
        if !self.allow_public
            && !self
                .allow
                .iter()
                .any(|a| a.scheme == parsed.scheme() && a.host == host && a.port == port)
        {
            return Err(SdkError::Forbidden(format!(
                "http.request to {host:?} is not in this plugin's http_allow manifest \
                 (scheme+host+port allowlist; default deny)"
            )));
        }
        Ok(parsed)
    }

    fn denied(&self, host: &str) -> bool {
        self.deny.iter().any(|d| {
            host == d || host.strip_suffix(d).is_some_and(|prefix| prefix.ends_with('.'))
        })
    }
}

fn normalize_deny_list(entries: Vec<String>) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| {
            let entry = entry.trim();
            let host = reqwest::Url::parse(entry)
                .ok()
                .and_then(|u| url_host_key(&u))
                .unwrap_or_else(|| normalize_host(entry));
            normalize_host(&host)
        })
        .filter(|h| !h.is_empty())
        .collect()
}

fn normalize_host(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn url_host_key(url: &reqwest::Url) -> Option<String> {
    url.host_str().map(normalize_host).filter(|h| !h.is_empty())
}

/// Names that are internal regardless of what DNS says. `.local`,
/// `.localhost` and `.internal` are commonly used for host-local services and
/// cloud metadata, so the policy refuses them by name as well as by address.
fn is_internal_name(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
}

/// True only for globally routable unicast addresses. Everything special-use
/// (loopback, link-local, RFC1918, CGNAT, documentation, multicast, unspecified,
/// broadcast, ULA, IPv4-mapped private, …) is false.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    if ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
    {
        return false;
    }
    // "This network" 0.0.0.0/8, CGNAT 100.64/10 (includes some cloud metadata),
    // 192.0.0.0/24, 6to4 relay 192.88.99.0/24, benchmarking 198.18/15,
    // reserved 240/4.
    !(o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
}

/// A resolver that only ever returns globally routable addresses. It closes the
/// "public hostname resolves to a private IP" path (DNS rebinding, metadata
/// hostnames) that a string-only host check cannot see.
#[derive(Debug, Clone, Copy, Default)]
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            let public: Vec<SocketAddr> = addrs
                .filter(|a| is_public_ip(a.ip()))
                .collect();
            if public.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refusing to resolve {host}: no public address (SSRF guard)"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Per-plugin view of [`CoreHttp`]: the same client, with that plugin's
/// [`HttpPolicy`] enforced before a request leaves the process.
pub struct PluginHttp {
    inner: std::sync::Arc<dyn HostHttp>,
    policy: HttpPolicy,
}

impl PluginHttp {
    pub fn new(inner: std::sync::Arc<dyn HostHttp>, policy: HttpPolicy) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { inner, policy })
    }

    pub fn policy(&self) -> &HttpPolicy {
        &self.policy
    }
}

#[async_trait]
impl HostHttp for PluginHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        self.policy.check_url(&url)?;
        self.inner.request(method, url, headers, body).await
    }
}

/// Core-backed HTTP for plugins (SPEC: host-mediated I/O — see SDK docs).
/// One shared `reqwest::Client` (connection pooling, 15s timeout, no
/// redirect-following surprises for OIDC endpoints). The client's resolver
/// refuses non-public addresses for every caller; [`PluginHttp`] adds the
/// per-plugin allowlist on top.
pub struct CoreHttp {
    client: reqwest::Client,
}

impl CoreHttp {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(std::sync::Arc::new(PublicOnlyResolver))
                .build()
                .expect("reqwest client"),
        })
    }
}

#[async_trait]
impl HostHttp for CoreHttp {
    async fn request(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<HttpResponse, SdkError> {
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| SdkError::BadRequest(format!("bad method {method}: {e}")))?;
        let mut req = self.client.request(m, &url);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = match body {
            Some((ct, bytes)) => req.header("content-type", ct).body(bytes),
            None => req,
        };
        let resp = req
            .send()
            .await
            .map_err(|e| SdkError::Internal(format!("http {method} {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let hdrs: std::collections::HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SdkError::Internal(format!("http body read failed: {e}")))?;
        Ok(HttpResponse {
            status,
            headers: hdrs,
            body: bytes.to_vec(),
        })
    }
}

/// Core-backed event publishing: persist first, then broadcast (SPEC §5.4).
/// One instance per plugin — owns the `source` stamp.
pub struct CoreEvents {
    pool: Arc<sqlx::PgPool>,
    tx: broadcast::Sender<Event>,
    source: String,
}

impl CoreEvents {
    pub fn new(pool: Arc<sqlx::PgPool>, tx: broadcast::Sender<Event>, source: String) -> Arc<Self> {
        Arc::new(Self { pool, tx, source })
    }
}

#[async_trait]
impl HostEvents for CoreEvents {
    async fn publish(&self, event_type: String, payload: Value) -> Result<(), SdkError> {
        let mut ev = Event {
            id: 0,
            event_type,
            payload: payload.clone(),
            source: self.source.clone(),
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&ev.payload).unwrap_or_else(|_| "{}".into());
        // Persist first, capture the row id, then broadcast with the real id
        // (SPEC §5.4: core.events is the durable record; subscribers see ids).
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO core.events (event_type, payload, source_plugin)              VALUES ($1, $2::jsonb, $3) RETURNING id",
        )
        .bind(&ev.event_type)
        .bind(&json)
        .bind(&ev.source)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| SdkError::Db(e.to_string()))?;
        ev.id = id;

        let _ = self.tx.send(ev); // no subscribers is fine
        Ok(())
    }

    async fn replay(&self, since_id: i64, limit: i64) -> Result<Vec<Event>, SdkError> {
        let rows = sqlx::query_as::<
            _,
            (i64, String, Value, String, chrono::DateTime<chrono::Utc>),
        >(
            "SELECT id, event_type, payload, source_plugin, created_at              FROM core.events WHERE id > $1 ORDER BY id ASC LIMIT $2",
        )
        .bind(since_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| SdkError::Db(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(id, event_type, payload, source, created_at)| Event {
                id,
                event_type,
                payload,
                source,
                timestamp: created_at,
            })
            .collect())
    }
}

// Decode-order coverage needs a real PgRow, so it lives in
// `server/tests/host_db.rs` (skipped unless ADJUTANT_TEST_DATABASE_URL is set).

#[cfg(test)]
mod tests {
    use super::*;
    use adjutant_sdk::testing::MockHttp;

    fn forbidden(r: Result<reqwest::Url, SdkError>) -> bool {
        matches!(r, Err(SdkError::Forbidden(_)))
    }

    #[test]
    fn non_public_ranges_and_internal_names_are_always_refused() {
        let policy = HttpPolicy::public_only(Vec::new());
        for url in [
            "http://127.0.0.1/admin",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost:8787/api/plugins",
            "http://metadata.google.internal/",
            "file:///etc/passwd",
            "ftp://example.com/",
        ] {
            assert!(forbidden(policy.check_url(url)), "{url} must be refused");
        }
        // A public literal and a public name are what the native policy allows.
        assert!(policy.check_url("https://93.184.216.34/").is_ok());
        assert!(policy.check_url("https://example.com/api").is_ok());
    }

    #[test]
    fn guest_policy_is_default_deny_and_exact_scheme_host() {
        // No declaration: no outbound HTTP at all.
        let none = HttpPolicy::untrusted(&[], Vec::new()).expect("empty policy");
        assert!(forbidden(none.check_url("https://api.example.com/")));

        let policy = HttpPolicy::untrusted(
            &["https://api.example.com".into()],
            Vec::new(),
        )
        .expect("valid allowlist");
        // Exact scheme+host (path is unrestricted; the entry names no path).
        assert!(policy.check_url("https://api.example.com/v1/things?q=1").is_ok());
        // Different host, subdomain, scheme, and non-http are all refused.
        for url in [
            "https://evil.example.com/",
            "https://api.example.com.evil.test/",
            "https://sub.api.example.com/",
            "http://api.example.com/",
            "https://api.example.com:8443/",
        ] {
            assert!(forbidden(policy.check_url(url)), "{url} must be refused");
        }
    }

    #[test]
    fn allowlist_entries_must_be_scheme_and_host_only() {
        for bad in [
            "api.example.com",
            "https://api.example.com/path",
            "https://api.example.com?q=1",
            "ftp://api.example.com",
        ] {
            assert!(
                HttpPolicy::validate_allowlist(&[bad.to_string()]).is_err(),
                "{bad:?} must not become a policy entry"
            );
        }
        assert!(HttpPolicy::validate_allowlist(&["https://api.example.com".into()]).is_ok());
    }

    #[test]
    fn operator_deny_list_covers_subdomains() {
        let policy = HttpPolicy::public_only(vec!["blocked.example".into()]);
        assert!(forbidden(policy.check_url("https://blocked.example/")));
        assert!(forbidden(policy.check_url("https://a.b.blocked.example/")));
        assert!(policy.check_url("https://notblocked.example/").is_ok());
    }

    #[test]
    fn private_addresses_are_not_public() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public_ip(ip.parse().expect("ip")), "{ip} is not public");
        }
        for ip in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            assert!(is_public_ip(ip.parse().expect("ip")), "{ip} is public");
        }
    }

    /// The exact host call a guest uses goes through `PluginHttp`: a denied URL
    /// must never reach the inner transport. This is the regression test for
    /// "every WASM guest has unrestricted outbound HTTP" (#155).
    #[tokio::test]
    async fn plugin_http_refuses_before_the_transport() {
        let inner = std::sync::Arc::new(MockHttp::new());
        let guest = PluginHttp::new(
            inner.clone(),
            HttpPolicy::untrusted(&[], Vec::new()).expect("empty guest policy"),
        );
        let err = guest
            .request("GET".into(), "http://127.0.0.1:8787/api/plugins".into(), vec![], None)
            .await
            .expect_err("a guest with no allowlist must not make the request");
        assert!(matches!(err, SdkError::Forbidden(_)));
        assert!(
            inner.request_urls().is_empty(),
            "the transport must not see a denied URL"
        );

        // An allowed entry does reach the transport (the mock then reports no
        // queued response, which is its own error, not a policy refusal).
        let allowed = PluginHttp::new(
            inner.clone(),
            HttpPolicy::untrusted(&["https://api.example.com".into()], Vec::new())
                .expect("valid guest policy"),
        );
        let _ = allowed
            .request("GET".into(), "https://api.example.com/v1".into(), vec![], None)
            .await;
        assert_eq!(
            inner.request_urls(),
            vec![("GET".to_string(), "https://api.example.com/v1".to_string())]
        );
    }
}
