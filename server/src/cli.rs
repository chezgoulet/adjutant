//! `adjutant` subcommands: `new-plugin` (scaffold), `validate-plugin` (static
//! checks without a database), and `test-plugin` (live route probes against a
//! pristine test database) — SPEC §15 M3 exit criteria + SPEC §5.2a.
//!
//! Design note: SPEC §5.2's `manifest.json` is folded into the trait
//! implementation — `id()`/`version()`/`permissions_granted()` *are* the
//! manifest, declared in code the compiler checks. The scaffold emits that
//! shape so a new plugin starts valid.

use std::path::{Path, PathBuf};

use crate::config::Config;

// ---------------------------------------------------------------------------
// new-plugin
// ---------------------------------------------------------------------------

/// Validate a new plugin name: `[a-z][a-z0-9_]{0,30}`, and not reserved. The
/// reserved set is [`crate::plugin_runtime::RESERVED_IDS`] — the same list the
/// loader enforces for untrusted WASM guests — so the scaffolder cannot create a
/// plugin that would collide with a first-party or core id.
pub fn validate_plugin_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 31
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(format!(
            "invalid plugin name {name:?}: expected [a-z][a-z0-9_]{{0,30}} (lowercase, underscore)"
        ));
    }
    if crate::plugin_runtime::RESERVED_IDS.contains(&name) {
        return Err(format!("{name:?} is reserved by the core or an existing plugin"));
    }
    Ok(())
}

/// Scaffold `plugins/<name>/` with a compiling plugin and register it in the
/// workspace. Returns the created directory.
pub fn scaffold_plugin(repo_root: &Path, name: &str) -> Result<PathBuf, String> {
    validate_plugin_name(name)?;
    let dir = repo_root.join("plugins").join(name);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }

    let crate_name = format!("adjutant-{name}");
    let cargo = format!(
        r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "TODO: one-line description of the {name} plugin."

[lib]
# cdylib: loaded dynamically by the core; rlink: usable as a test fixture.
crate-type = ["cdylib", "rlib"]

[dependencies]
adjutant-sdk.workspace = true
serde.workspace = true
serde_json.workspace = true
"#
    );

    // The trait impl IS the manifest (SPEC §5.2 manifest.json folded in).
    // Placeholders, not format!: a code template is full of literal braces
    // and escaping them all is how scaffolding bugs get shipped.
    let lib = r#"//! __NAME__ — TODO: what this plugin owns (SPEC section reference).
//!
//! Scaffolded by `adjutant new-plugin __NAME__`. The trait implementation below
//! is the plugin's manifest: id/version/permissions/routes/migrations are
//! declared in code the compiler checks, not a JSON file.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct __STRUCT__ {
    ctx: OnceLock<PluginContext>,
}

impl __STRUCT__ {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }
}

impl Default for __STRUCT__ {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for __STRUCT__ {
    fn id(&self) -> &str {
        "__NAME__"
    }

    fn name(&self) -> &str {
        "__TITLE__"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    /// Permissions this plugin defines. A route may only require a permission
    /// the plugin itself grants — the core rejects anything else at load time.
    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new("__NAME__:read", "Read __TITLE__ data"),
            Permission::new("__NAME__:manage", "Modify __TITLE__ data"),
        ]
    }

    /// Runs once, in order, inside this plugin's own PostgreSQL schema
    /// (search_path pre-set by the core). Recorded in core.schema_migrations.
    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "initial_schema",
            "CREATE TABLE IF NOT EXISTS items (\
                 id BIGSERIAL PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );",
        )]
    }

    /// Routes must live under /api/__NAME__/ — the core rejects namespace
    /// escapes at load. Gate them with the *_protected constructors.
    fn routes(&self) -> Vec<RouteDefinition> {
        let c = self.ctx().clone();
        let open = RouteDefinition::get(
            "/api/__NAME__/health",
            route_handler(|_req| async {
                PluginResponse::json(200, &serde_json::json!({"plugin": "__NAME__", "ok": true}))
            }),
        );

        let c2 = c.clone();
        let list = RouteDefinition::get_protected(
            "/api/__NAME__/items",
            "__NAME__:read",
            route_handler(move |_req| {
                let c = c2.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            &format!(
                                "SELECT id, name FROM {} ORDER BY id",
                                c.db.table("items")
                            ),
                            vec![],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({"items": rows}))
                }
            }),
        );

        let _ = c;
        vec![open, list]
    }
}

export_plugin!(__STRUCT__);
"#;
    let lib = lib
        .replace("__NAME__", name)
        .replace("__STRUCT__", &struct_name_for(name))
        .replace("__TITLE__", &title_for(name));

    std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("Cargo.toml"), cargo).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src").join("lib.rs"), lib).map_err(|e| e.to_string())?;

    // Register in the workspace members list (idempotent).
    let ws_path = repo_root.join("Cargo.toml");
    let ws = std::fs::read_to_string(&ws_path).map_err(|e| e.to_string())?;
    let member = format!("\"plugins/{name}\",");
    if !ws.contains(&member) {
        let anchor = "\"plugins/sdk\",";
        if !ws.contains(anchor) {
            return Err("workspace Cargo.toml has no plugins/sdk member to anchor on".into());
        }
        let ws = ws.replacen(anchor, &format!("{anchor}\n    {member}"), 1);
        std::fs::write(&ws_path, ws).map_err(|e| e.to_string())?;
    }

    Ok(dir)
}

/// `my_plugin` → `MyPlugin` (Rust type name).
fn struct_name_for(name: &str) -> String {
    name.split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `my_plugin` → `My Plugin` (display title).
fn title_for(name: &str) -> String {
    name.replace('_', " ")
}

// ---------------------------------------------------------------------------
// test-plugin
// ---------------------------------------------------------------------------

/// Pure test-database derivation: `…/adjutant_dev` → `…/adjutant_dev_test`,
/// preserving any query string and never doubling an existing `_test` suffix.
///
/// Split out from the environment lookup so it can be tested without depending
/// on ambient state (the earlier single-function version made the unit test fail
/// in any shell or CI job that exports `ADJUTANT_TEST_DATABASE_URL`, which is the
/// documented workflow for the DB-backed tests).
pub fn derive_test_database_url(base: &str) -> String {
    match base.rsplit_once('/') {
        Some((prefix, rest)) => {
            let (db, query) = match rest.split_once('?') {
                Some((d, q)) => (d, format!("?{q}")),
                None => (rest, String::new()),
            };
            let stem = db.strip_suffix("_test").unwrap_or(db);
            format!("{prefix}/{stem}_test{query}")
        }
        None => base.to_string(),
    }
}

/// Test database for `test-plugin` and the DB-backed tests.
/// `ADJUTANT_TEST_DATABASE_URL` overrides the derivation entirely.
pub fn test_database_url(cfg: &Config) -> String {
    test_database_url_with(cfg, std::env::var("ADJUTANT_TEST_DATABASE_URL").ok())
}

/// The override decision, with the environment value passed in — so tests cover
/// both branches without mutating process-global state. An empty override is
/// treated as unset: callers commonly export `ADJUTANT_TEST_DATABASE_URL=` to
/// mean "derive it", and treating that as a set URL failed with
/// `cannot parse db name from `.
pub fn test_database_url_with(cfg: &Config, override_url: Option<String>) -> String {
    override_url
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| derive_test_database_url(&cfg.database_url))
}

/// URL of the maintenance database (`/postgres`) for drop/create.
fn maintenance_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/postgres"),
        None => url.to_string(),
    }
}

/// Drop + recreate the test database so every run starts pristine.
pub async fn reset_database(test_url: &str) -> Result<(), String> {
    let maint = maintenance_url(test_url);
    let dbname = test_url
        .rsplit_once('/')
        .map(|(_, d)| d.split('?').next().unwrap_or(d).to_string())
        .ok_or_else(|| format!("cannot parse db name from {test_url}"))?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&maint)
        .await
        .map_err(|e| format!("connect {maint}: {e}"))?;

    // Terminate stragglers, then recreate. Identifiers are quoted, and the
    // name only ever comes from our own config URL.
    let kill = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{dbname}' AND pid <> pg_backend_pid()"
    );
    sqlx::query(&kill).execute(&pool).await.map_err(|e| e.to_string())?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{dbname}\""))
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(&format!("CREATE DATABASE \"{dbname}\""))
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    pool.close().await;
    Ok(())
}

/// One probe result for the report table.
pub struct Probe {
    pub name: String,
    pub status: u16,
    pub expect: String,
    pub ok: bool,
    pub detail: String,
}

// ---------------------------------------------------------------------------
// bootstrap-isolation
// ---------------------------------------------------------------------------

/// Create/refresh the per-plugin `LOGIN` roles, schema ownership, `core.*`
/// grants and stored passwords, for every plugin in the configured directory.
///
/// Run once by the operator against an admin URL; the runtime never needs
/// `CREATEROLE`. Idempotent — existing passwords are preserved unless `rotate`.
/// Returns the plugin ids that were bootstrapped.
pub async fn bootstrap_isolation(cfg: &Config, rotate: bool) -> Result<Vec<String>, String> {
    // Core migrations first: `db_secret` and `core.record_migration` must exist.
    let pool = crate::db::connect_and_migrate(cfg)
        .await
        .map_err(|e| format!("database: {e}"))?;

    let discovered = crate::plugin_runtime::discover_plugins(&cfg.plugin_dir)
        .await
        .map_err(|e| e.to_string())?;

    let mut ids = Vec::new();
    for d in discovered {
        let existing: Option<Option<String>> =
            sqlx::query_scalar::<_, Option<String>>("SELECT db_secret FROM core.plugins WHERE id = $1")
                .bind(&d.id)
                .fetch_optional(pool.as_ref())
                .await
                .map_err(|e| format!("read {} credential: {e}", d.id))?;
        let secret =
            crate::schema::bootstrap_role(pool.as_ref(), &d.id, existing.flatten().as_deref(), rotate)
                .await
                .map_err(|e| format!("bootstrap {}: {e}", d.id))?;
        sqlx::query(
            "INSERT INTO core.plugins (id, version, db_secret) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, \
             db_secret = EXCLUDED.db_secret, updated_at = now()",
        )
        .bind(&d.id)
        .bind(&d.version)
        .bind(&secret)
        .execute(pool.as_ref())
        .await
        .map_err(|e| format!("store {} credential: {e}", d.id))?;
        ids.push(d.id);
    }
    Ok(ids)
}

/// Boot the core against a pristine test database, then probe every registered
/// plugin route with mock permissions (dev headers = the mock identity layer):
///
/// * permission-gated route, anonymous → expect 401/403 (the gate fires)
/// * open GET route, anonymous → expect <500 and not 404 (handler runs)
/// * open GET route, as `chief` → expect <500 and not 404 (identity attaches)
/// * mutating open routes → listed as skipped (no side effects in a probe)
pub async fn run_test_plugin(cfg: &Config) -> Result<Vec<Probe>, String> {
    let test_url = test_database_url(cfg);
    if test_url == cfg.database_url {
        return Err(format!(
            "refusing to wipe the live database ({test_url}); set ADJUTANT_TEST_DATABASE_URL"
        ));
    }
    reset_database(&test_url).await?;

    let mut cfg = cfg.clone();
    cfg.database_url = test_url.clone();
    cfg.bind = "127.0.0.1:0".parse().expect("ephemeral");
    cfg.rate.max_requests = 0; // probes would trip the limiter otherwise
    cfg.allow_dev_headers = true; // mock permissions
    // The harness runs against a throwaway `_test` database, usually as a
    // superuser; the refusal is about a real deployment connection.
    cfg.allow_superuser = true;
    cfg.log_filter = "warn".into();

    // A plugin now loads on its own restricted role/pool, so the pristine test
    // database needs its roles bootstrapped (the runtime cannot create them).
    bootstrap_isolation(&cfg, false).await?;

    let (app, _state) = crate::build_app(&cfg)
        .await
        .map_err(|e| format!("build_app failed: {e}"))?;

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let base = format!("http://{addr}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    // Give the listener a beat, then read the live route table.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut probes = Vec::new();

    let health = client.get(format!("{base}/")).send().await.map_err(|e| e.to_string())?;
    probes.push(Probe {
        name: "core health".into(),
        status: health.status().as_u16(),
        expect: "200".into(),
        ok: health.status().as_u16() == 200,
        detail: "GET /".into(),
    });

    // /api/plugins is admin-gated (SPEC §5.3); this harness runs with the dev
    // identity stub on, so it authenticates as its mock chief.
    let list = client
        .get(format!("{base}/api/plugins"))
        .header("x-dev-user", "test-plugin")
        .header("x-dev-role", "chief")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let list_status = list.status().as_u16();
    let body: serde_json::Value = list.json().await.unwrap_or_default();
    let plugins = body["plugins"].as_array().cloned().unwrap_or_default();
    probes.push(Probe {
        name: "plugin registry".into(),
        status: list_status,
        expect: ">=1 plugin".into(),
        ok: list_status == 200 && !plugins.is_empty(),
        detail: format!("{} plugin(s) loaded", plugins.len()),
    });

    // Flatten every registered route from the live registry (M2 route_list).
    let mut routes: Vec<(String, String, String, Option<String>)> = Vec::new();
    for p in &plugins {
        let pid = p["id"].as_str().unwrap_or_default().to_string();
        for r in p["route_list"].as_array().into_iter().flatten() {
            routes.push((
                pid.clone(),
                r["method"].as_str().unwrap_or("GET").to_string(),
                r["path"].as_str().unwrap_or_default().to_string(),
                r["permission"].as_str().map(String::from),
            ));
        }
    }

    for (pid, method, path, perm) in routes {
        // A templated path has no concrete value to probe with. Sending the
        // template text itself would exercise the handler with a nonsense capture
        // and count as a pass, so report it as skipped instead (the pass tally
        // excludes `skipped`).
        if path.contains('{') {
            probes.push(Probe {
                name: format!("{pid} {method} {path} capture route"),
                status: 0,
                expect: "skipped".into(),
                ok: true,
                detail: "path capture — needs a concrete value (not probed)".into(),
            });
            continue;
        }
        let url = format!("{base}{path}");

        // --- anonymous pass -------------------------------------------------
        if method == "GET" {
            match client.get(&url).send().await {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    let (ok, expect) = match &perm {
                        Some(_) => (s == 401 || s == 403, "401|403 (gate)"),
                        // !404 = route resolves; !500 = handler didn't crash.
                        // 501 is a designed response (e.g. OIDC unconfigured).
                        None => (s != 404 && s != 500, "!404 !500"),
                    };
                    probes.push(Probe {
                        name: format!("{pid} GET {path} anonymous"),
                        status: s,
                        expect: expect.into(),
                        ok,
                        detail: perm
                            .clone()
                            .map(|p| format!("requires {p}"))
                            .unwrap_or_else(|| "open".into()),
                    });
                }
                Err(e) => probes.push(Probe {
                    name: format!("{pid} GET {path} anonymous"),
                    status: 0,
                    expect: "any".into(),
                    ok: false,
                    detail: format!("transport: {e}"),
                }),
            }

            // --- chief pass (mock identity) --------------------------------
            match client
                .get(&url)
                .header("x-dev-user", "chief-user")
                .header("x-dev-role", "chief")
                .send()
                .await
            {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    let ok = s != 404 && s != 500;
                    probes.push(Probe {
                        name: format!("{pid} GET {path} as chief"),
                        status: s,
                        expect: "!404 !500".into(),
                        ok,
                        detail: perm
                            .clone()
                            .map(|p| format!("requires {p}"))
                            .unwrap_or_else(|| "open".into()),
                    });
                }
                Err(e) => probes.push(Probe {
                    name: format!("{pid} GET {path} as chief"),
                    status: 0,
                    expect: "any".into(),
                    ok: false,
                    detail: format!("transport: {e}"),
                }),
            }
        } else if perm.is_some() {
            // Protected mutating route: anonymous must be rejected by the gate
            // BEFORE the handler runs — safe, provably side-effect free.
            let send = match method.as_str() {
                "POST" => client.post(&url),
                "PUT" => client.put(&url),
                "DELETE" => client.delete(&url),
                other => {
                    probes.push(Probe {
                        name: format!("{pid} {method} {path} anonymous"),
                        status: 0,
                        expect: "401|403".into(),
                        ok: false,
                        detail: format!("unsupported method {other}"),
                    });
                    continue;
                }
            };
            match send.send().await {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    probes.push(Probe {
                        name: format!("{pid} {method} {path} anonymous"),
                        status: s,
                        expect: "401|403 (gate)".into(),
                        ok: s == 401 || s == 403,
                        detail: format!("requires {}", perm.unwrap_or_default()),
                    });
                }
                Err(e) => probes.push(Probe {
                    name: format!("{pid} {method} {path} anonymous"),
                    status: 0,
                    expect: "any".into(),
                    ok: false,
                    detail: format!("transport: {e}"),
                }),
            }
        } else {
            // Open mutating route: probing it would cause side effects.
            probes.push(Probe {
                name: format!("{pid} {method} {path} anonymous"),
                status: 0,
                expect: "skipped".into(),
                ok: true,
                detail: "open mutating route — not probed (side effects)".into(),
            });
        }
    }

    // reqwest pools keep-alive connections; axum's graceful shutdown waits for
    // them to close — a test harness has no reason to drain, so abort instead.
    drop(client);
    let _ = shutdown_tx;
    server.abort();
    let _ = server.await;

    Ok(probes)
}

// ---------------------------------------------------------------------------
// validate-plugin
// ---------------------------------------------------------------------------

/// One line of the `validate-plugin` report.
#[derive(Debug)]
pub struct ValidationCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn ok_check(name: &str, detail: &str) -> ValidationCheck {
    ValidationCheck { name: name.into(), ok: true, detail: detail.into() }
}

fn fail_check(name: &str, detail: &str) -> ValidationCheck {
    ValidationCheck { name: name.into(), ok: false, detail: detail.into() }
}

/// Validate a compiled plugin `.so` without a database or a server: verify the
/// ABI handshake, construct the plugin, run `init` against the SDK's in-memory
/// host mocks, then validate the declaration (id, permissions, migrations,
/// route namespace/captures/references/duplicates). Never connects to
/// PostgreSQL — safe to run in CI before a plugin is ever installed.
pub async fn validate_plugin(so_path: &Path) -> Result<Vec<ValidationCheck>, String> {
    if !so_path.is_file() {
        return Err(format!("{} is not a file", so_path.display()));
    }
    let lib = unsafe { libloading::Library::new(so_path) }
        .map_err(|e| format!("load {}: {e}", so_path.display()))?;

    let mut checks = Vec::new();

    // 1. ABI handshake — must succeed before we touch the vtable.
    match crate::plugin_runtime::check_sdk_abi(&lib, so_path) {
        Ok(()) => checks.push(ok_check(
            "sdk abi",
            &format!("matches adjutant-sdk {}", adjutant_sdk::SDK_VERSION),
        )),
        Err(e) => {
            checks.push(fail_check("sdk abi", &e.to_string()));
            return Ok(checks); // cannot safely continue past a mismatch
        }
    }

    // 2. Construct the plugin (same symbol the core resolves).
    let mut plugin: Box<dyn adjutant_sdk::AdjutantPlugin> = {
        let factory = unsafe {
            lib.get::<adjutant_sdk::PluginFactory>(adjutant_sdk::ENTRY_SYMBOL)
        }
        .map_err(|e| {
            format!(
                "missing `{}` symbol: {e}",
                String::from_utf8_lossy(adjutant_sdk::ENTRY_SYMBOL)
            )
        })?;
        unsafe { Box::from_raw(factory()) }
    };
    let id = plugin.id().to_string();

    // 3. Identifier.
    match crate::plugin_runtime::validate_plugin_id(&id) {
        Ok(()) => checks.push(ok_check("id", &id)),
        Err(e) => checks.push(fail_check("id", &e)),
    }

    // 4. `init` against in-memory host mocks. Plugins set their context before
    //    any fallible work, so this is enough to make `routes()` callable.
    let host = adjutant_sdk::testing::TestHost::new();
    match plugin.init(host.context(&id)).await {
        Ok(()) => checks.push(ok_check("init", "ran against in-memory host mocks")),
        Err(e) => checks.push(fail_check("init", &e.to_string())),
    }

    let granted = plugin.permissions_granted();
    checks.push(ok_check("permissions", &format!("{} declared", granted.len())));

    let migrations = plugin.migrations();
    match crate::plugin_runtime::validate_migrations(&id, &migrations) {
        Ok(()) => checks.push(ok_check("migrations", &format!("{} declared", migrations.len()))),
        Err(e) => checks.push(fail_check("migrations", &e.to_string())),
    }

    let routes = plugin.routes();
    let route_detail = format!("{} declared", routes.len());
    let mut seen_routes = std::collections::HashMap::new();
    match crate::plugin_runtime::validate_declaration(&id, &granted, &routes, &mut seen_routes) {
        Ok(()) => checks.push(ok_check("routes", &route_detail)),
        Err(e) => checks.push(fail_check("routes", &e.to_string())),
    }

    Ok(checks)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_validation() {
        assert!(validate_plugin_name("scouting").is_ok());
        assert!(validate_plugin_name("gear_locker").is_ok());
        assert!(validate_plugin_name("").is_err());
        assert!(validate_plugin_name("Camel").is_err());
        assert!(validate_plugin_name("9lives").is_err());
        assert!(validate_plugin_name("drop table").is_err());
        assert!(validate_plugin_name("auth").is_err(), "reserved names rejected");
        assert!(validate_plugin_name("plugins").is_err());
        assert!(validate_plugin_name(&"x".repeat(40)).is_err());
    }

    #[tokio::test]
    async fn validate_plugin_rejects_a_missing_file() {
        let err = validate_plugin(Path::new("/no/such/plugin.so"))
            .await
            .unwrap_err();
        assert!(err.contains("not a file"), "got: {err}");
    }

    #[test]
    fn struct_names_camel_case() {        assert_eq!(struct_name_for("gear_locker"), "GearLocker");
        assert_eq!(struct_name_for("scouting"), "Scouting");
        assert_eq!(struct_name_for("a_b_c"), "ABC");
    }

    #[test]
    fn scaffold_creates_compiling_layout() {
        let tmp = std::env::temp_dir().join(format!("adjutant-scaffold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // minimal workspace anchor
        std::fs::write(
            tmp.join("Cargo.toml"),
            "[workspace]\nmembers = [\n    \"plugins/sdk\",\n]\n",
        )
        .unwrap();

        let dir = scaffold_plugin(&tmp, "gear_locker").unwrap();
        assert!(dir.join("Cargo.toml").exists());
        let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(lib.contains("impl AdjutantPlugin for GearLocker"));
        assert!(lib.contains("export_plugin!(GearLocker)"));
        assert!(lib.contains("\"gear_locker:read\""));
        assert!(lib.contains("Migration::new"));

        let ws = std::fs::read_to_string(tmp.join("Cargo.toml")).unwrap();
        assert!(ws.contains("\"plugins/gear_locker\","));
        assert_eq!(ws.matches("\"plugins/gear_locker\",").count(), 1, "idempotent");

        // duplicate refused; reserved refused
        assert!(scaffold_plugin(&tmp, "gear_locker").is_err());
        assert!(scaffold_plugin(&tmp, "auth").is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_database_url_derivation_is_env_independent() {
        // Assertions run against the pure function: the ambient environment
        // (notably ADJUTANT_TEST_DATABASE_URL, which CI and the documented
        // workflow both set) can no longer change the outcome.
        assert_eq!(
            derive_test_database_url("postgres://adjutant@127.0.0.1:5433/adjutant_dev"),
            "postgres://adjutant@127.0.0.1:5433/adjutant_dev_test",
            "live DB gets _test appended"
        );
        assert_eq!(
            derive_test_database_url("postgres://adjutant@127.0.0.1:5433/adjutant_test"),
            "postgres://adjutant@127.0.0.1:5433/adjutant_test",
            "an existing _test suffix is not doubled"
        );
        assert_eq!(
            derive_test_database_url("postgres://h/dbname?sslmode=disable"),
            "postgres://h/dbname_test?sslmode=disable",
            "query string survives the rewrite"
        );
        assert_eq!(derive_test_database_url("not-a-url"), "not-a-url");
        assert_eq!(
            maintenance_url("postgres://h:5433/adjutant_test"),
            "postgres://h:5433/postgres"
        );
    }

    #[test]
    fn test_database_url_override_is_decided_explicitly() {
        let cfg = Config {
            database_url: "postgres://adjutant@127.0.0.1:5433/adjutant_dev".into(),
            ..Config::default()
        };
        assert_eq!(
            test_database_url_with(&cfg, Some("postgres://other/db".into())),
            "postgres://other/db",
            "explicit override wins"
        );
        assert_eq!(
            test_database_url_with(&cfg, None),
            "postgres://adjutant@127.0.0.1:5433/adjutant_dev_test",
            "no override derives from the configured live URL"
        );
        // An empty (or whitespace) override is "unset", not a URL to connect to.
        assert_eq!(
            test_database_url_with(&cfg, Some(String::new())),
            "postgres://adjutant@127.0.0.1:5433/adjutant_dev_test",
            "an empty override falls back to the derivation"
        );
        assert_eq!(
            test_database_url_with(&cfg, Some("   ".into())),
            "postgres://adjutant@127.0.0.1:5433/adjutant_dev_test",
            "a whitespace override is treated as unset"
        );
        // The public wrapper must agree with whichever branch the environment
        // selects — this passes whether or not the variable is exported. An
        // empty value is "unset" (see `test_database_url_with`).
        match std::env::var("ADJUTANT_TEST_DATABASE_URL") {
            Ok(v) if !v.trim().is_empty() => assert_eq!(test_database_url(&cfg), v),
            _ => assert_eq!(
                test_database_url(&cfg),
                "postgres://adjutant@127.0.0.1:5433/adjutant_dev_test"
            ),
        }
    }
}
