//! DB-backed coverage for the host I/O layer — the code path behind M3 bug 2
//! (`text[]` arrays decoded to Null, which silently emptied every role list and
//! broke permission checks).
//!
//! These tests are `#[ignore]`d on purpose (issue #25). A bare
//! `cargo test --workspace` reports them as **ignored**, never as passed, so a
//! green local run cannot hide a database test that did not execute. CI runs
//! them explicitly against a throwaway `_test` database:
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//!   cargo test -p adjutant-server --test host_db -- --ignored
//! ```
//!
//! Under `--ignored` a missing, empty, or unreachable
//! `ADJUTANT_TEST_DATABASE_URL` is a hard failure — never a skip.
use std::sync::Arc;

use adjutant_sdk::{
    schedule_handler, HostDb, Identity, PermissionService, RoleGrant, Schedule, Scope, SqlValue,
};
use adjutant_server::host::CoreDb;
use adjutant_server::scheduler::Scheduler;
use adjutant_server::scope_hierarchy::ScopeHierarchy;
use adjutant_server::{db, host, schema};

async fn repository_pool() -> Arc<sqlx::PgPool> {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to run the DB-gated host tests \
         (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("ADJUTANT_TEST_DATABASE_URL is set but unreachable");
    Arc::new(pool)
}

async fn db() -> Arc<CoreDb> {
    CoreDb::new(repository_pool().await)
}

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn decode_covers_every_supported_type() {
    let db = db().await;
    let rows = db
        .query(
            "SELECT '{\"a\":1}'::jsonb AS j, \
                    true AS b, \
                    7::bigint AS i, \
                    1.5::float8 AS f, \
                    'txt'::text AS t, \
                    ARRAY['x','y']::text[] AS arr, \
                    '2026-09-22 00:00:00+00'::timestamptz::text AS ts, \
                    NULL::text AS n"
                .to_string(),
            vec![],
        )
        .await
        .expect("query");
    let row = rows.first().expect("one row");
    assert_eq!(row["j"], serde_json::json!({"a": 1}), "jsonb decodes");
    assert_eq!(row["b"], serde_json::json!(true), "bool decodes");
    assert_eq!(row["i"], serde_json::json!(7), "i64 decodes");
    assert_eq!(row["f"], serde_json::json!(1.5), "f64 decodes");
    assert_eq!(row["t"], serde_json::json!("txt"), "text decodes");
    // The regression: before Vec<String> was added to the fallback chain this
    // came back as Null.
    assert_eq!(row["arr"], serde_json::json!(["x", "y"]), "text[] decodes");
    assert!(row["ts"].is_string(), "timestamptz cast to text decodes");
    assert_eq!(row["n"], serde_json::Value::Null, "NULL stays Null");
}

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn bind_params_round_trips_every_variant() {
    let db = db().await;
    let out = db
        .query(
            "SELECT $1::text AS t, $2::bigint AS i, $3::bool AS b, $4::text[] AS arr, $5::jsonb AS j"
                .to_string(),
            vec![
                SqlValue::Text("hello".into()),
                SqlValue::Int(42),
                SqlValue::Bool(true),
                SqlValue::TextArray(vec!["a".into(), "b".into()]),
                SqlValue::Json("{\"k\":1}".into()),
            ],
        )
        .await
        .expect("query");
    let row = out.first().expect("one row");
    assert_eq!(row["t"], serde_json::json!("hello"));
    assert_eq!(row["i"], serde_json::json!(42));
    assert_eq!(row["b"], serde_json::json!(true));
    assert_eq!(row["arr"], serde_json::json!(["a", "b"]));
    assert_eq!(row["j"], serde_json::json!({"k": 1}));
}

// ---------------------------------------------------------------------------
// Confinement probes (issues #17/#18; design docs/design/plugin-isolation.md §5)
//
// The plugin's connection IS the restricted principal. Each probe below fails
// if the mechanism regresses. They need a database whose connecting role can
// `CREATEROLE` (a throwaway superuser container); a failure to establish the
// boundary fails the test rather than skipping (issue #25).
// ---------------------------------------------------------------------------

fn base_url() -> String {
    std::env::var("ADJUTANT_TEST_DATABASE_URL").expect("ADJUTANT_TEST_DATABASE_URL")
}

/// Serializes fixture setup. `CREATE SCHEMA/TABLE IF NOT EXISTS` still races
/// between sessions, so concurrent probes would both try to migrate `core` and
/// one would fail; the lock makes setup strict. Probes themselves still run in
/// parallel.
static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes the probes that mutate the shared `core.scope_hierarchy` table
/// (they assert on its global contents, so they must not interleave).
static HIERARCHY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes the audit probes: they truncate `core.audit_log`, and the keyed
/// one installs the process-global chain key.
static AUDIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Migrate `core` on the test database and bootstrap a plugin fixture role/schema
/// for each id, returning `(admin_pool, plugin_pools)`. Mirrors a real
/// deployment's first `bootstrap-isolation` run.
async fn setup(ids: &[&str]) -> (Arc<sqlx::PgPool>, Vec<Arc<sqlx::PgPool>>) {
    let _guard = SETUP.lock().await;
    let cfg = adjutant_server::config::Config {
        database_url: base_url(),
        ..Default::default()
    };
    let admin = db::connect_and_migrate(&cfg)
        .await
        .expect("core migrations on the test database");

    let mut pools = Vec::new();
    for id in ids {
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{id}\" CASCADE"))
            .execute(admin.as_ref())
            .await;
        let secret = schema::bootstrap_role(admin.as_ref(), id, None, false)
            .await
            .expect("bootstrap plugin role (needs CREATEROLE/superuser)");
        let pool = host::plugin_pool(&base_url(), id, &secret, 2)
            .await
            .expect("plugin pool");
        pools.push(pool);
    }
    (admin, pools)
}

/// 1. A plugin migration that writes `core.*` must fail (E1). The DDL runs as
/// the plugin role on its own pool, so it has no rights in `core`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_migration_cannot_create_a_core_table() {
    let (admin, pools) = setup(&["iso_mig"]).await;
    let pool = pools[0].as_ref();

    let err = db::run_plugin_migration(
        pool,
        "iso_mig",
        1,
        "evil",
        "CREATE TABLE core.pwned_mig (id INT);",
    )
    .await
    .expect_err("a plugin migration must not create a core table");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );

    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('core.pwned_mig')::TEXT")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(exists.is_none(), "core.pwned_mig must not exist");
}

/// 2. The E2 escape: a `DO` block that `SET LOCAL ROLE`s the deployment role and
/// then writes `core.*`. The session user is the plugin role, which is not a
/// member of the deployment role, so `SET ROLE` is refused and nothing lands.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_do_block_cannot_set_role_and_write_core() {
    let (admin, pools) = setup(&["iso_do"]).await;
    let deployment: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(admin.as_ref())
        .await
        .expect("current_user");

    let evil = format!(
        "DO $$ BEGIN EXECUTE 'SET LOCAL ROLE \"{deployment}\"'; \
         EXECUTE 'CREATE TABLE core.pwned_do (id INT)'; END $$;"
    );
    let err = host::CoreDb::new(pools[0].clone())
        .execute(evil, vec![])
        .await
        .expect_err("the DO-block escape must be refused");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );

    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('core.pwned_do')::TEXT")
        .fetch_one(admin.as_ref())
        .await
        .expect("regclass");
    assert!(exists.is_none(), "core.pwned_do must not exist");
}

/// 3. Plugin SQL cannot `CREATE ROLE … SUPERUSER` (E3).
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_plugin_sql_cannot_create_a_superuser_role() {
    let (_admin, pools) = setup(&["iso_createrole"]).await;
    let err = host::CoreDb::new(pools[0].clone())
        .execute("CREATE ROLE iso_evil SUPERUSER LOGIN".to_string(), vec![])
        .await
        .expect_err("plugin SQL must not create a role");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 4. `RESET ROLE` is inert (the session user is already the plugin role) and
/// `SET ROLE <other>` is refused.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_reset_role_is_inert_and_set_role_is_refused() {
    let (admin, pools) = setup(&["iso_role"]).await;
    let plugin = host::CoreDb::new(pools[0].clone());
    let expected = schema::role_for("iso_role");

    plugin
        .execute("RESET ROLE".to_string(), vec![])
        .await
        .expect("RESET ROLE is allowed but must be inert");
    let rows = plugin
        .query(
            "SELECT session_user AS u, current_user AS c".to_string(),
            vec![],
        )
        .await
        .expect("session_user query");
    assert_eq!(rows[0]["u"], serde_json::json!(expected));
    assert_eq!(rows[0]["c"], serde_json::json!(expected));

    let deployment: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(admin.as_ref())
        .await
        .expect("current_user");
    let err = plugin
        .execute(format!("SET ROLE \"{deployment}\""), vec![])
        .await
        .expect_err("SET ROLE to another role must be refused");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 5. Cross-schema read of another plugin's table is denied by PostgreSQL.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_cross_schema_read_is_denied() {
    let (_admin, pools) = setup(&["iso_a", "iso_b"]).await;
    let alpha = host::CoreDb::new(pools[0].clone());
    let beta = host::CoreDb::new(pools[1].clone());

    alpha
        .execute("CREATE TABLE secret (id INT)".to_string(), vec![])
        .await
        .expect("alpha creates a table in its own schema");
    let err = beta
        .query("SELECT id FROM iso_a.secret".to_string(), vec![])
        .await
        .expect_err("beta must not read alpha's table");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 6. A `core.*` table outside the plugin's allowlist is denied.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_unlisted_core_table_is_denied() {
    let (_admin, pools) = setup(&["iso_unlisted"]).await;
    let err = host::CoreDb::new(pools[0].clone())
        .query("SELECT count(*) FROM core.users".to_string(), vec![])
        .await
        .expect_err("core.users is not in the plugin's allowlist");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// 7. Every plugin connection is the plugin role — a host regression (e.g. a
/// plugin pool built from the wrong URL) is caught even if no plugin tries to
/// escape.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_every_plugin_connection_is_the_plugin_role() {
    let (_admin, pools) = setup(&["iso_conn"]).await;
    let plugin = host::CoreDb::new(pools[0].clone());
    let expected = serde_json::json!(schema::role_for("iso_conn"));

    // Two concurrent queries force at least two pooled connections.
    let (r1, r2) = tokio::join!(
        plugin.query("SELECT session_user AS u".to_string(), vec![]),
        plugin.query("SELECT session_user AS u".to_string(), vec![]),
    );
    assert_eq!(r1.expect("query 1")[0]["u"], expected);
    assert_eq!(r2.expect("query 2")[0]["u"], expected);
}

/// #19: `membership`'s role must have no access to `core.user_roles` at all —
/// it had `INSERT`, which made `membership:manage` a path to granting `chief`.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_membership_role_cannot_write_user_roles() {
    let (_admin, pools) = setup(&["membership"]).await;
    let membership = host::CoreDb::new(pools[0].clone());

    let err = membership
        .execute(
            "INSERT INTO core.user_roles (user_id, role_id) \
             VALUES ('00000000-0000-0000-0000-000000000000', 'chief')"
                .to_string(),
            vec![],
        )
        .await
        .expect_err("membership must not write core.user_roles");
    assert!(
        err.to_string().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
}

/// #148: `membership` maps a session to a roster entry; the table-level SELECT
/// it used to hold also exposed every argon2 password hash. Column-level grants
/// make the hash unreachable while the roster columns keep working.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_membership_reads_roster_columns_but_not_password_hash() {
    let (admin, pools) = setup(&["membership"]).await;
    let role = schema::role_for("membership");
    let membership = host::CoreDb::new(pools[0].clone());

    // The grant is on columns, not the whole table.
    let table_select: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1::name, 'core.users', 'SELECT')")
            .bind(&role)
            .fetch_one(admin.as_ref())
            .await
            .expect("has_table_privilege");
    assert!(
        !table_select,
        "membership must not hold table-level SELECT on core.users (#148)"
    );
    let hash_readable: bool = sqlx::query_scalar(
        "SELECT has_column_privilege($1::name, 'core.users', 'password_hash', 'SELECT')",
    )
    .bind(&role)
    .fetch_one(admin.as_ref())
    .await
    .expect("has_column_privilege");
    assert!(
        !hash_readable,
        "membership must not be able to read core.users.password_hash (#148)"
    );

    // The roster mapping still works: this is the query shape membership needs.
    membership
        .query(
            "SELECT username, display_name FROM core.users".to_string(),
            vec![],
        )
        .await
        .expect("membership reads the roster identity columns");

    // The negative probe. Assert *why* it fails: a privilege denial on the
    // users table, after the catalog probes above showed the relation exists
    // and only the column privilege is missing.
    let err = membership
        .query("SELECT password_hash FROM core.users".to_string(), vec![])
        .await
        .expect_err("membership must not read password hashes")
        .to_string();
    assert!(
        err.contains("permission denied") && err.contains("users"),
        "expected a column-privilege error on core.users, got: {err}"
    );

    // `SELECT *` is the shape a table-level grant used to permit. Under column
    // grants it must fail even though the roster columns are readable: the role
    // is not allowed to project the columns it was never granted.
    let err = membership
        .query("SELECT * FROM core.users".to_string(), vec![])
        .await
        .expect_err("membership must not be able to SELECT * from core.users")
        .to_string();
    assert!(
        err.contains("permission denied") && err.contains("users"),
        "expected a column-privilege error for SELECT * on core.users, got: {err}"
    );
}

/// #148 upgrade path: an install provisioned by the old code holds table-level
/// `SELECT` on `core.users`, and `GRANT SELECT (cols)` does not remove it. This
/// reproduces the old grant, shows the hash is exposed under it, re-runs the
/// fixed bootstrap on the same database, and shows the exposure is closed.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_rebootstrap_revokes_the_legacy_table_grant() {
    let (admin, pools) = setup(&["membership"]).await;
    let role = schema::role_for("membership");
    let legacy = host::CoreDb::new(pools[0].clone());

    // Reproduce what the pre-#148 bootstrap left behind.
    sqlx::query(&format!(
        "GRANT SELECT, INSERT, UPDATE ON core.users TO \"{role}\""
    ))
    .execute(admin.as_ref())
    .await
    .expect("install the legacy table-level grant");
    let exposed: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1::name, 'core.users', 'SELECT')")
            .bind(&role)
            .fetch_one(admin.as_ref())
            .await
            .expect("has_table_privilege");
    assert!(
        exposed,
        "the legacy grant must be reproduced for this test to prove anything"
    );
    println!("[#148] legacy grant: has_table_privilege(SELECT)={exposed}");
    legacy
        .query("SELECT password_hash FROM core.users".to_string(), vec![])
        .await
        .expect("the legacy table grant exposes password_hash — this is the defect");

    // Re-run the fixed bootstrap against the same database (fresh pool: the
    // run rotates the generated password).
    let secret = schema::bootstrap_role(admin.as_ref(), "membership", None, false)
        .await
        .expect("re-bootstrap against the legacy database");
    let membership = host::CoreDb::new(
        host::plugin_pool(&base_url(), "membership", &secret, 2)
            .await
            .expect("plugin pool after re-bootstrap"),
    );

    let table_select: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1::name, 'core.users', 'SELECT')")
            .bind(&role)
            .fetch_one(admin.as_ref())
            .await
            .expect("has_table_privilege");
    assert!(
        !table_select,
        "bootstrap must revoke the legacy table-level grant, not merely add columns"
    );
    let hash_readable: bool = sqlx::query_scalar(
        "SELECT has_column_privilege($1::name, 'core.users', 'password_hash', 'SELECT')",
    )
    .bind(&role)
    .fetch_one(admin.as_ref())
    .await
    .expect("has_column_privilege");
    assert!(
        !hash_readable,
        "the hash column must be unreadable after the upgrade re-run"
    );
    let username_readable: bool = sqlx::query_scalar(
        "SELECT has_column_privilege($1::name, 'core.users', 'username', 'SELECT')",
    )
    .bind(&role)
    .fetch_one(admin.as_ref())
    .await
    .expect("has_column_privilege");
    assert!(
        username_readable,
        "the allowlisted columns must be re-granted after the revoke"
    );
    println!(
        "[#148] after re-bootstrap: has_table_privilege(SELECT)={table_select}, \
         has_column_privilege(password_hash, SELECT)={hash_readable}, \
         has_column_privilege(username, SELECT)={username_readable}"
    );

    let err = membership
        .query("SELECT password_hash FROM core.users".to_string(), vec![])
        .await
        .expect_err("the fixed bootstrap must close the hash exposure")
        .to_string();
    assert!(
        err.contains("permission denied") && err.contains("users"),
        "expected a column-privilege error on core.users, got: {err}"
    );
    membership
        .query(
            "SELECT username, display_name FROM core.users".to_string(),
            vec![],
        )
        .await
        .expect("roster columns still readable after the upgrade");
}

/// #148 + #149: auth needs the hash to verify a login and INSERT to create
/// users. The password-change route needs UPDATE, but only on `password_hash`;
/// a table-level UPDATE would also permit rewriting identity and status columns.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_auth_hash_update_is_column_narrow() {
    let (admin, pools) = setup(&["auth"]).await;
    let role = schema::role_for("auth");
    let auth = host::CoreDb::new(pools[0].clone());
    let uid = "9f000000-0000-4000-8000-000000000149";

    let table_update: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1::name, 'core.users', 'UPDATE')")
            .bind(&role)
            .fetch_one(admin.as_ref())
            .await
            .expect("has_table_privilege");
    assert!(
        !table_update,
        "auth must not hold a table-level UPDATE on core.users (#148)"
    );
    for (column, expected, why) in [
        (
            "password_hash",
            true,
            "the password-change route writes the hash",
        ),
        (
            "display_name",
            false,
            "no auth route writes the display name",
        ),
        (
            "is_active",
            false,
            "no auth route activates or deactivates a user",
        ),
    ] {
        let granted: bool = sqlx::query_scalar(
            "SELECT has_column_privilege($1::name, 'core.users', $2::text, 'UPDATE')",
        )
        .bind(&role)
        .bind(column)
        .fetch_one(admin.as_ref())
        .await
        .expect("has_column_privilege");
        assert_eq!(granted, expected, "UPDATE on users.{column}: {why}");
    }
    let hash_select: bool = sqlx::query_scalar(
        "SELECT has_column_privilege($1::name, 'core.users', 'password_hash', 'SELECT')",
    )
    .bind(&role)
    .fetch_one(admin.as_ref())
    .await
    .expect("has_column_privilege");
    assert!(
        hash_select,
        "login verifies the hash, so auth keeps column SELECT"
    );
    let users_table_select: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1::name, 'core.users', 'SELECT')")
            .bind(&role)
            .fetch_one(admin.as_ref())
            .await
            .expect("has_table_privilege");
    assert!(
        !users_table_select,
        "auth reads named columns, never the whole of core.users (#148)"
    );
    // Auth reads the hash and the login columns but not `SELECT *`: it holds no
    // grant on `avatar_url`, `created_at` or `updated_at`, so the wildcard must
    // be refused rather than silently widening what the role may project.
    let err = auth
        .query("SELECT * FROM core.users".to_string(), vec![])
        .await
        .expect_err("auth must not be able to SELECT * from core.users")
        .to_string();
    assert!(
        err.contains("permission denied") && err.contains("users"),
        "expected a column-privilege error for SELECT * on core.users, got: {err}"
    );
    let username_insert: bool = sqlx::query_scalar(
        "SELECT has_column_privilege($1::name, 'core.users', 'username', 'INSERT')",
    )
    .bind(&role)
    .fetch_one(admin.as_ref())
    .await
    .expect("has_column_privilege");
    assert!(
        username_insert,
        "user creation inserts the columns named in the allowlist"
    );

    // The positive probe: the statement the route actually runs works.
    sqlx::query(
        "INSERT INTO core.users (id, display_name, username) VALUES ($1::uuid, 'Probe', 'auth_probe_149') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await
    .expect("fixture user");
    auth.execute(
        "UPDATE core.users SET password_hash = $1 WHERE id = $2::uuid".to_string(),
        vec![
            SqlValue::Text("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$cmVwbGFjZWQ".into()),
            SqlValue::Uuid(uid.into()),
        ],
    )
    .await
    .expect("auth's role can update the hash column (AUTH-3)");

    // The negative probe: the same table, a column auth was not granted. Assert
    // *why* it fails — a column privilege denial, not a missing row or table.
    let err = auth
        .execute(
            "UPDATE core.users SET display_name = 'Pwned' WHERE id = $1::uuid".to_string(),
            vec![SqlValue::Uuid(uid.into())],
        )
        .await
        .expect_err("a table-level UPDATE must remain impossible")
        .to_string();
    assert!(
        err.contains("permission denied") && err.contains("display_name"),
        "expected a column-privilege error on users.display_name, got: {err}"
    );

    // Sessions are revoked by writing `revoked_at`; auth's grant is whole-table
    // because it already held whole-table SELECT/INSERT/DELETE there.
    let sid = "9f000000-0000-4000-8000-00000000014a";
    sqlx::query(
        "INSERT INTO core.sessions (id, user_id, token_hash, expires_at) \
         VALUES ($1::uuid, $2::uuid, 'probe-hash', now() + interval '1 hour') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(sid)
    .bind(uid)
    .execute(admin.as_ref())
    .await
    .expect("fixture session");
    auth.execute(
        "UPDATE core.sessions SET revoked_at = now() WHERE id = $1::uuid".to_string(),
        vec![SqlValue::Uuid(sid.into())],
    )
    .await
    .expect("auth's role can revoke a session");

    sqlx::query("DELETE FROM core.users WHERE id = $1::uuid")
        .bind(uid)
        .execute(admin.as_ref())
        .await
        .expect("cleanup");
}

/// #33: `scope_id` is opaque TEXT owned by the plugin, `NULL` means troop-wide,
/// and the invalid combinations are unstorable. A lodge-scoped grant with a
/// bigint-looking id round-trips through storage and is honoured by
/// `has_in_scope` — the case the UUID column was blocking.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn probe_scope_id_is_opaque_text_and_round_trips() {
    let (admin, _pools) = setup(&[]).await;
    let uid = "11111111-1111-1111-1111-111111111111";

    sqlx::query("DELETE FROM core.user_roles")
        .execute(admin.as_ref())
        .await
        .expect("clear");
    sqlx::query(
        "INSERT INTO core.users (id, display_name) VALUES ($1::uuid, 'Bea') ON CONFLICT DO NOTHING",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await
    .expect("user");
    // The role must hold a permission for `has_in_scope` to allow.
    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) VALUES ('scout', 'core:admin') \
         ON CONFLICT DO NOTHING",
    )
    .execute(admin.as_ref())
    .await
    .expect("grant permission to scout");

    // A bigint lodge id is storable as text.
    sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'scout', 'lodge', '1')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await
    .expect("lodge-scoped row stores a text id");

    let (role_id, scope_type, scope_id): (String, String, Option<String>) = sqlx::query_as(
        "SELECT role_id, scope_type, scope_id FROM core.user_roles WHERE user_id = $1::uuid",
    )
    .bind(uid)
    .fetch_one(admin.as_ref())
    .await
    .expect("read the row back");
    assert_eq!(role_id, "scout");
    assert_eq!(scope_type, "lodge");
    assert_eq!(
        scope_id.as_deref(),
        Some("1"),
        "the opaque id round-trips verbatim"
    );

    // Build the identity the way a session would and check coverage.
    let identity = Identity::from_grants(
        uid,
        vec![RoleGrant {
            role_id,
            scope: Scope::lodge(scope_id.expect("id")),
        }],
    );
    let perms = PermissionService::new(host::CoreDb::new(admin.clone()));
    assert!(
        perms
            .has_in_scope(Some(&identity), "core:admin", &Scope::lodge("1"))
            .await,
        "a lodge-scoped grant covers its own lodge"
    );
    assert!(
        !perms
            .has_in_scope(Some(&identity), "core:admin", &Scope::lodge("2"))
            .await,
        "and not another lodge"
    );
    assert!(
        !perms
            .has_in_scope(Some(&identity), "core:admin", &Scope::troop())
            .await,
        "a lodge grant does not cover troop"
    );

    // Invalid combinations are unstorable (#33 acceptance).
    let lodge_without_id = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type) VALUES ($1::uuid, 'chief', 'lodge')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(
        lodge_without_id.is_err(),
        "a non-troop scope needs a scope_id"
    );

    let troop_with_id = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'chief', 'troop', '7')",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(
        troop_with_id.is_err(),
        "a troop scope must have a NULL scope_id"
    );

    let troop_with_null = sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id, scope_type, scope_id) \
         VALUES ($1::uuid, 'chief', 'troop', NULL)",
    )
    .bind(uid)
    .execute(admin.as_ref())
    .await;
    assert!(troop_with_null.is_ok(), "troop-wide is scope_id NULL");
}

/// Hierarchical coverage: with a declared lodge→patrol edge, a lodge-scoped
/// grant covers the patrol inside it (and not another), and coverage stays
/// downward (a patrol grant does not cover its lodge). Edges are core-owned
/// data; no plugin is consulted at authorization time.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn probe_scope_hierarchy_lodge_covers_its_patrols() {
    let _h = HIERARCHY.lock().await;
    let (admin, _pools) = setup(&[]).await;
    sqlx::query("DELETE FROM core.scope_hierarchy")
        .execute(admin.as_ref())
        .await
        .expect("clear");
    sqlx::query(
        "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
         VALUES ('lodge', '3', 'patrol', '7')",
    )
    .execute(admin.as_ref())
    .await
    .expect("declare the edge");

    // A self-edge is unstorable.
    let self_edge = sqlx::query(
        "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
         VALUES ('patrol', '7', 'patrol', '7')",
    )
    .execute(admin.as_ref())
    .await;
    assert!(
        self_edge.is_err(),
        "a self-edge must be rejected by the CHECK"
    );

    sqlx::query(
        "INSERT INTO core.role_permissions (role_id, permission_id) \
         VALUES ('chief', 'core:admin'), ('scout', 'core:admin') ON CONFLICT DO NOTHING",
    )
    .execute(admin.as_ref())
    .await
    .expect("permissions");

    let hierarchy = ScopeHierarchy::load(admin.as_ref())
        .await
        .expect("load the hierarchy");
    let perms = PermissionService::new(host::CoreDb::new(admin.clone()));

    let lodge_grant = Identity::from_grants(
        "u",
        vec![RoleGrant {
            role_id: "chief".into(),
            scope: Scope::lodge("3"),
        }],
    );
    let expanded = hierarchy.expand(&lodge_grant);
    assert!(
        perms
            .has_in_scope(Some(&expanded), "core:admin", &Scope::patrol("7"))
            .await,
        "a lodge grant covers the patrol declared inside it"
    );
    assert!(
        !perms
            .has_in_scope(Some(&expanded), "core:admin", &Scope::patrol("8"))
            .await,
        "and not a patrol that is not declared inside it"
    );

    let patrol_grant = Identity::from_grants(
        "u",
        vec![RoleGrant {
            role_id: "scout".into(),
            scope: Scope::patrol("7"),
        }],
    );
    let patrol_expanded = hierarchy.expand(&patrol_grant);
    assert!(
        !perms
            .has_in_scope(Some(&patrol_expanded), "core:admin", &Scope::lodge("3"))
            .await,
        "covering is downward only: a patrol grant does not cover its lodge"
    );
}

/// #37: hierarchy edges are owned per scope type. A plugin granted the
/// `core.scope_hierarchy` table but owning no scope type is refused — by the
/// trigger on raw SQL and by the declare function — with the edge absent, while
/// the owning plugin still succeeds and no plugin can write the ownership map.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_scope_edge_ownership_is_enforced() {
    let _h = HIERARCHY.lock().await;
    let (admin, pools) = setup(&["hello", "membership"]).await;
    let hello = pools[0].clone();
    let membership = pools[1].clone();
    let hello_db = host::CoreDb::new(hello);

    // The core seeds the ownership map; a plugin cannot write it.
    let seeded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_owners WHERE scope_type IN ('lodge','patrol')",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("read scope_owners");
    assert_eq!(seeded, 2, "the core seeds the ownership map");

    let map_write = hello_db
        .execute(
            "INSERT INTO core.scope_owners (scope_type, plugin_id) VALUES ('rogue', 'hello')"
                .to_string(),
            vec![],
        )
        .await;
    assert!(
        map_write.is_err(),
        "a plugin has no grant on core.scope_owners"
    );

    // Simulate a future plugin granted the edge table but owning no scope type.
    sqlx::query("GRANT SELECT, INSERT, DELETE ON core.scope_hierarchy TO adjutant_plugin_hello")
        .execute(admin.as_ref())
        .await
        .expect("grant the edge table to hello");

    // Raw SQL is refused by the trigger, with a message naming the type/owner.
    let raw_err = hello_db
        .execute(
            "INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id) \
             VALUES ('lodge', '30', 'patrol', '90')"
                .to_string(),
            vec![],
        )
        .await
        .expect_err("hello does not own lodge/patrol")
        .to_string();
    assert!(
        raw_err.contains("may not declare"),
        "clear trigger error: {raw_err}"
    );
    println!("[edge-ownership] trigger refusal: {raw_err}");

    // The declare API refuses it too, with its own clear message.
    let api_err = hello_db
        .execute(
            "SELECT core.declare_scope_parent('lodge', '30', 'patrol', '90')".to_string(),
            vec![],
        )
        .await
        .expect_err("the declare API refuses a type the plugin does not own")
        .to_string();
    assert!(
        api_err.contains("may not declare"),
        "clear API error: {api_err}"
    );
    println!("[edge-ownership] declare-API refusal: {api_err}");

    // Query the table, not only the errors: the refused edge must be absent.
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_hierarchy \
         WHERE parent_type = 'lodge' AND parent_id = '30' \
           AND child_type = 'patrol' AND child_id = '90'",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("count refused edge");
    assert_eq!(present, 0, "the refused edge must not be in the table");

    // The legitimate owner still declares through the API, and the edge lands.
    host::CoreDb::new(membership)
        .execute(
            "SELECT core.declare_scope_parent('lodge', '30', 'patrol', '70')".to_string(),
            vec![],
        )
        .await
        .expect("membership owns lodge/patrol");
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scope_hierarchy \
         WHERE parent_type = 'lodge' AND parent_id = '30' \
           AND child_type = 'patrol' AND child_id = '70'",
    )
    .fetch_one(admin.as_ref())
    .await
    .expect("count legit edge");
    assert_eq!(present, 1, "the legitimate edge lands");

    let _ = sqlx::query("DELETE FROM core.scope_hierarchy WHERE parent_id = '30'")
        .execute(admin.as_ref())
        .await;
    let _ = sqlx::query("REVOKE ALL ON core.scope_hierarchy FROM adjutant_plugin_hello")
        .execute(admin.as_ref())
        .await;
}

/// #45: the core scheduler runs a declared schedule repeatedly (driven by a
/// channel and the run table, never by waiting on wall-clock minutes), the
/// handler observes the plugin's own role, a failure is recorded without
/// crashing or spinning, and stopping the plugin stops the ticks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_scheduler_runs_records_and_stops() {
    let (admin, pools) = setup(&["sched_probe"]).await;
    let plugin_pool = pools[0].clone();
    let role = schema::role_for("sched_probe");
    let scheduler = Scheduler::new();

    // A failing tick must not take the server down or spin: the handler returns
    // an error; the scheduler records it and waits for the next interval.
    let fail_sched = Schedule::new(
        "boom",
        std::time::Duration::from_millis(100),
        schedule_handler(|| async {
            Err::<(), _>(adjutant_sdk::SdkError::Internal("boom".into()))
        }),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    let db = host::CoreDb::new(plugin_pool);
    let tick = Schedule::new(
        "tick",
        std::time::Duration::from_millis(100),
        schedule_handler(move || {
            let db = db.clone();
            let tx = tx.clone();
            async move {
                let rows = db
                    .query("SELECT session_user AS u".to_string(), vec![])
                    .await?;
                let who = rows[0]["u"].as_str().unwrap_or_default().to_string();
                let _ = tx.send(who).await;
                Ok(())
            }
        }),
    );
    scheduler.start("sched_probe", vec![tick, fail_sched], admin.clone());

    // Two ticks within a few seconds: repeated execution, observed live.
    for _ in 0..2 {
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a scheduled run within 5s")
            .expect("channel open");
        assert_eq!(got, role, "the handler runs as the plugin's own role");
    }

    // Durable evidence: rows in core.scheduled_runs. The handler signals the
    // channel before the scheduler writes the row, so poll briefly.
    let mut ok_runs: i64 = 0;
    for _ in 0..50 {
        ok_runs = sqlx::query_scalar(
            "SELECT count(*) FROM core.scheduled_runs WHERE plugin_id='sched_probe' AND schedule='tick' AND ok",
        )
        .fetch_one(admin.as_ref())
        .await
        .expect("count ok runs");
        if ok_runs >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        ok_runs >= 2,
        "at least two successful runs recorded, got {ok_runs}"
    );

    // The admin view (what PluginInfo reports) has last_run and next_run.
    let infos = scheduler.infos("sched_probe");
    assert_eq!(infos.len(), 2, "both schedules listed");
    let tick_info = infos
        .iter()
        .find(|s| s.name == "tick")
        .expect("tick listed");
    assert!(tick_info.last_run.is_some(), "last_run is reported");
    assert!(tick_info.next_run.is_some(), "next_run is reported");

    // The failing schedule records failures but does not spin.
    let count_bad = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM core.scheduled_runs \
             WHERE plugin_id='sched_probe' AND schedule='boom' AND NOT ok AND error IS NOT NULL",
        )
        .fetch_one(admin.as_ref())
        .await
        .expect("count failed runs")
    };
    let mut bad_runs = 0;
    for _ in 0..50 {
        bad_runs = count_bad().await;
        if bad_runs >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(bad_runs >= 1, "a failure is recorded");
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let bad_after = count_bad().await;
    assert!(
        bad_after <= bad_runs + 8,
        "a failing schedule must not spin: {bad_runs} -> {bad_after} in ~0.45s"
    );
    let boom = scheduler
        .infos("sched_probe")
        .into_iter()
        .find(|s| s.name == "boom")
        .unwrap();
    assert!(boom.last_error.is_some(), "last_error is reported");

    // Stop: the shutdown aborts the plugin's tasks, and what it *promises* is
    // that no new run starts after the abort. It does not promise that a run
    // already in flight when `stop()` returns fails to land its row — so
    // comparing two totals across a settle window convicts a correct scheduler
    // of "kept firing" for exactly that in-flight insert (issue #83). Assert the
    // promise instead: every row recorded after the stop boundary must have
    // *started* before it.
    scheduler.stop("sched_probe");
    assert!(
        scheduler
            .infos("sched_probe")
            .iter()
            .all(|s| s.next_run.is_none()),
        "a stopped plugin's schedules report no next run"
    );
    // `JoinHandle::abort` cancels the task at its next await point rather than
    // synchronously, so give the runtime a beat to drop the tasks — and any run
    // that was mid-flight during the stop — before taking the boundary. One
    // whole 100 ms cadence is enough that nothing already running can cross it.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let stopped_at = chrono::Utc::now();
    // Watch for ~4 more cadences: a scheduler that ignored the stop would have
    // to start a run past the boundary, and the `boom`/`tick` pair fires every
    // 100 ms, so it cannot hide.
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let after_stop: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.scheduled_runs \
         WHERE plugin_id='sched_probe' AND started_at > $1",
    )
    .bind(stopped_at)
    .fetch_one(admin.as_ref())
    .await
    .expect("count runs started after stop");
    assert_eq!(
        after_stop, 0,
        "a stopped plugin's schedules must not start new runs after the stop"
    );
}

// ---------------------------------------------------------------------------
// Audit chain v2 + the keyed layer (#159, #160)
//
// These are the issue reproductions as tests. They are #[ignore]d with the rest
// of this file, so a bare workspace run reports them as ignored, never passed.
// ---------------------------------------------------------------------------

/// Empty the chain without tripping the append-only/TRUNCATE guards. The guards
/// are the mechanism under test in other probes; here we are the owner preparing
/// a fixture, which is exactly the adversary #160 describes.
async fn reset_audit_log(pool: &sqlx::PgPool) {
    sqlx::raw_sql(
        "ALTER TABLE core.audit_log DISABLE TRIGGER audit_append_only;\
         ALTER TABLE core.audit_log DISABLE TRIGGER audit_no_truncate;\
         TRUNCATE core.audit_log RESTART IDENTITY;\
         ALTER TABLE core.audit_log ENABLE TRIGGER audit_append_only;\
         ALTER TABLE core.audit_log ENABLE TRIGGER audit_no_truncate;",
    )
    .execute(pool)
    .await
    .expect("reset core.audit_log");
}

async fn unkeyed_verify(pool: &sqlx::PgPool) -> (Option<i64>, i64) {
    sqlx::query_as("SELECT first_bad, rows_checked FROM core.audit_verify()")
        .fetch_one(pool)
        .await
        .expect("core.audit_verify()")
}

/// #159's reproduction: change only the actor and the timestamp, and the v1
/// chain kept saying ok. v2 must name the row.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn changing_user_id_and_created_at_breaks_the_chain() {
    let _guard = AUDIT.lock().await;
    let (pool, _) = setup(&[]).await;
    reset_audit_log(pool.as_ref()).await;

    let alice = "aaaaaaaa-1111-1111-1111-111111111111";
    let bob = "bbbbbbbb-2222-2222-2222-222222222222";
    for (id, name) in [(alice, "Alice"), (bob, "Bob")] {
        sqlx::query(
            "INSERT INTO core.users (id, display_name) VALUES ($1::uuid, $2) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(name)
        .execute(pool.as_ref())
        .await
        .expect("fixture user");
    }

    for (action, user) in [("first", alice), ("second", bob)] {
        sqlx::query(
            "INSERT INTO core.audit_log (user_id, action, resource_type, resource_id, details, source) \
             VALUES ($1::uuid, $2, 'thing', '1', '{}', 'core')",
        )
        .bind(user)
        .bind(action)
        .execute(pool.as_ref())
        .await
        .expect("append audit row");
    }

    let (first_bad, rows) = unkeyed_verify(pool.as_ref()).await;
    assert_eq!(first_bad, None, "the untouched chain verifies");
    assert_eq!(rows, 2);

    // The CLI reports the same two layers and does not claim keyed protection
    // when configured without a key.
    let cfg = adjutant_server::config::Config {
        database_url: base_url(),
        audit_key: None,
        ..Default::default()
    };
    let (good, report) = adjutant_server::cli::audit_verify(&cfg)
        .await
        .expect("audit-verify CLI");
    assert!(good, "the unkeyed chain is good");
    assert!(
        report.contains("\"keyed\": false"),
        "no key reports keyed:false, got: {report}"
    );

    // The owner disables the append-only guard, changes only who and when, and
    // re-arms it. Nothing hashed by v1 changed.
    sqlx::query("ALTER TABLE core.audit_log DISABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("disable guard");
    sqlx::query(
        "UPDATE core.audit_log \
         SET user_id = $1::uuid, created_at = created_at - interval '30 days' \
         WHERE id = 1",
    )
    .bind(bob)
    .execute(pool.as_ref())
    .await
    .expect("rewrite actor and timestamp");
    sqlx::query("ALTER TABLE core.audit_log ENABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("re-arm guard");

    let (first_bad, rows) = unkeyed_verify(pool.as_ref()).await;
    assert_eq!(
        first_bad,
        Some(1),
        "v2 hashes user_id and created_at, so the re-attributed row is named"
    );
    assert_eq!(rows, 2);
}

const TEST_KEY_HEX: &str =
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// #160's reproduction: the owner rewrites an action and recomputes the unkeyed
/// chain. SQL says ok — and the keyed verifier names the row. Without the key,
/// there is nothing to name.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn keyed_layer_detects_an_owner_rewrite_the_unkeyed_chain_accepts() {
    use adjutant_sdk::{audit::ChainKey, AuditService};

    let _guard = AUDIT.lock().await;
    let (pool, _) = setup(&[]).await;
    reset_audit_log(pool.as_ref()).await;

    let key = ChainKey::parse(TEST_KEY_HEX).expect("fixed test key");
    assert!(
        adjutant_sdk::audit::set_chain_key(key.clone()) || adjutant_sdk::audit::chain_key().is_some(),
        "the process-global key is installed once"
    );

    let audit = AuditService::new(host::CoreDb::new(pool.clone()), "core".into());
    audit
        .log(None, "first", "thing", "1", serde_json::json!({"n": 1}))
        .await
        .expect("keyed append 1");
    audit
        .log(None, "second", "thing", "2", serde_json::json!({"n": 2}))
        .await
        .expect("keyed append 2");

    let keyed: Vec<Option<String>> =
        sqlx::query_scalar("SELECT keyed_hash FROM core.audit_log ORDER BY id")
            .fetch_all(pool.as_ref())
            .await
            .expect("read keyed hashes");
    assert_eq!(keyed.len(), 2);
    assert!(
        keyed.iter().all(Option::is_some),
        "the writer computes keyed_hash before the INSERT; the row lands with both hashes"
    );

    let report = adjutant_server::audit::verify(pool.as_ref(), Some(&key))
        .await
        .expect("keyed verify");
    assert!(report.ok, "the chain verifies");
    assert!(report.keyed);
    assert_eq!(report.keyed_rows_checked, 2);
    assert_eq!(report.keyed_rows_unkeyed, 0);
    assert_eq!(report.keyed_first_bad, None);

    // Without a key the verifier says so and checks nothing — it does not
    // report a green keyed result.
    let no_key = adjutant_server::audit::verify(pool.as_ref(), None)
        .await
        .expect("unkeyed verify");
    assert!(!no_key.keyed);
    assert_eq!(no_key.keyed_rows_checked, 0);
    assert_eq!(no_key.keyed_first_bad, None);

    // As the owning role: disable the guard, rewrite the action, recompute the
    // unkeyed chain exactly as core.audit_chain_fill would, re-arm. The owner
    // also *guesses* the keyed layer by writing the unkeyed digest into
    // `keyed_hash` — the best a writer without the key can do. The real HMAC
    // still disagrees, which is the whole point of the key.
    sqlx::query("ALTER TABLE core.audit_log DISABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("disable guard");
    sqlx::query("UPDATE core.audit_log SET action = 'owner-rewrote' WHERE id = 1")
        .execute(pool.as_ref())
        .await
        .expect("rewrite action");
    sqlx::raw_sql(
        r#"
        DO $fn$
        DECLARE r RECORD; prev TEXT := repeat('0', 64); h TEXT;
        BEGIN
          FOR r IN SELECT id, user_id, action, resource_type, resource_id, details, source, created_at
                   FROM core.audit_log ORDER BY id LOOP
            h := encode(public.digest(core.audit_canonical(prev, r.id, r.action, r.resource_type,
                 r.resource_id, r.details, r.source, r.user_id, r.created_at), 'sha256'), 'hex');
            UPDATE core.audit_log
               SET prev_hash = prev, entry_hash = h, keyed_hash = h
             WHERE id = r.id;
            prev := h;
          END LOOP;
        END $fn$;
        "#,
    )
    .execute(pool.as_ref())
    .await
    .expect("recompute the unkeyed chain as the owner");
    sqlx::query("ALTER TABLE core.audit_log ENABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("re-arm guard");

    let (first_bad, rows) = unkeyed_verify(pool.as_ref()).await;
    assert_eq!(
        first_bad, None,
        "the unkeyed chain is internally consistent again — this is the point of #160"
    );
    assert_eq!(rows, 2);

    let report = adjutant_server::audit::verify(pool.as_ref(), Some(&key))
        .await
        .expect("keyed verify after rewrite");
    assert!(report.ok, "the unkeyed layer still says ok");
    assert_eq!(
        report.keyed_first_bad,
        Some(1),
        "the keyed layer names the rewritten row"
    );
    assert_eq!(report.keyed_rows_checked, 2);

    // The operator's CLI path reaches the same two layers: the unkeyed chain is
    // good, the keyed layer is not, and the command must exit non-zero.
    let cfg = adjutant_server::config::Config {
        database_url: base_url(),
        audit_key: Some(key.clone()),
        ..Default::default()
    };
    let (good, cli_report) = adjutant_server::cli::audit_verify(&cfg)
        .await
        .expect("audit-verify CLI with a key");
    assert!(!good, "the keyed rewrite must fail the CLI");
    assert!(
        cli_report.contains("\"keyed_first_bad\": 1"),
        "the CLI names the row, got: {cli_report}"
    );

    // And the row the owner erased is genuinely gone from the unkeyed view.
    let action: String = sqlx::query_scalar("SELECT action FROM core.audit_log WHERE id = 1")
        .fetch_one(pool.as_ref())
        .await
        .expect("read row 1");
    assert_eq!(action, "owner-rewrote");
}

/// Finding 4: a keyed deployment with pre-key (NULL `keyed_hash`) rows reports
/// them in one plain-text line but still succeeds — the rows are not proof of a
/// bypass, and they must not be counted as keyed protection either.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn keyed_deployment_names_unkeyed_rows_without_failing() {
    use adjutant_sdk::{audit::ChainKey, AuditService};

    let _guard = AUDIT.lock().await;
    let (pool, _) = setup(&[]).await;
    reset_audit_log(pool.as_ref()).await;

    let key = ChainKey::parse(TEST_KEY_HEX).expect("fixed test key");
    assert!(
        adjutant_sdk::audit::set_chain_key(key.clone())
            || adjutant_sdk::audit::chain_key().is_some(),
        "the process-global key is installed once"
    );

    let audit = AuditService::new(host::CoreDb::new(pool.clone()), "core".into());
    audit
        .log(None, "keyed", "thing", "1", serde_json::json!({}))
        .await
        .expect("keyed append");

    // A writer outside the keyed path: the trigger fills the unkeyed chain, but
    // keyed_hash stays NULL, exactly like history written before the key.
    sqlx::query(
        "INSERT INTO core.audit_log (action, resource_type, resource_id, details, source) \
         VALUES ('pre-key', 'thing', '2', '{}', 'core')",
    )
    .execute(pool.as_ref())
    .await
    .expect("unkeyed append");

    let report = adjutant_server::audit::verify(pool.as_ref(), Some(&key))
        .await
        .expect("keyed verify");
    assert!(report.ok, "the unkeyed chain verifies");
    assert!(report.keyed, "the keyed layer was checked");
    assert_eq!(report.keyed_rows_checked, 1);
    assert_eq!(report.keyed_rows_unkeyed, 1);
    assert_eq!(report.keyed_first_bad, None);

    let cfg = adjutant_server::config::Config {
        database_url: base_url(),
        audit_key: Some(key),
        ..Default::default()
    };
    let (good, cli_report) = adjutant_server::cli::audit_verify(&cfg)
        .await
        .expect("audit-verify CLI with a key");
    assert!(good, "unkeyed rows are a warning, not a failure");
    assert!(
        cli_report.contains("1 audit row(s) have no keyed_hash"),
        "the count is stated, got: {cli_report}"
    );
    assert!(
        cli_report.contains("outside the keyed chain"),
        "the bypass possibility is stated, got: {cli_report}"
    );
}

/// The fixed vector, computed by PostgreSQL itself. The Rust half lives in
/// `plugins/sdk` (`audit_vector_is_pinned_for_sql_and_rust`); both must agree or
/// the trigger and the app verifier disagree about every row.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn sql_and_rust_agree_on_the_pinned_audit_vector() {
    use adjutant_sdk::audit::ChainKey;

    let (pool, _) = setup(&[]).await;

    // Finding 1: the wrapper calls to_char, whose timestamp forms are STABLE.
    // An IMMUTABLE marker would be a false promise to the planner.
    let volatility: String = sqlx::query_scalar(
        "SELECT provolatile::text FROM pg_proc \
         WHERE proname = 'audit_canonical' AND pronamespace = 'core'::regnamespace",
    )
    .fetch_one(pool.as_ref())
    .await
    .expect("core.audit_canonical exists");
    assert_eq!(
        volatility, "s",
        "core.audit_canonical must be STABLE, not IMMUTABLE"
    );
    let payload: String = sqlx::query_scalar(
        "SELECT core.audit_canonical( \
             $1::text, 42::bigint, $2::text, $3::text, $4::text, $5::jsonb, $6::text, \
             $7::uuid, $8::timestamptz)",
    )
    .bind(adjutant_sdk::AUDIT_ZERO_HASH)
    .bind("member.update")
    .bind("member")
    .bind("11111111-1111-1111-1111-111111111111")
    .bind("{\"b\":\"x\",\"a\":1}")
    .bind("core")
    .bind("22222222-2222-2222-2222-222222222222")
    .bind("2026-01-02T03:04:05.123456Z")
    .fetch_one(pool.as_ref())
    .await
    .expect("SQL canonical payload");

    let (sha, hmac): (String, String) = sqlx::query_as(
        "SELECT encode(public.digest($1, 'sha256'), 'hex'), \
                encode(public.hmac(convert_to($1, 'UTF8'), decode($2, 'hex'), 'sha256'), 'hex')",
    )
    .bind(&payload)
    .bind(TEST_KEY_HEX)
    .fetch_one(pool.as_ref())
    .await
    .expect("SQL digests");

    let rust_payload = adjutant_sdk::canonical_payload(
        adjutant_sdk::AUDIT_ZERO_HASH,
        42,
        "member.update",
        "member",
        "11111111-1111-1111-1111-111111111111",
        "{\"a\": 1, \"b\": \"x\"}",
        "core",
        Some("22222222-2222-2222-2222-222222222222"),
        "2026-01-02T03:04:05.123456+00:00",
    );
    assert_eq!(
        payload, rust_payload,
        "SQL and Rust must pin the same canonical form"
    );
    assert_eq!(
        sha,
        adjutant_sdk::unkeyed_digest(&rust_payload),
        "the unkeyed entry_hash agrees"
    );
    let key = ChainKey::parse(TEST_KEY_HEX).expect("fixed key");
    assert_eq!(
        hmac,
        key.keyed_digest(&rust_payload),
        "the keyed HMAC agrees"
    );
}

/// The one path a fresh install never takes: upgrading a database whose rows
/// already carry v1 hashes. Re-running migration 17's SQL must repair them
/// (the DO block) and leave the append-only guard armed.
#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
async fn migration_17_recomputes_a_v1_chain_on_upgrade() {
    let _guard = AUDIT.lock().await;
    let (pool, _) = setup(&[]).await;
    reset_audit_log(pool.as_ref()).await;

    for action in ["first", "second"] {
        sqlx::query(
            "INSERT INTO core.audit_log (action, resource_type, resource_id, details, source) \
             VALUES ($1, 'thing', '1', '{}', 'core')",
        )
        .bind(action)
        .execute(pool.as_ref())
        .await
        .expect("append phase-2 row");
    }

    // Rewrite the hashes with the v1 formula, as a database that applied
    // migration 2 and not 17 would have them.
    sqlx::query("ALTER TABLE core.audit_log DISABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("disable guard");
    sqlx::raw_sql(
        r#"
        DO $fn$
        DECLARE r RECORD; prev TEXT := repeat('0', 64); h TEXT;
        BEGIN
          FOR r IN SELECT id, action, resource_type, resource_id, details, source
                   FROM core.audit_log ORDER BY id LOOP
            h := encode(public.digest(prev || '|' || r.id::text || '|' || r.action || '|' ||
                 r.resource_type || '|' || r.resource_id || '|' || r.details::text || '|' ||
                 r.source, 'sha256'), 'hex');
            UPDATE core.audit_log SET prev_hash = prev, entry_hash = h WHERE id = r.id;
            prev := h;
          END LOOP;
        END $fn$;
        "#,
    )
    .execute(pool.as_ref())
    .await
    .expect("write v1 hashes");
    sqlx::query("ALTER TABLE core.audit_log ENABLE TRIGGER audit_append_only")
        .execute(pool.as_ref())
        .await
        .expect("re-arm guard");

    let (first_bad, _) = unkeyed_verify(pool.as_ref()).await;
    assert_eq!(
        first_bad,
        Some(1),
        "v1 hashes do not satisfy the v2 verifier"
    );

    let sql = db::CORE_MIGRATIONS
        .iter()
        .find(|(version, _, _)| *version == 17)
        .map(|(_, _, sql)| *sql)
        .expect("migration 17 is present");
    sqlx::raw_sql(&format!("BEGIN; {sql}; COMMIT;"))
        .execute(pool.as_ref())
        .await
        .expect("run migration 17 over existing rows");

    let (first_bad, rows) = unkeyed_verify(pool.as_ref()).await;
    assert_eq!(
        first_bad, None,
        "the upgrade recomputed the chain instead of leaving every row bad"
    );
    assert_eq!(rows, 2);

    let armed: bool = sqlx::query_scalar(
        "SELECT tgenabled <> 'D' FROM pg_trigger WHERE tgname = 'audit_append_only'",
    )
    .fetch_one(pool.as_ref())
    .await
    .expect("trigger state");
    assert!(armed, "the append-only guard is re-armed after the recompute");

    // Prove it: an UPDATE is refused again.
    let err = sqlx::query("UPDATE core.audit_log SET action = 'nope' WHERE id = 1")
        .execute(pool.as_ref())
        .await
        .expect_err("the append-only guard is back");
    assert!(
        err.to_string().contains("append-only"),
        "expected the append-only error, got: {err}"
    );
}
