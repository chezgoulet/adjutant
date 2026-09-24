# API Reference

Adjutant's HTTP API is JSON over HTTP. Plugin routes live under
`/api/{plugin}/…`; the core owns a small admin surface. See
[`architecture.md`](architecture.md) for the request path and
[`plugin-development.md`](plugin-development.md) for the SDK.

## Conventions

- **Content type:** request and response bodies are JSON unless a plugin says
  otherwise.
- **Errors:** every error response has the shape `{"error": "message"}`. Statuses
  follow `SdkError::status()`: `400` bad request, `401` unauthenticated, `403`
  forbidden, `404` not found, `409` conflict, `500` internal. **5xx responses
  never include internal detail** (it is logged server-side).
- **Auth:** send the session cookie `adjutant_session=…` (set by login), or
  `Authorization: Bearer <token>`. The spoofable `x-dev-user`/`x-dev-role`
  headers are available only when dev headers are explicitly enabled, and only
  when no identity provider claimed the request.
- **Request id:** every response carries `x-request-id`; the same id appears in
  the access log.
- **Rate limiting:** fixed window per client IP (config `ADJUTANT_RATE_MAX`,
  `ADJUTANT_RATE_WINDOW`; `0` disables). A `429` includes `retry-after`.
- **CORS:** controlled by `ADJUTANT_CORS` (empty = same-origin only).

## Core

| Method | Path | Permission | Purpose |
|---|---|---|---|
| GET | `/` | — | Health: `{"service":"adjutant","status":"ok"}` |
| GET | `/api/plugins` | `core:admin` | Plugin registry, route/permission inventory, `kind` (`native`/`wasm`) |
| POST | `/api/plugins/{name}/enable` | `core:admin` | Enable a plugin |
| POST | `/api/plugins/{name}/disable` | `core:admin` | Disable (routes 404, subscriptions stop) |
| DELETE | `/api/plugins/{name}` | `core:admin` | Uninstall (data archived, not dropped) |
| POST | `/api/plugins/reload` | `core:admin` | Rescan the plugin dir and hot-swap |
| GET | `/api/events/recent?since=<id>&limit=<1..500>` | `core:admin` | Event replay with cursor |
| GET | `/api/audit/verify` | `core:admin` | Verify the audit hash chain |

## Auth plugin

Config lives in the `auth` row's `core.plugins.config` (`{"oidc": {…},
"session_ttl_hours": N}`); reload to apply. OIDC routes answer `501` when no
`oidc` block is configured.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/auth/login` | — | `{username, password}` → session token + `adjutant_session` cookie |
| POST | `/api/auth/logout` | — | Ends the session |
| GET | `/api/auth/me` | session | Current user and scoped role grants |
| POST | `/api/auth/register` | — | `{username, email?, password}`; **open only while no users exist**; first user becomes `chief` (min 8-char password) |
| POST | `/api/auth/users` | `auth:manage_users` | `{username, email?, password, roles?}` |
| POST | `/api/auth/roles` | `auth:manage_users` | `{username, roles: string[]}` (replaces grants) |
| GET | `/api/auth/users` | `auth:manage_users` | List users |
| GET | `/api/auth/oidc/login` | — | Redirects to the IdP authorize endpoint (state stored server-side) |
| GET | `/api/auth/oidc/callback` | — | Code exchange, id_token verification, session creation |

## Membership plugin

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| GET | `/api/membership/members` | `membership:read_all` | Roster |
| GET | `/api/membership/member?username=<u>` | `membership:read` | One member (query param) |
| POST | `/api/membership/member` | `membership:manage` | `{username, email?, display_name, trail_name?, osg_id?, bg_check?, patrol?, is_active?}` |
| POST | `/api/membership/import` | `membership:manage` | OSG CSV (see plugin docs); upsert by username |
| GET | `/api/membership/lodges` | `membership:read` | Lodges |
| POST | `/api/membership/lodge` | `membership:manage` | `{name}` |
| POST | `/api/membership/patrol` | `membership:manage` | `{name, lodge?}` |
| GET | `/api/membership/proficiencies` | `membership:read` | Proficiencies |
| POST | `/api/membership/proficiency` | `membership:manage` | `{code, title, domain?}` |
| POST | `/api/membership/proficiency/complete` | `membership:manage` | `{member_id, proficiency_id, signed_off_by?}` |
| GET | `/api/membership/stewards` | `membership:read` | Stewards |
| POST | `/api/membership/steward` | `membership:manage` | `{member_id, position, lodge?}` |

## Example plugins

`hello` (native) and `hello_wasm` (sandboxed) both expose:

| Method | Path | Permission | Purpose |
|---|---|---|---|
| GET | `/api/hello` | — | Open route |
| GET | `/api/hello/greetings` | `hello:read` | List greetings |
| POST | `/api/hello/greet` | `hello:write` | Insert + publish `hello.greeted` + audit |
| GET | `/api/hello/greetings/{id}` | `hello:read` | Path capture |

`hello_wasm` is the same shape under `/api/hello_wasm` with the
`hello_wasm:*` permissions, running in the WASM sandbox.

## SDK surface (`adjutant-sdk`)

| Item | Purpose |
|---|---|
| `AdjutantPlugin` | The trait every plugin implements (`id`, `name`, `version`, `init`, `routes`, `migrations`, `permissions_granted`, `subscriptions`, `shutdown`) |
| `PluginContext` | Runtime services handed to `init`: `db`, `config`, `events`, `permissions`, `audit`, `identity`, `http` |
| `RouteDefinition`, `Method` | Route registration. `get`/`post`/`put`/`patch`/`delete`/`head` (+ `_protected`, which requires a troop-covering grant) and `_protected_any_scope` (the handler checks the object's scope; `delete` is always troop-only) |
| `PluginRequest` / `PluginResponse` | Framework-neutral request/response (`param`, `query_param`, `json`, `redirect`, `created`, …) |
| `HostDb` / `HostEvents` / `HostHttp` | Host-mediated I/O traits (implemented by the core) |
| `Migration`, `Permission`, `Scope`, `ScopeType`, `RoleGrant` | Declarations and scoped permissions; `Identity.grants` is the source of truth (`roles()` is derived) |
| `SdkError` | Error type with a single HTTP mapping (`status()`) |
| `SqlValue` | Typed bind parameters (uuid, typed nulls, arrays, JSON) |
| `prelude` | One import for plugin authors |
| `testing` | `TestHost`, `MockDb`/`MockEvents`/`MockHttp`/`MockIdentity`, `TestRequest` |

Generated API docs: `cargo doc --workspace --no-deps --open`.
