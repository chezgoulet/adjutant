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
| GET | `/api/membership/member?id=<id>` | `membership:read` | One member (query param); `read_all` reaches any, `read_lodge` reaches its lodge, `read` only your own |
| POST | `/api/membership/member` | `membership:manage` | `{username, email?, display_name, trail_name?, osg_id?, bg_check?, patrol?, is_active?}` |
| POST | `/api/membership/import` | `membership:manage` | OSG CSV (see plugin docs); upsert by username |
| GET | `/api/membership/lodges` | `membership:read_lodge` | Lodges |
| POST | `/api/membership/lodge` | `membership:manage` | `{name}` |
| POST | `/api/membership/patrol` | `membership:manage` | `{name, lodge?}` |
| GET | `/api/membership/proficiencies` | `membership:read_lodge` | Proficiencies |
| POST | `/api/membership/proficiency` | `membership:manage` | `{code, title, domain?}` |
| POST | `/api/membership/proficiency/complete` | `membership:manage` | `{member_id, proficiency_id, signed_off_by?}` |
| GET | `/api/membership/stewards` | `membership:read_lodge` | Stewards |
| POST | `/api/membership/steward` | `membership:manage` | `{member_id, position, lodge?}` |

### Missions (SPEC §7.3, Accords Art 8)

Config lives in the `missions` row's `core.plugins.config` (unused today).

The six stages are `request → review → approval → execution → debrief → report`;
`state` is `open`, `rejected`, or `completed`. A rejected mission can be appealed
to the Troop Council, which decides it once one other Council member seconds it.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| GET | `/api/missions/missions?stage=&state=&category=&lodge=&limit=` | `missions:read` | Narrowed to the caller's lodge scopes and their own proposals unless they read troop-wide |
| GET | `/api/missions/mission/{id}` | `missions:read` | The mission + milestones, mentorships, progress, stage trail, appeals |
| POST | `/api/missions/mission` | `missions:create` | `{title, purpose, objectives, expected_impact, category?, lodge_id?, lodge_name?, tags?, location?, starts_on?, ends_on?, resources_needed?, risk_notes?, youth_safety_notes?, participant_count?}` — the structured proposal form |
| PATCH | `/api/missions/mission/{id}` | `missions:update` | Any subset of the proposal fields, while the mission is in `request` or `review` |
| POST | `/api/missions/mission/{id}/submit` | `missions:update` | `request → review` |
| POST | `/api/missions/mission/{id}/review` | `missions:approve` | `{success_criteria, scope_notes?, mentor?, recommendation?(advance\|return), guidance?}` → `review → approval` |
| POST | `/api/missions/mission/{id}/decision` | `missions:approve` | `{decision: approved\|rejected, guidance?}` (guidance required on rejection) → `approval → execution` |
| POST | `/api/missions/mission/{id}/appeal` | `missions:appeal` | `{reason}` — the mission must be `rejected` |
| POST | `/api/missions/appeal/{id}/decide` | `missions:approve` (troop) | `{seconded_by, outcome: overturned\|upheld, votes_for?, votes_against?, note?}` — overturning puts the mission into `execution` |
| POST | `/api/missions/mission/{id}/milestone` | `missions:update` | `{title, detail?, due_on?, position?}` |
| PATCH | `/api/missions/milestone/{id}` | `missions:update` | `{title?, detail?, status?, progress_pct?, due_on?}` (`done` implies 100%) |
| DELETE | `/api/missions/milestone/{id}` | `missions:approve` (troop) | Destructive: troop-covering grant only |
| POST | `/api/missions/mission/{id}/progress` | `missions:update` | `{note, progress_pct?, service_hours?, participant_count?}` — execution only |
| POST | `/api/missions/mission/{id}/debrief` | `missions:update` | `{notes, goals_met?, lessons?, service_hours?, participant_count?}` → `execution → debrief` |
| POST | `/api/missions/mission/{id}/report` | `missions:update` | `{summary, impact_metrics?, service_hours?, participant_count?}` → `debrief → report` |
| POST | `/api/missions/mission/{id}/complete` | `missions:approve` | `report → completed`; publishes `mission.completed` |
| POST | `/api/missions/mentor/profile` | `missions:mentor` | `{member_id, display_name?, affiliation?, expertise?, capacity?, is_active?, notes?}` |
| GET | `/api/missions/mission/{id}/mentor/suggestions` | `missions:approve` | Ranked mentors: expertise overlap, then spare capacity |
| POST | `/api/missions/mission/{id}/mentor` | `missions:approve` | `{mentor_member, mentee_member?, role?, notes?}` |
| POST | `/api/missions/mentorship/{id}/close` | `missions:update` | `{status?: completed\|ended, notes?}` |
| GET | `/api/missions/mentorships?member=&mission=` | `missions:read` | Scoped to the caller unless they read troop-wide |
| GET | `/api/missions/impact?lodge=` | `missions:read` (troop) | Cumulative Impact Report: totals, by category, by year, by lodge |

**Events:** `mission.created` (propose), `mission.approved` (approve),
`mission.completed` (complete, payload `MissionCompleted`).

### Governance (SPEC §7.4, Accords Art 5/9/12/17)

Motion lifecycle: `proposed → seconded → debate → voting → decided →
implemented` (or `withdrawn`). Thresholds: `simple_majority` (default),
`two_thirds`, `unanimous`. Vote methods: `voice`, `show_of_hands`, `ballot`,
`roll_call`. Quorum bases: `one_third_registered` (Congress), `majority_members`
(Troop Council, default), `fixed`.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/governance/meeting` | `governance:manage` | `{body: congress\|tc\|lodge\|committee, title, scheduled_for?, lodge_id?, location?, quorum_basis?, expected_voters?, quorum_required?}` |
| GET | `/api/governance/meetings?body=&status=&lodge=&limit=` | `governance:read` | Meetings with present count and motion count |
| GET | `/api/governance/meeting/{id}` | `governance:read` | Meeting + live quorum + motions |
| PATCH | `/api/governance/meeting/{id}` | `governance:manage` | `{title?, scheduled_for?, location?, quorum_basis?, expected_voters?, quorum_required?}` |
| POST | `/api/governance/meeting/{id}/open` | `governance:manage` | Opens the meeting and reports quorum |
| POST | `/api/governance/meeting/{id}/close` | `governance:manage` | Closes it; reports any undecided motions |
| POST | `/api/governance/meeting/{id}/attendance` | `governance:manage` | `{member_id, present?, method?: present\|remote\|proxy\|absent}` (upsert) |
| GET | `/api/governance/meeting/{id}/quorum` | `governance:read` | Real-time display: required, present, met, short by |
| POST | `/api/governance/meeting/{id}/minutes/draft` | `governance:manage` | Auto-drafts minutes from the motion record |
| POST | `/api/governance/meeting/{id}/minutes/adopt` | `governance:manage` | `{minutes?}` — adopts (and stores) the minutes of record |
| GET | `/api/governance/meeting/{id}/minutes` | `governance:read` | The stored minutes and their status |
| POST | `/api/governance/motion` | `governance:propose` | `{title, text, body, meeting_id?, lodge_id?, category?, threshold?, amends_accords?}` |
| GET | `/api/governance/motions?meeting=&body=&stage=&result=&category=&limit=` | `governance:read` | Motion list |
| GET | `/api/governance/motion/{id}` | `governance:read` | Motion + votes + amendments + the tally a close would produce now |
| POST | `/api/governance/motion/{id}/second` | `governance:vote` | A second member seconds it; the mover cannot second their own |
| POST | `/api/governance/motion/{id}/debate` | `governance:manage` | `{open: bool, note?}` — opens debate (`seconded → debate`) or closes it (`debate → voting`) |
| POST | `/api/governance/motion/{id}/vote` | `governance:vote` | `{choice: yes\|no\|abstain, method?, note?}` — one vote per member; a re-vote is `409` |
| POST | `/api/governance/motion/{id}/close` | `governance:manage` | Tallies; requires quorum when the motion is in a meeting; publishes `motion.passed` / `motion.failed` |
| POST | `/api/governance/motion/{id}/implement` | `governance:manage` | `{note?}` — a passed motion only |
| POST | `/api/governance/motion/{id}/withdraw` | `governance:propose` | `{note?}` — the mover, or a chair |
| POST | `/api/governance/motion/{id}/amendment` | `governance:amend` | `{kind: friendly\|formal, text, rationale?}` |
| POST | `/api/governance/amendment/{id}/accept` | `governance:amend` | Friendly only; the mover (or a chair) accepts, and the text is appended to the motion |
| POST | `/api/governance/amendment/{id}/reject` | `governance:manage` | `{note?}` |
| POST | `/api/governance/amendment/{id}/vote` | `governance:vote` | `{choice, method?, note?}` — formal only |
| POST | `/api/governance/amendment/{id}/close` | `governance:manage` | Tallies a formal amendment; a passing one is applied to the motion text |
| POST | `/api/governance/accords/adopt` | `governance:manage` | `{motion_id, title, summary?, body_md?, adopted_on?, congress?}` — the motion must have **passed in a Congress**; creates the next version and supersedes the previous adopted one |
| GET | `/api/governance/accords?status=` | `governance:read` | Version list |
| GET | `/api/governance/accords/{version}` | `governance:read` | One version, including its text |

**Events:** `motion.proposed`, `motion.passed`, `motion.failed` (the SPEC §5.4
trio), plus `accords.adopted` for the archive.

**Role grants are the operator's, not the plugin's.** The core seeds `chief`
with every permission it finds after load; other roles are mapped through
`core.role_permissions`. For the Accords' own pathway, `lodge_commander` needs
`missions:approve` (scoped to their lodge) and `tc`-equivalent roles need
`missions:approve` at troop scope plus `governance:manage` for the chair:

```sql
INSERT INTO core.role_permissions (role_id, permission_id) VALUES
  ('lodge_commander', 'missions:approve'),
  ('lodge_commander', 'missions:read'),
  ('scout',           'missions:read'),
  ('scout',           'missions:create'),
  ('scout',           'governance:read'),
  ('scout',           'governance:propose'),
  ('scout',           'governance:vote')
ON CONFLICT DO NOTHING;
```

## Hermes MCP plugin (SPEC §7.10)

The permissions-aware MCP surface Hermes connects to. Every tool **is** an
Adjutant API call: the plugin checks the permission the tool's route requires,
then calls the route over the core's mediated HTTP client with the caller's own
`authorization`/`cookie` headers forwarded — so the core's route gate checks the
same permission a second time, and the agent path and the UI path meet at one
authorization decision. The plugin holds no credentials of its own, so an
invocation can never exceed the calling user's authority.

`GET /api/mcp/tools` lists only the tools the caller may invoke (a tool whose
plugin is not installed is invisible: no role holds its permission). Every
invocation — including every refusal — is recorded in `mcp.invocations` and in
`core.audit_log`.

Config lives in the `mcp` row's `core.plugins.config`:
`{"base_url": "http://127.0.0.1:8787", "connection_ttl_hours": 24, "max_result_bytes": 65536,
"tools": {"disable": [], "override": {}, "add": []}}`.

| Method | Path | Permission | Body / notes |
|---|---|---|---|
| POST | `/api/mcp/connect` | `mcp:connect` | `{client?, client_version?, protocolVersion?, capabilities?}` → `{connection_id, token, expires_at, protocolVersion, server, capabilities}`. Only the token's SHA-256 is stored; the token is returned once |
| GET | `/api/mcp/tools` | `mcp:connect` | The filtered catalogue: `{name, description, inputSchema, requiredPermission, scope}` per tool, plus `warnings` |
| POST | `/api/mcp/invoke` | `mcp:invoke` | `{tool, arguments, connection_id?}` or a connection token in `connection_token` / the `mcp-session-id` header → `{tool, status: ok\|denied\|error, http_status?, result?, error?, duration_ms, invocation_id}`. The tool's own permission is re-checked, arguments are strictly validated against the tool's schema, and the API's status is passed through |
| GET | `/api/mcp/invocations?tool=&status=&limit=` | `mcp:connect` (own rows) / `mcp:audit` (troop) | The audit trail: user, connection, tool, arguments, the downstream request, status, HTTP status, duration |

Tools (path and permission mirror the owning plugin's route exactly):

| Tool | Route | Permission |
|---|---|---|
| `membership_list_members` | `GET /api/membership/members` | `membership:read_all` (troop) |
| `membership_get_member` | `GET /api/membership/member` | `membership:read` (object scope) |
| `missions_list_missions` | `GET /api/missions/missions` | `missions:read` (object scope) |
| `missions_get_mission` | `GET /api/missions/mission/{id}` | `missions:read` (object scope) |
| `missions_create_mission` | `POST /api/missions/mission` | `missions:create` (object scope) |
| `governance_list_motions` | `GET /api/governance/motions` | `governance:read` (troop) |
| `governance_get_motion` | `GET /api/governance/motion/{id}` | `governance:read` (troop) |
| `governance_create_motion` | `POST /api/governance/motion` | `governance:propose` (troop) |
| `calendar_list_events` | `GET /api/calendar/events` | `calendar:read` (object scope) |
| `calendar_get_event` | `GET /api/calendar/event/{id}` | `calendar:read` (object scope) |
| `calendar_create_event` | `POST /api/calendar/event` | `calendar:create` (object scope) |

Permissions this plugin defines: `mcp:connect`, `mcp:invoke`, `mcp:audit`.

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
