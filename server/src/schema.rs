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

/// One `core.*` privilege set a plugin role may hold at runtime.
///
/// `columns: None` is a whole-table grant. `columns: Some(&[…])` is a
/// **column-level** grant, which PostgreSQL stores separately from a table
/// grant: it permits exactly the named columns and nothing else, and a `SELECT
/// *` (or a column not named) fails with `permission denied for table …`.
///
/// The allowlist is authoritative rather than additive: [`bootstrap_role`]
/// revokes every table- and column-level privilege the role holds on `core.*`
/// before applying these entries. A table or column that is not named here is
/// therefore not merely ungranted — it is actively revoked, including on an
/// installation bootstrapped by an older release.
pub struct CoreGrant {
    pub table: &'static str,
    pub columns: Option<&'static [&'static str]>,
    pub privileges: &'static str,
}

/// Core tables a plugin may touch at runtime, and the privileges it needs.
///
/// Everything a plugin requires from another plugin's schema is a bug in the
/// plugin; everything extra it wants from `core.*` must be added here
/// deliberately. Tables and columns without an entry are unreachable.
pub fn core_grants(plugin_id: &str) -> Option<&'static [CoreGrant]> {
    // `core.users` holds the argon2 password hashes, so the two plugins that
    // need it get column-level grants rather than table-level ones. Auth reads
    // the hash to verify a login and inserts hashes for new users; membership
    // needs only the roster-identity columns and must never see the hash.
    // `avatar_url`, `created_at` and `updated_at` are read by neither plugin.
    const AUTH_USERS_SELECT: &[&str] = &[
        "id",
        "external_id",
        "username",
        "email",
        "display_name",
        "is_active",
        "password_hash",
    ];
    // Columns named by auth's INSERTs (password registration, admin create,
    // OIDC first login). `id` is a defaulted column, but `RETURNING id`
    // additionally needs SELECT on it, which `AUTH_USERS_SELECT` supplies.
    const AUTH_USERS_INSERT: &[&str] = &[
        "external_id",
        "username",
        "email",
        "display_name",
        "password_hash",
    ];
    // AUTH-3's password-change route rewrites exactly one column. It gets a
    // column-level UPDATE, never a table-level one: `bootstrap_role` revokes the
    // whole privilege set first, so an upgrade drops any older table grant (#148).
    const AUTH_USERS_UPDATE: &[&str] = &["password_hash"];
    const AUTH: &[CoreGrant] = &[
        CoreGrant {
            table: "users",
            columns: Some(AUTH_USERS_SELECT),
            privileges: "SELECT",
        },
        CoreGrant {
            table: "users",
            columns: Some(AUTH_USERS_INSERT),
            privileges: "INSERT",
        },
        CoreGrant {
            table: "users",
            columns: Some(AUTH_USERS_UPDATE),
            privileges: "UPDATE",
        },
        // Revoking a session writes `revoked_at`. The grant stays whole-table
        // because auth already holds whole-table SELECT/INSERT/DELETE on this
        // table (it creates and deletes sessions), so a column-level UPDATE
        // would narrow nothing that is not already exposed — and it would be
        // the only mixed-layer grant here. The narrower `users` grant is where
        // the meaningful boundary is.
        CoreGrant {
            table: "sessions",
            columns: None,
            privileges: "SELECT, INSERT, UPDATE, DELETE",
        },
        CoreGrant {
            table: "user_roles",
            columns: None,
            privileges: "SELECT, INSERT, DELETE",
        },
        // The one bootstrap slot: `POST /api/auth/register` claims it before it
        // creates the first user, so exactly one anonymous caller can win the
        // race (#173). DELETE is the give-back on a failed registration.
        CoreGrant {
            table: "bootstrap_claim",
            columns: None,
            privileges: "SELECT, INSERT, DELETE",
        },
    ];
    // Membership reads users (to map a session to a roster entry) but must not
    // write `core.user_roles`: role assignment belongs to auth (#19). It had
    // INSERT here, which made `membership:manage` a path to granting `chief`.
    //
    // `core.scope_hierarchy` is the declared lodge->patrol hierarchy; membership
    // owns the roster it is derived from, so it is the one plugin allowed to
    // write it (see docs/design/scoped-permissions.md 3.2). The grant is
    // table-level, so it can write any row: the residual is called out in the PR
    // and the loader rejects cycles/self-edges/unknown types, but per-row
    // ownership is not expressible with a table grant alone.
    const MEMBERSHIP_USERS_SELECT: &[&str] =
        &["id", "username", "email", "display_name", "is_active"];
    const MEMBERSHIP: &[CoreGrant] = &[
        CoreGrant {
            table: "users",
            columns: Some(MEMBERSHIP_USERS_SELECT),
            privileges: "SELECT",
        },
        CoreGrant {
            table: "scope_hierarchy",
            columns: None,
            privileges: "SELECT, INSERT, DELETE",
        },
    ];
    match plugin_id {
        "auth" => Some(AUTH),
        "membership" => Some(MEMBERSHIP),
        _ => None,
    }
}

/// Which plugin owns each scope type — the authority to declare hierarchy edges
/// for it (issue #37). Seeded into `core.scope_owners` at boot from this
/// constant; the operator may extend it for a future plugin.
///
/// **The core writes this map; plugins must never hold a grant on it.** Whoever
/// can write it can legitimise any edge and so widen any plugin's coverage, so
/// it is the trust anchor for the `core.scope_hierarchy` edge table.
pub const SCOPE_OWNERS: &[(&str, &str)] = &[("lodge", "membership"), ("patrol", "membership")];

/// Seed `core.scope_owners` from [`SCOPE_OWNERS`]. `ON CONFLICT DO NOTHING` so an
/// operator extension (or an explicit reassignment) is preserved across boots.
pub async fn seed_scope_owners(pool: &PgPool) -> Result<(), sqlx::Error> {
    for (scope_type, plugin_id) in SCOPE_OWNERS {
        sqlx::query(
            "INSERT INTO core.scope_owners (scope_type, plugin_id) VALUES ($1, $2) \
             ON CONFLICT (scope_type) DO NOTHING",
        )
        .bind(scope_type)
        .bind(plugin_id)
        .execute(pool)
        .await?;
    }
    Ok(())
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

    // The allowlist is authoritative, not additive. Start from an empty
    // privilege set on `core.*` and re-apply [`core_grants`]: an installation
    // bootstrapped by the old table-level code still holds `SELECT` on all of
    // `core.users`, and `GRANT SELECT (cols)` does not remove that. PostgreSQL
    // also keeps table and column grants separately, so `REVOKE … ON table`
    // alone would leave an older column grant standing. Revoking both layers
    // first is what makes the fix reach existing databases and what makes a
    // column dropped from the allowlist actually disappear.
    revoke_core_grants(pool, &role).await?;

    if let Some(grants) = core_grants(plugin_id) {
        for grant in grants {
            // Every field comes from the static allowlist above — never user
            // input.
            let privileges = match grant.columns {
                Some(columns) => format!("{} ({})", grant.privileges, columns.join(", ")),
                None => grant.privileges.to_string(),
            };
            sqlx::query(&format!(
                "GRANT {privileges} ON core.{} TO \"{role}\"",
                grant.table
            ))
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

/// Remove every table- and column-level privilege `role` holds on `core.*`.
///
/// PostgreSQL stores table and column grants as separate ACL entries:
/// `REVOKE ALL ON core.users` does **not** clear `GRANT SELECT (password_hash)
/// ON core.users`, and a table-level grant masks any column grants beneath it.
/// A DO block walks both layers so [`bootstrap_role`] can rebuild the grant set
/// from [`core_grants`] alone. `schema`/`table` names come from `pg_tables` and
/// `pg_attribute`, and `role` is core-validated before this is called.
async fn revoke_core_grants(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    let sql = format!(
        "DO $revoke$
         DECLARE r RECORD; c RECORD;
         BEGIN
           FOR r IN SELECT tablename FROM pg_tables WHERE schemaname = 'core' LOOP
             EXECUTE format('REVOKE ALL ON core.%I FROM %I', r.tablename, '{role}');
             FOR c IN SELECT attname FROM pg_attribute
                      WHERE attrelid = format('core.%I', r.tablename)::regclass
                        AND attnum > 0 AND NOT attisdropped
             LOOP
               EXECUTE format('REVOKE ALL (%I) ON core.%I FROM %I',
                              c.attname, r.tablename, '{role}');
             END LOOP;
           END LOOP;
         END $revoke$;"
    );
    sqlx::query(&sql).execute(pool).await?;
    Ok(())
}

/// A fresh 256-bit hex password from `pgcrypto` (already installed; no Rust RNG
/// dependency). Hex keeps the value safe to interpolate into `ALTER ROLE`.
async fn generate_secret(pool: &PgPool) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT encode(gen_random_bytes(32), 'hex')")
        .fetch_one(pool)
        .await
}

/// Create/refresh the dedicated **application** role (design `plugin-isolation.md`
/// §3.6): `LOGIN`, no `SUPERUSER`, no `CREATEROLE`, no `CREATEDB`. It owns no
/// *plugin* schema. `password` is set when given (interpolated; single quotes
/// escaped); when absent the existing password is left alone.
pub async fn bootstrap_app_role(
    pool: &PgPool,
    role: &str,
    password: Option<&str>,
) -> Result<(), sqlx::Error> {
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
    let mut alter = format!("ALTER ROLE \"{role}\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE");
    if let Some(pw) = password {
        alter.push_str(&format!(" PASSWORD '{}'", pw.replace('\'', "''")));
    }
    sqlx::query(&alter).execute(pool).await?;
    Ok(())
}

/// Transfer ownership of the `core` schema and every object inside it to
/// `role`, and grant it `CONNECT, CREATE` on the current database.
///
/// The application role must own `core` so it can run core migrations (which
/// `ALTER core.*` tables) and serve core services **without being a superuser**.
/// This is the one deviation from §3.6's "owns nothing": it owns the core
/// schema, not any plugin schema. Requires a connection that owns the objects.
pub async fn transfer_core_ownership(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await?;
    sqlx::query(&format!(
        "GRANT CONNECT, CREATE ON DATABASE \"{db}\" TO \"{role}\""
    ))
    .execute(pool)
    .await?;
    let sql = format!(
        "DO $own$
         DECLARE r RECORD;
         BEGIN
           FOR r IN SELECT tablename FROM pg_tables WHERE schemaname = 'core' LOOP
             EXECUTE format('ALTER TABLE core.%I OWNER TO %I', r.tablename, '{role}');
           END LOOP;
           FOR r IN SELECT sequencename FROM pg_sequences WHERE schemaname = 'core' LOOP
             EXECUTE format('ALTER SEQUENCE core.%I OWNER TO %I', r.sequencename, '{role}');
           END LOOP;
           FOR r IN SELECT p.proname, pg_get_function_identity_arguments(p.oid) AS args
                    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                    WHERE n.nspname = 'core' LOOP
             EXECUTE format('ALTER FUNCTION core.%I(%s) OWNER TO %I', r.proname, r.args, '{role}');
           END LOOP;
         END $own$;"
    );
    sqlx::query(&sql).execute(pool).await?;
    sqlx::query(&format!("ALTER SCHEMA core OWNER TO \"{role}\""))
        .execute(pool)
        .await?;
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO \"{role}\""))
        .execute(pool)
        .await?;
    tracing::info!(role = %role, "core schema ownership transferred to the application role");
    Ok(())
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
        assert!(
            core_grants("hello").is_none(),
            "unknown plugins get no core access"
        );
        let auth = core_grants("auth").expect("auth has a core allowlist");
        assert!(auth
            .iter()
            .any(|g| g.table == "users" && g.privileges.contains("INSERT")));
        // #149: the password-change route makes UPDATE real, but only on the
        // hash column. A table-level UPDATE would also allow rewriting usernames,
        // emails and `is_active`, which no auth route does.
        assert!(
            !auth.iter().any(|g| g.table == "users"
                && g.privileges.contains("UPDATE")
                && g.columns.is_none()),
            "auth must not re-introduce a table-level UPDATE on core.users (#148)"
        );
        let hash_update = auth
            .iter()
            .find(|g| g.table == "users" && g.privileges == "UPDATE")
            .expect("the password-change route needs an UPDATE grant");
        assert_eq!(
            hash_update.columns,
            Some(&["password_hash"][..]),
            "UPDATE is narrowed to the one column the route writes"
        );
        assert!(
            auth.iter()
                .any(|g| g.table == "sessions" && g.privileges.contains("UPDATE")),
            "revoking a session writes core.sessions.revoked_at"
        );

        let membership = core_grants("membership").expect("membership has a core allowlist");
        assert!(
            !membership.iter().any(|g| g.table == "user_roles"),
            "membership must not write/read core.user_roles (role assignment is auth's, #19)"
        );
        assert!(
            membership
                .iter()
                .any(|g| g.table == "scope_hierarchy" && g.privileges.contains("INSERT")),
            "membership declares the lodge->patrol hierarchy"
        );
        assert!(
            membership
                .iter()
                .filter(|g| g.table != "scope_hierarchy")
                .all(|g| !g.privileges.contains("DELETE")),
            "membership is read-only on the data tables it touches"
        );

        // `core.users` carries password hashes, so both plugins get
        // column-level grants. `SELECT *` must not work for either role, and
        // the hash is reachable only by auth.
        let users = |plugin: &str| -> Vec<&CoreGrant> {
            core_grants(plugin)
                .expect("allowlist")
                .iter()
                .filter(|g| g.table == "users")
                .collect()
        };
        for id in ["auth", "membership"] {
            assert!(
                users(id).iter().all(|g| g.columns.is_some()),
                "{id} must hold column-level, not table-level, privileges on core.users (#148)"
            );
        }
        let membership_cols = users("membership")[0].columns.expect("columns");
        assert_eq!(
            membership_cols,
            &["id", "username", "email", "display_name", "is_active"],
            "membership gets only the roster-identity columns"
        );
        for forbidden in [
            "password_hash",
            "external_id",
            "avatar_url",
            "created_at",
            "updated_at",
        ] {
            assert!(
                !membership_cols.contains(&forbidden),
                "membership must not be granted core.users.{forbidden}"
            );
        }
        let auth_select = users("auth")
            .into_iter()
            .find(|g| g.privileges == "SELECT")
            .and_then(|g| g.columns)
            .expect("auth has a column-level SELECT");
        assert!(
            auth_select.contains(&"password_hash"),
            "login verifies the hash, so auth keeps SELECT on that column"
        );
        assert!(
            !auth_select.contains(&"avatar_url"),
            "auth reads no column it was not checked to read"
        );

        // The ownership map is the trust anchor for hierarchy edges: no plugin
        // may hold a grant on it (issue #37).
        for id in ["auth", "membership", "hello"] {
            if let Some(grants) = core_grants(id) {
                assert!(
                    !grants.iter().any(|g| g.table == "scope_owners"),
                    "{id} must not be granted core.scope_owners"
                );
            }
        }
    }
}
