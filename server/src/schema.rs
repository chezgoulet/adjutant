//! Per-plugin PostgreSQL role isolation (SPEC §5.2: each plugin gets its own
//! schema and cannot read or write other plugins' schemas).
//!
//! The core's per-call `search_path` (see `host.rs`) is a *convenience*; it is
//! not a boundary — a plugin could still write `other_plugin.secret` explicitly.
//! This module adds the boundary: each plugin gets a `NOLOGIN` role
//! `adjutant_plugin_<id>` that owns nothing outside its schema and holds only an
//! explicit allowlist of privileges on `core.*` tables. At runtime the plugin's
//! database handle runs every call inside `SET LOCAL ROLE`, so a query that
//! reaches into another schema fails with `permission denied`.
//!
//! **Migrations still run as the base role** (they are trusted code and some,
//! like auth's, deliberately `ALTER TABLE core.users`). Isolation applies to the
//! plugin's *runtime* queries, which is where plugin (and eventually
//! third-party) code actually executes.
//!
//! **Privileges required:** the connecting role must be able to `CREATE ROLE`
//! and `GRANT` (i.e. superuser or `CREATEROLE`). Without that, isolation is
//! skipped with a warning and the plugin runs as the base role — see the README.

use sqlx::PgPool;

/// The PostgreSQL role backing a plugin's isolated runtime handle.
///
/// `plugin_id` is core-validated (`[a-z][a-z0-9_]{0,30}`) before this is called,
/// so the name is safe to interpolate into `CREATE ROLE`.
pub fn role_for(plugin_id: &str) -> String {
    format!("adjutant_plugin_{plugin_id}")
}

/// Core tables a plugin may touch at runtime, and the privileges it needs.
///
/// Everything a plugin requires from another plugin's schema is a bug in the
/// plugin; everything extra it wants from `core.*` must be added here
/// deliberately. Tables without an entry are unreachable to the plugin.
pub fn core_grants(plugin_id: &str) -> Option<&'static [(&'static str, &'static str)]> {
    const AUTH: &[(&str, &str)] = &[
        ("users", "SELECT, INSERT, UPDATE"),
        ("sessions", "SELECT, INSERT, DELETE"),
        ("user_roles", "SELECT, INSERT, DELETE"),
    ];
    const MEMBERSHIP: &[(&str, &str)] = &[("users", "SELECT"), ("user_roles", "INSERT")];
    match plugin_id {
        "auth" => Some(AUTH),
        "membership" => Some(MEMBERSHIP),
        _ => None,
    }
}

/// Create/refresh the plugin's role and grants. Returns the role name on
/// success; `Err` when the database role cannot manage roles (isolation is then
/// unavailable and the caller should run the plugin without it, loudly).
pub async fn ensure_isolation(pool: &PgPool, plugin_id: &str) -> Result<String, sqlx::Error> {
    let role = role_for(plugin_id);

    // Idempotent role creation (42710 = duplicate_object).
    match sqlx::query(&format!("CREATE ROLE \"{role}\" NOLOGIN"))
        .execute(pool)
        .await
    {
        Ok(_) => {}
        Err(e)
            if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42710") => {}
        Err(e) => return Err(e),
    }

    // The base role must be able to `SET ROLE` into it.
    sqlx::query(&format!("GRANT \"{role}\" TO CURRENT_USER"))
        .execute(pool)
        .await?;

    // Rights on everything currently in the plugin's schema.
    grant_schema_objects(pool, plugin_id, &role).await?;

    // pgcrypto and friends live in `public`.
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO \"{role}\""))
        .execute(pool)
        .await?;

    // Explicit cross-schema allowlist.
    if let Some(grants) = core_grants(plugin_id) {
        sqlx::query(&format!("GRANT USAGE ON SCHEMA core TO \"{role}\""))
            .execute(pool)
            .await?;
        for (table, privs) in grants {
            // `table`/`privs` come from the static allowlist above — never user
            // input.
            sqlx::query(&format!("GRANT {privs} ON core.{table} TO \"{role}\""))
                .execute(pool)
                .await?;
        }
    }

    tracing::info!(plugin = plugin_id, role = %role, "schema isolation active");
    Ok(role)
}

/// Grant the plugin role full rights on everything in its schema, plus default
/// privileges for future objects. Call once when isolation is established and
/// again after migrations, so tables the migrations created are covered.
pub async fn grant_schema_objects(
    pool: &PgPool,
    plugin_id: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    let stmts = [
        format!("GRANT USAGE ON SCHEMA \"{plugin_id}\" TO \"{role}\""),
        format!("GRANT ALL ON ALL TABLES IN SCHEMA \"{plugin_id}\" TO \"{role}\""),
        format!("GRANT ALL ON ALL SEQUENCES IN SCHEMA \"{plugin_id}\" TO \"{role}\""),
        format!("GRANT ALL ON ALL FUNCTIONS IN SCHEMA \"{plugin_id}\" TO \"{role}\""),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA \"{plugin_id}\" GRANT ALL ON TABLES TO \"{role}\""),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA \"{plugin_id}\" GRANT ALL ON SEQUENCES TO \"{role}\""),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA \"{plugin_id}\" GRANT ALL ON FUNCTIONS TO \"{role}\""),
    ];
    for stmt in stmts {
        sqlx::query(&stmt).execute(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_name_is_derived_from_the_plugin_id() {
        assert_eq!(role_for("gear_locker"), "adjutant_plugin_gear_locker");
        assert_eq!(role_for("auth"), "adjutant_plugin_auth");
    }

    #[test]
    fn core_grants_are_plugin_specific() {
        assert!(core_grants("hello").is_none(), "unknown plugins get no core access");
        let auth = core_grants("auth").expect("auth has a core allowlist");
        assert!(auth.iter().any(|(t, p)| *t == "users" && p.contains("INSERT")));
        let membership = core_grants("membership").expect("membership has a core allowlist");
        assert!(
            membership.iter().all(|(_, p)| !p.contains("DELETE")),
            "membership is read/insert only on core tables"
        );
    }
}
