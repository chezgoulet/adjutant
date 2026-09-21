//! Database: connection pool + core schema migrations.
//!
//! Core owns `core.*` (users, sessions, roles, permissions, plugins, events,
//! audit — SPEC §8.1). Plugin schemas are created and migrated by the plugin
//! runtime (see `plugin_runtime.rs`).

use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::Config;

/// Core schema DDL (SPEC §8.1). Idempotent — `IF NOT EXISTS` throughout.
const CORE_MIGRATIONS: &[(i64, &str, &str)] = &[(1, "core_schema", "
CREATE SCHEMA IF NOT EXISTS core;

CREATE TABLE IF NOT EXISTS core.users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    external_id     TEXT UNIQUE,
    email           TEXT UNIQUE,
    display_name    TEXT NOT NULL,
    avatar_url      TEXT,
    is_active       BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS core.sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    token_hash      TEXT NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    device_info     JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS core.plugins (
    id              TEXT PRIMARY KEY,
    version         TEXT NOT NULL,
    enabled         BOOLEAN NOT NULL DEFAULT true,
    config          JSONB NOT NULL DEFAULT '{}',
    installed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS core.roles (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    description     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS core.permissions (
    id              TEXT PRIMARY KEY,
    description     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS core.role_permissions (
    role_id         TEXT NOT NULL REFERENCES core.roles(id) ON DELETE CASCADE,
    permission_id   TEXT NOT NULL REFERENCES core.permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

CREATE TABLE IF NOT EXISTS core.user_roles (
    user_id         UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    role_id         TEXT NOT NULL REFERENCES core.roles(id) ON DELETE CASCADE,
    scope_type      TEXT NOT NULL DEFAULT 'troop',
    -- NOT NULL DEFAULT (not COALESCE-in-PK): PostgreSQL forbids expressions
    -- in PRIMARY KEY column lists. The zero UUID means troop-wide scope.
    scope_id        UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    granted_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    granted_by      UUID REFERENCES core.users(id),
    PRIMARY KEY (user_id, role_id, scope_id)
);

CREATE TABLE IF NOT EXISTS core.events (
    id              BIGSERIAL PRIMARY KEY,
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    source_plugin   TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_core_events_type ON core.events (event_type);
CREATE INDEX IF NOT EXISTS idx_core_events_created ON core.events (created_at);

CREATE TABLE IF NOT EXISTS core.audit_log (
    id              BIGSERIAL PRIMARY KEY,
    user_id         UUID REFERENCES core.users(id),
    action          TEXT NOT NULL,
    resource_type   TEXT NOT NULL,
    resource_id     TEXT NOT NULL,
    details         JSONB NOT NULL DEFAULT '{}',
    source          TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_core_audit_created ON core.audit_log (created_at);
")];

/// Bootstrap roles + permissions grants. `chief` gets everything (SPEC §9 —
/// real role management arrives with the auth plugin in Milestone 2; this is
/// the seed the prototype enforces against).
const SEED: &str = "
INSERT INTO core.roles (id, display_name, description) VALUES
    ('chief', 'Chief', 'Full troop authority (placeholder until auth plugin)'),
    ('scout', 'Scout', 'Standard member (placeholder until auth plugin)')
ON CONFLICT (id) DO NOTHING;
";

/// Connect, run core migrations, seed bootstrap roles.
pub async fn connect_and_migrate(cfg: &Config) -> Result<Arc<PgPool>, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;

    // The schema must exist before run_migration can create its bookkeeping
    // table (core.schema_migrations) — fresh databases don't have it yet.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS core").execute(&pool).await?;

    for (version, name, sql) in CORE_MIGRATIONS {
        run_migration(&pool, "core", *version, name, sql).await?;
    }

    sqlx::query(SEED).execute(&pool).await?;
    tracing::info!("database ready (core schema migrated, bootstrap roles seeded)");

    Ok(Arc::new(pool))
}

/// Run one migration inside `schema`, recording it in `core.schema_migrations`.
/// Single-connection so `SET search_path` applies to the DDL.
pub async fn run_migration(
    pool: &PgPool,
    schema: &str,
    version: i64,
    name: &str,
    sql: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_name(schema)?;

    let mut conn = pool.acquire().await?;

    // Migration bookkeeping table (lives in core schema, always present after
    // migration 1 — but guard for plugins racing the first core migration).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS core.schema_migrations (\
             schema TEXT NOT NULL, version BIGINT NOT NULL, name TEXT NOT NULL,\
             applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),\
             PRIMARY KEY (schema, version))",
    )
    .execute(&mut *conn)
    .await?;

    let already: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM core.schema_migrations WHERE schema = $1 AND version = $2",
    )
    .bind(schema)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?;

    if already.is_some() {
        return Ok(());
    }

    // Multi-statement DDL + search_path in one simple-query protocol round trip.
    // Parameterized APIs can't do this; every input is either validated or
    // plugin-authored SQL (which is trusted by construction — plugins are code).
    let script = format!(
        "SET search_path TO \"{schema}\";\
         BEGIN;\
         {sql};\
         INSERT INTO core.schema_migrations (schema, version, name) VALUES ('{schema}', {version}, '{name}');\
         COMMIT;"
    );
    sqlx::raw_sql(&script).execute(&mut *conn).await?;

    tracing::info!(schema, version, name, "migration applied");
    Ok(())
}

/// Plugin ids double as PostgreSQL schema names — constrain hard.
fn validate_schema_name(schema: &str) -> Result<(), sqlx::Error> {
    let ok = !schema.is_empty()
        && schema.len() <= 31
        && schema.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && schema.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(sqlx::Error::config(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid schema name {schema:?}: expected [a-z][a-z0-9_]{{0,30}}"),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_name_validation() {
        assert!(validate_schema_name("hello").is_ok());
        assert!(validate_schema_name("membership").is_ok());
        assert!(validate_schema_name("a1_b2").is_ok());
        assert!(validate_schema_name("").is_err());
        assert!(validate_schema_name("Hello").is_err());
        assert!(validate_schema_name("1hello").is_err());
        assert!(validate_schema_name("he;llo").is_err());
        assert!(validate_schema_name("hello-world").is_err());
        assert!(validate_schema_name(&"x".repeat(40)).is_err());
    }
}
