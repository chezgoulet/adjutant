# Scoped permissions: from declared to enforced

**Status:** design for sign-off (no code written).
**Issues:** #22 (scope is never enforced), and the two concrete failures it exposes — #19 and #20.
**Depends on:** nothing. **Blocks:** missions and governance (Lodge Commander approval, voting).
**Related:** [`plugin-roadmap.md`](../plugin-roadmap.md) §5, SPEC §9.2, `docs/architecture.md`.

---

## 1. What is true today

M4 shipped the data model for scope (`ScopeType`/`Scope`/`RoleGrant`,
`Identity::from_grants`, `roles_covering`) and auth populates it from `core.user_roles`. The
enforcement half did not ship:

- **The gate is unscoped.** `authorize()` calls `permissions.has(identity, required)`
  (`server/src/permissions.rs:34-46`), and `has` matches *any* held role with no reference to
  its scope. `Identity::from_grants` puts every grant's role into `roles` regardless of scope.
- **`has_in_scope` has zero production call sites** — `grep` finds it in the SDK definition and
  in SDK tests only.
- **Consequence:** a grant of `membership:manage` scoped to lodge L1 satisfies every
  `membership:manage` route troop-wide. The confinement the model describes does not exist.
  Today this is latent only because nothing creates a scoped row; every existing grant is
  troop-wide by default (`core.user_roles.scope_type DEFAULT 'troop'`).
- **The parse fails open.** `plugins/auth/src/lib.rs:181-186` maps any unrecognised
  `scope_type` (`'Lodge'`, a typo, a future name) to `ScopeType::Troop`, which
  `Scope::covers` treats as covering everything. A malformed row *escalates*.
- **`scope_type` is free text** with no `CHECK` (`server/src/db.rs:66-76`), and nothing
  requires a `scope_id` for a non-troop type.
- **The two fields can disagree.** `Identity.roles` and `Identity.grants` are both public and
  independent: `Identity { roles: ["chief"], grants: [] }` compiles and yields
  `has == true`, `has_in_scope == false`. `#[serde(default)]` on `grants` means a stale
  payload silently has no scopes rather than failing.

Two shipped bugs are the same root, and are the reason this design is not optional:

- #19 — `membership:manage` can grant **any** role, including `chief`, through the CSV import
  (`plugins/membership/src/lib.rs:538-549`), which auth gates behind `auth:manage_users`.
- #20 — `GET /api/membership/member?id=` returns any member's full PII under
  `membership:read`, whose own description is "View own profile" — no ownership check, and
  `membership:read_lodge`/`read_all` are never consulted.

---

## 2. The principle

**A scope is checked where the resource is known, and the default is the restrictive one.**

The core knows a route was called, not which lodge a row belongs to. So:

- Routes whose reach is **global** (collections, imports, admin) are gated by the core on a
  **troop-covering** grant. A lodge-scoped grant does not open them.
- Routes whose reach is **one object** must check that object's scope **in the handler**,
  using the same scope types as the grant. The core's gate still fires first (the caller must
  hold the permission somewhere); the handler decides whether they hold it *here*.
- **Forgetting a check must deny, not allow.** Declaring a route the ordinary way makes it
  troop-covering; the permissive form is an explicit, greppable choice.

---

## 3. Design

### 3.1 Route declarations

`RouteDefinition` gains a declared scope, and the vocabulary makes intent explicit:

| Constructor | Meaning | Default for |
|---|---|---|
| `get_protected(path, perm, handler)` | requires a grant covering **troop** | collections, admin, reference data |
| `get_protected_any_scope(path, perm, handler)` | requires the permission at *some* scope; **the handler must check the object's scope** | object routes (`/member?id=`, `/mission/{id}/approve`) |
| `get(path, handler)` | unauthenticated (unchanged) | health, login, OIDC |

`validate_declaration` keeps its rules and gains one: a route declared `_any_scope` must
carry a note in its doc/test that names the scope check (enforced socially + by the conformance
tests in §4, not statically — a static check would be a lie).

### 3.2 The gate

- `authorize(identity, permissions, permission, required_scope)` where `required_scope`
  defaults to `Scope::troop()` for ordinary routes and is `None` for `_any_scope` routes.
- A missing identity, an empty grant list, or a grant that does not *cover* the required scope
  is a denial. Coverage is strict: `Troop ⊇ Lodge/Patrol/Personal`, `Lodge ⊇ Patrol`, and
  `Personal` covers nothing but itself.
- Denials are logged with permission + required scope + the caller's grant scopes. (Today a
  denial says only "insufficient permissions".)

### 3.3 In-handler checks

Add one helper to the SDK so the safe path is the short one:

```rust
// ctx.permissions
pub async fn reach(&self, id: Option<&Identity>, perm: &str, scope: &Scope) -> Result<(), SdkError>
//   Ok(())  → allowed at that scope
//   Err(Forbidden) → 403 with a message naming the scope
```

And make the unscoped check impossible to call by accident: rename `PermissionService::has`
to `has_any_scope` and mark it `#[doc(hidden)]`, used only by the core gate. A plugin author
wanting the old behaviour must write `has_in_scope(…, &Scope::troop())` — which states the
intent instead of hiding it. *(ABI: this is a boundary-visible change; see §6.)*

### 3.4 Identity

- **`grants` is the single source of truth.** `roles` becomes a derived getter
  (`roles()`), not a public field, so the two cannot disagree.
- Drop `#[serde(default)]` on `grants`: an identity payload without grants fails
  deserialization loudly rather than silently holding no scopes.
- `Identity::new(id, roles)` keeps its current behaviour (every role at troop scope) but its
  doc must say so in one line, because it is the dev-header stub's constructor and the reason
  dev environments are more permissive than production.

### 3.5 Fail closed in the data

- `plugins/auth`: an unrecognised or missing `scope_type` **drops the grant** and logs an
  error, instead of widening it to troop.
- A non-troop `scope_type` with no/zero `scope_id` is an invalid grant, dropped.
- Migration adds `CHECK (scope_type IN ('troop','lodge','patrol','personal'))` to
  `core.user_roles`.

### 3.6 The two concrete fixes

- **#19** — remove the `core.user_roles` write from membership's importer entirely (auth owns
  roles), drop membership's `INSERT` grant on `user_roles` in `schema.rs`, and log each
  granted role in the audit `details`.
- **#20** — `/api/membership/member` requires one of: `membership:read_all`; or
  `membership:read_lodge` reaching the target's lodge; or `membership:read` where the target
  *is* the caller. Reference-data routes (`/lodges`, `/proficiencies`, `/stewards`) move to
  the lodge scope rather than the self-service one.

---

## 4. Test plan

A conformance matrix, run as a normal test with mock permissions *and* as live probes:

| Grant scope | Route kind | Expected |
|---|---|---|
| troop | troop route | allow |
| lodge L1 | troop route | **deny** |
| lodge L1 | object route, target lodge L1 | allow |
| lodge L1 | object route, target lodge L2 | **deny** |
| patrol P | object route, target P | allow |
| patrol P | object route, target sibling patrol | **deny** |
| personal | any troop route | **deny** |
| none / no identity | anything protected | 401/403 |

Plus: the malformed-`scope_type` case must **drop** the grant (a row with `'Lodge'` grants
nothing), and the two #19/#20 scenarios become probes: a `membership:manage` caller cannot
grant `chief` through the import, and a `membership:read` caller cannot read `?id=<someone
else>`.

---

## 5. Migration and docs

- Existing rows default to `'troop'`, so current behaviour is preserved for real grants.
- `SPEC.md` §9.2 is rewritten to describe enforcement, not intent; `docs/architecture.md`
  gains the scope table; `docs/plugin-development.md` gains the route-constructor table and
  the "check where the resource is known" rule.
- The `dev-headers` stub stays troop-wide and stays off by default.

---

## 6. Compatibility

`RouteDefinition` and the permission-service surface cross the plugin boundary, so this is a
**breaking SDK change** under the policy in `docs/sdk-compatibility.md`:

- new field on `RouteDefinition`, renamed method, new helper → bump `SDK_ABI_VERSION` to 3 and
  document it in `CHANGELOG.md` with a migration note for plugin authors.
- Native plugins built against ABI 2 are refused at load with the existing actionable error,
  which is the correct outcome.

---

## 7. Out of scope

- Per-object ACLs beyond scope (per-row grants, delegations).
- Role hierarchy changes; the role ids stay as they are.
- Audit of *denials* as first-class events — worth doing later, not in this change.

---

## 8. Open questions for sign-off

1. **`personal` scope:** currently covers nothing but itself. Is that right for member-facing
   routes (a scout reading their own record), or should self-access be handled purely by an
   ownership check with no scope involved?
2. **Multi-lodge commanders:** one lodge per grant today. A commander over two lodges needs two
   rows, which is fine — confirm that is intended rather than a `Scope::lodges([..])`.
3. **Mutations default:** should `post`/`put`/`patch`/`delete` require a troop-covering grant
   even when declared `_any_scope`? Recommended: yes for `delete`, no for others.
4. **Order of work:** this after plugin isolation (#17/#18), or in parallel in a separate
   worktree? Both touch `plugin_runtime.rs`; serialising is safer.
