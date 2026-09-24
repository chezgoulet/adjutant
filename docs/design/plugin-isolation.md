# Plugin isolation: making the boundary a confinement

**Status:** design for sign-off (no code written).
**Issues:** #17, #18 (the demonstrated escapes), the core-public part of #21.
**Depends on:** nothing. **Blocks:** any third-party/WASM plugin story, and the OSG conversation.
**Related:** [`plugin-roadmap.md`](../plugin-roadmap.md) §5, SPEC §5.2, `docs/architecture.md` "Trust model".

---

## 1. What is true today

Isolation is applied as `SET LOCAL ROLE adjutant_plugin_<id>` inside a transaction opened
on a connection whose **session user is the deployment role** (`server/src/host.rs:163-186`).
PostgreSQL permits a session user to `SET ROLE` to itself, and a `DO` block is a single
statement, so the prepared-statement rule that blocks naive multi-statement SQL does not
help. All of the following were reproduced against a running server at `v0.2.0`
(`server/src/wasm.rs`, guest `escape_probe`):

| # | Attempt | Result |
|---|---|---|
| E1 | guest **migration** declaring `CREATE TABLE core.pwned_mig` | **succeeded** — migrations run on the base pool via `raw_sql` |
| E2 | `DO $$ BEGIN EXECUTE 'SET LOCAL ROLE adjutant'; EXECUTE 'CREATE TABLE core.pwned_do'; END $$` | **succeeded** — one statement, no multi-statement rule involved |
| E3 | same shape, `EXECUTE 'CREATE ROLE evil_probe SUPERUSER LOGIN'` | **succeeded** — the connecting role was a superuser |
| E4 | `RESET ROLE` as a plugin statement | succeeded (harmless alone; shows the role is not a confinement) |
| E5 | `SET LOCAL ROLE adjutant; SELECT …` (two statements) | **refused** by the prepared-statement rule — the only reason E2 needed a `DO` block |

Two further facts, from reading rather than execution:

- `ctx.permissions`, `ctx.audit` and `ctx.events` are built from `CoreDb::new(pool)` — **no
  schema, no role** (`server/src/plugin_runtime.rs:295, 439-443`). Their SQL is core-authored
  and parameter-bound, so this is not itself an injection, but it means `core.audit_log` and
  `core.events` are written as the base role regardless of the plugin's allowlist.
- The id→grants map (`server/src/schema.rs:36-48`) keys privileges on a *declarable* name.
  #21 narrowed the impersonation risk to the untrusted path; it did not remove the pattern.

**Conclusion.** `SET LOCAL ROLE` is a state change on a privileged session, not a restricted
principal. Tightening it (scanning SQL for `SET ROLE`/`RESET ROLE`/`set_config`, using
`SECURITY DEFINER`) means writing a SQL firewall, and a SQL firewall is a losing game. The
boundary has to be the *identity of the connection*.

---

## 2. Options

### A — Per-plugin `LOGIN` role and its own pool ★ recommended

Each plugin gets `adjutant_plugin_<id>` as a **LOGIN** role that **owns its schema**, and the
host runs that plugin's SQL on a pool authenticated as that role. The plugin cannot become
anything else: `SET ROLE`/`RESET ROLE` can only return it to its own identity, and
`DO`-block tricks are inert.

- Cost: one small pool per plugin; credentials to manage; schema ownership moves from the
  deployment role to the plugin role.
- Wins: confinement is a property of the connection, not of a statement filter. Migrations run
  on the plugin's own pool, so guest DDL can only ever touch the plugin's schema (E1 dies
  without needing a special case). The `arg` for the old mechanism — "one pool, set the schema
  per call" — disappears.

### B — Keep one pool, grant the app role less

Run the deployment role as a non-superuser that holds only the plugin roles. This shrinks the
blast radius (no `CREATEROLE`, no superuser) but does **not** confine anything: `RESET ROLE`
lands on the app role, which holds core grants by construction. It fixes the *severity* of E3
and leaves E1/E2 intact. Worth doing **in addition to A** (see §3.6), never instead of it.

### C — `SECURITY DEFINER` functions / SQL allowlist

Rejected. It requires parsing plugin SQL to decide what is permitted, which is the firewall
problem again, and it breaks the plugin API (arbitrary parameterised SQL is the product).

---

## 3. Recommended design

### 3.1 Roles and schemas

- One role per plugin: `adjutant_plugin_<id>`, `LOGIN`, no `SUPERUSER`, no `CREATEROLE`, no
  `CREATEDB`.
- One schema per plugin, **owned by that role**: `CREATE SCHEMA "<id>" AUTHORIZATION
  adjutant_plugin_<id>`.
- No implicit `core.*` access. The explicit allowlist in `schema.rs` stays, but it now grants
  to a role that cannot shed it.
- The plugin role is a member of nothing, and nothing is a member of it except the role that
  needs to administer it at bootstrap.

### 3.2 Bootstrap

Role creation and the generated passwords need a privileged path; the *runtime* must not.

- `adjutant bootstrap-isolation` (new CLI subcommand) run once by the operator against an
  admin URL: creates/updates the per-plugin roles, sets schema ownership, writes the
  allowlist grants, and emits credentials.
- **Decided:** credentials live in the database, in a **dedicated `core.plugins.db_secret`
  column** — *not* inside the `config` JSON, because `config` is handed to every plugin as
  `ctx.config` and a password placed there would be given straight to the plugin it is meant
  to constrain. `bootstrap-isolation` writes it; the host reads it at load and never
  serialises it. Two conditions are part of the design because they are what make this safe:
  (a) `core.plugins` is never added to a plugin's `core.*` allowlist, and (b) no API route
  ever returns a plugin's config or secret — the admin surface reports
  id/name/version/enabled/kind/routes/permissions and nothing else. Precedent for secrets in
  the DB: the auth plugin's OIDC `client_secret` already lives in this table.
- Boot **fails** if a role from the plugin directory has no credential. No silent fallback.

### 3.3 Pools and budget

- One `PgPool` per plugin, `max_connections = 2` by default, capped by a new
  `plugins.max_pools` / documented total (PG's `max_connections` is 100 by default; the
  shipped compose should raise it to 200 or the default pool cap should drop to 1).
- The pool's connection string is built by the host from the secrets file. Plugins receive
  `ctx.db` as today; nothing in the plugin API changes shape.
- Delete the role-less `CoreDb::for_plugin(...)` path once nothing uses it.

### 3.4 Migrations

Migrations run on the plugin's own pool, as the plugin role, inside the same
schema-scoped transaction as today. Consequences:

- A guest's migration can create/alter objects **in its own schema** and nothing else. E1
  becomes impossible without any new validation rule.
- The deployment role needs no DDL rights at all.
- `validate_migrations` keeps its version/uniqueness checks.

### 3.5 Core services (`permissions`, `audit`, `events`)

These stay on the core's own handle, and that stays deliberate: the SQL is core-authored, the
actor is bound by the core, and the plugin cannot choose either. What changes is the claim:

- Document them as **core-mediated privileged services**, not as covered by plugin isolation.
- `audit.log` continues to attribute via `core.audit_log.user_id` / `details.user_id` (the
  ordering fix from #24 stays).
- Optionally, add the plugin's own role as the *source* column value so the audit trail
  records which plugin asked, independent of the actor.

### 3.6 The deployment role (option B as a companion)

The shipped `docker-compose.yml` makes `POSTGRES_USER` a superuser, which is what turned E3
into total compromise. The design requires:

- A dedicated `adjutant_app` role: no `SUPERUSER`, no `CREATEROLE`, owns nothing, holds the
  plugin roles, plus the `core.*` privileges it needs for core services.
- Documented in `docs/deployment.md` with the grants, and asserted at boot: if the connected
  role is a superuser, log loudly and (configurably) refuse.

### 3.7 Fail closed

Today a failure to establish isolation degrades to "run as the base role" with one warning
(`plugin_runtime.rs:387-400`). Under this design there is nothing to degrade *to*: if the
plugin's pool cannot be established, the plugin does not load. Add `isolated: true/false`
(always true, or the plugin is absent) to `PluginInfo` so the admin surface can show it.

**Decided:** the server **refuses to boot** when the connected role is a PostgreSQL
superuser, unless `ADJUTANT_ALLOW_SUPERUSER=true` is set explicitly. The message names the
reason (E3: a plugin escape became total compromise) and the opt-out. The shipped
`docker-compose.yml` moves to a dedicated `adjutant_app` role so the default is the safe one.

---

## 4. What changes in the code

| Area | Change |
|---|---|
| `server/src/schema.rs` | role creation moves to bootstrap; allowlist unchanged; ownership + `LOGIN` |
| `server/src/host.rs` | `CoreDb` becomes role-scoped by construction; remove the role-less plugin path |
| `server/src/plugin_runtime.rs` | build one pool per plugin; fail the load when isolation is unavailable |
| `server/src/cli.rs` | new `bootstrap-isolation` subcommand |
| `docs/deployment.md` | app role, grants, secrets file, pool budget |
| `docs/architecture.md` | trust model: what is confined and what is core-mediated |
| SPEC §5.2 / §11 | record the decision |
| `plugins/sdk` | **no API change required** — `ctx.db` keeps its shape |

---

## 5. Test plan

The escapes are the regression suite. Each becomes a committed probe that fails if the
boundary regresses:

1. guest migration attempting `CREATE TABLE core.*` → must fail to load or fail the migration
2. `DO $$ … SET LOCAL ROLE <base> … $$` → must be refused
3. `CREATE ROLE … SUPERUSER` from guest SQL → must be refused
4. `RESET ROLE` / `SET ROLE <other>` → must be refused or inert
5. cross-schema read (`SELECT … FROM <other_plugin>.<table>`) → permission denied (exists today)
6. `core.*` table outside the allowlist → permission denied (exists today)
7. **assert the session user** is the plugin role on every plugin connection (a boot probe, so
   a regression in the host is caught even if a plugin never tries to escape)

Gate: run against a throwaway Postgres 18 in CI (the DB-gated suite already exists), plus the
existing native and WASM load paths.

---

## 6. Compatibility and cost

- **SDK ABI:** no change. Plugins keep `ctx.db.query/execute`; what changes is the connection
  underneath them.
- **Deployment:** existing installs need one `bootstrap-isolation` run and the secrets file.
  `docs/deployment.md` gains an upgrade section; the old `SET LOCAL ROLE` code path is removed
  in the same release, so the upgrade is not optional.
- **Performance:** one extra connection per active plugin. A troop server runs ~4–15 plugins;
  the cost is a few MB of Postgres backends, not a redesign.
- **Risk:** schema ownership transfers to plugin roles, so a plugin can `DROP` its own tables.
  That is correct (it owns them) and the destructive path is already covered by uninstall
  semantics, but it should be stated.

---

## 7. Out of scope

- Sandbox hardening beyond the database (wasmtime fuel/memory/preopens stay as they are).
- Signing/verifying third-party plugin artifacts; the plugin directory remains operator-trusted.
- Multi-tenancy, per-troop database separation (SPEC M7+).

---

## 8. Decisions (signed off 2026-09-24)

| # | Question | Decision |
|---|---|---|
| 1 | Native plugins | **Same per-plugin role and pool as WASM guests** — one code path for both; a boundary that exists on only one path is one typo away from not existing. |
| 2 | Plugin passwords | **In the database, in a dedicated `core.plugins.db_secret` column** — not inside the `config` JSON, which plugins receive as `ctx.config` (§3.2). |
| 3 | Superuser connection | **Refuse to boot** unless `ADJUTANT_ALLOW_SUPERUSER=true` is set explicitly (§3.7). |
| 4 | Work order | **Isolation first**, then scoped permissions — serial, because both touch `plugin_runtime.rs`. |

Recorded by the steward, not open questions: the escapes in §1 become **committed regression
probes** (§5), and the existing native path (`SET LOCAL ROLE` on a base-role connection) is
**deleted** in the same change rather than left as a fallback — the whole point is that there
is no unisolated path to fall back to.

---

## 9. Implementation sequence (proposed)

1. `bootstrap-isolation` + secrets in `core.plugins.config` + per-plugin pools (roles, ownership, allowlist grants).
2. Migrations move to the plugin pool; delete `CoreDb::for_plugin`'s role-less form.
3. Boot refusal for superuser; `isolated` in `PluginInfo`; deployment docs + compose app role.
4. The seven regression probes from §5, wired into the DB-gated CI suite.
5. Then, and only then, scoped permissions ([`scoped-permissions.md`](scoped-permissions.md)) — same branch cadence, separate PR.
