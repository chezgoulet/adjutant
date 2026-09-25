//! # adjutant-mcp — the permissions-aware MCP server (SPEC §7.10)
//!
//! > **Purpose:** Permissions-aware MCP server for Hermes agent integration.
//! > **Responsibilities:** Expose Adjutant functionality as MCP tools; filter
//! > tools based on the authenticated user's permissions; check permissions on
//! > every tool invocation; execute through the same plugin API as the UI; log
//! > all MCP invocations for audit.
//!
//! ## One permission system, one execution path
//!
//! An MCP tool *is* an Adjutant API call. Each tool in the catalogue names the
//! HTTP method, the path, the arguments, and **the permission the route itself
//! requires**; the plugin checks that permission against the caller's scoped
//! grants before it runs anything, and then performs the call with the caller's
//! own credentials forwarded (see [`forward_headers`]). The core's route gate
//! checks the same permission a second time on the forwarded request, so the
//! agent path and the UI path meet at exactly one authorization decision.
//!
//! That structure is what makes privilege escalation impossible rather than
//! merely discouraged:
//!
//! * the plugin holds **no credentials of its own** — it never mints, upgrades,
//!   or substitutes an identity, it only relays the caller's;
//! * every tool declares the permission its route requires, so a caller can only
//!   reach what their role and scope already reach through the UI;
//! * tools the caller cannot use are **omitted** from `GET /api/mcp/tools`, and a
//!   hidden tool's invocation is refused (403) and logged as `denied`;
//! * an MCP *connection* is an audit/session artifact, not a credential: a
//!   connection minted by user A can never be used by user B
//!   ([`McpPlugin`]'s connect route), and `mcp.invocations.user_id` is always the
//!   session's user.
//!
//! A tool whose owning plugin is not installed is invisible for free: the
//! permission it requires does not exist in `core.permissions`, so no role holds
//! it and the tool filters out. (The `calendar` tools below therefore appear the
//! moment the calendar plugin is installed and its permissions are granted.)
//!
//! ## Interface
//!
//! | Method | Path | Permission | Purpose |
//! |---|---|---|---|
//! | POST | `/api/mcp/connect` | `mcp:connect` | Open an MCP session; mint a connection token |
//! | GET | `/api/mcp/tools` | `mcp:connect` | The tools **this** caller may use, with JSON Schemas |
//! | POST | `/api/mcp/invoke` | `mcp:invoke` | Invoke one tool (re-checks the tool's own permission) |
//! | GET | `/api/mcp/invocations` | `mcp:connect` | The audit trail: own rows, or all rows with `mcp:audit` |
//!
//! The surface is HTTP + JSON. `POST /api/mcp/invoke` is the `tools/call`
//! analogue and `GET /api/mcp/tools` the `tools/list` analogue; a JSON-RPC facade
//! over these two (for a stdio/SSE transport) is a client concern, which is why
//! the routes are named for what they do rather than for a protocol revision.
//!
//! ## Schema (`mcp`)
//!
//! `mcp.connections` — one row per MCP session: who opened it, which client, the
//! **hash** of its token (never the token), when it was last used, how many
//! times, and when it expires or was revoked.
//!
//! `mcp.invocations` — the append-only audit trail: user, connection, tool,
//! arguments, the downstream request that was made, status
//! (`ok` / `denied` / `error`), HTTP status, result, error, and duration.
//! `denied` is recorded as carefully as `ok`: a refused attempt is the most
//! security-relevant row in the table.
//!
//! ## Configuration
//!
//! `core.plugins.config` for the `mcp` plugin:
//!
//! ```json
//! {
//!   "base_url": "http://127.0.0.1:8787",
//!   "connection_ttl_hours": 24,
//!   "max_result_bytes": 65536,
//!   "tools": {
//!     "disable": ["calendar_create_event"],
//!     "override": { "calendar_list_events": { "path": "/api/calendar/events", "permission": "calendar:read" } },
//!     "add": [
//!       {
//!         "name": "finance_balance", "description": "The troop's current balance",
//!         "permission": "finance:read", "scope": "troop", "method": "GET",
//!         "path": "/api/finance/balance",
//!         "params": [ { "name": "fund", "in": "query", "type": "string", "required": false } ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! `base_url` is the address of *this* Adjutant instance — the plugin calls the
//! API over the core's mediated HTTP client, because one plugin may not reach
//! into another plugin's routes or schema in process (SPEC §5.2a). It defaults
//! to the core's default bind (`http://127.0.0.1:8787`). `override` / `add` /
//! `disable` exist so an operator can retarget a tool whose owning plugin named
//! its route differently — the catalogue is data, not a hard-coded map.
//!
//! ## Deployment notes
//!
//! * **The core must be reachable at `base_url`.** The plugin relays to the
//!   public API, so `base_url` must be an address this process can dial (the
//!   default is the core's own bind). Behind a reverse proxy, use the internal
//!   address, not the public hostname.
//! * **Dev-header identities do not work end to end.** `x-dev-user`/`x-dev-role`
//!   are deliberately *not* forwarded: the dev stub is not a credential, and
//!   forwarding it would reintroduce exactly the spoofable-header hole SPEC §7.1
//!   closes. In that mode the MCP pre-check passes but the downstream route
//!   answers `401` — give Hermes a real session (cookie or `Authorization`).
//! * **Grant the `mcp:*` permissions.** Like every other plugin, nothing is
//!   reachable until a role holds `mcp:connect` / `mcp:invoke` (the core grants
//!   `chief` every permission at boot); the tools a member then sees are still
//!   filtered by the permissions their own role and scope already carry.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use adjutant_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The protocol revision advertised when the client asks for none (or for one
/// this server does not implement).
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Revisions this plugin will echo back. Kept explicit so claiming support is a
/// decision rather than a passthrough of whatever the client sent.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2024-11-05", "2025-06-18"];

/// The core's default bind (SPEC §11.1 / `ADJUTANT_BIND` default) — the address
/// this plugin calls when `base_url` is not configured.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8787";

/// Default MCP connection lifetime. Shorter than a login session on purpose: an
/// agent session that is forgotten about should stop being usable.
pub const DEFAULT_CONNECTION_TTL_HOURS: i64 = 24;

/// Largest downstream body retained in a result (and in `mcp.invocations`).
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024;

/// Token entropy: 32 bytes = 256 bits, hex-encoded (matching the auth plugin's
/// session tokens).
const TOKEN_BYTES: usize = 32;

/// Page size ceiling for `GET /api/mcp/invocations`.
const MAX_INVOCATION_PAGE: i64 = 200;

/// The three permissions this plugin defines.
pub const PERM_CONNECT: &str = "mcp:connect";
pub const PERM_INVOKE: &str = "mcp:invoke";
pub const PERM_AUDIT: &str = "mcp:audit";

// ---------------------------------------------------------------------------
// The tool catalogue
// ---------------------------------------------------------------------------

/// The scope at which a tool's permission must be held.
///
/// This mirrors the core route gate: `Troop` is an ordinary protected route (a
/// grant covering troop), `Any` is the `*_protected_any_scope` shape where the
/// downstream handler decides which object the caller may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolScope {
    Troop,
    Any,
}

impl ToolScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolScope::Troop => "troop",
            ToolScope::Any => "any",
        }
    }
}

/// Where an argument goes in the downstream request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamLocation {
    /// Substituted into a `{name}` capture in the tool's `path`.
    Path,
    /// Appended to the query string.
    Query,
    /// Sent as a JSON body field.
    Body,
}

/// The JSON type an argument accepts (drives the generated input schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    StringArray,
}

impl ParamType {
    /// The JSON Schema type name — and the JSON type an argument must have when
    /// the invocation is built.
    pub fn json_type(&self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Integer | ParamType::Number => "number",
            ParamType::Boolean => "boolean",
            ParamType::Object => "object",
            ParamType::StringArray => "array",
        }
    }
}

/// One argument of a tool — the single source of both the input schema and the
/// downstream request builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(rename = "in")]
    pub location: ParamLocation,
    #[serde(rename = "type", default = "default_param_type")]
    pub param_type: ParamType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    /// Permitted values, rendered as JSON Schema `enum` (used for the closed
    /// vocabularies the API itself validates, e.g. mission categories).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
}

fn default_param_type() -> ParamType {
    ParamType::String
}

impl ToolParam {
    fn new(
        name: &str,
        location: ParamLocation,
        param_type: ParamType,
        description: &str,
        required: bool,
    ) -> Self {
        Self {
            name: name.into(),
            location,
            param_type,
            description: description.into(),
            required,
            allowed: Vec::new(),
        }
    }

    fn allowed(mut self, values: &[&str]) -> Self {
        self.allowed = values.iter().map(|v| v.to_string()).collect();
        self
    }
}

/// One MCP tool: a name, a description, the permission the underlying route
/// requires, and the request to make once the permission holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// The permission the tool's route requires (`missions:read`, …).
    pub permission: String,
    pub scope: ToolScope,
    /// `GET` / `POST`.
    pub method: String,
    /// The API path, with `{arg}` captures for [`ParamLocation::Path`] params.
    pub path: String,
    #[serde(default)]
    pub params: Vec<ToolParam>,
}

impl ToolSpec {
    /// The JSON Schema advertised for this tool's arguments.
    ///
    /// `additionalProperties: false` is not decoration: the invoke handler
    /// refuses arguments the tool does not declare, so the schema and the
    /// behaviour cannot drift.
    pub fn input_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for p in &self.params {
            let mut schema = Map::new();
            schema.insert("type".into(), json!(p.param_type.json_type()));
            if p.param_type == ParamType::StringArray {
                schema.insert("items".into(), json!({ "type": "string" }));
            }
            if !p.description.is_empty() {
                schema.insert("description".into(), json!(p.description));
            }
            if !p.allowed.is_empty() {
                schema.insert("enum".into(), json!(p.allowed));
            }
            properties.insert(p.name.clone(), Value::Object(schema));
            if p.required {
                required.push(Value::String(p.name.clone()));
            }
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    /// The catalogue entry as an MCP `tools/list` item.
    pub fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema(),
            "requiredPermission": self.permission,
            "scope": self.scope.as_str(),
        })
    }

    /// Structural checks a tool must pass to be usable. Returns the reason it is
    /// not, so an operator's `tools.add` entry can be reported instead of
    /// silently ignored.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("a tool needs a name".into());
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "tool name {:?} must be lowercase [a-z0-9_] (the name Hermes sees)",
                self.name
            ));
        }
        if self.permission.trim().is_empty() || !self.permission.contains(':') {
            return Err(format!(
                "tool {:?} must name a permission as `plugin:action`, got {:?}",
                self.name, self.permission
            ));
        }
        let method = self.method.to_ascii_uppercase();
        if method != "GET" && method != "POST" {
            return Err(format!(
                "tool {:?} must use GET or POST, got {:?}",
                self.name, self.method
            ));
        }
        if !self.path.starts_with("/api/") {
            return Err(format!(
                "tool {:?} path {:?} must be an API path (/api/…)",
                self.name, self.path
            ));
        }
        for p in &self.params {
            let placeholder = format!("{{{}}}", p.name);
            if p.location == ParamLocation::Path {
                if !self.path.contains(&placeholder) {
                    return Err(format!(
                        "tool {:?} declares path argument {:?} but its path {:?} has no \
                         {placeholder} capture",
                        self.name, p.name, self.path
                    ));
                }
                if !p.required {
                    return Err(format!(
                        "tool {:?} path argument {:?} must be required — a capture with no value \
                         cannot be filled",
                        self.name, p.name
                    ));
                }
            } else if self.path.contains(&placeholder) {
                return Err(format!(
                    "tool {:?} path contains {placeholder} but argument {:?} is not a path argument",
                    self.name, p.name
                ));
            }
            if p.location == ParamLocation::Query && p.param_type == ParamType::Object {
                return Err(format!(
                    "tool {:?} query argument {:?} cannot be an object — query values are scalars",
                    self.name, p.name
                ));
            }
        }
        // Every capture must be an argument: a path with an unfillable `{id}` is
        // a request that can never be built, which belongs at load time rather
        // than in a caller's 500.
        let mut rest = self.path.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                return Err(format!(
                    "tool {:?} path {:?} has an unclosed capture",
                    self.name, self.path
                ));
            };
            let capture = &rest[open + 1..open + close];
            if capture.is_empty() || !self.params.iter().any(|p| p.name == capture) {
                return Err(format!(
                    "tool {:?} path {:?} captures {{{capture}}} but no argument declares it",
                    self.name, self.path
                ));
            }
            rest = &rest[open + close + 1..];
        }
        Ok(())
    }
}

// helper constructors that keep the catalogue literal readable
fn arg(name: &str, description: &str, required: bool) -> ToolParam {
    ToolParam::new(name, ParamLocation::Query, ParamType::String, description, required)
}

fn int_arg(name: &str, description: &str, required: bool) -> ToolParam {
    ToolParam::new(name, ParamLocation::Query, ParamType::Integer, description, required)
}

fn path_arg(name: &str, description: &str) -> ToolParam {
    ToolParam::new(name, ParamLocation::Path, ParamType::Integer, description, true)
}

fn body(name: &str, param_type: ParamType, description: &str, required: bool) -> ToolParam {
    ToolParam::new(name, ParamLocation::Body, param_type, description, required)
}

/// The built-in catalogue: membership, missions, governance, and calendar
/// (SPEC §7.2, §7.3, §7.4, §7.7).
///
/// Paths, arguments, and permissions mirror the owning plugin's routes exactly —
/// a tool that disagreed with its route would be a second, weaker API, which is
/// the failure mode this plugin exists to avoid.
pub fn builtin_tools() -> Vec<ToolSpec> {
    vec![
        // --- membership (SPEC §7.2) ------------------------------------------
        ToolSpec {
            name: "membership_list_members".into(),
            description: "List the troop roster (optionally one patrol). Requires \
                          membership:read_all at troop scope — the same gate as the roster screen."
                .into(),
            permission: "membership:read_all".into(),
            scope: ToolScope::Troop,
            method: "GET".into(),
            path: "/api/membership/members".into(),
            params: vec![
                arg("patrol", "Only members in this patrol (by patrol name)", false),
                ToolParam::new(
                    "include_inactive",
                    ParamLocation::Query,
                    ParamType::Boolean,
                    "Include members marked inactive",
                    false,
                ),
            ],
        },
        ToolSpec {
            name: "membership_get_member".into(),
            description: "One member by id. Requires membership:read at some scope; the member route \
                          decides whether that reaches this member (own record, lodge, or troop)."
                .into(),
            permission: "membership:read".into(),
            scope: ToolScope::Any,
            method: "GET".into(),
            path: "/api/membership/member".into(),
            params: vec![int_arg("id", "The member id", true)],
        },
        // --- missions (SPEC §7.3) --------------------------------------------
        ToolSpec {
            name: "missions_list_missions".into(),
            description: "List missions the caller may see (own lodge, own proposals, or troop). \
                          Requires missions:read at some scope."
                .into(),
            permission: "missions:read".into(),
            scope: ToolScope::Any,
            method: "GET".into(),
            path: "/api/missions/missions".into(),
            params: vec![
                arg("stage", "Lifecycle stage filter", false)
                    .allowed(&["request", "review", "approval", "execution", "debrief", "report"]),
                arg("state", "State filter", false).allowed(&["open", "rejected", "completed"]),
                arg("category", "Category filter", false).allowed(&[
                    "service",
                    "conservation",
                    "expedition",
                    "training",
                    "community",
                    "ceremonial",
                    "other",
                ]),
                arg("lodge", "Lodge id filter", false),
                int_arg("limit", "Maximum rows (1–200, default 50)", false),
            ],
        },
        ToolSpec {
            name: "missions_get_mission".into(),
            description: "One mission with its milestones, mentorships, progress notes, stage log, and \
                          appeals. Requires missions:read at the mission's scope."
                .into(),
            permission: "missions:read".into(),
            scope: ToolScope::Any,
            method: "GET".into(),
            path: "/api/missions/mission/{id}".into(),
            params: vec![path_arg("id", "The mission id")],
        },
        ToolSpec {
            name: "missions_create_mission".into(),
            description: "Propose a mission (Accords Art 8, stage 1). Requires missions:create at the \
                          lodge the mission is proposed for."
                .into(),
            permission: "missions:create".into(),
            scope: ToolScope::Any,
            method: "POST".into(),
            path: "/api/missions/mission".into(),
            params: vec![
                body("title", ParamType::String, "Mission title", true),
                body("purpose", ParamType::String, "Why this mission exists", true),
                body("objectives", ParamType::String, "What it will do, concretely", true),
                body(
                    "expected_impact",
                    ParamType::String,
                    "The change it expects to make — the raw material of the Impact Report",
                    true,
                ),
                body("category", ParamType::String, "Proposal category", false).allowed(&[
                    "service",
                    "conservation",
                    "expedition",
                    "training",
                    "community",
                    "ceremonial",
                    "other",
                ]),
                body("lodge_id", ParamType::String, "The lodge the mission is proposed for", false),
                body("lodge_name", ParamType::String, "That lodge's display name", false),
                body(
                    "tags",
                    ParamType::StringArray,
                    "Expertise/interest tags (used by mentor matching)",
                    false,
                ),
                body("location", ParamType::String, "Where it happens", false),
                body("starts_on", ParamType::String, "Start date, ISO YYYY-MM-DD", false),
                body("ends_on", ParamType::String, "End date, ISO YYYY-MM-DD", false),
                body("resources_needed", ParamType::String, "Resources required", false),
                body("risk_notes", ParamType::String, "Risk assessment", false),
                body(
                    "youth_safety_notes",
                    ParamType::String,
                    "Mission-specific youth-safety answer",
                    false,
                ),
                body("participant_count", ParamType::Integer, "Expected participants", false),
            ],
        },
        // --- governance (SPEC §7.4) ------------------------------------------
        ToolSpec {
            name: "governance_list_motions".into(),
            description: "List motions. Requires governance:read at troop scope — governance is \
                          troop-wide deliberation, not a lodge-scoped record."
                .into(),
            permission: "governance:read".into(),
            scope: ToolScope::Troop,
            method: "GET".into(),
            path: "/api/governance/motions".into(),
            params: vec![
                int_arg("meeting", "Only motions attached to this meeting id", false),
                arg("body", "Body filter (congress, troop_council, lodge)", false),
                arg("stage", "Stage filter (proposed, debate, voting, decided, implemented)", false),
                arg("result", "Result filter (passed, failed)", false),
                arg("category", "Category filter", false),
                int_arg("limit", "Maximum rows (1–200, default 50)", false),
            ],
        },
        ToolSpec {
            name: "governance_get_motion".into(),
            description: "One motion with its votes, amendments, current tally, and quorum state. \
                          Requires governance:read at troop scope."
                .into(),
            permission: "governance:read".into(),
            scope: ToolScope::Troop,
            method: "GET".into(),
            path: "/api/governance/motion/{id}".into(),
            params: vec![path_arg("id", "The motion id")],
        },
        ToolSpec {
            name: "governance_create_motion".into(),
            description: "Propose a motion (Accords Art 5). Requires governance:propose; the motion \
                          still needs its second and its vote."
                .into(),
            permission: "governance:propose".into(),
            scope: ToolScope::Troop,
            method: "POST".into(),
            path: "/api/governance/motion".into(),
            params: vec![
                body("title", ParamType::String, "Motion title", true),
                body("text", ParamType::String, "The full motion text", true),
                body(
                    "body",
                    ParamType::String,
                    "Which body decides it (congress, troop_council, lodge)",
                    true,
                ),
                body("meeting_id", ParamType::Integer, "The meeting it is brought at", false),
                body("lodge_id", ParamType::String, "Lodge id, for a lodge motion", false),
                body("category", ParamType::String, "Category (general, finance, policy, …)", false),
                body(
                    "threshold",
                    ParamType::String,
                    "Pass threshold (simple_majority, two_thirds, unanimous)",
                    false,
                ),
                body("amends_accords", ParamType::Boolean, "Whether it amends the Accords", false),
            ],
        },
        // --- calendar (SPEC §7.7) ---------------------------------------------
        // Invisible until the calendar plugin is installed and its permissions
        // granted (an unheld permission filters the tool out).
        ToolSpec {
            name: "calendar_list_events".into(),
            description: "List calendar events the caller may see (troop-wide events, their lodges' \
                          events, and their own). Requires calendar:read at some scope."
                .into(),
            permission: "calendar:read".into(),
            scope: ToolScope::Any,
            method: "GET".into(),
            path: "/api/calendar/events".into(),
            params: vec![
                arg("status", "Status filter (scheduled, cancelled, or all)", false)
                    .allowed(&["scheduled", "cancelled", "all"]),
                arg("scope_type", "Only events at this scope level", false)
                    .allowed(&["troop", "lodge"]),
                arg("scope_id", "Only events at this scope id (a lodge id)", false),
                arg("body", "Only events belonging to this governing body", false)
                    .allowed(&["congress", "tc", "lodge", "committee"]),
                arg("category", "Category filter", false).allowed(&[
                    "meeting",
                    "training",
                    "service",
                    "mission",
                    "debrief",
                    "social",
                    "ceremony",
                    "camp",
                    "other",
                ]),
                arg("from", "Only events starting on or after this local timestamp", false),
                arg("to", "Only events starting on or before this local timestamp", false),
                int_arg("limit", "Maximum rows (1–200, default 50)", false),
            ],
        },
        ToolSpec {
            name: "calendar_get_event".into(),
            description: "One event with its RSVPs and quorum state. Requires calendar:read at some \
                          scope."
                .into(),
            permission: "calendar:read".into(),
            scope: ToolScope::Any,
            method: "GET".into(),
            path: "/api/calendar/event/{id}".into(),
            params: vec![path_arg("id", "The event id")],
        },
        ToolSpec {
            name: "calendar_create_event".into(),
            description: "Create a troop or lodge event. Requires calendar:create; a recurring \
                          event is one RRULE, not one row per occurrence."
                .into(),
            permission: "calendar:create".into(),
            scope: ToolScope::Any,
            method: "POST".into(),
            path: "/api/calendar/event".into(),
            params: vec![
                body("title", ParamType::String, "Event title", true),
                body(
                    "starts_at",
                    ParamType::String,
                    "Wall-clock start (YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS])",
                    true,
                ),
                body("ends_at", ParamType::String, "Wall-clock end, same shape", false),
                body("description", ParamType::String, "Details", false),
                body("category", ParamType::String, "Event category", false).allowed(&[
                    "meeting",
                    "training",
                    "service",
                    "mission",
                    "debrief",
                    "social",
                    "ceremony",
                    "camp",
                    "other",
                ]),
                body("scope_type", ParamType::String, "Who the event belongs to", false)
                    .allowed(&["troop", "lodge"]),
                body("scope_id", ParamType::String, "The lodge id, when scope_type is lodge", false),
                body("body", ParamType::String, "The governing body it serves, if any", false)
                    .allowed(&["congress", "tc", "lodge", "committee"]),
                body("meeting_id", ParamType::Integer, "Governance meeting id, when this is that meeting", false),
                body("location", ParamType::String, "Where it happens", false),
                body("timezone", ParamType::String, "IANA timezone for the wall-clock times", false),
                body("all_day", ParamType::Boolean, "Whether it is an all-day event", false),
                body("rrule", ParamType::String, "Recurrence rule (iCal RRULE)", false),
                body("exdates", ParamType::String, "Recurrence exceptions (iCal EXDATE list)", false),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The plugin's `core.plugins.config` block.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    /// Address of this Adjutant instance (default [`DEFAULT_BASE_URL`]).
    #[serde(default)]
    pub base_url: Option<String>,
    /// How long a minted MCP connection stays usable.
    #[serde(default)]
    pub connection_ttl_hours: Option<i64>,
    /// Cap on bytes of a downstream body kept in the result and the audit row.
    #[serde(default)]
    pub max_result_bytes: Option<usize>,
    #[serde(default)]
    pub tools: ToolConfig,
}

/// Catalogue adjustments — data, so a route-renaming plugin does not require a
/// rebuild of this one.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ToolConfig {
    /// Tool names to leave out of the catalogue entirely.
    #[serde(default)]
    pub disable: Vec<String>,
    /// Tools to add (a plugin this catalogue does not cover yet).
    #[serde(default)]
    pub add: Vec<ToolSpec>,
    /// Per-tool replacements, keyed by tool name.
    #[serde(default, rename = "override")]
    pub overrides: HashMap<String, ToolOverride>,
}

/// A partial replacement of a built-in tool.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ToolOverride {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `false` removes the tool (same effect as `disable`, per-tool).
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl McpConfig {
    /// Parse the plugin config. A malformed block is reported through `warnings`
    /// and the defaults are used — a typo in an operator's config must not make
    /// the plugin refuse to load.
    pub fn from_value(value: &Value) -> (Self, Vec<String>) {
        if value.is_null() {
            return (Self::default(), Vec::new());
        }
        match serde_json::from_value::<McpConfig>(value.clone()) {
            Ok(cfg) => (cfg, Vec::new()),
            Err(e) => (
                Self::default(),
                vec![format!("ignoring unusable mcp config ({e}); using defaults")],
            ),
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string()
    }

    pub fn connection_ttl_hours(&self) -> i64 {
        self.connection_ttl_hours
            .filter(|h| *h > 0 && *h <= 24 * 90)
            .unwrap_or(DEFAULT_CONNECTION_TTL_HOURS)
    }

    pub fn max_result_bytes(&self) -> usize {
        self.max_result_bytes
            .filter(|b| *b >= 256)
            .unwrap_or(DEFAULT_MAX_RESULT_BYTES)
    }
}

/// The tools this plugin serves: the built-ins, adjusted by config.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    tools: Vec<ToolSpec>,
    /// Config problems worth telling an operator about (a bad `add` entry is
    /// dropped, not obeyed silently, and shows up in `GET /api/mcp/tools`).
    warnings: Vec<String>,
}

impl Catalogue {
    /// The built-ins with no config applied.
    pub fn builtin() -> Self {
        Self { tools: builtin_tools(), warnings: Vec::new() }
    }

    /// Built-ins → disable → override → add, dropping anything structurally
    /// invalid with a warning.
    pub fn from_config(cfg: &McpConfig, mut warnings: Vec<String>) -> Self {
        let mut tools: Vec<ToolSpec> = builtin_tools()
            .into_iter()
            .filter(|t| !cfg.tools.disable.iter().any(|d| d == &t.name))
            .collect();

        let mut switched_off: Vec<String> = Vec::new();
        for (name, change) in &cfg.tools.overrides {
            let Some(tool) = tools.iter_mut().find(|t| &t.name == name) else {
                warnings.push(format!("tools.override names unknown tool {name:?}"));
                continue;
            };
            if let Some(path) = &change.path {
                tool.path = path.clone();
            }
            if let Some(method) = &change.method {
                tool.method = method.to_ascii_uppercase();
            }
            if let Some(permission) = &change.permission {
                tool.permission = permission.clone();
            }
            if let Some(description) = &change.description {
                tool.description = description.clone();
            }
            if change.enabled == Some(false) {
                switched_off.push(name.clone());
            }
        }
        for name in &switched_off {
            warnings.push(format!("tool {name:?} disabled by config"));
        }
        tools.retain(|t| !switched_off.contains(&t.name));

        for extra in &cfg.tools.add {
            if tools.iter().any(|t| t.name == extra.name) {
                warnings.push(format!("tools.add redefines tool {:?}; ignored", extra.name));
                continue;
            }
            tools.push(extra.clone());
        }

        let mut invalid = Vec::new();
        tools.retain(|t| match t.validate() {
            Ok(()) => true,
            Err(reason) => {
                invalid.push(format!("tool {:?} dropped: {reason}", t.name));
                false
            }
        });
        warnings.extend(invalid);

        Self { tools, warnings }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn find(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// The distinct permissions the catalogue asks for, in first-appearance
    /// order — the check list, one entry per permission rather than per tool.
    pub fn permissions(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for t in &self.tools {
            if !seen.contains(&t.permission) {
                seen.push(t.permission.clone());
            }
        }
        seen
    }
}

// ---------------------------------------------------------------------------
// Building the downstream request (pure — unit-tested below)
// ---------------------------------------------------------------------------

/// The API call a tool invocation turns into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downstream {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl Downstream {
    /// The audit form stored in `mcp.invocations.downstream`.
    pub fn summary(&self) -> Value {
        json!({
            "method": self.method,
            "path": self.path,
            "query": self.query.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "has_body": self.body.is_some(),
        })
    }
}

/// Percent-encode everything outside the unreserved set, so a caller-supplied
/// value can never add a path segment, a query separator, or a capture.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `base_url` + path + encoded query.
pub fn build_url(base_url: &str, path: &str, query: &[(String, String)]) -> String {
    let mut url = format!("{}{}", base_url.trim_end_matches('/'), path);
    if !query.is_empty() {
        let pairs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect();
        url.push('?');
        url.push_str(&pairs.join("&"));
    }
    url
}

/// An argument value as the scalar text a query parameter or capture needs.
fn scalar_text(name: &str, value: &Value) -> Result<String, SdkError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Array(items) if items.iter().all(Value::is_string) => Ok(items
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(",")),
        other => Err(SdkError::BadRequest(format!(
            "argument {name:?} must be a string, number, boolean, or array of strings, got {other}"
        ))),
    }
}

/// Turn tool arguments into the API request the tool describes.
///
/// Validation is strict in both directions: a missing required argument and an
/// argument the tool does not declare are both `400`, so a caller cannot smuggle
/// a field past the tool's published schema into the API.
pub fn build_downstream(tool: &ToolSpec, args: &Map<String, Value>) -> Result<Downstream, SdkError> {
    for key in args.keys() {
        if !tool.params.iter().any(|p| &p.name == key) {
            let accepted: Vec<&str> = tool.params.iter().map(|p| p.name.as_str()).collect();
            return Err(SdkError::BadRequest(format!(
                "unknown argument {key:?} for tool {:?}; accepted arguments: {}",
                tool.name,
                if accepted.is_empty() { "(none)".to_string() } else { accepted.join(", ") }
            )));
        }
    }

    let mut path = tool.path.clone();
    let mut query: Vec<(String, String)> = Vec::new();
    let mut body = Map::new();

    for param in &tool.params {
        let value = match args.get(&param.name) {
            Some(v) if !v.is_null() => v,
            // An explicit null is "absent", which is only fine when optional.
            _ if param.required => {
                return Err(SdkError::BadRequest(format!(
                    "argument {:?} is required for tool {:?}",
                    param.name, tool.name
                )))
            }
            _ => continue,
        };
        match param.location {
            ParamLocation::Path => {
                let placeholder = format!("{{{}}}", param.name);
                let encoded = percent_encode(&scalar_text(&param.name, value)?);
                path = path.replace(&placeholder, &encoded);
            }
            ParamLocation::Query => {
                query.push((param.name.clone(), scalar_text(&param.name, value)?));
            }
            ParamLocation::Body => {
                body.insert(param.name.clone(), value.clone());
            }
        }
    }

    // A capture left over means the catalogue is wrong, not the caller — report
    // it as a plugin bug, the way `PluginRequest::int_param` does.
    if let Some(open) = path.find('{') {
        return Err(SdkError::Internal(format!(
            "tool {:?} left the capture {} unfilled — its declaration and its arguments disagree",
            tool.name,
            &path[open..]
        )));
    }

    // A typed body argument must really be that type: the downstream handler
    // deserializes into `Option<i64>`/`bool`, so a string would be a 400 from the
    // API rather than from here.
    for param in &tool.params {
        if param.location != ParamLocation::Body {
            continue;
        }
        let Some(value) = body.get(&param.name) else { continue };
        let ok = match param.param_type {
            ParamType::Integer | ParamType::Number => value.is_number(),
            ParamType::Boolean => value.is_boolean(),
            ParamType::String => value.is_string(),
            ParamType::StringArray => {
                value.is_array() && value.as_array().is_some_and(|a| a.iter().all(Value::is_string))
            }
            ParamType::Object => value.is_object(),
        };
        if !ok {
            return Err(SdkError::BadRequest(format!(
                "argument {:?} for tool {:?} must be {}, got {value}",
                param.name,
                tool.name,
                param.param_type.json_type()
            )));
        }
    }

    let method = tool.method.to_ascii_uppercase();
    let body = if method == "GET" {
        None
    } else {
        Some(Value::Object(body))
    };

    Ok(Downstream { method, path, query, body })
}

/// The headers relayed downstream: **the caller's own credentials**, and nothing
/// else.
///
/// The plugin never mints a credential for the API call, which is precisely why
/// an MCP invocation cannot exceed the calling user's authority. MCP-specific
/// headers are deliberately not forwarded — the agent's identity travels as the
/// session it already has.
pub fn forward_headers(req: &PluginRequest) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in ["authorization", "cookie"] {
        let value = req.headers.get(name).or_else(|| {
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        });
        if let Some(value) = value {
            if !value.trim().is_empty() {
                out.push((name.to_string(), value.clone()));
            }
        }
    }
    out
}

/// The MCP connection token a request carries: the body value first, then the
/// `mcp-session-id` header an MCP client sends naturally.
pub fn connection_token(req: &PluginRequest, from_body: Option<&str>) -> Option<String> {
    from_body
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .or_else(|| {
            req.headers
                .get("mcp-session-id")
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(String::from)
        })
}

/// Decode a downstream body for the result, capping what is kept.
pub fn decode_body(content_type: Option<&str>, bytes: &[u8], max_bytes: usize) -> Value {
    let text = String::from_utf8_lossy(bytes).to_string();
    if text.len() > max_bytes {
        let head: String = text.chars().take(max_bytes).collect();
        return json!({
            "truncated": true,
            "bytes": bytes.len(),
            "content_type": content_type.unwrap_or(""),
            "text": head,
        });
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => value,
        Err(_) => json!({
            "content_type": content_type.unwrap_or(""),
            "text": text,
        }),
    }
}

/// The message to show for a non-2xx downstream answer: the API's own `error`
/// when it sent one, else a plain statement of the status.
pub fn error_message(result: &Value, status: u16) -> String {
    match result.get("error").and_then(Value::as_str) {
        Some(message) if !message.is_empty() => message.to_string(),
        _ => format!("the Adjutant API answered {status}"),
    }
}

/// Mint a connection token: 32 random bytes, hex-encoded.
pub fn mint_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The hex SHA-256 of a token. Only the hash is stored — a leaked database dump
/// must not hand out usable MCP sessions.
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Negotiate the protocol revision: the client's if this server implements it.
pub fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    let Some(value) = requested.map(str::trim) else {
        return MCP_PROTOCOL_VERSION;
    };
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|supported| **supported == value)
        .copied()
        .unwrap_or(MCP_PROTOCOL_VERSION)
}

// ---------------------------------------------------------------------------
// Permission checks
// ---------------------------------------------------------------------------

/// Does `identity` hold `permission` at a scope the tool's route accepts?
///
/// `Troop` is the ordinary gate: a grant that covers troop. `Any` mirrors
/// `*_protected_any_scope`: the permission at *some* scope the caller holds
/// (troop first, then each scope in their grants) — and the downstream route
/// still decides which object they may touch, so this is a filter, not a grant.
async fn permission_holds(
    c: &PluginContext,
    identity: Option<&Identity>,
    permission: &str,
    scope: ToolScope,
) -> bool {
    let Some(identity) = identity else { return false };
    if scope == ToolScope::Troop {
        return c
            .permissions
            .has_in_scope(Some(identity), permission, &Scope::troop())
            .await;
    }
    let mut scopes: Vec<Scope> = vec![Scope::troop()];
    for grant in &identity.grants {
        if !scopes.contains(&grant.scope) {
            scopes.push(grant.scope.clone());
        }
    }
    for scope in scopes {
        if c.permissions
            .has_in_scope(Some(identity), permission, &scope)
            .await
        {
            return true;
        }
    }
    false
}

/// The tools the caller may use, in catalogue order.
///
/// Sharing one implementation between the listing and the invoke pre-check is
/// the point: a tool cannot be advertised to someone who then cannot call it, or
/// refused to someone who was told it was available.
async fn visible_tools<'a>(
    c: &PluginContext,
    identity: Option<&Identity>,
    catalogue: &'a Catalogue,
) -> Vec<&'a ToolSpec> {
    // One check per (permission, scope) pair, not per tool.
    let mut cache: HashMap<(String, ToolScope), bool> = HashMap::new();
    let mut visible = Vec::new();
    for tool in catalogue.tools() {
        let key = (tool.permission.clone(), tool.scope);
        let ok = match cache.get(&key) {
            Some(ok) => *ok,
            None => {
                let ok = permission_holds(c, identity, &tool.permission, tool.scope).await;
                cache.insert(key, ok);
                ok
            }
        };
        if ok {
            visible.push(tool);
        }
    }
    visible
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct McpPlugin {
    ctx: OnceLock<PluginContext>,
    catalogue: OnceLock<Catalogue>,
    config: OnceLock<McpConfig>,
}

impl McpPlugin {
    pub fn new() -> Self {
        Self {
            ctx: OnceLock::new(),
            catalogue: OnceLock::new(),
            config: OnceLock::new(),
        }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }

    /// The configured catalogue. Panics only if `routes()` is called before
    /// `init()`, which the core's lifecycle guarantees cannot happen.
    pub fn catalogue(&self) -> &Catalogue {
        self.catalogue
            .get()
            .expect("core must call init() before routes()")
    }

    /// The parsed config, defaulted when absent or unusable.
    pub fn config(&self) -> &McpConfig {
        self.config
            .get()
            .expect("core must call init() before routes()")
    }
}

impl Default for McpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// --- connections ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ConnectBody {
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    client_version: Option<String>,
    #[serde(default, alias = "protocolVersion")]
    protocol_version: Option<String>,
    #[serde(default)]
    capabilities: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct InvokeBody {
    #[serde(alias = "name")]
    tool: String,
    #[serde(default, alias = "params")]
    arguments: Value,
    #[serde(default)]
    connection_id: Option<i64>,
    #[serde(default)]
    connection_token: Option<String>,
}

/// A live connection row (only the fields the plugin acts on).
#[derive(Debug, Clone)]
struct Connection {
    id: i64,
}

/// Resolve the connection an invocation names, refusing one that belongs to
/// somebody else. `Ok(None)` means the request named no connection, which is
/// allowed: the session identity is the authority either way.
async fn resolve_connection(
    c: &PluginContext,
    identity: &Identity,
    connection_id: Option<i64>,
    token: Option<String>,
) -> Result<Option<Connection>, SdkError> {
    if connection_id.is_some() && token.is_some() {
        return Err(SdkError::BadRequest(
            "give either connection_id or a connection token, not both".into(),
        ));
    }
    let (sql, params) = match (connection_id, token.as_deref()) {
        (Some(id), None) => (
            format!(
                "SELECT id, user_id FROM {} \
                 WHERE id = $1 AND revoked_at IS NULL AND expires_at > now()",
                c.db.table("connections")
            ),
            vec![SqlValue::Int(id)],
        ),
        (None, Some(token)) => (
            format!(
                "SELECT id, user_id FROM {} \
                 WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
                c.db.table("connections")
            ),
            vec![SqlValue::Text(hash_token(token))],
        ),
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => unreachable!("checked above"),
    };
    let Some(row) = c.db.query_one(sql, params).await? else {
        return Err(SdkError::Forbidden(
            "no live MCP connection matches this request (it may have expired or been revoked)"
                .into(),
        ));
    };
    let owner = row["user_id"].as_str().unwrap_or_default();
    if owner != identity.user_id {
        return Err(SdkError::Forbidden(
            "that MCP connection belongs to another user; a connection never carries authority \
             beyond the session that opened it"
                .into(),
        ));
    }
    Ok(Some(Connection { id: row["id"].as_i64().unwrap_or_default() }))
}

// --- invocation logging -----------------------------------------------------

/// One row of `mcp.invocations`. Every field is explicit: this table is the
/// audit trail SPEC §7.10 asks for, and a defaulted column is a fact nobody
/// recorded.
struct InvocationRow<'a> {
    connection_id: Option<i64>,
    user_id: &'a str,
    tool: &'a str,
    arguments: &'a Value,
    downstream: Option<&'a Downstream>,
    status: &'a str,
    http_status: Option<u16>,
    result: Option<&'a Value>,
    error: Option<&'a str>,
    duration_ms: i64,
}

/// Write the invocation; returns the new row id.
///
/// The log is best-effort by design: a refusal must still refuse, and a tool
/// result must still reach the caller, even if the audit insert fails. The
/// failure is reported (stderr and `log_error` in the response) rather than
/// swallowed.
async fn record_invocation(
    c: &PluginContext,
    row: InvocationRow<'_>,
) -> Result<Option<i64>, SdkError> {
    let downstream = row
        .downstream
        .map(|d| serde_json::to_string(&d.summary()).unwrap_or_else(|_| "{}".into()));
    let arguments = serde_json::to_string(row.arguments).unwrap_or_else(|_| "{}".into());
    let result = row
        .result
        .map(|r| serde_json::to_string(r).unwrap_or_else(|_| "null".into()));
    let inserted = c
        .db
        .query(
            format!(
                "INSERT INTO {} \
                   (connection_id, user_id, tool, arguments, downstream, status, http_status, \
                    result, error, duration_ms) \
                 VALUES ($1, $2, $3, $4::jsonb, $5::jsonb, $6, $7, $8::jsonb, $9, $10) \
                 RETURNING id",
                c.db.table("invocations")
            ),
            vec![
                row.connection_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                SqlValue::Text(row.user_id.to_string()),
                SqlValue::Text(row.tool.to_string()),
                SqlValue::Json(arguments),
                downstream.map(SqlValue::Json).unwrap_or(SqlValue::Null),
                SqlValue::Text(row.status.to_string()),
                row.http_status
                    .map(|s| SqlValue::Int(s as i64))
                    .unwrap_or(SqlValue::NullInt),
                result.map(SqlValue::Json).unwrap_or(SqlValue::Null),
                row.error.map(|e| SqlValue::Text(e.to_string())).unwrap_or(SqlValue::Null),
                SqlValue::Int(row.duration_ms),
            ],
        )
        .await?;
    Ok(inserted.first().and_then(|r| r["id"].as_i64()))
}

/// Split an invocation-log result into `(id, log_error)` so a failed audit write
/// is reported in the response instead of vanishing.
fn split_log(result: Result<Option<i64>, SdkError>) -> (Option<i64>, Option<String>) {
    match result {
        Ok(id) => (id, None),
        Err(e) => {
            let message = format!("invocation log write failed: {e}");
            eprintln!("[adjutant-mcp] {message}");
            (None, Some(message))
        }
    }
}

/// The response envelope every invocation answers with, so a client can render
/// (and correlate) an invocation without guessing at the API's own shape.
struct Outcome<'a> {
    status: &'a str,
    http_status: Option<u16>,
    result: Option<&'a Value>,
    error: Option<&'a str>,
    duration_ms: i64,
    invocation_id: Option<i64>,
    log_error: Option<String>,
}

fn envelope(tool: &ToolSpec, outcome: Outcome<'_>) -> Value {
    let mut out = json!({
        "tool": tool.name,
        "status": outcome.status,
        "requiredPermission": tool.permission,
        "duration_ms": outcome.duration_ms,
    });
    let map = out.as_object_mut().expect("json! built an object");
    if let Some(code) = outcome.http_status {
        map.insert("http_status".into(), json!(code));
    }
    if let Some(result) = outcome.result {
        map.insert("result".into(), result.clone());
    }
    if let Some(error) = outcome.error {
        map.insert("error".into(), json!(error));
    }
    if let Some(id) = outcome.invocation_id {
        map.insert("invocation_id".into(), json!(id));
    }
    if let Some(log_error) = outcome.log_error {
        map.insert("log_error".into(), json!(log_error));
    }
    out
}

/// Record the audit-log entry for an invocation. Separate from
/// `mcp.invocations` on purpose: the hash-chained `core.audit_log` is the
/// tamper-evident record, the plugin table is the detailed one.
async fn audit_invocation(
    c: &PluginContext,
    identity: &Identity,
    action: &str,
    tool_name: &str,
    details: Value,
) {
    if let Err(e) = c
        .audit
        .log(Some(identity), action, "mcp_tool", tool_name, details)
        .await
    {
        eprintln!("[adjutant-mcp] audit log for {tool_name} failed: {e}");
    }
}

/// Record a refused invocation (denied or malformed) and answer with the
/// envelope. Every way of refusing goes through here, so no refusal can skip the
/// audit trail.
#[allow(clippy::too_many_arguments)]
async fn refuse(
    c: &PluginContext,
    identity: &Identity,
    tool: &ToolSpec,
    args: &Value,
    connection_id: Option<i64>,
    started: Instant,
    status_code: u16,
    message: String,
    audit_action: &str,
) -> Result<PluginResponse, SdkError> {
    let duration_ms = started.elapsed().as_millis() as i64;
    let log = record_invocation(
        c,
        InvocationRow {
            connection_id,
            user_id: &identity.user_id,
            tool: &tool.name,
            arguments: args,
            downstream: None,
            status: "denied",
            http_status: None,
            result: None,
            error: Some(&message),
            duration_ms,
        },
    )
    .await;
    let (invocation_id, log_error) = split_log(log);
    audit_invocation(
        c,
        identity,
        audit_action,
        &tool.name,
        json!({
            "required_permission": tool.permission,
            "scope": tool.scope.as_str(),
            "reason": message,
            "invocation_id": invocation_id,
        }),
    )
    .await;
    PluginResponse::json(
        status_code,
        &envelope(
            tool,
            Outcome {
                status: "denied",
                http_status: None,
                result: None,
                error: Some(&message),
                duration_ms,
                invocation_id,
                log_error,
            },
        ),
    )
}

/// The MCP server block every handshake response carries.
fn server_info() -> Value {
    json!({
        "name": "adjutant-mcp",
        "title": "Adjutant",
        "version": env!("CARGO_PKG_VERSION"),
        "plugin": "mcp",
    })
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[async_trait]
impl AdjutantPlugin for McpPlugin {
    fn id(&self) -> &str {
        "mcp"
    }

    fn name(&self) -> &str {
        "Hermes MCP"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let (cfg, warnings) = McpConfig::from_value(&ctx.config);
        for warning in &warnings {
            eprintln!("[adjutant-mcp] {warning}");
        }
        let catalogue = Catalogue::from_config(&cfg, warnings);
        for warning in catalogue.warnings() {
            eprintln!("[adjutant-mcp] {warning}");
        }
        let _ = self.catalogue.set(catalogue);
        let _ = self.config.set(cfg);
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new(
                PERM_CONNECT,
                "Open an MCP connection and list the tools your account may use",
            ),
            Permission::new(
                PERM_INVOKE,
                "Invoke MCP tools (each tool still checks the permission its route requires)",
            ),
            Permission::new(PERM_AUDIT, "Read every MCP invocation, not only your own"),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "mcp_connections_and_invocations",
            "CREATE TABLE IF NOT EXISTS connections (\
                 id BIGSERIAL PRIMARY KEY, \
                 user_id TEXT NOT NULL, \
                 client_name TEXT NOT NULL DEFAULT '', \
                 client_version TEXT NOT NULL DEFAULT '', \
                 protocol_version TEXT NOT NULL DEFAULT '', \
                 token_hash TEXT NOT NULL UNIQUE, \
                 capabilities JSONB NOT NULL DEFAULT '{}', \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 last_used_at TIMESTAMPTZ, \
                 last_tool TEXT, \
                 invocation_count BIGINT NOT NULL DEFAULT 0, \
                 expires_at TIMESTAMPTZ NOT NULL, \
                 revoked_at TIMESTAMPTZ\
             );\
             CREATE INDEX IF NOT EXISTS idx_mcp_connections_user ON connections(user_id);\
             CREATE INDEX IF NOT EXISTS idx_mcp_connections_expiry ON connections(expires_at);\
             CREATE TABLE IF NOT EXISTS invocations (\
                 id BIGSERIAL PRIMARY KEY, \
                 connection_id BIGINT REFERENCES connections(id) ON DELETE SET NULL, \
                 user_id TEXT NOT NULL, \
                 tool TEXT NOT NULL, \
                 arguments JSONB NOT NULL DEFAULT '{}', \
                 downstream JSONB, \
                 status TEXT NOT NULL, \
                 http_status INTEGER, \
                 result JSONB, \
                 error TEXT, \
                 duration_ms INTEGER NOT NULL DEFAULT 0, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_mcp_invocations_user ON invocations(user_id, created_at DESC);\
             CREATE INDEX IF NOT EXISTS idx_mcp_invocations_tool ON invocations(tool);\
             CREATE INDEX IF NOT EXISTS idx_mcp_invocations_connection ON invocations(connection_id);",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx().clone();
        let catalogue = self.catalogue().clone();
        let cfg = self.config().clone();

        // --- connect ---------------------------------------------------------
        // `mcp:connect` at *some* scope: a lodge member with the permission may
        // open a connection, but the tools they then see are still filtered by
        // their own grants.
        let c = ctx.clone();
        let connect_cfg = cfg.clone();
        let connect = RouteDefinition::post_protected_any_scope(
            "/api/mcp/connect",
            PERM_CONNECT,
            route_handler(move |req| {
                let c = c.clone();
                let cfg = connect_cfg.clone();
                async move {
                    let Some(identity) = req.identity.clone() else {
                        return PluginResponse::error(
                            401,
                            "authentication required to open an MCP connection",
                        );
                    };
                    let body: ConnectBody = req.json()?;
                    let protocol = negotiate_protocol(body.protocol_version.as_deref());
                    let token = mint_token();
                    let ttl = cfg.connection_ttl_hours();
                    let capabilities = body.capabilities.clone().unwrap_or_else(|| json!({}));
                    let row = c
                        .db
                        .query_one(
                            format!(
                                "INSERT INTO {} \
                                   (user_id, client_name, client_version, protocol_version, \
                                    token_hash, capabilities, expires_at) \
                                 VALUES ($1, $2, $3, $4, $5, $6::jsonb, \
                                         now() + ($7 || ' hours')::interval) \
                                 RETURNING id, created_at::text AS created_at, \
                                           expires_at::text AS expires_at",
                                c.db.table("connections")
                            ),
                            vec![
                                SqlValue::Text(identity.user_id.clone()),
                                SqlValue::Text(body.client.clone().unwrap_or_default()),
                                SqlValue::Text(body.client_version.clone().unwrap_or_default()),
                                SqlValue::Text(protocol.to_string()),
                                // Only the hash is stored: the token is returned
                                // once, to the caller that minted it.
                                SqlValue::Text(hash_token(&token)),
                                SqlValue::Json(
                                    serde_json::to_string(&capabilities)
                                        .unwrap_or_else(|_| "{}".into()),
                                ),
                                SqlValue::Text(ttl.to_string()),
                            ],
                        )
                        .await?
                        .ok_or_else(|| {
                            SdkError::Internal("connection insert returned no row".into())
                        })?;

                    c.audit
                        .log(
                            Some(&identity),
                            "mcp.connect",
                            "mcp_connection",
                            &row["id"].as_i64().unwrap_or_default().to_string(),
                            json!({
                                "client": body.client,
                                "client_version": body.client_version,
                                "protocol_version": protocol,
                                "ttl_hours": ttl,
                            }),
                        )
                        .await?;

                    PluginResponse::json(
                        201,
                        &json!({
                            "connection_id": row["id"],
                            "token": token,
                            "expires_at": row["expires_at"],
                            "created_at": row["created_at"],
                            "protocolVersion": protocol,
                            "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
                            "server": server_info(),
                            "capabilities": { "tools": { "listChanged": false } },
                            "tools_endpoint": "/api/mcp/tools",
                            "invoke_endpoint": "/api/mcp/invoke",
                        }),
                    )
                }
            }),
        );

        // --- tools ------------------------------------------------------------
        // The filtered catalogue: a tool the caller cannot invoke is not
        // advertised. An agent that discovers a tool here can use it — the
        // listing and the permission check share one implementation.
        let c = ctx.clone();
        let tools_cat = catalogue.clone();
        let tools = RouteDefinition::get_protected_any_scope(
            "/api/mcp/tools",
            PERM_CONNECT,
            route_handler(move |req| {
                let c = c.clone();
                let cat = tools_cat.clone();
                async move {
                    let Some(identity) = req.identity.clone() else {
                        return PluginResponse::error(401, "authentication required to list MCP tools");
                    };
                    let visible = visible_tools(&c, Some(&identity), &cat).await;
                    let listed: Vec<Value> = visible.iter().map(|t| t.describe()).collect();
                    PluginResponse::json(
                        200,
                        &json!({
                            "protocolVersion": MCP_PROTOCOL_VERSION,
                            "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
                            "server": server_info(),
                            "capabilities": { "tools": { "listChanged": false } },
                            "tools": listed,
                            "count": listed.len(),
                            "filtered": true,
                            "warnings": cat.warnings(),
                        }),
                    )
                }
            }),
        );

        // --- invoke -----------------------------------------------------------
        let c = ctx.clone();
        let invoke_cat = catalogue.clone();
        let invoke_cfg = cfg.clone();
        let invoke = RouteDefinition::post_protected_any_scope(
            "/api/mcp/invoke",
            PERM_INVOKE,
            route_handler(move |req| {
                let c = c.clone();
                let cat = invoke_cat.clone();
                let cfg = invoke_cfg.clone();
                async move {
                    let Some(identity) = req.identity.clone() else {
                        return PluginResponse::error(401, "authentication required to invoke MCP tools");
                    };
                    let body: InvokeBody = req.json()?;
                    let started = Instant::now();
                    let max_bytes = cfg.max_result_bytes();

                    // Arguments: absent means "none"; anything but an object is a
                    // client bug, not a tool's.
                    let args: Map<String, Value> = match body.arguments.clone() {
                        Value::Null => Map::new(),
                        Value::Object(map) => map,
                        other => {
                            return PluginResponse::error(
                                400,
                                format!("arguments must be a JSON object, got {other}"),
                            )
                        }
                    };
                    let args_value = Value::Object(args.clone());

                    let Some(tool) = cat.find(body.tool.trim()) else {
                        // An unknown tool is not a permission decision; listing
                        // the tools the caller can actually see is the useful
                        // answer, and it leaks nothing they cannot already read.
                        let visible = visible_tools(&c, Some(&identity), &cat).await;
                        let available: Vec<&str> = visible.iter().map(|t| t.name.as_str()).collect();
                        return PluginResponse::json(
                            404,
                            &json!({
                                "error": format!("unknown tool {:?}", body.tool),
                                "available_tools": available,
                            }),
                        );
                    };

                    let token = connection_token(&req, body.connection_token.as_deref());
                    let connection =
                        match resolve_connection(&c, &identity, body.connection_id, token).await {
                            Ok(connection) => connection,
                            Err(e) => {
                                let status = e.status();
                                return refuse(
                                    &c,
                                    &identity,
                                    tool,
                                    &args_value,
                                    body.connection_id,
                                    started,
                                    status,
                                    e.to_string(),
                                    "mcp.invoke.denied",
                                )
                                .await;
                            }
                        };
                    let connection_id = connection.as_ref().map(|c| c.id);

                    // The permission check that makes a hidden tool a refusal
                    // rather than a surprise: the same call the listing made.
                    if !permission_holds(&c, Some(&identity), &tool.permission, tool.scope).await {
                        let message = format!(
                            "requires {} at {} scope — the permission the route itself requires",
                            tool.permission,
                            tool.scope.as_str()
                        );
                        return refuse(
                            &c,
                            &identity,
                            tool,
                            &args_value,
                            connection_id,
                            started,
                            403,
                            message,
                            "mcp.invoke.denied",
                        )
                        .await;
                    }

                    // Build the API call. A malformed invocation is answered
                    // before anything is sent, and logged like any other refusal.
                    let downstream = match build_downstream(tool, &args) {
                        Ok(downstream) => downstream,
                        Err(e) => {
                            let status = e.status();
                            return refuse(
                                &c,
                                &identity,
                                tool,
                                &args_value,
                                connection_id,
                                started,
                                status,
                                e.to_string(),
                                "mcp.invoke.denied",
                            )
                            .await;
                        }
                    };

                    let url = build_url(&cfg.base_url(), &downstream.path, &downstream.query);
                    let http_body = downstream
                        .body
                        .as_ref()
                        .and_then(|b| serde_json::to_vec(b).ok())
                        .map(|bytes| ("application/json".to_string(), bytes));

                    // The call itself: the caller's credentials, the caller's
                    // authority, the same route the UI uses. The core's gate
                    // re-checks the permission on this request.
                    let response = c
                        .http
                        .request(
                            downstream.method.clone(),
                            url.clone(),
                            forward_headers(&req),
                            http_body,
                        )
                        .await;

                    let duration_ms = started.elapsed().as_millis() as i64;
                    let (status, result, http_status, error, response_status) = match response {
                        Ok(response) => {
                            let content_type = response.header("content-type").map(String::from);
                            let result = decode_body(
                                content_type.as_deref(),
                                &response.body,
                                max_bytes,
                            );
                            if (200..300).contains(&response.status) {
                                (
                                    "ok".to_string(),
                                    Some(result),
                                    Some(response.status),
                                    None,
                                    response.status,
                                )
                            } else {
                                (
                                    "error".to_string(),
                                    Some(result.clone()),
                                    Some(response.status),
                                    Some(error_message(&result, response.status)),
                                    response.status,
                                )
                            }
                        }
                        Err(e) => (
                            "error".to_string(),
                            None,
                            None,
                            Some(format!("the tool could not reach the Adjutant API: {e}")),
                            502,
                        ),
                    };

                    let log = record_invocation(
                        &c,
                        InvocationRow {
                            connection_id,
                            user_id: &identity.user_id,
                            tool: &tool.name,
                            arguments: &args_value,
                            downstream: Some(&downstream),
                            status: &status,
                            http_status,
                            result: result.as_ref(),
                            error: error.as_deref(),
                            duration_ms,
                        },
                    )
                    .await;
                    let (invocation_id, log_error) = split_log(log);

                    if let Some(connection_id) = connection_id {
                        let touch = c
                            .db
                            .execute(
                                format!(
                                    "UPDATE {} SET last_used_at = now(), last_tool = $2, \
                                            invocation_count = invocation_count + 1 WHERE id = $1",
                                    c.db.table("connections")
                                ),
                                vec![
                                    SqlValue::Int(connection_id),
                                    SqlValue::Text(tool.name.clone()),
                                ],
                            )
                            .await;
                        if let Err(e) = touch {
                            eprintln!("[adjutant-mcp] connection touch failed: {e}");
                        }
                    }

                    let (action, details) = if status == "ok" {
                        (
                            "mcp.invoke",
                            json!({
                                "required_permission": tool.permission,
                                "downstream": downstream.summary(),
                                "http_status": http_status,
                                "invocation_id": invocation_id,
                            }),
                        )
                    } else {
                        (
                            "mcp.invoke.failed",
                            json!({
                                "downstream": downstream.summary(),
                                "http_status": http_status,
                                "error": error,
                                "invocation_id": invocation_id,
                            }),
                        )
                    };
                    audit_invocation(&c, &identity, action, &tool.name, details).await;

                    PluginResponse::json(
                        response_status,
                        &envelope(
                            tool,
                            Outcome {
                                status: &status,
                                http_status,
                                result: result.as_ref(),
                                error: error.as_deref(),
                                duration_ms,
                                invocation_id,
                                log_error,
                            },
                        ),
                    )
                }
            }),
        );

        // --- the audit trail --------------------------------------------------
        // `mcp:connect` gates the route (everyone may see their own record);
        // `mcp:audit` widens it to the troop. The object-route shape the other
        // plugins use: the gate needs the permission at some scope, the handler
        // decides which rows.
        let c = ctx.clone();
        let invocations = RouteDefinition::get_protected_any_scope(
            "/api/mcp/invocations",
            PERM_CONNECT,
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let Some(identity) = req.identity.clone() else {
                        return PluginResponse::error(401, "authentication required");
                    };
                    let all = c
                        .permissions
                        .has_in_scope(Some(&identity), PERM_AUDIT, &Scope::troop())
                        .await;
                    let limit = req.query_int("limit").unwrap_or(50).clamp(1, MAX_INVOCATION_PAGE);
                    let rows = c
                        .db
                        .query(
                            format!(
                                "SELECT id, connection_id, user_id, tool, status, http_status, error, \
                                        duration_ms, created_at::text AS created_at \
                                 FROM {} \
                                 WHERE ($1::bool OR user_id = $2) \
                                   AND ($3::text IS NULL OR tool = $3) \
                                   AND ($4::text IS NULL OR status = $4) \
                                 ORDER BY id DESC LIMIT $5",
                                c.db.table("invocations")
                            ),
                            vec![
                                SqlValue::Bool(all),
                                SqlValue::Text(identity.user_id.clone()),
                                req.query_param("tool").map(String::from).into(),
                                req.query_param("status").map(String::from).into(),
                                SqlValue::Int(limit),
                            ],
                        )
                        .await?;
                    PluginResponse::json(
                        200,
                        &json!({
                            "invocations": rows,
                            "scope": if all { "troop" } else { "own" },
                            "limit": limit,
                        }),
                    )
                }
            }),
        );

        vec![connect, tools, invoke, invocations]
    }
}

// The core resolves this symbol when it loads the library (and checks
// `adjutant_sdk_abi` before calling the factory, so a stale build is a clear
// load error rather than undefined behaviour).
export_plugin!(McpPlugin);

// ---------------------------------------------------------------------------
// Unit tests — the pure half (route behaviour lives in tests/mcp.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolSpec {
        builtin_tools()
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tool {name}"))
    }

    #[test]
    fn every_builtin_tool_is_well_formed() {
        for spec in builtin_tools() {
            assert!(
                spec.validate().is_ok(),
                "tool {} is not usable: {:?}",
                spec.name,
                spec.validate()
            );
        }
    }

    #[test]
    fn the_catalogue_covers_the_documented_surface() {
        let names: Vec<String> = builtin_tools().into_iter().map(|t| t.name).collect();
        for expected in [
            "membership_list_members",
            "membership_get_member",
            "missions_list_missions",
            "missions_get_mission",
            "missions_create_mission",
            "governance_list_motions",
            "governance_get_motion",
            "governance_create_motion",
            "calendar_list_events",
            "calendar_get_event",
            "calendar_create_event",
        ] {
            assert!(names.contains(&expected.to_string()), "missing tool {expected}");
        }
    }

    #[test]
    fn tools_match_their_plugin_routes_exactly() {
        // The exact routes and permissions the owning plugins declare — a tool
        // that disagreed would be a second, weaker API.
        let cases = [
            (
                "membership_list_members",
                "GET",
                "/api/membership/members",
                "membership:read_all",
                ToolScope::Troop,
            ),
            (
                "membership_get_member",
                "GET",
                "/api/membership/member",
                "membership:read",
                ToolScope::Any,
            ),
            (
                "missions_list_missions",
                "GET",
                "/api/missions/missions",
                "missions:read",
                ToolScope::Any,
            ),
            (
                "missions_get_mission",
                "GET",
                "/api/missions/mission/{id}",
                "missions:read",
                ToolScope::Any,
            ),
            (
                "missions_create_mission",
                "POST",
                "/api/missions/mission",
                "missions:create",
                ToolScope::Any,
            ),
            (
                "governance_list_motions",
                "GET",
                "/api/governance/motions",
                "governance:read",
                ToolScope::Troop,
            ),
            (
                "governance_get_motion",
                "GET",
                "/api/governance/motion/{id}",
                "governance:read",
                ToolScope::Troop,
            ),
            (
                "governance_create_motion",
                "POST",
                "/api/governance/motion",
                "governance:propose",
                ToolScope::Troop,
            ),
            (
                "calendar_list_events",
                "GET",
                "/api/calendar/events",
                "calendar:read",
                ToolScope::Any,
            ),
            (
                "calendar_get_event",
                "GET",
                "/api/calendar/event/{id}",
                "calendar:read",
                ToolScope::Any,
            ),
            (
                "calendar_create_event",
                "POST",
                "/api/calendar/event",
                "calendar:create",
                ToolScope::Any,
            ),
        ];
        for (name, method, path, permission, scope) in cases {
            let spec = tool(name);
            assert_eq!(spec.method, method, "{name} method");
            assert_eq!(spec.path, path, "{name} path");
            assert_eq!(spec.permission, permission, "{name} permission");
            assert_eq!(spec.scope, scope, "{name} scope");
        }
    }

    #[test]
    fn input_schema_is_generated_from_the_arguments() {
        let schema = tool("missions_create_mission").input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["title"]["type"], "string");
        assert_eq!(schema["properties"]["participant_count"]["type"], "number");
        assert_eq!(schema["properties"]["tags"]["type"], "array");
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(schema["properties"]["category"]["enum"][0], "service");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"title"));
        assert!(!required.contains(&"lodge_id"));
    }

    #[test]
    fn the_capture_argument_is_declared_as_required_and_typed() {
        let schema = tool("missions_get_mission").input_schema();
        assert_eq!(schema["properties"]["id"]["type"], "number");
        assert_eq!(schema["required"][0], "id");
    }

    #[test]
    fn describe_advertises_the_name_schema_and_permission() {
        let described = tool("governance_create_motion").describe();
        assert_eq!(described["name"], "governance_create_motion");
        assert_eq!(described["requiredPermission"], "governance:propose");
        assert_eq!(described["scope"], "troop");
        assert_eq!(described["inputSchema"]["type"], "object");
    }

    #[test]
    fn validate_rejects_a_tool_whose_capture_has_no_argument() {
        let broken = ToolSpec {
            name: "broken".into(),
            description: String::new(),
            permission: "x:y".into(),
            scope: ToolScope::Troop,
            method: "GET".into(),
            path: "/api/x/{id}".into(),
            params: vec![],
        };
        assert!(broken.validate().is_err());
    }

    #[test]
    fn validate_rejects_a_namespace_escape_and_a_permission_without_a_colon() {
        let mut spec = tool("missions_list_missions");
        spec.path = "http://elsewhere/api/x".into();
        assert!(spec.validate().is_err());

        let mut spec = tool("missions_list_missions");
        spec.permission = "missions".into();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn build_downstream_maps_arguments_to_path_query_and_body() {
        let spec = tool("missions_list_missions");
        let mut args = Map::new();
        args.insert("stage".into(), json!("review"));
        args.insert("limit".into(), json!(10));
        let built = build_downstream(&spec, &args).unwrap();
        assert_eq!(built.method, "GET");
        assert_eq!(built.path, "/api/missions/missions");
        assert_eq!(
            built.query,
            vec![
                ("stage".to_string(), "review".to_string()),
                ("limit".to_string(), "10".to_string())
            ]
        );
        assert!(built.body.is_none());
    }

    #[test]
    fn build_downstream_fills_a_capture_and_encodes_it() {
        let spec = tool("missions_get_mission");
        let mut args = Map::new();
        args.insert("id".into(), json!(42));
        let built = build_downstream(&spec, &args).unwrap();
        assert_eq!(built.path, "/api/missions/mission/42");

        let mut args = Map::new();
        args.insert("id".into(), json!("1/../decide?x=1"));
        let built = build_downstream(&spec, &args).unwrap();
        assert_eq!(built.path, "/api/missions/mission/1%2F..%2Fdecide%3Fx%3D1");
        assert!(!built.path.contains("decide?"), "no escape from the capture");
    }

    #[test]
    fn build_downstream_posts_a_body_and_requires_what_the_api_requires() {
        let spec = tool("missions_create_mission");
        let mut args = Map::new();
        args.insert("title".into(), json!("Coyote survey"));
        args.insert("purpose".into(), json!("learn"));
        args.insert("objectives".into(), json!("map"));
        args.insert("expected_impact".into(), json!("data"));
        args.insert("participant_count".into(), json!(6));
        let built = build_downstream(&spec, &args).unwrap();
        assert_eq!(built.method, "POST");
        assert_eq!(built.body.as_ref().unwrap()["participant_count"], 6);
        assert_eq!(built.body.as_ref().unwrap()["title"], "Coyote survey");

        let mut args = Map::new();
        args.insert("purpose".into(), json!("learn"));
        let err = build_downstream(&spec, &args).unwrap_err();
        assert!(matches!(err, SdkError::BadRequest(_)));
        assert!(err.to_string().contains("title"));
    }

    #[test]
    fn build_downstream_posts_an_empty_body_rather_than_none() {
        let mut spec = tool("missions_create_mission");
        spec.params = vec![];
        let built = build_downstream(&spec, &Map::new()).unwrap();
        assert_eq!(built.body, Some(json!({})));
    }

    #[test]
    fn build_downstream_refuses_arguments_the_tool_does_not_declare() {
        let spec = tool("missions_get_mission");
        let mut args = Map::new();
        args.insert("id".into(), json!(1));
        args.insert("created_by".into(), json!("someone-else"));
        let err = build_downstream(&spec, &args).unwrap_err();
        assert!(matches!(err, SdkError::BadRequest(_)), "unknown args are refused");
        assert!(err.to_string().contains("created_by"));
    }

    #[test]
    fn build_downstream_type_checks_body_arguments() {
        let spec = tool("missions_create_mission");
        let mut args = Map::new();
        args.insert("title".into(), json!("t"));
        args.insert("purpose".into(), json!("p"));
        args.insert("objectives".into(), json!("o"));
        args.insert("expected_impact".into(), json!("e"));
        args.insert("participant_count".into(), json!("6"));
        let err = build_downstream(&spec, &args).unwrap_err();
        assert!(err.to_string().contains("participant_count"));
    }

    #[test]
    fn build_downstream_treats_an_explicit_null_as_absent() {
        let spec = tool("missions_get_mission");
        let mut args = Map::new();
        args.insert("id".into(), Value::Null);
        let err = build_downstream(&spec, &args).unwrap_err();
        assert!(err.to_string().contains("required"));
    }

    #[test]
    fn build_downstream_reports_an_unfilled_capture_as_a_plugin_bug() {
        let mut spec = tool("missions_get_mission");
        spec.params = vec![]; // declaration and path disagree
        let err = build_downstream(&spec, &Map::new()).unwrap_err();
        assert!(matches!(err, SdkError::Internal(_)));
    }

    #[test]
    fn build_url_joins_base_path_and_query() {
        assert_eq!(
            build_url("http://127.0.0.1:8787/", "/api/mcp/tools", &[]),
            "http://127.0.0.1:8787/api/mcp/tools"
        );
        assert_eq!(
            build_url("http://host", "/api/x", &[("a b".into(), "1&2".into())]),
            "http://host/api/x?a%20b=1%262"
        );
    }

    #[test]
    fn percent_encode_leaves_only_unreserved_characters() {
        assert_eq!(percent_encode("aZ0-._~"), "aZ0-._~");
        assert_eq!(percent_encode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
    }

    #[test]
    fn forward_headers_relays_only_the_callers_credentials() {
        let req = TestRequest::new()
            .header("authorization", "Bearer tok")
            .header("cookie", "adjutant_session=abc")
            .header("mcp-session-id", "deadbeef")
            .header("host", "adjutant.local")
            .build();
        assert_eq!(
            forward_headers(&req),
            vec![
                ("authorization".to_string(), "Bearer tok".to_string()),
                ("cookie".to_string(), "adjutant_session=abc".to_string()),
            ],
            "the agent's own credentials, never the MCP connection token"
        );
    }

    #[test]
    fn forward_headers_skips_blank_values_and_tolerates_any_case() {
        let req = TestRequest::new()
            .raw_header("Authorization", "  ")
            .raw_header("Cookie", "a=b")
            .build();
        assert_eq!(
            forward_headers(&req),
            vec![("cookie".to_string(), "a=b".to_string())]
        );
    }

    #[test]
    fn connection_token_prefers_the_body_then_the_mcp_header() {
        let req = TestRequest::new().header("mcp-session-id", " from-header ").build();
        assert_eq!(
            connection_token(&req, Some(" from-body ")),
            Some("from-body".to_string())
        );
        assert_eq!(connection_token(&req, Some("")), Some("from-header".to_string()));
        let bare = TestRequest::new().build();
        assert_eq!(connection_token(&bare, None), None);
    }

    #[test]
    fn tokens_are_hashed_never_stored_raw() {
        let token = mint_token();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        let hash = hash_token(&token);
        assert_ne!(hash, token);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_token(&token));
        assert_ne!(hash, hash_token(&mint_token()));
    }

    #[test]
    fn protocol_negotiation_answers_what_it_implements() {
        assert_eq!(negotiate_protocol(None), MCP_PROTOCOL_VERSION);
        assert_eq!(negotiate_protocol(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate_protocol(Some("1999-01-01")), MCP_PROTOCOL_VERSION);
    }

    #[test]
    fn decode_body_parses_json_and_caps_large_bodies() {
        let parsed = decode_body(Some("application/json"), br#"{"members":[]}"#, 1024);
        assert_eq!(parsed["members"], json!([]));

        let text = decode_body(Some("text/plain"), b"hello", 1024);
        assert_eq!(text["text"], "hello");

        let big = vec![b'x'; 4096];
        let capped = decode_body(None, &big, 1024);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["bytes"], 4096);
    }

    #[test]
    fn error_message_prefers_the_api_s_own_words() {
        assert_eq!(
            error_message(&json!({ "error": "no such mission" }), 404),
            "no such mission"
        );
        assert!(error_message(&json!({}), 500).contains("500"));
    }

    #[test]
    fn catalogue_is_derived_from_config() {
        let config: McpConfig = serde_json::from_value(json!({
            "tools": {
                "disable": ["calendar_create_event"],
                "override": {
                    "calendar_list_events": {
                        "path": "/api/calendar/event",
                        "permission": "calendar:read"
                    },
                    "membership_list_members": { "enabled": false }
                },
                "add": [{
                    "name": "finance_balance",
                    "description": "The troop's balance",
                    "permission": "finance:read",
                    "scope": "troop",
                    "method": "GET",
                    "path": "/api/finance/balance"
                }]
            }
        }))
        .unwrap();
        let catalogue = Catalogue::from_config(&config, Vec::new());

        assert!(
            catalogue.find("calendar_create_event").is_none(),
            "disable drops a tool"
        );
        assert!(
            catalogue.find("membership_list_members").is_none(),
            "enabled:false drops a tool"
        );
        assert_eq!(
            catalogue.find("calendar_list_events").unwrap().path,
            "/api/calendar/event"
        );
        assert!(
            catalogue.find("finance_balance").is_some(),
            "add extends the catalogue"
        );
        assert!(!catalogue.warnings().is_empty(), "an override is reported");
    }

    #[test]
    fn catalogue_warns_and_drops_a_bad_added_tool() {
        let config: McpConfig = serde_json::from_value(json!({
            "tools": { "add": [{
                "name": "broken", "description": "", "permission": "nocolon",
                "scope": "troop", "method": "GET", "path": "/api/x"
            }]}
        }))
        .unwrap();
        let catalogue = Catalogue::from_config(&config, Vec::new());
        assert!(catalogue.find("broken").is_none());
        assert!(catalogue
            .warnings()
            .iter()
            .any(|w| w.contains("broken") && w.contains("dropped")));
    }

    #[test]
    fn catalogue_lists_each_permission_once() {
        let catalogue = Catalogue::builtin();
        let permissions = catalogue.permissions();
        let mut unique = permissions.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(permissions.len(), unique.len(), "permissions are deduplicated");
        assert!(permissions.contains(&"missions:read".to_string()));
        // `missions:read` backs two tools but is only checked once.
        assert_eq!(
            catalogue
                .tools()
                .iter()
                .filter(|t| t.permission == "missions:read")
                .count(),
            2
        );
    }

    #[test]
    fn an_unusable_config_falls_back_to_defaults_with_a_warning() {
        let (cfg, warnings) = McpConfig::from_value(&json!({ "base_url": 42 }));
        assert_eq!(cfg.base_url(), DEFAULT_BASE_URL);
        assert_eq!(cfg.connection_ttl_hours(), DEFAULT_CONNECTION_TTL_HOURS);
        assert_eq!(cfg.max_result_bytes(), DEFAULT_MAX_RESULT_BYTES);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn config_defaults_and_bounds_are_sane() {
        let (cfg, warnings) = McpConfig::from_value(&Value::Null);
        assert!(warnings.is_empty());
        assert_eq!(cfg.base_url(), DEFAULT_BASE_URL);
        assert_eq!(cfg.connection_ttl_hours(), DEFAULT_CONNECTION_TTL_HOURS);

        let (cfg, _) = McpConfig::from_value(&json!({ "base_url": "http://api:9000/" }));
        assert_eq!(cfg.base_url(), "http://api:9000");

        let (cfg, _) = McpConfig::from_value(&json!({ "connection_ttl_hours": -5 }));
        assert_eq!(cfg.connection_ttl_hours(), DEFAULT_CONNECTION_TTL_HOURS);
        let (cfg, _) = McpConfig::from_value(&json!({ "connection_ttl_hours": 999999 }));
        assert_eq!(cfg.connection_ttl_hours(), DEFAULT_CONNECTION_TTL_HOURS);
        let (cfg, _) = McpConfig::from_value(&json!({ "connection_ttl_hours": 48 }));
        assert_eq!(cfg.connection_ttl_hours(), 48);

        let (cfg, _) = McpConfig::from_value(&json!({ "max_result_bytes": 10 }));
        assert_eq!(cfg.max_result_bytes(), DEFAULT_MAX_RESULT_BYTES);
    }

    /// A minimal request builder for the pure tests. The SDK's `TestRequest`
    /// lowercases header names; this one deliberately does not, so
    /// `forward_headers` is exercised on both shapes.
    struct TestRequest {
        req: PluginRequest,
    }

    impl TestRequest {
        fn new() -> Self {
            Self {
                req: PluginRequest {
                    method: "POST".into(),
                    path: "/api/mcp/invoke".into(),
                    params: HashMap::new(),
                    query: Vec::new(),
                    headers: HashMap::new(),
                    body: Vec::new(),
                    identity: None,
                },
            }
        }

        fn raw_header(mut self, key: &str, value: &str) -> Self {
            self.req.headers.insert(key.to_string(), value.to_string());
            self
        }

        fn header(self, key: &str, value: &str) -> Self {
            self.raw_header(&key.to_lowercase(), value)
        }

        fn build(self) -> PluginRequest {
            self.req
        }
    }
}
