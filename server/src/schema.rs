//! Per-plugin PostgreSQL role isolation (SPEC §5.2; design:
//! `docs/design/plugin-isolation.md`).
//!
//! The boundary is the **identity of the connection**, not a statement filter.
//! Each plugin gets a `LOGIN` role `adjutant_plugin_<id>` that **owns its
//! schema**, and the host runs that plugin's SQL on a pool authenticated as that
//! role (see [`crate::host::plugin_pool`]). A plugin therefore cannot become
//! anything else: `SET ROLE`/`RESET ROLE` can only return it to its own identity
//! and `DO`-block tricks are inert. The old mechanism — `SET LOCAL ROLE` on a
//! connection whose session user was the deployment role — was a state change on
//! a privileged session, not a restricted principal, and was defeated by a
//! single `DO` block (design §1, E2).
//!
//! Roles, schema ownership and the `core.*` allowlist are established by
//! [`bootstrap_role`], which the operator runs through `adjutant
//! bootstrap-isolation` against an admin URL. The **runtime never needs
//! `CREATEROLE`**: it reads the stored credential and connects.
//!
//! **Migrations run on the plugin's own pool as the plugin role**, so a guest
//! migration can create/alter objects in its own schema and nowhere else (E1).

use sqlx::PgPool;

/// The PostgreSQL role backing a plugin's connection.
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

/// Create/refresh a plugin's `LOGIN` role, its schema ownership, and its
/// `core.*` allowlist grants. Returns the role's password (a new random one when
/// `rotate` is set or none was supplied, otherwise `existing_secret`).
///
/// Idempotent: safe to re-run. Passwords are **preserved by default** so a
/// re-run does not invalidate a running deployment; `rotate` changes only the
/// secret (the role, schema and grants are re-asserted either way).
pub async fn bootstrap_role(
    pool: &PgPool,
    plugin_id: &str,
    existing_secret: Option<&str>,
    rotate: bool,
) -> Result<String, sqlx::Error> {
    let role = role_for(plugin_id);
    let secret = match existing_secret {
        Some(existing) if !rotate => existing.to_string(),
        _ => generate_secret(pool).await?,
    };

    // Create the role if absent (42710 = duplicate_object).
    match sqlx::query(&format!(
        "CREATE ROLE \"{role}\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE"
    ))
    .execute(pool)
    .await
    {
        Ok(_) => {}
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42710") => {}
        Err(e) => return Err(e),
    }

    // Re-assert the attributes and password on every run. The secret is hex, so
    // it is safe to interpolate into the statement.
    sqlx::query(&format!(
        "ALTER ROLE \"{role}\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD '{secret}'"
    ))
    .execute(pool)
    .await?;

    // The plugin's schema is owned by its role; bare names resolve there via the
    // role's default search_path, and `public` stays reachable for pgcrypto.
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{plugin_id}\" AUTHORIZATION \"{role}\""
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!("ALTER SCHEMA \"{plugin_id}\" OWNER TO \"{role}\""))
        .execute(pool)
        .await?;

    sqlx::query(&format!(
        "ALTER ROLE \"{role}\" SET search_path TO \"{plugin_id}\", public"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO \"{role}\""))
        .execute(pool)
        .await?;
    // USAGE on core is needed to call `core.record_migration` during plugin
    // migrations. It does not grant table access — the allowlist below does.
    sqlx::query(&format!("GRANT USAGE ON SCHEMA core TO \"{role}\""))
        .execute(pool)
        .await?;

    // Explicit cross-schema allowlist (unchanged contents).
    if let Some(grants) = core_grants(plugin_id) {
        for (table, privs) in grants {
            // `table`/`privs` come from the static allowlist above — never user
            // input.
            sqlx::query(&format!("GRANT {privs} ON core.{table} TO \"{role}\""))
                .execute(pool)
                .await?;
        }
    }

    // Upgrade path: an install bootstrapped by an older version has its schema
    // (and objects) owned by the deployment role. Transfer ownership of
    // everything *inside the plugin's own schema*. Deliberately scoped — a bare
    // `REASSIGN OWNED` would move core objects too.
    transfer_schema_ownership(pool, plugin_id, &role).await?;

    tracing::info!(plugin = plugin_id, role = %role, "plugin role bootstrapped");
    Ok(secret)
}

/// A fresh 256-bit hex password from `pgcrypto` (already installed; no Rust RNG
/// dependency). Hex keeps the value safe to interpolate into `ALTER ROLE`.
async fn generate_secret(pool: &PgPool) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT encode(gen_random_bytes(32), 'hex')")
        .fetch_one(pool)
        .await
}

/// `ALTER … OWNER TO` for every table/sequence/view inside one schema. Scoped to
/// `schema` on purpose; idempotent (re-owning by the same role is a no-op).
async fn transfer_schema_ownership(
    pool: &PgPool,
    schema: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "DO $own$
         DECLARE r RECORD;
         BEGIN
           FOR r IN SELECT tablename FROM pg_tables WHERE schemaname = '{schema}' LOOP
             EXECUTE format('ALTER TABLE %I.%I OWNER TO %I', '{schema}', r.tablename, '{role}');
           END LOOP;
           FOR r IN SELECT sequencename FROM pg_sequences WHERE schemaname = '{schema}' LOOP
             EXECUTE format('ALTER SEQUENCE %I.%I OWNER TO %I', '{schema}', r.sequencename, '{role}');
           END LOOP;
           FOR r IN SELECT viewname FROM pg_views WHERE schemaname = '{schema}' LOOP
             EXECUTE format('ALTER VIEW %I.%I OWNER TO %I', '{schema}', r.viewname, '{role}');
           END LOOP;
           FOR r IN SELECT matviewname FROM pg_matviews WHERE schemaname = '{schema}' LOOP
             EXECUTE format('ALTER MATERIALIZED VIEW %I.%I OWNER TO %I', '{schema}', r.matviewname, '{role}');
           END LOOP;
         END $own$;"
    );
    sqlx::query(&sql).execute(pool).await?;
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
