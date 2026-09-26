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
"),
(7, "scope_owners", "
-- Per-scope-type ownership (#37). Written by the CORE only (seeded from a core
-- constant at boot); plugins have NO grant on it. It is the trust anchor for
-- hierarchy edges: whoever can write this map can legitimise any edge and so
-- widen any plugin's coverage. It is created empty here; the core seeds it.
CREATE TABLE IF NOT EXISTS core.scope_owners (
    scope_type TEXT PRIMARY KEY,
    plugin_id  TEXT NOT NULL
);

-- Refuse an edge whose parent or child scope type the declaring plugin does not
-- own. The declaring plugin is derived from `session_user` (plugins authenticate
-- as adjutant_plugin_<id>), so a native plugin running raw SQL is caught, not
-- only the declare function. SECURITY DEFINER so the check can read
-- `core.scope_owners`, which the caller cannot. Non-plugin roles (the core/app
-- role) seed and repair the table freely.
CREATE OR REPLACE FUNCTION core.scope_hierarchy_owner_check() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = core, pg_temp AS $fn$
DECLARE
  caller TEXT := session_user;
  plugin TEXT;
  owner TEXT;
  pt TEXT;
  ct TEXT;
BEGIN
  IF TG_OP = 'DELETE' THEN
    pt := OLD.parent_type; ct := OLD.child_type;
  ELSE
    pt := NEW.parent_type; ct := NEW.child_type;
  END IF;
  IF caller NOT LIKE 'adjutant_plugin_%' THEN
    RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
  END IF;
  plugin := substring(caller FROM length('adjutant_plugin_') + 1);
  SELECT plugin_id INTO owner FROM core.scope_owners WHERE scope_type = pt;
  IF owner IS DISTINCT FROM plugin THEN
    RAISE EXCEPTION 'plugin % may not declare an edge with parent scope type % (owned by %)',
      plugin, pt, COALESCE(owner, '<unowned>');
  END IF;
  SELECT plugin_id INTO owner FROM core.scope_owners WHERE scope_type = ct;
  IF owner IS DISTINCT FROM plugin THEN
    RAISE EXCEPTION 'plugin % may not declare an edge with child scope type % (owned by %)',
      plugin, ct, COALESCE(owner, '<unowned>');
  END IF;
  RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
END
$fn$;

DROP TRIGGER IF EXISTS scope_hierarchy_owner_check ON core.scope_hierarchy;
CREATE TRIGGER scope_hierarchy_owner_check
  BEFORE INSERT OR UPDATE OR DELETE ON core.scope_hierarchy
  FOR EACH ROW EXECUTE FUNCTION core.scope_hierarchy_owner_check();

-- The declare API: a plugin gets a clear error at declaration time; the trigger
-- above is the backstop for raw SQL. SECURITY DEFINER so it can read
-- `core.scope_owners`; it still uses `session_user` to identify the plugin.
CREATE OR REPLACE FUNCTION core.declare_scope_parent(
    p_parent_type TEXT, p_parent_id TEXT, p_child_type TEXT, p_child_id TEXT
) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = core, pg_temp AS $fn$
DECLARE
  plugin TEXT;
  owner TEXT;
BEGIN
  IF session_user LIKE 'adjutant_plugin_%' THEN
    plugin := substring(session_user FROM length('adjutant_plugin_') + 1);
    SELECT plugin_id INTO owner FROM core.scope_owners WHERE scope_type = p_parent_type;
    IF owner IS DISTINCT FROM plugin THEN
      RAISE EXCEPTION 'plugin % may not declare a % edge (owned by %)',
        plugin, p_parent_type, COALESCE(owner, '<unowned>');
    END IF;
    SELECT plugin_id INTO owner FROM core.scope_owners WHERE scope_type = p_child_type;
    IF owner IS DISTINCT FROM plugin THEN
      RAISE EXCEPTION 'plugin % may not declare a % edge (owned by %)',
        plugin, p_child_type, COALESCE(owner, '<unowned>');
    END IF;
  END IF;
  INSERT INTO core.scope_hierarchy (parent_type, parent_id, child_type, child_id)
  VALUES (p_parent_type, p_parent_id, p_child_type, p_child_id)
  ON CONFLICT DO NOTHING;
END
$fn$;

GRANT EXECUTE ON FUNCTION core.declare_scope_parent(TEXT, TEXT, TEXT, TEXT) TO PUBLIC;
"),
(8, "scheduled_runs", "
-- Durable record of scheduled runs (#45): 'did the timer actually run?'
-- In-memory state cannot answer that after a restart. The core writes one row
-- per run; a failure is recorded here and logged, never retried in a loop.
CREATE TABLE IF NOT EXISTS core.scheduled_runs (
    id          BIGSERIAL PRIMARY KEY,
    plugin_id   TEXT NOT NULL,
    schedule    TEXT NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ NOT NULL,
    ok          BOOLEAN NOT NULL,
    error       TEXT
);
CREATE INDEX IF NOT EXISTS idx_scheduled_runs_recent
  ON core.scheduled_runs (plugin_id, schedule, finished_at DESC);
"),
    (
        9,
        "outbox",
        "-- Why this is a new version rather than more DDL in 1: an applied version
-- is skipped by number and its SQL is never re-run or compared, so anything
-- added to an earlier version would exist on a fresh database and be absent on
-- every deployed one. Same reason `finance` cannot edit its version 1.
-- Numbered 9 because 1-8 are already applied in the field; the next core
-- migration takes 10.
--
-- The money path (docs/design/plugin-to-plugin.md §3.2): a durable intent for a
-- machine-originated fact that has no caller to forward a credential from.

-- A declared service principal: a first-class, non-human identity. The role and
-- its grant live in core.roles / core.role_permissions so an operator sees it
-- beside a member's and can revoke it; this table records which plugin may
-- enqueue a delivery *as* it, and enforces producer uniqueness there.
CREATE TABLE IF NOT EXISTS core.service_principals (
    principal       TEXT PRIMARY KEY REFERENCES core.roles(id) ON DELETE CASCADE,
    producer_plugin TEXT NOT NULL,
    description     TEXT NOT NULL,
    declared_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at      TIMESTAMPTZ,
    -- Set by the core's own seeding from SERVICE_PRINCIPALS, never by an operator:
    -- the compiled declaration is what authorizes a delivery, so a row the core
    -- never declared may not enqueue an intent that could never be delivered.
    declared_by_core BOOLEAN NOT NULL DEFAULT false,
    CONSTRAINT service_principals_producer_is_named CHECK (length(producer_plugin) > 0),
    CONSTRAINT service_principals_revoked_after_declaration
        CHECK (revoked_at IS NULL OR revoked_at >= declared_at)
);
CREATE INDEX IF NOT EXISTS idx_service_principals_producer
  ON core.service_principals (producer_plugin);
-- One principal per (producer, operation) is the declaration's contract; two
-- rows for the same producer would leave 'which one authorised this?' unanswerable.
CREATE UNIQUE INDEX IF NOT EXISTS idx_service_principals_one_per_producer
  ON core.service_principals (principal, producer_plugin);

-- (Deliberately no ALTER TABLE here. It would be inert: a migration whose version is
-- already recorded is skipped whole, so an ALTER added to this file never executes on
-- a database that applied the earlier shape of it — which is the only case it would
-- be written for. This migration has never been applied outside development, so its
-- shape was amended in place; had it reached a real deployment, the column would need
-- its own version. A database still carrying the earlier shape needs re-provisioning.)

-- The durable intent. Written in the same statement as the fact it describes, so
-- the two commit together or neither does: there is no window in which a payment
-- is recorded and its ledger booking is not.
CREATE TABLE IF NOT EXISTS core.outbox (
    id              BIGSERIAL PRIMARY KEY,
    producer_plugin TEXT NOT NULL,
    principal       TEXT NOT NULL REFERENCES core.service_principals(principal),
    target_method   TEXT NOT NULL DEFAULT 'POST',
    target_route    TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload         JSONB NOT NULL,
    state           TEXT NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 6,
    answer_status   INTEGER,
    answer          JSONB,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at      TIMESTAMPTZ,
    last_attempt_at TIMESTAMPTZ,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at    TIMESTAMPTZ,
    CONSTRAINT outbox_state_known
        CHECK (state IN ('pending', 'attempting', 'delivered', 'refused', 'exhausted')),
    -- No upper bound on `attempts`: it is spent at the claim, so a crash between
    -- claim and record would otherwise hit a constraint on the reclaim instead of
    -- spending its last attempt -- the row would wedge and the relay would fail
    -- on it forever. Exhaustion is the relay's decision (attempts >= max_attempts),
    -- not a constraint's.
    CONSTRAINT outbox_attempts_are_sane CHECK (attempts >= 0 AND max_attempts >= 1),
    CONSTRAINT outbox_target_is_a_route CHECK (target_route LIKE '/%'),
    CONSTRAINT outbox_key_is_not_empty CHECK (length(idempotency_key) > 0),
    -- One intent per producer per key: a producer retrying its own write cannot
    -- produce a second delivery of the same fact.
    CONSTRAINT outbox_one_intent_per_key UNIQUE (producer_plugin, idempotency_key),
    -- Terminal states are recorded with their evidence, not inferred.
    CONSTRAINT outbox_delivered_was_at_a_time CHECK (state <> 'delivered' OR delivered_at IS NOT NULL),
    CONSTRAINT outbox_answer_is_not_a_delivery CHECK (state <> 'delivered' OR answer_status IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS idx_outbox_due
  ON core.outbox (next_attempt_at) WHERE state IN ('pending', 'attempting');
CREATE INDEX IF NOT EXISTS idx_outbox_producer
  ON core.outbox (producer_plugin, id DESC);
CREATE INDEX IF NOT EXISTS idx_outbox_reconcile
  ON core.outbox (state, created_at);

-- No plugin reads or writes the table directly; both directions go through the
-- two functions below. Stated as a REVOKE rather than left implied, so a future
-- blanket GRANT on the core schema cannot quietly open it.
REVOKE ALL ON core.outbox FROM PUBLIC;
REVOKE ALL ON core.service_principals FROM PUBLIC;

-- Enqueue an intent.
--
-- SECURITY DEFINER so a plugin needs no grant on the table, and **there is no
-- identity parameter at all**: the producer is derived from session_user, and the
-- principal is refused unless it is declared for that producer. So a plugin
-- cannot assert an identity -- the only one it can name is its own. This is the
-- §3.1 refusal made mechanical rather than a convention.
CREATE OR REPLACE FUNCTION core.outbox_enqueue(
    p_principal       TEXT,
    p_target_method   TEXT,
    p_target_route    TEXT,
    p_payload         JSONB,
    p_idempotency_key TEXT
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = core, pg_temp
AS $fn$
DECLARE
    caller_role TEXT := session_user;
    producer    TEXT;
    declared    RECORD;
    existing    BIGINT;
    new_id      BIGINT;
BEGIN
    IF caller_role NOT LIKE 'adjutant_plugin_%' THEN
        RAISE EXCEPTION 'core.outbox_enqueue is for plugin roles; % may not enqueue an intent',
            caller_role;
    END IF;
    producer := substring(caller_role FROM length('adjutant_plugin_') + 1);

    -- No declaration, or a revoked one, is not a delivery: it is an error the
    -- producer sees now, in its own transaction, rather than an intent that
    -- cannot be delivered later.
    SELECT sp.producer_plugin, sp.revoked_at, sp.declared_by_core INTO declared
      FROM core.service_principals sp
     WHERE sp.principal = p_principal;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no service principal % is declared for this core', p_principal;
    END IF;
    IF declared.revoked_at IS NOT NULL THEN
        RAISE EXCEPTION 'service principal % was revoked at % and may not enqueue',
            p_principal, declared.revoked_at;
    END IF;
    IF declared.producer_plugin <> producer THEN
        RAISE EXCEPTION 'plugin % may not enqueue as principal % (declared for %)',
            producer, p_principal, declared.producer_plugin;
    END IF;
    -- The declaration that authorizes a delivery is the core's compiled
    -- SERVICE_PRINCIPALS; this row is its mirror. Without this check the two could
    -- disagree, and a row the core never declared would enqueue an intent that
    -- could never be delivered -- a producer would inherit a stuck intent instead
    -- of being told now, in its own transaction.
    IF NOT declared.declared_by_core THEN
        RAISE EXCEPTION 'service principal % is not declared by this core, so no intent enqueued as it could be delivered',
            p_principal;
    END IF;

    -- Idempotent: the same fact enqueued twice (a producer retrying its own write,
    -- a redelivered webhook) returns the intent it already has.
    SELECT o.id INTO existing
      FROM core.outbox o
     WHERE o.producer_plugin = producer AND o.idempotency_key = p_idempotency_key;
    IF existing IS NOT NULL THEN
        RETURN existing;
    END IF;

    INSERT INTO core.outbox
        (producer_plugin, principal, target_method, target_route, payload, idempotency_key)
    VALUES
        (producer, p_principal, upper(p_target_method), p_target_route, p_payload, p_idempotency_key)
    RETURNING id INTO new_id;
    RETURN new_id;
END
$fn$;
-- EXECUTE on the *function*, never on the table: that is the whole design.
GRANT EXECUTE ON FUNCTION core.outbox_enqueue(TEXT, TEXT, TEXT, JSONB, TEXT) TO PUBLIC;

-- A producer reads the state of its own intents (and its own only) without a
-- grant on core.outbox. Scoped by session_user, the same derivation as enqueue.
CREATE OR REPLACE FUNCTION core.outbox_producer_view()
RETURNS TABLE (
    id              BIGINT,
    principal       TEXT,
    target_method   TEXT,
    target_route    TEXT,
    idempotency_key TEXT,
    state           TEXT,
    attempts        INTEGER,
    max_attempts    INTEGER,
    answer_status   INTEGER,
    last_error      TEXT,
    payload         JSONB,
    answer          JSONB,
    created_at      TIMESTAMPTZ,
    last_attempt_at TIMESTAMPTZ,
    delivered_at    TIMESTAMPTZ,
    next_attempt_at TIMESTAMPTZ
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = core, pg_temp
AS $fn$
DECLARE
    caller_role TEXT := session_user;
    producer    TEXT;
BEGIN
    IF caller_role NOT LIKE 'adjutant_plugin_%' THEN
        RAISE EXCEPTION 'core.outbox_producer_view is for plugin roles; % may not read it',
            caller_role;
    END IF;
    producer := substring(caller_role FROM length('adjutant_plugin_') + 1);
    RETURN QUERY
        SELECT o.id, o.principal, o.target_method, o.target_route, o.idempotency_key,
               o.state, o.attempts, o.max_attempts, o.answer_status, o.last_error,
               o.payload, o.answer, o.created_at, o.last_attempt_at, o.delivered_at,
               o.next_attempt_at
          FROM core.outbox o
         WHERE o.producer_plugin = producer
         ORDER BY o.id DESC;
END
$fn$;
GRANT EXECUTE ON FUNCTION core.outbox_producer_view() TO PUBLIC;
"
    ),
    (
        10,
        "notifications",
        "-- Why this is a new version and not more DDL in 1: an applied version is
-- skipped by number and its SQL is never re-run or compared, so anything added
-- to an earlier version would exist on a fresh database and be absent on every
-- deployed one. Numbered 10 because 1-9 are already applied in the field; the
-- next core migration takes 11. Design: docs/design/notifications.md.
--
-- The recorded in-app notification (#46, slice 1): a machine-recorded fact
-- addressed to ONE user, plus a per-user read state. It is the record the
-- owner called 'not a fallback' — the board in the app. Web Push, device push
-- and email are later transports behind this same row; none of them is built.

CREATE TABLE IF NOT EXISTS core.notifications (
    id               BIGSERIAL PRIMARY KEY,
    -- The recipient is a user, not a scope: a notification is addressed to a
    -- person. ON DELETE CASCADE because the row is about them, like a session.
    recipient        UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    -- Which plugin (or 'core') recorded it. Also the namespace of message_code.
    source           TEXT NOT NULL,
    -- A stable identifier, NEVER display text (docs/design/localization.md §4):
    -- the client renders (code, params, locale) from its own catalogue.
    message_code     TEXT NOT NULL,
    -- Data only — ISO-8601 dates, ids, counts. Never a sentence.
    message_params   JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- The recipient's language AT CREATION (a snapshot), BCP-47. 'und' is the
    -- honest value when the producer does not know the recipient's language.
    locale           TEXT NOT NULL,
    -- Delivery is policy, not a caller's choice: this is the transport the row
    -- is recorded for, picked by the core (notifications::DELIVERY_CHANNELS).
    delivery_channel TEXT NOT NULL DEFAULT 'in_app',
    -- The DELIVERY fact, kept separate from read_at below. 'recorded' means
    -- exactly that: nothing was sent. Slice 1 only ever writes this value.
    delivery_state   TEXT NOT NULL DEFAULT 'recorded',
    -- The evidence of a delivery. NULL while nothing has been delivered.
    delivered_at     TIMESTAMPTZ,
    -- The READ fact: a fact about the person, not about a transport. Marking a
    -- notification read writes this and touches delivery_state not at all.
    read_at          TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT notifications_source_present CHECK (btrim(source) <> ''),
    CONSTRAINT notifications_code_is_a_code CHECK (message_code ~ '^[a-z][a-z0-9_.]*$'),
    CONSTRAINT notifications_locale_is_a_tag CHECK (locale ~ '^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$'),
    -- A row may not claim a channel the core has no transport for, or it would
    -- record a delivery promise no code can keep. Widening this list (with
    -- DELIVERY_CHANNELS) is what adding email/push *is*.
    CONSTRAINT notifications_channel_known CHECK (delivery_channel IN ('in_app')),
    CONSTRAINT notifications_delivery_state_known CHECK (
        delivery_state IN ('recorded', 'delivered', 'failed')
    ),
    -- The hard rule, stated in the schema's own terms: 'delivered' is a fact and
    -- it is stored with its evidence, and no delivery time may exist without the
    -- delivered state. This is what makes it impossible to report a notification
    -- as delivered when it was only recorded.
    CONSTRAINT notifications_delivered_was_at_a_time CHECK (
        delivery_state <> 'delivered' OR delivered_at IS NOT NULL
    ),
    CONSTRAINT notifications_delivery_time_only_when_delivered CHECK (
        delivered_at IS NULL OR delivery_state = 'delivered'
    )
);

-- The inbox: one user's records, newest first.
CREATE INDEX IF NOT EXISTS idx_notifications_inbox
  ON core.notifications (recipient, created_at DESC);
-- The unread count, without scanning read records.
CREATE INDEX IF NOT EXISTS idx_notifications_unread
  ON core.notifications (recipient) WHERE read_at IS NULL;
-- The worklist a future transport will drain. Inert in slice 1 (every row is
-- 'recorded'), which is precisely why it is safe to add now.
CREATE INDEX IF NOT EXISTS idx_notifications_undelivered
  ON core.notifications (delivery_channel, created_at) WHERE delivery_state <> 'delivered';

-- The producer seam. A plugin's scheduled run executes on its OWN pool, as its
-- own isolation role, so it cannot INSERT into core.notifications (the table is
-- REVOKE'd below). It writes through this SECURITY DEFINER function instead, and
-- **source is derived from session_user, never a parameter** — a plugin can only
-- ever record a notification as itself. Same discipline as core.outbox_enqueue.
CREATE OR REPLACE FUNCTION core.notify(
    p_recipient      UUID,
    p_message_code   TEXT,
    p_message_params JSONB,
    p_locale         TEXT
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = core, pg_temp
AS $fn$
DECLARE
    caller    TEXT := session_user;
    source    TEXT;
    new_id    BIGINT;
BEGIN
    IF caller NOT LIKE 'adjutant_plugin_%' THEN
        RAISE EXCEPTION 'core.notify is for plugin roles: a notification is created by a scheduled run (as its plugin) or by the core directly -- % may not create one', caller;
    END IF;
    source := substring(caller FROM length('adjutant_plugin_') + 1);

    -- A notification is addressed to a person. A recipient with no user row is
    -- the FK's error, reported now, in the producer's own transaction, rather
    -- than a row nothing can ever address.
    IF NOT EXISTS (SELECT 1 FROM core.users u WHERE u.id = p_recipient) THEN
        RAISE EXCEPTION 'recipient % is not a user; a notification is addressed to a person', p_recipient;
    END IF;

    INSERT INTO core.notifications
        (recipient, source, message_code, message_params, locale, delivery_channel)
    VALUES
        -- The channel is policy, not a producer's choice: the core decides which
        -- transport a record is for, so a producer cannot name one that does not
        -- exist.
        (p_recipient, source, p_message_code, COALESCE(p_message_params, '{}'::jsonb), p_locale, 'in_app')
    RETURNING id INTO new_id;
    RETURN new_id;
END
$fn$;
-- EXECUTE on the *function*, never on the table: that is the whole design.
GRANT EXECUTE ON FUNCTION core.notify(UUID, TEXT, JSONB, TEXT) TO PUBLIC;

-- No plugin reads or writes the table directly; the read path is the core's own
-- routes. Stated as a REVOKE rather than left implied, so a future blanket GRANT
-- on the core schema cannot quietly open it.
REVOKE ALL ON core.notifications FROM PUBLIC;
"
    ),
];

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

/// Connect (no migrations). Split from migration so the caller can refuse a
/// superuser connection **before** running any DDL.
pub async fn connect(cfg: &Config) -> Result<Arc<PgPool>, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;
    Ok(Arc::new(pool))
}

/// Run core migrations and seed bootstrap roles. Idempotent.
pub async fn migrate_core(pool: &PgPool) -> Result<(), sqlx::Error> {
    // The schema must exist before run_migration can create its bookkeeping
    // table (core.schema_migrations) — fresh databases don't have it yet.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS core").execute(pool).await?;

    for (version, name, sql) in CORE_MIGRATIONS {
        run_migration(pool, "core", *version, name, sql).await?;
    }

    sqlx::query(SEED_ROLES).execute(pool).await?;
    sqlx::query(SEED_PERMS).execute(pool).await?;
    // Seed the scope-type ownership map (issue #37): the core writes it; plugins
    // cannot, so it is the trust anchor for hierarchy edges.
    crate::schema::seed_scope_owners(pool).await?;
    tracing::info!("database ready (core schema migrated, bootstrap roles seeded)");
    Ok(())
}

/// Connect, run core migrations, seed bootstrap roles.
pub async fn connect_and_migrate(cfg: &Config) -> Result<Arc<PgPool>, sqlx::Error> {
    let pool = connect(cfg).await?;
    migrate_core(&pool).await?;
    Ok(pool)
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
