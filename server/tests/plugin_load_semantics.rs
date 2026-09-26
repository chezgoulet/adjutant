//! DB-gated probes for issue #89: **`enabled` decides what is loaded**.
//!
//! The acceptance this file exists to prove, against a real database and real
//! plugin libraries:
//!
//! 1. a plugin whose `core.plugins.enabled = false` is **not loaded** at boot —
//!    no `dlopen`, no pool, no `init`, no migrations, and therefore **no schema
//!    content** (the tables its migrations create do not exist);
//! 2. its route answers exactly as a route does when the library is absent from
//!    disk (a plain `404`, from `RouteLookup::NotFound` — there is no
//!    "disabled" special case left);
//! 3. no connection exists as its role (`pg_stat_activity`);
//! 4. enable → disable → enable is idempotent, closes the pool on disable (the
//!    quiesce step), and **deletes nothing**: schema, rows and grants survive
//!    any number of cycles (disable is not uninstall).
//!
//! It uses the workspace's own cdylibs out of `target/debug` (copied to a temp
//! dir two at a time, so the "only the enabled plugins have schema content"
//! claim is checkable), never a hand-built fixture. `cargo build --workspace`
//! is therefore a prerequisite; a missing library is a hard failure, not a
//! skip.
//!
//! Run with `ADJUTANT_TEST_DATABASE_URL=…` and `-- --ignored`. A missing,
//! empty or unreachable URL is a hard failure (issue #25).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adjutant_sdk::HostDb;
use adjutant_server::config::Config;
use adjutant_server::plugin_runtime::{self, LoadEnv, PluginRegistry};
use adjutant_server::{cli, db, host, schema};

/// The two plugins this probe loads. Both have migrations and a table of their
/// own, which is what makes "no schema content" observable: a plugin with no
/// migrations would leave nothing to look at. `store` is the one we disable.
const STORE: &str = "store";
const FINANCE: &str = "finance";

/// Serializes the probes: they share the test database and both drop and
/// re-create `store`/`finance`, so they must not interleave.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to run the DB-gated plugin-load probes \
         (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );
    url
}

/// `target/debug` — where `cargo build --workspace` puts the cdylibs.
fn workspace_lib_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(Path::parent)
        .expect("target/debug")
        .to_path_buf()
}

/// A temp plugin dir holding exactly `store` and `finance`.
fn fixture_dir() -> PathBuf {
    fixture_dir_named("")
}

/// The same fixture, under a distinct directory name.
///
/// A test that must corrupt a library *before anything opens it* needs its own
/// path: the loader maps a library once per path for the life of the process, so
/// a test reusing this fixture's directory would `dlopen` the copy already in
/// memory and never see the corruption. (Learned the hard way — see
/// `probe_a_failed_load_is_a_409_and_records_the_reason`.)
fn fixture_dir_named(tag: &str) -> PathBuf {
    let name = if tag.is_empty() {
        format!("adjutant-load-sem-{}", std::process::id())
    } else {
        format!("adjutant-load-sem-{tag}-{}", std::process::id())
    };
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp plugin dir");
    let src = workspace_lib_dir();
    for id in [STORE, FINANCE] {
        let lib = format!("libadjutant_{id}.so");
        std::fs::copy(src.join(&lib), dir.join(&lib)).unwrap_or_else(|e| {
            panic!("{} must exist in {} (run `cargo build --workspace`): {e}", lib, src.display())
        });
    }
    dir
}

/// A migrated core schema plus the provisioning `bootstrap-isolation` does for
/// every plugin on disk: a LOGIN role, its (empty) schema, and its stored
/// secret. Roles and namespaces are provisioning, not content — they exist for a
/// disabled plugin too, deliberately (issue #89 §7).
///
/// The two fixtures start from **nothing**: their namespaces are dropped and
/// their migration records forgotten *before* provisioning re-creates the
/// namespaces. That is what makes the assertions below statements about *this*
/// boot — "the enabled plugin's migrations ran" and "the disabled plugin's did
/// not" — instead of about whatever a previous run left behind. It is the same
/// discipline `adjutant test-plugin` uses on a throwaway `_test` database.
async fn provision(dir: &Path) -> Arc<sqlx::PgPool> {
    let cfg = Config {
        database_url: base_url(),
        plugin_dir: dir.to_path_buf(),
        ..Default::default()
    };
    let admin = db::connect_and_migrate(&cfg)
        .await
        .expect("core migrations on the test database");
    for id in [STORE, FINANCE] {
        sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{id}\" CASCADE"))
            .execute(admin.as_ref())
            .await
            .expect("drop the fixture namespace");
        sqlx::query(&format!("DELETE FROM core.schema_migrations WHERE schema = '{id}'"))
            .execute(admin.as_ref())
            .await
            .expect("forget the fixture's migrations");
    }
    cli::bootstrap_isolation(&cfg, false, None, None)
        .await
        .expect("bootstrap-isolation (needs CREATEROLE)");
    admin
}

/// Set the enable flag the way the admin endpoint would (the flag is the only
/// input to the loading decision).
async fn set_enabled(admin: &sqlx::PgPool, id: &str, on: bool) {
    // The row must exist: `provision` created it.
    let n = sqlx::query("UPDATE core.plugins SET enabled = $2 WHERE id = $1")
        .bind(id)
        .bind(on)
        .execute(admin)
        .await
        .expect("set enabled")
        .rows_affected();
    assert_eq!(n, 1, "core.plugins row for {id} exists");
}

fn env_for<'a>(url: &'a str, admin: &Arc<sqlx::PgPool>) -> LoadEnv<'a> {
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    LoadEnv {
        database_url: url,
        pool: admin.clone(),
        event_tx: tx,
        config: serde_json::json!({}),
        identity: adjutant_server::identity::IdentityHub::new(),
        http: adjutant_server::host::CoreHttp::new(),
    }
}

async fn boot(dir: &Path, url: &str, admin: &Arc<sqlx::PgPool>) -> PluginRegistry {
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    plugin_runtime::load_all(
        dir,
        url,
        admin.clone(),
        tx,
        serde_json::json!({}),
        adjutant_server::identity::IdentityHub::new(),
        adjutant_server::host::CoreHttp::new(),
    )
    .await
    .expect("load_all")
}

async fn count(admin: &sqlx::PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(admin)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Connections open as a plugin's own role.
async fn role_connections(admin: &sqlx::PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE usename = $1")
        .bind(schema::role_for(id))
        .fetch_one(admin)
        .await
        .expect("pg_stat_activity")
}

/// Poll until the role has no connections (a pool close is graceful and drains
/// on a spawned task).
async fn wait_no_connections(admin: &sqlx::PgPool, id: &str) -> i64 {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let n = role_connections(admin, id).await;
        if n == 0 || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The plugin's own pool, the way `bootstrap-isolation`'s stored credential
/// allows it — used to plant a row that must survive every disable.
async fn plugin_pool(admin: &sqlx::PgPool, url: &str, id: &str) -> Arc<sqlx::PgPool> {
    let secret: String = sqlx::query_scalar::<_, Option<String>>("SELECT db_secret FROM core.plugins WHERE id = $1")
        .bind(id)
        .fetch_one(admin)
        .await
        .expect("stored credential")
        .expect("credential is present");
    host::plugin_pool(url, id, &secret, 2)
        .await
        .expect("plugin pool")
}

// ---------------------------------------------------------------------------
// 1. Boot with a plugin disabled: nothing of it is loaded
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_disabled_plugin_is_not_loaded() {
    let _serial = SERIAL.lock().await;
    let dir = fixture_dir();
    let url = base_url();
    let admin = provision(&dir).await;
    set_enabled(&admin, FINANCE, true).await;
    set_enabled(&admin, STORE, false).await;

    let reg = boot(&dir, &url, &admin).await;

    // --- the registry: store is KNOWN but not LOADED -------------------------
    let store = reg.info(STORE).expect("store is known");
    assert!(!store.loaded, "a disabled plugin must not be loaded");
    assert!(!store.enabled, "and its record says so");
    assert_eq!(store.routes, 0, "no routes");
    assert!(reg.record(STORE).is_some(), "the record (and its path) survive");
    assert!(
        reg.info(FINANCE).is_some_and(|i| i.loaded),
        "finance was enabled and must be loaded"
    );
    println!(
        "[plugin_load_semantics] registry: store loaded={} enabled={} routes={} | finance loaded={}",
        store.loaded,
        store.enabled,
        store.routes,
        reg.info(FINANCE).map(|i| i.loaded).unwrap_or(false)
    );

    // --- no schema CONTENT: the disabled plugin's migrations never ran ------
    // The namespace exists for every plugin on disk (provisioning) and was
    // re-created by bootstrap-isolation; what must not exist is a single object
    // inside it. The fixture started empty, so this is this boot's doing.
    let store_schema = count(
        &admin,
        "SELECT count(*) FROM information_schema.schemata WHERE schema_name = 'store'",
    )
    .await;
    let store_table = count(
        &admin,
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'store'",
    )
    .await;
    assert_eq!(store_table, 0, "a disabled plugin's migrations must not have run");
    assert_eq!(
        store_schema, 1,
        "its namespace remains: the schema is provisioning, created by bootstrap-isolation"
    );
    let store_table_present: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('store.catalogue_items')::text")
            .fetch_one(admin.as_ref())
            .await
            .expect("regclass");
    assert!(store_table_present.is_none(), "store.catalogue_items must not exist");
    println!(
        "[plugin_load_semantics] store: schemas={store_schema} tables_in_schema={store_table} \
         to_regclass(store.catalogue_items)={}",
        store_table_present.as_deref().unwrap_or("NULL")
    );

    // The enabled plugin did migrate — the probe is not vacuous.
    let finance_schema = count(
        &admin,
        "SELECT count(*) FROM information_schema.schemata WHERE schema_name = 'finance'",
    )
    .await;
    let finance_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('finance.receipts')::text")
            .fetch_one(admin.as_ref())
            .await
            .expect("regclass");
    assert_eq!(finance_schema, 1, "the enabled plugin's schema exists");
    assert!(finance_table.is_some(), "and its migrations ran");
    println!(
        "[plugin_load_semantics] finance schema count={finance_schema} to_regclass(finance.receipts)={}",
        finance_table.unwrap_or_default()
    );

    // --- no pool ------------------------------------------------------------
    let conns = role_connections(&admin, STORE).await;
    assert_eq!(conns, 0, "a disabled plugin holds no connections");
    println!("[plugin_load_semantics] pg_stat_activity as {} = {conns}", schema::role_for(STORE));

    // --- 404 parity: same answer as a library that is absent from disk -------
    let disabled_answer = reg.find("GET", "/api/store/health");
    let absent_answer = reg.find("GET", "/api/not_a_plugin_at_all/health");
    assert!(
        matches!(disabled_answer, plugin_runtime::RouteLookup::NotFound),
        "a disabled plugin's route must be NotFound"
    );
    assert!(
        matches!(absent_answer, plugin_runtime::RouteLookup::NotFound),
        "an absent plugin's route is NotFound"
    );
    println!(
        "[plugin_load_semantics] 404 parity: disabled={} absent={} (both RouteLookup::NotFound)",
        matches!(disabled_answer, plugin_runtime::RouteLookup::NotFound),
        matches!(absent_answer, plugin_runtime::RouteLookup::NotFound)
    );
}

// ---------------------------------------------------------------------------
// 2. enable → disable → enable, twice: idempotent, nothing deleted
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_enable_disable_re_enable_is_idempotent() {
    let _serial = SERIAL.lock().await;
    let dir = fixture_dir();
    let url = base_url();
    let admin = provision(&dir).await;
    set_enabled(&admin, FINANCE, true).await;
    set_enabled(&admin, STORE, false).await;

    let mut reg = boot(&dir, &url, &admin).await;
    let store_lib = dir.join(format!("libadjutant_{STORE}.so"));
    let env = env_for(&url, &admin);

    // --- enable: a load, exactly the boot body ------------------------------
    let mut seen_routes = reg.live_route_keys();
    let loaded = plugin_runtime::load_plugin(&store_lib, &env, &mut seen_routes)
        .await
        .expect("enable loads the plugin");
    assert_eq!(loaded.info.id, STORE);
    assert!(loaded.info.loaded && loaded.info.enabled);
    assert!(reg.install(loaded), "the record is replaced by the loaded plugin");
    // The flag is written only after a successful load (requirement #3).
    set_enabled(&admin, STORE, true).await;

    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('store.catalogue_items')::text")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(table.is_some(), "enable migrated the schema");
    let after_enable = role_connections(&admin, STORE).await;
    assert!(after_enable >= 1, "enable opened the plugin's pool");
    println!(
        "[plugin_load_semantics] after enable: to_regclass(store.catalogue_items)={} connections={after_enable}",
        table.unwrap_or_default()
    );

    // Plant a row through the plugin's own role: the data the cycles must keep.
    {
        let pool = plugin_pool(&admin, &url, STORE).await;
        host::CoreDb::new(pool)
            .execute(
                "INSERT INTO catalogue_items (kind, name, category, base_price_cents) \
                 VALUES ('product', 'Load Semantics Probe', 'other', 100)"
                    .to_string(),
                vec![],
            )
            .await
            .expect("insert through the plugin's own role");
    }
    let rows_before = count(&admin, "SELECT count(*) FROM store.catalogue_items").await;
    assert_eq!(rows_before, 1);

    // --- two full disable → re-enable cycles --------------------------------
    for cycle in 1..=2 {
        // Disable: routing stops first, then the pool closes (the quiesce step).
        let parts = reg.stop_live(STORE).expect("live plugin yields its parts");
        plugin_runtime::close_pool(&parts);
        set_enabled(&admin, STORE, false).await;

        assert!(
            matches!(reg.find("GET", "/api/store/health"), plugin_runtime::RouteLookup::NotFound),
            "a disabled plugin answers 404 exactly like an absent one"
        );
        let left = wait_no_connections(&admin, STORE).await;
        assert_eq!(left, 0, "disable closed the plugin's pool");

        // **Nothing is deleted** (disable ≠ uninstall).
        let rows_after_disable = count(&admin, "SELECT count(*) FROM store.catalogue_items").await;
        assert_eq!(
            rows_after_disable, rows_before,
            "disable must not delete data (cycle {cycle})"
        );
        let grants = count(
            &admin,
            "SELECT count(*) FROM core.permissions WHERE id LIKE 'store:%'",
        )
        .await;
        assert!(grants > 0, "declared permissions are left in place (cycle {cycle})");

        // Re-enable: a load, and migrations are idempotent — already-applied
        // versions are skipped (a plugin disabled before an upgrade and enabled
        // after comes up on the new schema, or refuses here).
        let mut seen = reg.live_route_keys();
        let again = plugin_runtime::load_plugin(&store_lib, &env, &mut seen)
            .await
            .unwrap_or_else(|e| panic!("re-enable failed on cycle {cycle}: {e}"));
        assert!(reg.install(again), "the record is replaced by the loaded plugin");
        set_enabled(&admin, STORE, true).await;
        assert!(
            matches!(reg.find("GET", "/api/store/health"), plugin_runtime::RouteLookup::Found { .. }),
            "re-enabled plugin serves again (cycle {cycle})"
        );

        let rows_after = count(&admin, "SELECT count(*) FROM store.catalogue_items").await;
        assert_eq!(rows_after, rows_before, "data intact after the cycle (cycle {cycle})");
        println!(
            "[plugin_load_semantics] cycle {cycle}: connections_after_disable={left} \
             catalogue_items={rows_after} store_permissions={grants} route_404_while_disabled=true"
        );
    }

    // --- the record still exists, so a later enable can find it by id -------
    // Leave it disabled, which is the state an admin would act on.
    let parts = reg.stop_live(STORE).expect("live plugin yields its parts");
    plugin_runtime::close_pool(&parts);
    set_enabled(&admin, STORE, false).await;
    let record = reg.record(STORE).expect("the disabled record survives the cycles");
    assert_eq!(record.info.id, STORE);
    assert!(!record.info.loaded && !record.info.enabled);
    assert_eq!(record.info.routes, 0);
    assert!(record.path.ends_with("libadjutant_store.so"), "the record keeps the library path");
    assert!(reg.contains(STORE), "and the registry still knows it");
    println!(
        "[plugin_load_semantics] final: store known=true loaded=false routes=0 path={}",
        record.path.display()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A sanity check on the fixture itself: the two libraries really do follow the
/// staging convention the loader uses to decide the disabled case without a
/// `dlopen`.
#[test]
fn fixture_libraries_follow_the_staging_convention() {
    let dir = fixture_dir();
    for id in [STORE, FINANCE] {
        let path = dir.join(format!("libadjutant_{id}.so"));
        assert!(path.exists(), "{}", path.display());
        assert_eq!(plugin_runtime::staged_plugin_id(&path), Some(id.to_string()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. The admin endpoints themselves — the live enable/disable path
// ---------------------------------------------------------------------------
//
// Why this section exists. The registry half of this file (1 and 2) proves the
// primitives the lifecycle is *built from*: `load_plugin`, `install`,
// `stop_live`, `close_pool`, and the flag written by hand with SQL. It never
// touches `enable_plugin` / `disable_plugin`, so it cannot see the
// serialization (`LifecycleLocks`), the drain the handler reports, the
// flag-written-last ordering on the real path, or the 409 a failed load
// produces. Those live in the handler and were the part of requirements #1-#3
// that no probe reached.
//
// These probes boot the real application — the same router, the same
// middleware, the same handlers — on an ephemeral port and speak HTTP to it, so
// what is asserted is what an admin would actually get.
//
// `finance` is the subject, not `store`. The endpoint correctly refuses to
// enable `store` while `stripe` is not loaded (`PLUGIN_DEPENDENCIES`), and that
// refusal has its own unit test; `finance` declares no dependencies, so it is
// the plugin that isolates the lifecycle path from the dependency rules.

/// Boot the real app and return its base URL. The returned `JoinHandle` keeps
/// the serve task alive for the duration of the caller's scope.
async fn spawn_app(
    dir: &Path,
    url: &str,
) -> (String, Arc<adjutant_server::server::AppState>, tokio::task::JoinHandle<()>) {
    let cfg = Config {
        database_url: url.to_string(),
        plugin_dir: dir.to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind address"),
        allow_dev_headers: true,
        allow_superuser: true,
        ..Default::default()
    };
    let (app, state) = adjutant_server::build_app(&cfg).await.expect("the real app boots");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port");
    let addr = listener.local_addr().expect("local address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state, handle)
}

/// One admin request, as the chief. Returns (status, body).
///
/// Named `admin_req`, not `admin`: these tests hold their pool in a local called
/// `admin`, and a same-named helper would be shadowed by it.
async fn admin_req(base: &str, method: reqwest::Method, path: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .header("x-dev-user", "christopher")
        .header("x-dev-role", "chief")
        .send()
        .await
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// The plain body a request gets when nothing serves the path — the answer a
/// disabled plugin must be indistinguishable from (issue #89's parity claim).
fn route_not_found() -> serde_json::Value {
    serde_json::json!({ "error": "route not found" })
}

async fn flag_enabled(admin: &sqlx::PgPool, id: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT enabled FROM core.plugins WHERE id = $1")
        .bind(id)
        .fetch_one(admin)
        .await
        .expect("flag read")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_admin_endpoints_drive_the_lifecycle() {
    let _serial = SERIAL.lock().await;
    let dir = fixture_dir();
    let url = base_url();
    let admin = provision(&dir).await;
    set_enabled(&admin, FINANCE, true).await;
    set_enabled(&admin, STORE, false).await;

    let (base, state, _serve) = spawn_app(&dir, &url).await;

    /// The permission-gated route this test uses as its routing probe.
    const HEALTH: &str = "/api/finance/health";

    // Establish the absent answer first, so the parity assertion below compares
    // against something observed rather than something assumed.
    let (absent_status, absent_body) =
        admin_req(&base, reqwest::Method::GET, "/api/nothing_here/nothing").await;
    assert_eq!(absent_status, 404, "a path nothing serves is a 404");
    assert_eq!(absent_body, route_not_found(), "with the plain not-found body");

    // The boot state, read through the registry the handlers will mutate.
    {
        let reg = state.registry.read().await;
        let fin = reg.info(FINANCE).expect("finance is known");
        assert!(fin.loaded && fin.enabled, "finance booted enabled");
        let store = reg.info(STORE).expect("store is known");
        assert!(!store.loaded && !store.enabled, "store booted disabled");
        assert!(reg.record(STORE).is_some(), "and keeps a record, with a path");
    }
    assert!(flag_enabled(&admin, FINANCE).await);

    // --- disable, through the endpoint --------------------------------------
    let (status, body) = admin_req(&base, reqwest::Method::POST, "/api/plugins/finance/disable").await;
    assert_eq!(status, 200, "disable must succeed: {body}");
    assert_eq!(body["enabled"], serde_json::json!(false));
    assert_eq!(body["loaded"], serde_json::json!(false));
    // The drain is reported, not assumed (requirement #1). Nothing is in flight
    // here so it completes at once; the bounded timeout has its own unit test
    // (`server::in_flight_counts_and_drains`).
    assert_eq!(body["drained"], serde_json::json!(true), "an idle plugin drains");
    assert!(body["drain_ms"].is_u64(), "the wait is reported, not hidden: {body}");
    assert!(
        body["data"].as_str().is_some_and(|d| d.contains("preserved")),
        "the response says what survived: {body}"
    );

    assert!(!flag_enabled(&admin, FINANCE).await, "the durable flag follows the teardown");

    // Routing really stopped, and the answer is the absent one — not a special
    // "this plugin is disabled" message. Note the enabled case below cannot be
    // asserted as a 200: the health route is permission-gated, so a live plugin
    // may answer 403 to a caller without `finance:read`. What the disabled
    // state must not do is answer as though the route existed at all.
    let (disabled_status, disabled_body) = admin_req(&base, reqwest::Method::GET, HEALTH).await;
    assert_eq!(
        disabled_status, 404,
        "a disabled plugin's route does not resolve: {disabled_body}"
    );
    assert_eq!(
        disabled_body, absent_body,
        "and it answers exactly as an absent plugin does, byte for byte"
    );

    // Nothing is deleted: the schema content the plugin created survives.
    let present: Option<String> = sqlx::query_scalar("SELECT to_regclass('finance.receipts')::text")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(present.is_some(), "disable is not uninstall: the table is still there");

    // --- and back on, through the endpoint ----------------------------------
    let (status, body) = admin_req(&base, reqwest::Method::POST, "/api/plugins/finance/enable").await;
    assert_eq!(status, 200, "enable must succeed: {body}");
    assert_eq!(body["enabled"], serde_json::json!(true));
    assert_eq!(body["loaded"], serde_json::json!(true), "the endpoint reports what it loaded");
    assert!(
        flag_enabled(&admin, FINANCE).await,
        "the durable flag follows a successful load (requirement #3)"
    );

    let (enabled_status, enabled_body) = admin_req(&base, reqwest::Method::GET, HEALTH).await;
    assert_ne!(
        enabled_status, 404,
        "the route resolves again — a 403 from the permission gate would still prove that, \
         a 404 would not"
    );
    assert_ne!(
        enabled_body,
        route_not_found(),
        "and it is no longer the absent-plugin answer"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #89 requirement #2: one lifecycle per plugin. Two enables racing for
/// the same plugin must not both load it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_enable_is_serialized() {
    let _serial = SERIAL.lock().await;
    let dir = fixture_dir();
    let url = base_url();
    let admin = provision(&dir).await;
    set_enabled(&admin, FINANCE, true).await;
    set_enabled(&admin, STORE, false).await;

    let (base, state, _serve) = spawn_app(&dir, &url).await;

    // Take it down first, so the two requests below have a real load to race
    // for. Enabling an already-loaded plugin is idempotent by design, so
    // racing on a live plugin would make this test vacuous.
    let (ds, db) = admin_req(&base, reqwest::Method::POST, "/api/plugins/finance/disable").await;
    assert_eq!(ds, 200, "the setup disable must succeed: {db}");

    // --- two concurrent enables: exactly one performs the load --------------
    let a = tokio::spawn({
        let base = base.clone();
        async move { admin_req(&base, reqwest::Method::POST, "/api/plugins/finance/enable").await }
    });
    let b = tokio::spawn({
        let base = base.clone();
        async move { admin_req(&base, reqwest::Method::POST, "/api/plugins/finance/enable").await }
    });
    let (ra, rb) = (a.await.expect("join a"), b.await.expect("join b"));

    assert_eq!(ra.0, 200, "first enable: {}", ra.1);
    assert_eq!(rb.0, 200, "second enable: {}", rb.1);
    let already = [&ra.1, &rb.1]
        .iter()
        .filter(|body| body.get("note").and_then(|n| n.as_str()) == Some("already loaded"))
        .count();
    assert_eq!(
        already, 1,
        "the lifecycle lock must serialize: exactly one performs the load and the other is told \
         it is already loaded. Two loads, or none, means the lock is not holding \
         (got {ra:?} / {rb:?})"
    );
    assert!(
        state.registry.read().await.live_ids().contains(FINANCE),
        "and the plugin is live once the race is over"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
