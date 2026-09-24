# The core/plugin boundary: what must be core, what should be a plugin

**Status:** decided (owner's direction, 2026-09-24: keep as much as possible in plugins so a
deployment can be customized).
**Purpose:** one place to settle the argument, so every future component has a home before someone
builds it in the wrong one.

---

## 1. The rule

Three tests, in order. The first one that answers wins.

1. **Could a plugin weaken it by being wrong or malicious?** → **Core.** The permission gate, the
   isolation roles, the append-only audit store, the plugin registry, the SDK ABI. A plugin must
   never own the data that decides what a plugin may do.
2. **Is it domain content, or a policy that two deployments could reasonably set differently?** →
   **Plugin**, or plugin-declared config. Missions, governance, finance, membership, the Accords
   library, which OIDC provider, what a mission's lifecycle is.
3. **Is it plumbing that several plugins would otherwise each reinvent?** → **A host service in the
   core, exposed to plugins** — the *service* is core, the *domain* that uses it stays a plugin.
   Database access, events, HTTP. And the three we still owe: scheduling, notifications, and (for
   field plugins) declared network capabilities.

The one-line version, for future arguments: **if two deployments could differ, it belongs in a
plugin or config; if a plugin could weaken it, it belongs in the core.**

## 2. Where we actually stand

Audited, not assumed:

- **Core modules** — `cli, config, db, events, host, identity, lib, main, middleware, permissions,
  plugin_runtime, schema, scope_hierarchy, server, wasm`. All infrastructure. **No domain content.**
- **Core routes** — `/`, `/api/plugins`, `/api/plugins/{name}`, enable/disable/reload,
  `/api/events/recent`, `/api/audit/verify`. All operational or administrative.
- **Core tables** — `users, sessions, plugins, roles, permissions, role_permissions, user_roles,
  events, audit_log, scope_hierarchy, scope_owners, schema_migrations`.
- **Host services available to plugins** — `HostDb`, `HostEvents`, `HostHttp` (three).

So there is nothing to move *out* of the core. The core is already the thin thing SPEC §5.1 says it
should be. What remains are three boundary questions, and they are worth answering once each.

## 3. The three deliberate exceptions, and why they are not violations

**3.1 Identity lives in `core.*` even though `auth` is a plugin.** `users`, `sessions`, `roles`,
`permissions`, `role_permissions`, `user_roles` are in the core schema, owned by the auth plugin's
migrations. This is deliberate: **permissions gate everything**, so the tables that hold them are
part of the boundary, not part of a deployment's choices. A troop can replace *how* people
authenticate (OIDC provider, local passwords, OIDC-as-provider later) without moving the
authorization data. Keep it; the cost is that auth is not fully swappable, and that is the right
trade.

**3.2 The scope hierarchy is domain data, but the core owns its resolution.**
`membership.patrols.lodge_id` is domain; `core.scope_hierarchy` is a **declared projection** of it,
and `core.scope_owners` records who may declare what. The core needs the hierarchy to answer an
authorization question, and it must not call a plugin during a permission check.

This generalizes, and it is the pattern to reuse whenever the core needs to know something
domain-shaped: **the domain data stays in the plugin's schema; the plugin declares a projection;
the core owns the projection and its enforcement.** Stated here so the next case does not
re-litigate it.

**3.3 Events and the audit log are core.** `core.events` is the durable record behind the bus;
`core.audit_log` is append-only by trigger and hash-chained. Both must be core because a plugin that
could rewrite them could hide its own actions. The *policy* — what gets audited, which events are
emitted — is plugin-declared.

## 4. Where each pending component goes

| Component | Home | Why |
|---|---|---|
| Scheduler / periodic work (#45) | **Core service**, plugin declares schedules | every plugin needing a timer would otherwise grow a thread the core cannot audit or stop |
| Notification inbox + queue (#46) | **Core service** | one durable record of "this person was told" |
| Notification **channels** (email, push) | **Plugins** — or config'd providers | a deployment chooses its channel; the queue does not care |
| Background-check record, status, expiry rule | **`membership` plugin** (domain) | OSG's yearly cycle is domain policy, not core |
| Expiry *timer* | **Core scheduler, declared by membership** | the boundary rule working as intended |
| S3-compatible storage | **Plugin**, per SPEC §145 | it is HTTP to an S3 endpoint; `HostHttp` already suffices, so no new core capability |
| Documents / Accords library / 34 templates | **Plugin** (governance, archive) | content, and the troop's own text |
| Widget vocabulary + metadata contract | **Core** (`client-and-plugin-ui.md`) | it is the client's contract with *every* plugin; screens themselves are plugin-declared |
| Localization: client chrome | **Client** | `localization.md` |
| Localization: plugin strings | **Plugin** | it owns its words |
| Locale plumbing (`Accept-Language`) | **Core** | one request property, not per-plugin reinvented |
| MCP server | **Plugin** (Tier A) | already planned |
| Field/bridge plugins (ATAK, Meshcore) | **Plugins**, using declared capabilities | `plugin-capabilities.md` |
| OSG sync | **Plugin**, deferred | `osg-interop.md` |
| Migrations runner, config, logging, backup/restore | **Core / ops** | infrastructure, not a deployment choice |
| Role *content* (what a Lodge Commander may do) | **Config / plugin-declared**, enforced by core | deployments legitimately differ; the gate does not |

## 5. The test to apply when someone proposes a new core component

Ask in this order, and write the answer down in the PR:

1. Could a plugin weaken it? → core.
2. Do two deployments plausibly differ? → plugin or config.
3. Would several plugins each rebuild it? → core host service, with the domain remaining in plugins.
4. If none of the three: it is probably neither — it is client work, or it is not needed yet.

The failure mode this prevents is the one that costs the most later: a domain's *policy* hardening
inside the core, where a troop cannot change it without forking the binary.
