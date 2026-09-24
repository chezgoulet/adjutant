//! # adjutant-auth — authentication plugin (SPEC §7.1)
//!
//! Owns `core.users` / `core.sessions`. Three things a troop admin needs:
//!
//! 1. **Local passwords** — argon2-hashed, login/logout/me/register.
//! 2. **Sessions** — random 32-byte token, only its SHA-256 is stored; handed
//!    out as an `adjutant_session` cookie or `Authorization: Bearer`. The
//!    plugin registers itself as the core's **identity provider**, so it — not
//!    the dev-header stub — answers "who is this request?" (SPEC §7.1).
//! 3. **OIDC** — authorization-code flow through `ctx.http` (host-mediated;
//!    a plugin-linked `reqwest` would panic on the missing reactor, the M1
//!    lesson). id_tokens verified with `jsonwebtoken` (HS256 client-secret or
//!    RS256 via JWKS).
//!
//! Bootstrap: while `core.users` is empty, `POST /api/auth/register` is open
//! and the first user becomes `chief`. Afterwards registration is closed —
//! only `auth:manage_users` may create users.
//!
//! Config (per-plugin row `core.plugins.config`, admin-set, reload to apply):
//! `{"oidc": {"issuer", "client_id", "client_secret", "redirect_uri"},
//!   "session_ttl_hours": 720}`

use std::collections::HashMap;
use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use serde::{Deserialize, Serialize};

const SESSION_COOKIE: &str = "adjutant_session";
const DEFAULT_TTL_HOURS: i64 = 720; // 30 days

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
struct OidcCfg {
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AuthConfig {
    oidc: Option<OidcCfg>,
    session_ttl_hours: Option<i64>,
}

// ---------------------------------------------------------------------------
// Crypto helpers (pure CPU — safe to link into a plugin)
// ---------------------------------------------------------------------------

fn hash_password(password: &str) -> Result<String, SdkError> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| SdkError::Internal(format!("password hash failed: {e}")))
}

fn verify_password(password: &str, stored: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    PasswordHash::new(stored)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

// ---------------------------------------------------------------------------
// Session handling (shared by routes + identity provider)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SessionUser {
    id: String,
    username: String,
    email: Option<String>,
    display_name: String,
    roles: Vec<String>,
    /// Scoped role grants (SPEC §9.2); `roles` is derived from these.
    grants: Vec<RoleGrant>,
}

/// Extract a raw session token from `Cookie: adjutant_session=…` or
/// `Authorization: Bearer …`. Header lookup is case-insensitive (HTTP/2
/// lowercases; some HTTP/1.1 stacks don't).
fn extract_token(headers: &HashMap<String, String>) -> Option<String> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    if let Some(auth) = get("authorization") {
        if let Some(tok) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            if !tok.is_empty() {
                return Some(tok.trim().to_string());
            }
        }
    }
    let cookie = get("cookie")?;
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if let Some(v) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Session token → identity. `Ok(None)` = no/unknown/expired token.
async fn session_identity(db: &DbHandle, token: &str) -> Result<Option<SessionUser>, SdkError> {
    let rows = db
        .query(
            "SELECT u.id::text AS id, u.username, u.email, u.display_name, u.is_active, \
                    COALESCE(ARRAY(\
                        SELECT role_id FROM core.user_roles WHERE user_id = u.id\
                    ), '{}') AS roles, \
                    COALESCE((\
                        SELECT jsonb_agg(jsonb_build_object(\
                            'role_id', role_id, \
                            'scope_type', scope_type, \
                            'scope_id', scope_id::text)) \
                        FROM core.user_roles WHERE user_id = u.id\
                    ), '[]'::jsonb) AS grant_list \
             FROM core.sessions s \
             JOIN core.users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND s.expires_at > now()",
            vec![SqlValue::Text(token_hash(token))],
        )
        .await?;
    let row = match rows.into_iter().next() {
        Some(r) => r,
        None => return Ok(None),
    };
    if row["is_active"] != serde_json::json!(true) {
        return Ok(None);
    }
    let roles: Vec<String> = row["roles"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let grants: Vec<RoleGrant> = row["grant_list"]
        .as_array()
        .map(|a| a.iter().filter_map(grant_from_row).collect())
        .unwrap_or_default();
    Ok(Some(SessionUser {
        id: row["id"].as_str().unwrap_or_default().to_string(),
        username: row["username"].as_str().unwrap_or_default().to_string(),
        email: row["email"].as_str().map(String::from),
        display_name: row["display_name"].as_str().unwrap_or_default().to_string(),
        roles,
        grants,
    }))
}

/// One `core.user_roles` row (as JSON) → [`RoleGrant`]. A zero-UUID scope id
/// means troop-wide (`None`).
fn grant_from_row(g: &serde_json::Value) -> Option<RoleGrant> {
    let role_id = g["role_id"].as_str()?.to_string();
    if role_id.is_empty() {
        return None;
    }
    let scope_type = match g["scope_type"].as_str().unwrap_or("troop") {
        "lodge" => ScopeType::Lodge,
        "patrol" => ScopeType::Patrol,
        "personal" => ScopeType::Personal,
        _ => ScopeType::Troop,
    };
    let scope_id = match g["scope_id"].as_str() {
        Some(s) if !s.is_empty() && s != "00000000-0000-0000-0000-000000000000" => {
            Some(s.to_string())
        }
        _ => None,
    };
    Some(RoleGrant { role_id, scope: Scope { scope_type, scope_id } })
}

/// PostgreSQL unique-violation detection. sqlx surfaces the driver message
/// (SQLSTATE 23505) as text across the host boundary, so match both forms.
fn is_duplicate_key(err: &str) -> bool {
    err.contains("duplicate key") || err.contains("23505")
}

/// Local username for an IdP-created user: lowercase, `[a-z0-9._-]`, capped at
/// 32 chars. Falls back to a deterministic subject-derived handle when the
/// IdP handle sanitizes to nothing usable.
fn sanitize_handle(handle: &str, subject: &str) -> String {
    let cleaned: String = handle
        .trim()
        .to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| matches!(c, '-' | '.' | '_'));
    let mut out: String = trimmed.chars().take(32).collect();
    if out.len() < 3 {
        out = format!("idp-{}", &token_hash(subject)[..8]);
    }
    out
}

/// Create a session row, return the raw token (only its hash is stored).
async fn create_session(
    db: &DbHandle,
    user_id: &str,
    ttl_hours: i64,
) -> Result<(String, String), SdkError> {
    let token = new_token();
    db.execute(
        "INSERT INTO core.sessions (user_id, token_hash, expires_at) \
         VALUES ($1::uuid, $2, now() + make_interval(hours => $3::int))",
        vec![
            SqlValue::Text(user_id.to_string()),
            SqlValue::Text(token_hash(&token)),
            SqlValue::Int(ttl_hours),
        ],
    )
    .await?;
    Ok((token, chrono::Utc::now().to_rfc3339()))
}

fn session_cookie(token: &str, ttl_hours: i64) -> (String, String) {
    (
        "set-cookie".into(),
        format!(
            "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
            ttl_hours * 3600
        ),
    )
}

// ---------------------------------------------------------------------------
// Identity provider (registered with the core at init)
// ---------------------------------------------------------------------------

struct SessionIdentityProvider {
    db: DbHandle,
}

#[async_trait]
impl IdentityProvider for SessionIdentityProvider {
    async fn identify(&self, headers: &HashMap<String, String>) -> Result<Option<Identity>, SdkError> {
        let Some(token) = extract_token(headers) else {
            return Ok(None); // no credentials → core may fall back to dev headers
        };
        match session_identity(&self.db, &token).await? {
            Some(user) => Ok(Some(Identity::from_grants(user.id, user.grants))),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct AuthPlugin {
    ctx: OnceLock<PluginContext>,
    cfg: AuthConfig,
}

impl AuthPlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new(), cfg: AuthConfig::default() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }

    async fn require_session(&self, req: &PluginRequest) -> Result<SessionUser, PluginResponse> {
        let token = extract_token(&req.headers).ok_or_else(|| {
            PluginResponse::error(401, "authentication required").unwrap()
        })?;
        session_identity(&self.ctx().db, &token)
            .await
            .map_err(|e| PluginResponse::error(500, e.to_string()).unwrap())
            .and_then(|u| u.ok_or_else(|| PluginResponse::error(401, "session expired or invalid").unwrap()))
    }

    fn ttl(&self) -> i64 {
        self.cfg.session_ttl_hours.unwrap_or(DEFAULT_TTL_HOURS)
    }
}

impl Default for AuthPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for AuthPlugin {
    fn id(&self) -> &str {
        "auth"
    }
    fn name(&self) -> &str {
        "Authentication"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        // Per-plugin config from core.plugins.config (admin-editable, reload to apply).
        self.cfg = serde_json::from_value(ctx.config.clone()).unwrap_or_default();
        // Register as THE identity provider: sessions now answer "who is this
        // request?" for every route in the core (SPEC §7.1).
        ctx.identity.register(
            "auth",
            std::sync::Arc::new(SessionIdentityProvider { db: ctx.db.clone() }),
        );
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![Permission::new(
            "auth:manage_users",
            "Create users and assign roles",
        )]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "oidc_states_and_session_index",
            "ALTER TABLE core.users ADD COLUMN IF NOT EXISTS username TEXT;\
             ALTER TABLE core.users ADD COLUMN IF NOT EXISTS password_hash TEXT;\
             CREATE TABLE IF NOT EXISTS oidc_states (\
                 state TEXT PRIMARY KEY, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_core_sessions_hash \
                 ON core.sessions (token_hash);\
             CREATE UNIQUE INDEX IF NOT EXISTS idx_core_users_username \
                 ON core.users (lower(username)) WHERE username IS NOT NULL;", 
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- login ----------------------------------------------------------
        let c = ctx.clone();
        let ttl = self.ttl();
        let login = RouteDefinition::post(
            "/api/auth/login",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        username: String,
                        password: String,
                    }
                    let body: Body = req.json()?;
                    let rows = c
                        .db
                        .query(
                            "SELECT id::text AS id, username, password_hash, is_active \
                             FROM core.users WHERE lower(username) = lower($1)",
                            vec![SqlValue::Text(body.username.clone())],
                        )
                        .await?;
                    let row = rows.into_iter().next().ok_or_else(|| {
                        SdkError::BadRequest("invalid credentials".into())
                    })?;
                    let stored = row["password_hash"].as_str().unwrap_or("");
                    if stored.is_empty()
                        || !verify_password(&body.password, stored)
                        || row["is_active"] != serde_json::json!(true)
                    {
                        // Same message for unknown user and bad password — no user enumeration.
                        return Err(SdkError::BadRequest("invalid credentials".into()));
                    }
                    let uid = row["id"].as_str().unwrap_or_default().to_string();
                    let (token, expires) = create_session(&c.db, &uid, ttl).await?;
                    let mut resp = PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "token": token,
                            "token_type": "Bearer",
                            "expires_at": expires,
                        }),
                    )?;
                    resp.headers.push(session_cookie(&token, ttl));
                    Ok(resp)
                }
            }),
        );

        // --- logout ---------------------------------------------------------
        let c = ctx.clone();
        let logout = RouteDefinition::post(
            "/api/auth/logout",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let token = extract_token(&req.headers).unwrap_or_default();
                    if !token.is_empty() {
                        c.db
                            .execute(
                                "DELETE FROM core.sessions WHERE token_hash = $1",
                                vec![SqlValue::Text(token_hash(&token))],
                            )
                            .await?;
                    }
                    let mut resp = PluginResponse::json(200, &serde_json::json!({"ok": true}))?;
                    resp.headers
                        .push(("set-cookie".into(), format!("{SESSION_COOKIE}=; Path=/; Max-Age=0")));
                    Ok(resp)
                }
            }),
        );

        // --- me -------------------------------------------------------------
        let me_self = AuthPlugin { ctx: self.ctx.clone(), cfg: self.cfg.clone() };
        let me = RouteDefinition::get(
            "/api/auth/me",
            route_handler(move |req| {
                let me = AuthPlugin { ctx: me_self.ctx.clone(), cfg: me_self.cfg.clone() };
                async move {
                    match me.require_session(&req).await {
                        Err(resp) => Ok(resp),
                        Ok(user) => PluginResponse::json(
                            200,
                            &serde_json::json!({
                                "id": user.id,
                                "username": user.username,
                                "email": user.email,
                                "display_name": user.display_name,
                                "roles": user.roles,
                            }),
                        ),
                    }
                }
            }),
        );

        // --- register (bootstrap: open only while zero users exist) ---------
        let c = ctx.clone();
        let ttl = self.ttl();
        let register = RouteDefinition::post(
            "/api/auth/register",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        username: String,
                        #[serde(default)]
                        email: Option<String>,
                        password: String,
                    }
                    let n: i64 = c
                        .db
                        .query("SELECT COUNT(*) AS n FROM core.users", vec![])
                        .await?
                        .first()
                        .and_then(|r| r["n"].as_i64())
                        .unwrap_or(0);
                    if n > 0 {
                        return PluginResponse::error(
                            403,
                            "registration is closed — ask an admin to create the user",
                        );
                    }
                    let body: Body = req.json()?;
                    if body.password.len() < 8 {
                        return PluginResponse::error(400, "password must be at least 8 characters");
                    }
                    let pw = hash_password(&body.password)?;
                    let rows = match c
                        .db
                        .query(
                            "INSERT INTO core.users (username, email, display_name, password_hash) \
                             VALUES ($1, $2, $3, $4) \
                             RETURNING id::text AS id",
                            vec![
                                SqlValue::Text(body.username.clone()),
                                body.email.clone().into(),
                                SqlValue::Text(body.username.clone()),
                                SqlValue::Text(pw),
                            ],
                        )
                        .await
                    {
                        Ok(rows) => rows,
                        // A unique violation is the caller's problem, not a server
                        // error: report it as such (the earlier 500 was a defect).
                        Err(SdkError::Db(e)) if is_duplicate_key(&e) => {
                            return PluginResponse::error(
                                409,
                                "already registered — log in instead",
                            )
                        }
                        Err(e) => return Err(e),
                    };
                    let uid = rows
                        .first()
                        .and_then(|r| r["id"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    // First user of a fresh troop = chief (SPEC §9 bootstrap).
                    c.db
                        .execute(
                            "INSERT INTO core.user_roles (user_id, role_id) \
                             VALUES ($1::uuid, 'chief') ON CONFLICT DO NOTHING",
                            vec![SqlValue::Text(uid.clone())],
                        )
                        .await?;
                    let (token, expires) = create_session(&c.db, &uid, ttl).await?;
                    let mut resp = PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "id": uid, "username": body.username,
                            "token": token, "expires_at": expires,
                            "bootstrap_role": "chief",
                        }),
                    )?;
                    resp.headers.push(session_cookie(&token, ttl));
                    Ok(resp)
                }
            }),
        );

        // --- admin: create user ---------------------------------------------
        let c = ctx.clone();
        let create_user = RouteDefinition::post_protected(
            "/api/auth/users",
            "auth:manage_users",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        username: String,
                        #[serde(default)]
                        email: Option<String>,
                        password: String,
                        #[serde(default)]
                        roles: Vec<String>,
                    }
                    let body: Body = req.json()?;
                    if body.password.len() < 8 {
                        return PluginResponse::error(400, "password must be at least 8 characters");
                    }
                    let pw = hash_password(&body.password)?;
                    let rows = match c
                        .db
                        .query(
                            "INSERT INTO core.users (username, email, display_name, password_hash) \
                             VALUES ($1, $2, $3, $4) \
                             RETURNING id::text AS id",
                            vec![
                                SqlValue::Text(body.username.clone()),
                                body.email.clone().into(),
                                SqlValue::Text(body.username.clone()),
                                SqlValue::Text(pw),
                            ],
                        )
                        .await
                    {
                        Ok(rows) => rows,
                        Err(SdkError::Db(e)) if is_duplicate_key(&e) => {
                            return PluginResponse::error(
                                409,
                                format!("username {:?} is already taken", body.username),
                            )
                        }
                        Err(e) => return Err(e),
                    };
                    let uid = rows
                        .first()
                        .and_then(|r| r["id"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    for role in &body.roles {
                        c.db
                            .execute(
                                "INSERT INTO core.user_roles (user_id, role_id) \
                                 VALUES ($1::uuid, $2) ON CONFLICT DO NOTHING",
                                vec![SqlValue::Text(uid.clone()), SqlValue::Text(role.clone())],
                            )
                            .await?;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "user.create",
                            "user",
                            &uid,
                            serde_json::json!({"username": body.username, "roles": body.roles}),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({"id": uid, "username": body.username, "roles": body.roles}),
                    )
                }
            }),
        );

        // --- admin: assign roles ----------------------------------------------
        let c = ctx.clone();
        let assign_roles = RouteDefinition::post_protected(
            "/api/auth/roles",
            "auth:manage_users",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        username: String,
                        roles: Vec<String>,
                    }
                    let body: Body = req.json()?;
                    let rows = c
                        .db
                        .query(
                            "SELECT id::text AS id FROM core.users WHERE lower(username) = lower($1)",
                            vec![SqlValue::Text(body.username.clone())],
                        )
                        .await?;
                    let uid = rows
                        .first()
                        .and_then(|r| r["id"].as_str())
                        .ok_or_else(|| SdkError::BadRequest("unknown user".into()))?
                        .to_string();
                    // Replace the full role set (explicit PUT-like semantics).
                    c.db
                        .execute(
                            "DELETE FROM core.user_roles WHERE user_id = $1::uuid",
                            vec![SqlValue::Text(uid.clone())],
                        )
                        .await?;
                    for role in &body.roles {
                        c.db
                            .execute(
                                "INSERT INTO core.user_roles (user_id, role_id) \
                                 VALUES ($1::uuid, $2) ON CONFLICT DO NOTHING",
                                vec![SqlValue::Text(uid.clone()), SqlValue::Text(role.clone())],
                            )
                            .await?;
                    }
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "user.roles.set",
                            "user",
                            &uid,
                            serde_json::json!({"roles": body.roles}),
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &serde_json::json!({"username": body.username, "roles": body.roles}),
                    )
                }
            }),
        );

        // --- admin: list users ------------------------------------------------
        let c = ctx.clone();
        let list_users = RouteDefinition::get_protected(
            "/api/auth/users",
            "auth:manage_users",
            route_handler(move |_req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            "SELECT u.id::text AS id, u.username, u.email, u.display_name, \
                                    u.is_active, \
                                    COALESCE(ARRAY(\
                                        SELECT role_id FROM core.user_roles WHERE user_id = u.id\
                                    ), '{}') AS roles \
                             FROM core.users u ORDER BY u.username",
                            vec![],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "users": rows }))
                }
            }),
        );

        vec![login, logout, me, register, create_user, assign_roles, list_users]
            .into_iter()
            .chain(self.oidc_routes(ctx.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// OIDC (SPEC §7.1: OIDC as source — Adjutant trusts an upstream IdP)
// ---------------------------------------------------------------------------

impl AuthPlugin {
    /// OIDC routes. Configured only when `core.plugins.config` carries an
    /// `oidc` block — otherwise both routes answer 501 so a misconfigured
    /// deploy says so plainly instead of half-working.
    fn oidc_routes(&self, ctx: PluginContext) -> Vec<RouteDefinition> {
        let configured = self.cfg.oidc.is_some();

        // Step 1: hand the client an authorize URL (state is CSRF protection).
        let c = ctx.clone();
        let oidc = self.cfg.oidc.clone();
        let start = RouteDefinition::get(
            "/api/auth/oidc/login",
            route_handler(move |req| {
                let c = c.clone();
                let oidc = oidc.clone();
                async move {
                    let Some(oidc) = oidc else {
                        return PluginResponse::error(
                            501,
                            "OIDC not configured (set core.plugins.config for auth, then reload)",
                        );
                    };
                    // One-time state, stored server-side (SPEC: no client trust).
                    let state = new_token();
                    c.db
                        .execute(
                            "INSERT INTO oidc_states (state) VALUES ($1) \
                             ON CONFLICT (state) DO NOTHING",
                            vec![SqlValue::Text(state.clone())],
                        )
                        .await?;
                    // Separate execute: sqlx prepared statements accept one
                    // command only ("cannot insert multiple commands").
                    c.db
                        .execute(
                            "DELETE FROM oidc_states \
                             WHERE created_at < now() - interval '15 minutes'",
                            vec![],
                        )
                        .await?;
                    let redirect = req
                        .query_param("redirect")
                        .unwrap_or("1")
                        .to_string();
                    // Per OIDC, the authorize endpoint comes from discovery —
                    // never guess it (the sep() helper builds DISCOVERY urls).
                    let auth_ep =
                        discover(&c.http, &oidc.issuer, "authorization_endpoint").await?;
                    let sep_char = if auth_ep.contains('?') { "&" } else { "?" };
                    let url = format!(
                        "{auth_ep}{sep_char}response_type=code&client_id={}&redirect_uri={}&state={}&scope=openid+email+profile",
                        urlencode(&oidc.client_id),
                        urlencode(&oidc.redirect_uri),
                        urlencode(&state),
                    );
                    if redirect == "1" {
                        let mut resp = PluginResponse::empty(302);
                        resp.headers.push(("location".into(), url.clone()));
                        resp.headers
                            .push(("cache-control".into(), "no-store".into()));
                        return Ok(resp);
                    }
                    PluginResponse::json(200, &serde_json::json!({
                        "authorize_url": url, "state": state,
                    }))
                }
            }),
        );

        // Step 2: callback — exchange code, verify id_token, session out.
        let c = ctx.clone();
        let oidc2 = self.cfg.oidc.clone();
        let ttl = self.ttl();
        let callback = RouteDefinition::get(
            "/api/auth/oidc/callback",
            route_handler(move |req| {
                let c = c.clone();
                let oidc = oidc2.clone();
                async move {
                    let _ = configured; // readability anchor
                    let Some(oidc) = oidc else {
                        return PluginResponse::error(501, "OIDC not configured");
                    };
                    let code = req.query_param("code").unwrap_or("").to_string();
                    let state = req.query_param("state").unwrap_or("").to_string();
                    if code.is_empty() || state.is_empty() {
                        return PluginResponse::error(400, "missing code or state");
                    }
                    // Consume state (single-use, 15 min TTL enforced on insert).
                    let consumed = c
                        .db
                        .query(
                            "DELETE FROM oidc_states WHERE state = $1 RETURNING state",
                            vec![SqlValue::Text(state)],
                        )
                        .await?;
                    if consumed.is_empty() {
                        return PluginResponse::error(400, "unknown or expired state");
                    }

                    // Code exchange (host-mediated HTTP, M1 lesson).
                    let form = format!(
                        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
                        urlencode(&code),
                        urlencode(&oidc.redirect_uri),
                        urlencode(&oidc.client_id),
                        urlencode(&oidc.client_secret),
                    );
                    let token_url = discover(&c.http, &oidc.issuer, "token_endpoint").await?;
                    let resp = c
                        .http
                        .request(
                            "POST".into(),
                            token_url,
                            vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
                            Some(("application/x-www-form-urlencoded".into(), form.into_bytes())),
                        )
                        .await?;
                    if resp.status >= 400 {
                        return PluginResponse::error(
                            502,
                            format!("token endpoint returned {}", resp.status),
                        );
                    }
                    #[derive(Deserialize)]
                    struct TokenResp {
                        id_token: String,
                    }
                    let tok: TokenResp = resp.json()?;

                    // Verify id_token BEFORE trusting any claim inside it.
                    let claims = verify_id_token(&c.http, &oidc, &tok.id_token).await?;
                    let subject = claims.sub.clone();
                    let email = claims.email.clone();

                    // Find-or-create the user keyed on the IdP subject.
                    let rows = c
                        .db
                        .query(
                            "SELECT id::text AS id FROM core.users WHERE external_id = $1",
                            vec![SqlValue::Text(subject.clone())],
                        )
                        .await?;
                    let uid = if let Some(r) = rows.first() {
                        r["id"].as_str().unwrap_or_default().to_string()
                    } else {
                        let display = claims
                            .name
                            .clone()
                            .or_else(|| email.clone())
                            .unwrap_or_else(|| subject.clone());
                        // IdP users get a local username: preferred_username,
                        // else the email local-part, else the subject. Without
                        // one they are invisible to username-keyed admin
                        // routes (role grants, /users).
                        let handle = claims
                            .preferred_username
                            .clone()
                            .filter(|h| !h.trim().is_empty())
                            .or_else(|| {
                                email
                                    .as_ref()
                                    .and_then(|e| e.split('@').next())
                                    .map(|s| s.to_string())
                            })
                            .filter(|h| !h.trim().is_empty())
                            .unwrap_or_else(|| subject.clone());
                        let username = sanitize_handle(&handle, &subject);
                        let insert = |u: String| {
                            let db = c.db.clone();
                            let (subj, mail, disp) =
                                (subject.clone(), email.clone(), display.clone());
                            async move {
                                db.query(
                                    "INSERT INTO core.users (external_id, email, display_name, username) \
                                     VALUES ($1, $2, $3, $4) RETURNING id::text AS id",
                                    vec![
                                        SqlValue::Text(subj),
                                        mail.into(),
                                        SqlValue::Text(disp),
                                        SqlValue::Text(u),
                                    ],
                                )
                                .await
                            }
                        };
                        let created = match insert(username.clone()).await {
                            Ok(rows) => rows,
                            // Username taken by another account (or a local
                            // user) — disambiguate deterministically.
                            Err(SdkError::Db(e)) if is_duplicate_key(&e) => {
                                insert(format!("{username}-{}", &token_hash(&subject)[..8])).await?
                            }
                            Err(e) => return Err(e),
                        };
                        created
                            .first()
                            .and_then(|r| r["id"].as_str())
                            .unwrap_or_default()
                            .to_string()
                    };

                    let (session_token, expires) = create_session(&c.db, &uid, ttl).await?;
                    c.audit
                        .log(
                            Some(&Identity::from_grants(uid.clone(), vec![])),
                            "auth.oidc.login",
                            "user",
                            &uid,
                            serde_json::json!({"issuer": oidc.issuer, "subject": subject}),
                        )
                        .await?;
                    let redirect = req.query_param("app_redirect").unwrap_or("/");
                    let mut resp = PluginResponse::json(
                        200,
                        &serde_json::json!({
                            "token": session_token, "token_type": "Bearer",
                            "expires_at": expires, "user_id": uid,
                            "app_redirect": redirect,
                        }),
                    )?;
                    resp.headers.push(session_cookie(&session_token, ttl));
                    Ok(resp)
                }
            }),
        );

        vec![start, callback]
    }
}

/// Percent-encode for query values (OIDC params: client ids, URLs, state).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Discovery document URL for an issuer; tolerant of a trailing slash.
fn discovery_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

/// Fetch the issuer's discovery document and pull one field out of it.
async fn discover(
    http: &std::sync::Arc<dyn HostHttp>,
    issuer: &str,
    field: &str,
) -> Result<String, SdkError> {
    let url = discovery_url(issuer);
    let resp = http.request("GET".into(), url.clone(), vec![], None).await?;
    if resp.status >= 400 {
        return Err(SdkError::Internal(format!("discovery failed: HTTP {}", resp.status)));
    }
    let doc: serde_json::Value = resp.json()?;
    doc[field]
        .as_str()
        .map(String::from)
        .ok_or_else(|| SdkError::Internal(format!("discovery doc lacks {field}")))
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    sub: String,
    /// Preferred handle from the IdP — used as the local username when present.
    #[serde(default)]
    preferred_username: Option<String>,
    /// `iss`/`aud`/`exp` are enforced by `Validation` during `decode` —
    /// jsonwebtoken rejects the token before these are ever read.
    #[serde(default, rename = "iss")]
    #[allow(dead_code)]
    iss: Option<String>,
    #[serde(default, rename = "aud")]
    #[allow(dead_code)]
    aud: Option<serde_json::Value>,
    #[serde(default, rename = "exp")]
    #[allow(dead_code)]
    exp: Option<u64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Verify an id_token: signature (HS256 shared-secret or RS256 via JWKS) plus
/// iss/aud/exp. Returns claims ONLY on success — never decode-trust.
async fn verify_id_token(
    http: &std::sync::Arc<dyn HostHttp>,
    oidc: &OidcCfg,
    token: &str,
) -> Result<IdClaims, SdkError> {
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

    let header = decode_header(token)
        .map_err(|e| SdkError::BadRequest(format!("id_token header: {e}")))?;

    let key = match header.alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            DecodingKey::from_secret(oidc.client_secret.as_bytes())
        }
        Algorithm::RS256 => {
            let jwks_url = discover(http, &oidc.issuer, "jwks_uri").await?;
            let resp = http.request("GET".into(), jwks_url, vec![], None).await?;
            let jwks: JwkSet = resp
                .json()
                .map_err(|e| SdkError::Internal(format!("JWKS parse: {e}")))?;
            let kid = header.kid.ok_or_else(|| {
                SdkError::BadRequest("RS256 id_token missing kid; cannot select JWKS key".into())
            })?;
            let jwk = jwks
                .find(&kid)
                .ok_or_else(|| SdkError::BadRequest(format!("no JWKS key for kid {kid}")))?;
            DecodingKey::from_jwk(jwk)
                .map_err(|e| SdkError::Internal(format!("JWKS key unusable: {e}")))?
        }
        other => {
            return Err(SdkError::BadRequest(format!(
                "unsupported id_token alg {other:?}"
            )))
        }
    };

    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[oidc.issuer.trim_end_matches('/')]);
    validation.set_audience(std::slice::from_ref(&oidc.client_id));
    let data = decode::<IdClaims>(token, &key, &validation)
        .map_err(|e| SdkError::BadRequest(format!("id_token verification failed: {e}")))?;
    Ok(data.claims)
}

#[cfg(test)]
mod oidc_tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};

    #[test]
    fn urlencode_escapes_query_unsafe_bytes() {
        assert_eq!(urlencode("ab_c-1.2~"), "ab_c-1.2~");
        assert_eq!(urlencode("https://a b/c?d=e&f"), "https%3A%2F%2Fa%20b%2Fc%3Fd%3De%26f");
    }

    #[test]
    fn discovery_url_joins_issuer_cleanly() {
        assert_eq!(
            discovery_url("https://idp.example"),
            "https://idp.example/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url("https://idp.example/"),
            "https://idp.example/.well-known/openid-configuration"
        );
    }

    #[test]
    fn hs256_id_token_roundtrip() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        #[derive(Serialize, Deserialize)]
        struct C {
            sub: String,
            iss: String,
            aud: String,
            exp: u64,
            iat: u64,
            email: String,
        }
        let claims = C {
            sub: "idp-user-1".into(),
            iss: "https://idp.example".into(),
            aud: "adjutant".into(),
            exp: u64::MAX,
            iat: 1,
            email: "u@example.org".into(),
        };
        let tok = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"shared-secret"),
        )
        .unwrap();

        // Pure verification (no network): issuer/audience matching only —
        // the network-backed paths (discovery/JWKS) are exercised live.
        let mut v = Validation::new(Algorithm::HS256);
        v.set_issuer(&["https://idp.example"]);
        v.set_audience(&["adjutant"]);
        let out = decode::<C>(
            &tok,
            &DecodingKey::from_secret(b"shared-secret"),
            &v,
        )
        .expect("valid token verifies");
        assert_eq!(out.claims.sub, "idp-user-1");
        assert_eq!(out.claims.email, "u@example.org");

        // Wrong secret must fail.
        let mut v2 = Validation::new(Algorithm::HS256);
        v2.set_issuer(&["https://idp.example"]);
        v2.set_audience(&["adjutant"]);
        assert!(decode::<C>(&tok, &DecodingKey::from_secret(b"WRONG"), &v2).is_err());
    }

    #[test]
    fn state_is_single_use_shape() {
        // The single-use guarantee is SQL-level (DELETE ... RETURNING);
        // here we pin the token shape the state relies on.
        let s = new_token();
        assert_eq!(s.len(), 64);
        assert_ne!(new_token(), s);
    }
}
export_plugin!(AuthPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &h));
        assert!(!verify_password("wrong password", &h));
        assert!(!verify_password("correct horse battery staple", "not-a-hash"));
    }

    #[test]
    fn tokens_are_random_and_hashed_stably() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "tokens must not collide");
        assert_eq!(token_hash(&a), token_hash(&a));
        assert_ne!(token_hash(&a), token_hash(&b));
    }

    #[test]
    fn extract_token_from_cookie_and_bearer() {
        let mut h = HashMap::new();
        h.insert("cookie".into(), format!("{SESSION_COOKIE}=abc123; theme=dark"));
        assert_eq!(extract_token(&h).as_deref(), Some("abc123"));

        let mut h = HashMap::new();
        h.insert("authorization".into(), "Bearer tok456".into());
        assert_eq!(extract_token(&h).as_deref(), Some("tok456"));

        let mut h = HashMap::new();
        h.insert("authorization".into(), "Basic dXNlcjpwdw==".into());
        assert_eq!(extract_token(&h), None, "non-Bearer schemes are ignored");
        assert_eq!(extract_token(&HashMap::new()), None);
    }

    #[test]
    fn cookie_format_is_http_only() {
        let (_, v) = session_cookie("tok", 24);
        assert!(v.contains("HttpOnly"));
        assert!(v.contains("SameSite=Lax"));
        assert!(v.contains("Max-Age=86400"));
    }

    #[test]
    fn config_parses_oidc_and_ttl() {
        let cfg: AuthConfig = serde_json::from_str(
            r#"{"session_ttl_hours": 48,
                "oidc": {"issuer":"https://idp","client_id":"c","client_secret":"s",
                          "redirect_uri":"https://app/cb"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.session_ttl_hours, Some(48));
        assert_eq!(cfg.oidc.unwrap().issuer, "https://idp");
        assert!(serde_json::from_str::<AuthConfig>("{}").is_ok());
    }
}

#[cfg(test)]
mod id_token_tests {
    //! Coverage for `verify_id_token` itself — the decision point that decides
    //! whether an upstream identity is trusted. The old OIDC unit test only
    //! round-tripped a token through `jsonwebtoken`, so this function (and the
    //! RS256/JWKS branch, and exp/iss/aud enforcement) had no test at all.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct StubHttp {
        jwks: Option<String>,
    }

    #[async_trait]
    impl HostHttp for StubHttp {
        async fn request(
            &self,
            _method: String,
            url: String,
            _headers: Vec<(String, String)>,
            _body: Option<(String, Vec<u8>)>,
        ) -> Result<HttpResponse, SdkError> {
            let body = if url.ends_with("/.well-known/openid-configuration") {
                serde_json::json!({
                    "issuer": ISSUER,
                    "authorization_endpoint": format!("{ISSUER}/authorize"),
                    "token_endpoint": format!("{ISSUER}/token"),
                    "jwks_uri": format!("{ISSUER}/jwks"),
                })
                .to_string()
                .into_bytes()
            } else if url.ends_with("/jwks") {
                match &self.jwks {
                    Some(j) => j.clone().into_bytes(),
                    None => return Err(SdkError::Internal("test stub has no JWKS".into())),
                }
            } else {
                return Err(SdkError::Internal(format!("unexpected url {url}")));
            };
            Ok(HttpResponse { status: 200, headers: HashMap::new(), body })
        }
    }

    const ISSUER: &str = "https://idp.example";
    const SECRET: &str = "shared-secret";
    const CLIENT: &str = "adjutant-test";
    const RSA_PEM: &str = include_str!("../tests/fixtures/rs256-test-key.pem");
    const JWKS: &str = include_str!("../tests/fixtures/rs256-jwks.json");

    fn oidc() -> OidcCfg {
        OidcCfg {
            issuer: ISSUER.into(),
            client_id: CLIENT.into(),
            client_secret: SECRET.into(),
            redirect_uri: "https://app.example/cb".into(),
        }
    }

    fn claims(iss: &str, aud: &str, exp_offset: i64) -> serde_json::Value {
        let now = chrono::Utc::now().timestamp();
        serde_json::json!({
            "sub": "idp-user-1",
            "iss": iss,
            "aud": aud,
            "exp": now + exp_offset,
            "iat": now,
            "email": "u@example.org",
        })
    }

    fn hs256(claims: &serde_json::Value, secret: &[u8]) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            claims,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn hs256_valid_token_is_accepted() {
        let http: Arc<dyn HostHttp> = Arc::new(StubHttp { jwks: None });
        let tok = hs256(&claims(ISSUER, CLIENT, 3600), SECRET.as_bytes());
        let got = verify_id_token(&http, &oidc(), &tok).await.expect("valid token");
        assert_eq!(got.sub, "idp-user-1");
        assert_eq!(got.email.as_deref(), Some("u@example.org"));
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        let http: Arc<dyn HostHttp> = Arc::new(StubHttp { jwks: None });
        // An hour past expiry: comfortably outside jsonwebtoken's default 60s
        // clock-skew leeway (which is why -60 would still be accepted).
        let tok = hs256(&claims(ISSUER, CLIENT, -3600), SECRET.as_bytes());
        assert!(verify_id_token(&http, &oidc(), &tok).await.is_err(), "expired token must fail");
    }

    #[tokio::test]
    async fn wrong_issuer_audience_and_secret_are_rejected() {
        let http: Arc<dyn HostHttp> = Arc::new(StubHttp { jwks: None });
        for tok in [
            hs256(&claims("https://evil.example", CLIENT, 3600), SECRET.as_bytes()),
            hs256(&claims(ISSUER, "someone-else", 3600), SECRET.as_bytes()),
            hs256(&claims(ISSUER, CLIENT, 3600), b"wrong-secret"),
        ] {
            assert!(
                verify_id_token(&http, &oidc(), &tok).await.is_err(),
                "iss/aud/secret mismatch must fail"
            );
        }
    }

    #[tokio::test]
    async fn rs256_token_is_verified_against_jwks() {
        let http: Arc<dyn HostHttp> = Arc::new(StubHttp { jwks: Some(JWKS.into()) });
        let header = jsonwebtoken::Header {
            alg: jsonwebtoken::Algorithm::RS256,
            kid: Some("test-key-1".into()),
            ..Default::default()
        };
        let tok = jsonwebtoken::encode(
            &header,
            &claims(ISSUER, CLIENT, 3600),
            &jsonwebtoken::EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
        )
        .unwrap();
        let got = verify_id_token(&http, &oidc(), &tok).await.expect("RS256 token via JWKS");
        assert_eq!(got.sub, "idp-user-1");
    }

    #[tokio::test]
    async fn rs256_token_with_unknown_kid_is_rejected() {
        let http: Arc<dyn HostHttp> = Arc::new(StubHttp { jwks: Some(JWKS.into()) });
        let header = jsonwebtoken::Header {
            alg: jsonwebtoken::Algorithm::RS256,
            kid: Some("no-such-key".into()),
            ..Default::default()
        };
        let tok = jsonwebtoken::encode(
            &header,
            &claims(ISSUER, CLIENT, 3600),
            &jsonwebtoken::EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(verify_id_token(&http, &oidc(), &tok).await.is_err());
    }
}
