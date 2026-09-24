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
"),
(2, "audit_chain_and_uninstall", "
ALTER TABLE core.plugins ADD COLUMN IF NOT EXISTS uninstalled BOOLEAN NOT NULL DEFAULT false;

CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public;

ALTER TABLE core.audit_log ADD COLUMN IF NOT EXISTS prev_hash TEXT;
ALTER TABLE core.audit_log ADD COLUMN IF NOT EXISTS entry_hash TEXT;

DO $fn$
DECLARE r RECORD; prev TEXT := repeat('0', 64); h TEXT;
BEGIN
  FOR r IN SELECT id, action, resource_type, resource_id, details, source FROM core.audit_log ORDER BY id LOOP
    h := encode(public.digest(prev || '|' || r.id::text || '|' || r.action || '|' || r.resource_type || '|' || r.resource_id || '|' || r.details::text || '|' || r.source, 'sha256'), 'hex');
    UPDATE core.audit_log SET prev_hash = prev, entry_hash = h WHERE id = r.id;
    prev := h;
  END LOOP;
END
$fn$;

CREATE OR REPLACE FUNCTION core.audit_chain_fill() RETURNS trigger
LANGUAGE plpgsql AS $fn$
DECLARE prev TEXT;
BEGIN
  PERFORM pg_advisory_xact_lock(918273645);
  SELECT entry_hash INTO prev FROM core.audit_log ORDER BY id DESC LIMIT 1;
  NEW.prev_hash := COALESCE(prev, repeat('0', 64));
  NEW.entry_hash := encode(public.digest(
    NEW.prev_hash || '|' || NEW.id::text || '|' || NEW.action || '|' ||
    NEW.resource_type || '|' || NEW.resource_id || '|' ||
    NEW.details::text || '|' || NEW.source, 'sha256'), 'hex');
  RETURN NEW;
END
$fn$;

DROP TRIGGER IF EXISTS audit_chain_fill ON core.audit_log;
CREATE TRIGGER audit_chain_fill BEFORE INSERT ON core.audit_log
  FOR EACH ROW EXECUTE FUNCTION core.audit_chain_fill();

CREATE OR REPLACE FUNCTION core.audit_append_only() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
  RAISE EXCEPTION 'core.audit_log is append-only (attempt %)', TG_OP;
END
$fn$;

DROP TRIGGER IF EXISTS audit_append_only ON core.audit_log;
CREATE TRIGGER audit_append_only BEFORE UPDATE OR DELETE ON core.audit_log
  FOR EACH ROW EXECUTE FUNCTION core.audit_append_only();

-- TRUNCATE does not fire row-level triggers, so without this the whole chain can
-- be erased in one statement and core.audit_verify() then reports a healthy empty
-- log (ok:true, rows_checked:0). Statement-level guard, same function.
DROP TRIGGER IF EXISTS audit_no_truncate ON core.audit_log;
CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON core.audit_log
  FOR EACH STATEMENT EXECUTE FUNCTION core.audit_append_only();

CREATE OR REPLACE FUNCTION core.audit_verify()
RETURNS TABLE(first_bad BIGINT, rows_checked BIGINT)
LANGUAGE sql STABLE AS $fn$
SELECT min(id) FILTER (WHERE bad)::BIGINT, count(*)::BIGINT FROM (
  SELECT a.id,
    (a.prev_hash IS DISTINCT FROM COALESCE(lag(a.entry_hash, 1) OVER w, repeat('0', 64)))
    OR (a.entry_hash IS DISTINCT FROM encode(public.digest(
        COALESCE(lag(a.entry_hash, 1) OVER w, repeat('0', 64)) || '|' || a.id::text || '|' ||
        a.action || '|' || a.resource_type || '|' || a.resource_id || '|' ||
        a.details::text || '|' || a.source, 'sha256'), 'hex')) AS bad
  FROM core.audit_log a WINDOW w AS (ORDER BY a.id)
) t
$fn$;
"),
(3, "plugin_isolation", "
-- Per-plugin LOGIN credentials live here, never in `config` (which is handed
-- to the plugin as ctx.config). See docs/design/plugin-isolation.md §3.2.
ALTER TABLE core.plugins ADD COLUMN IF NOT EXISTS db_secret TEXT;

-- Core-owned columns/indexes that used to be created by the auth plugin's own
-- migration. A plugin migration now runs as the plugin role and can only touch
-- its own schema, so anything on core.* belongs to core migrations.
ALTER TABLE core.users ADD COLUMN IF NOT EXISTS username TEXT;
ALTER TABLE core.users ADD COLUMN IF NOT EXISTS password_hash TEXT;
CREATE INDEX IF NOT EXISTS idx_core_sessions_hash ON core.sessions (token_hash);
CREATE UNIQUE INDEX IF NOT EXISTS idx_core_users_username
  ON core.users (lower(username)) WHERE username IS NOT NULL;

-- Migration bookkeeping for the plugin pool. A plugin role cannot write
-- core.schema_migrations directly (that would let it forge another plugin's
-- migration state); it records its own migrations through this function, which
-- is SECURITY DEFINER and checks the caller. The core migration runner connects
-- as the deployment role and may record any schema.
CREATE OR REPLACE FUNCTION core.record_migration(p_schema TEXT, p_version BIGINT, p_name TEXT)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = core, pg_temp
AS $fn$
BEGIN
  IF session_user LIKE 'adjutant_plugin_%'
     AND p_schema <> substring(session_user FROM length('adjutant_plugin_') + 1) THEN
    RAISE EXCEPTION 'role % may only record migrations for its own schema (got %)',
      session_user, p_schema;
  END IF;
  INSERT INTO core.schema_migrations (schema, version, name)
  VALUES (p_schema, p_version, p_name)
  ON CONFLICT (schema, version) DO NOTHING;
END
$fn$;

GRANT EXECUTE ON FUNCTION core.record_migration(TEXT, BIGINT, TEXT) TO PUBLIC;
"),
(4, "scope_type_check", "
-- Scope types are now a closed set (issue #22 / scoped-permissions design §3.5).
-- Grants whose scope_type is not one of these are dropped, never widened: a
-- malformed row used to map to 'troop' in the auth parser, which escalated it.
-- The 'personal' value is retired; drop it and anything else unrecognised, with
-- a warning naming the user and role so the loss is never silent.
DO $scope$
DECLARE r RECORD;
BEGIN
  FOR r IN
    SELECT user_id, role_id, scope_type
    FROM core.user_roles
    WHERE scope_type NOT IN ('troop', 'lodge', 'patrol')
  LOOP
    RAISE WARNING 'dropping %-scoped grant for user % role % (invalid body scope type; troop/lodge/patrol only)',
      r.scope_type, r.user_id, r.role_id;
  END LOOP;
END
$scope$;
DELETE FROM core.user_roles WHERE scope_type NOT IN ('troop', 'lodge', 'patrol');

ALTER TABLE core.user_roles DROP CONSTRAINT IF EXISTS user_roles_scope_type_check;
ALTER TABLE core.user_roles ADD CONSTRAINT user_roles_scope_type_check
  CHECK (scope_type IN ('troop', 'lodge', 'patrol'));
"),
(5, "scope_id_text", "
-- #33: a scope reference is opaque text owned by the plugin (a lodge id may be a
-- bigint, a UUID or a slug). NULL means troop-wide, replacing the zero-UUID
-- convention. `core.user_roles` had a PRIMARY KEY over scope_id, and a PK column
-- cannot be nullable, so the PK is replaced by a unique index over
-- COALESCE(scope_id,'') — which still forbids duplicate troop grants.
ALTER TABLE core.user_roles DROP CONSTRAINT IF EXISTS user_roles_pkey;
ALTER TABLE core.user_roles ALTER COLUMN scope_id DROP DEFAULT;
ALTER TABLE core.user_roles ALTER COLUMN scope_id DROP NOT NULL;
ALTER TABLE core.user_roles ALTER COLUMN scope_id TYPE TEXT USING scope_id::text;

-- Normalise existing (all troop-wide) rows to NULL.
UPDATE core.user_roles SET scope_id = NULL
  WHERE scope_type = 'troop' OR scope_id = '00000000-0000-0000-0000-000000000000';

CREATE UNIQUE INDEX IF NOT EXISTS idx_user_roles_unique
  ON core.user_roles (user_id, role_id, COALESCE(scope_id, ''));

-- Troop-wide <=> scope_id IS NULL. Both malformed combinations are unstorable.
ALTER TABLE core.user_roles DROP CONSTRAINT IF EXISTS user_roles_scope_ref_check;
ALTER TABLE core.user_roles ADD CONSTRAINT user_roles_scope_ref_check
  CHECK ((scope_type = 'troop' AND scope_id IS NULL)
      OR (scope_type <> 'troop' AND scope_id IS NOT NULL));
"),
(6, "scope_hierarchy", "
-- Declared scope edges: 'patrol 7 is inside lodge 3'. The core loads these into
-- an in-memory map and expands a caller's grants with the descendants of each
-- grant scope, so a lodge grant covers the patrols declared inside it. Edges are
-- data, not policy: the core never hardcodes which type is broader, it only
-- follows parent -> child downward.
CREATE TABLE IF NOT EXISTS core.scope_hierarchy (
    parent_type TEXT NOT NULL,
    parent_id   TEXT NOT NULL,
    child_type  TEXT NOT NULL,
    child_id    TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- troop is the implicit root and carries no id; it is never a declared edge.
    CHECK (parent_type <> 'troop' AND child_type <> 'troop'),
    -- No self-edge. Deeper cycles are rejected by the loader (it drops an edge
    -- that would close one) so a bad declaration cannot hang the walk.
    CHECK (NOT (parent_type = child_type AND parent_id = child_id))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_scope_hierarchy_edge
  ON core.scope_hierarchy (parent_type, parent_id, child_type, child_id);
")];

/// Bootstrap roles + permissions grants. `chief` gets everything (SPEC §9 —
/// real role management arrives with the auth plugin in Milestone 2; this is
/// the seed the prototype enforces against).
// Two statements, two constants: `sqlx::query` prepares a single statement
// ("cannot insert multiple commands into a prepared statement").
const SEED_ROLES: &str = "
INSERT INTO core.roles (id, display_name, description) VALUES
    ('chief', 'Chief', 'Full troop authority (placeholder until auth plugin)'),
    ('scout', 'Scout', 'Standard member (placeholder until auth plugin)')
ON CONFLICT (id) DO NOTHING;
";

const SEED_PERMS: &str = "
INSERT INTO core.permissions (id, description) VALUES
    ('core:admin', 'Administer plugins, reload, and audit verification')
ON CONFLICT (id) DO UPDATE SET description = EXCLUDED.description;
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

    sqlx::query(SEED_ROLES).execute(&pool).await?;
    sqlx::query(SEED_PERMS).execute(&pool).await?;
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
    validate_migration_name(name)?;

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
    // SET LOCAL inside the transaction: a bare SET before BEGIN is auto-committed
    // and leaks the plugin's search_path onto the pooled connection for the rest
    // of its life.
    let script = format!(
        "BEGIN;\
         SET LOCAL search_path TO \"{schema}\";\
         {sql};\
         INSERT INTO core.schema_migrations (schema, version, name) VALUES ('{schema}', {version}, '{name}');\
         COMMIT;"
    );
    // Call `Executor::execute` directly rather than `RawSql::execute` (an
    // `async fn` wrapper). The wrapper's future trips rustc's HRTB limit
    // ("implementation of Executor is not general enough") and can't be proven
    // Send, which makes every handler that awaits run_migration (hot-reload)
    // fail axum's Handler bound. The direct call returns a concrete
    // `BoxFuture` (= `Pin<Box<dyn Future + Send>>`) — provably Send.
    // Found by bisecting with probe handlers: raw_sql-via-wrapper was the
    // only !Send piece (2026-09-21, M2).
    use sqlx::Executor;
    Executor::execute(&mut *conn, sqlx::raw_sql(&script)).await?;

    tracing::info!(schema, version, name, "migration applied");
    Ok(())
}

/// Version numbers already applied for `schema`, read from
/// `core.schema_migrations` on the **core** pool: a plugin role cannot read that
/// table (only record its own migrations through `core.record_migration`).
pub async fn applied_migrations(pool: &PgPool, schema: &str) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT version FROM core.schema_migrations WHERE schema = $1")
        .bind(schema)
        .fetch_all(pool)
        .await
}

/// Run one plugin migration on the **plugin's own pool**, as the plugin role.
///
/// The DDL and its bookkeeping are one transaction, so a crash cannot leave DDL
/// applied but unrecorded. The plugin role cannot write `core.schema_migrations`
/// directly; it records through the SECURITY DEFINER `core.record_migration`,
/// which refuses a schema that is not the caller's own (core migration 3). The
/// DDL can therefore only touch objects inside the plugin's schema — E1 from
/// `docs/design/plugin-isolation.md` §1 is impossible without any SQL firewall.
pub async fn run_plugin_migration(
    pool: &PgPool,
    schema: &str,
    version: i64,
    name: &str,
    sql: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_name(schema)?;
    validate_migration_name(name)?;

    let mut conn = pool.acquire().await?;
    let script = format!(
        "BEGIN;\
         SET LOCAL search_path TO \"{schema}\", public;\
         {sql};\
         SELECT core.record_migration('{schema}', {version}, '{name}');\
         COMMIT;"
    );
    use sqlx::Executor;
    Executor::execute(&mut *conn, sqlx::raw_sql(&script)).await?;
    tracing::info!(schema, version, name, "plugin migration applied");
    Ok(())
}

/// Migration names are interpolated into the runner's script, so validate the
/// shape the same way schema names are (a stray quote otherwise aborts the whole
/// batch with an opaque error).
fn validate_migration_name(name: &str) -> Result<(), sqlx::Error> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '));
    if ok {
        Ok(())
    } else {
        Err(sqlx::Error::config(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid migration name {name:?}: expected [A-Za-z0-9_. -]{{1,64}}"),
        )))
    }
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
