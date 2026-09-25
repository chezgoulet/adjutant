# Adjutant — Master Specification

**Sovereignty-first administration software for democratic scout troops**

Version: 0.1.0-draft
Date: September 2026
Status: Design Specification

---

## 1. What Adjutant Is

Adjutant is an open-source, self-hosted administration platform designed for democratic, mission-driven scout troops operating under the Constellationism model. It is built to embody the governance principles of the Catamount Accords — authority flows from the ground up, transparency is the default state, and the system serves the scouts, not the other way around.

Adjutant is not a hierarchy management tool with a scouting skin. It is a sovereignty tool — built for communities that govern themselves.

**License:** MIT (or Apache 2.0 — TBD at implementation)
**Repository:** https://github.com/chezgoulet/adjutant
**Status:** Not a troop project. An open-source project by Christopher Goulet, given freely to the world.

---

## 2. Design Principles

1. **Thin core, thick plugins.** The core does almost nothing. Plugins do everything. If it's not essential to the plugin system itself, it's a plugin.

2. **Permission everywhere.** One permission system, enforced at the UI, API, and MCP layers. No action escapes authorization.

3. **Sovereignty by default.** Self-hosted. Data stays on the troop's infrastructure. No SaaS dependency, no vendor lock-in.

4. **Offline-first.** Scouts are in the woods. The client works without connectivity and syncs when back online.

5. **Open and forkable.** Other troops, other organizations, other communities can take Adjutant and make it their own. The code is given freely.

6. **One source of truth.** PostgreSQL is the database. No external search engines, no separate caching layers, no microservices. One server, one database, one binary.

---

## 3. Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│                    ADJUTANT SERVER                   │
│                  (Rust + Axum)                       │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │                   CORE                        │  │
│  │  • Plugin registry & lifecycle                │  │
│  │  • WASM runtime (wasmtime)                    │  │
│  │  • PostgreSQL connection & schema mgmt        │  │
│  │  • HTTP server (Axum) & route dispatch        │  │
│  │  • Configuration                              │  │
│  │  • Event bus (inter-plugin)                   │  │
│  │  • Logging                                    │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │              PLUGIN LAYER                     │  │
│  │                                               │  │
│  │  First-class plugins (ship with core):        │  │
│  │  • auth          (OIDC, sessions, roles)      │  │
│  │  • membership    (OSG sync, roles, patrols)   │  │
│  │  • missions      (6-stage lifecycle)          │  │
│  │  • governance    (motions, votes, Accords)    │  │
│  │  • finance       (funds, transactions, dues)  │  │
│  │  • equipment     (inventory, checkout)        │  │
│  │  • calendar      (events, RSVPs)              │  │
│  │  • archive       (historical records)         │  │
│  │  • conflicts     (resolution pathway)         │  │
│  │  • hermes-mcp    (permissions-aware MCP)      │  │
│  │  • meshcore      (LoRa mesh integration)      │  │
│  │  • atak          (tactical mapping)           │  │
│  │  • stripe        (payments)                   │  │
│  │  • announcements (internal comms)             │  │
│  │                                               │  │
│  │  Community plugins (install separately):      │  │
│  │  • google-sync   (Drive, Calendar, Gmail)     │  │
│  │  • apple-sync    (iCloud, Calendar)           │  │
│  │  • microsoft-sync (OneDrive, Outlook)         │  │
│  │  • nextcloud-sync (self-hosted files)         │  │
│  │  • proton-sync   (encrypted storage/mail)     │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────┬───────────────────────────────┘
                      │ HTTP + WebSocket
                      ▼
┌─────────────────────────────────────────────────────┐
│                 FLUTTER CLIENT                      │
│            (Dart — all platforms)                   │
│                                                     │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌──────────┐ │
│  │ Android │ │   iOS   │ │   Web   │ │ Desktop  │ │
│  │  (APK)  │ │(IPA/TF) │ │  (PWA)  │ │(Win/Mac/ │ │
│  │         │ │         │ │         │ │  Linux)  │ │
│  └─────────┘ └─────────┘ └─────────┘ └──────────┘ │
└─────────────────────────────────────────────────────┘
                      │
                      ▼

│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │                   SDK                         │  │
│  │  adjutant-sdk crate — the contract            │  │
│  │  • Plugin trait definitions                   │  │
│  │  • PluginContext, RouteDefinition, etc.       │  │
│  │  • CLI scaffolding (adjutant new-plugin)      │  │
│  │  • Plugin test harness                        │  │
│  │  • Versioned with the core                    │  │
│  │                                               │  │
│  │  The SDK is how we build Adjutant itself.     │  │
│  │  First-party plugins use the same API as      │  │
│  │  third-party plugins. Dogfooding is the       │  │
│  │  validation.                                  │  │
│  └───────────────────────────────────────────────┘  │

┌─────────────────────────────────────────────────────┐
│                  POSTGRESQL                         │
│                                                     │
│  core.*        — Foundation (users, roles, audit)   │
│  membership.*  — Members, proficiencies, lodges     │
│  missions.*    — Missions, milestones, mentorships  │
│  governance.*  — Motions, amendments, votes          │
│  finance.*     — Funds, transactions, budgets       │
│  equipment.*   — Items, checkouts                   │
│  calendar.*    — Events, RSVPs                      │
│  conflicts.*   — Cases, stage transitions           │
│  archive.*     — Historical records                 │
│  mcp.*         — MCP connections, invocations       │
│  meshcore.*    — Nodes, positions, messages         │
└─────────────────────────────────────────────────────┘
```

---

## 4. Technology Stack

| Layer | Technology | Rationale |
|---|---|---|
| Server | Rust + Axum | Safety, performance, WASM plugins, single binary, edge deployment |
| Database | PostgreSQL 14+ (18.6 in development) | JSONB, full-text search, proven reliability; `pgcrypto` for the audit hash chain |
| Plugin runtime | native `cdylib` today; wasmtime (WASM) **planned** | Sandboxed execution for untrusted/third-party plugins. Milestone 1 proved the native host-API path and deferred WASM (§14-R1); the SDK trait API is the same either way |
| Client (all platforms) | Flutter (Dart) | Single codebase, native performance, offline-first, PWA support |
| Auth | Plugin (OIDC) | Delegate to upstream identity providers |
| Payments | Plugin (Stripe) | Industry standard, PCI compliant |
| MCP Server | Plugin (permissions-aware) | AI agent integration with scoped access |
| Storage | S3-compatible (plugin) | MinIO self-hosted or Cloudflare R2 |
| SDK | adjutant-sdk (Rust crate) | Plugin API contract, CLI scaffolding, test harness — we build our own plugins with this |
| Hosting | Docker Compose or bare metal | One command deployment |
| CI/CD | GitHub Actions | Automated testing, building, publishing |

**Languages in the stack:**
- Rust (server core + plugins) — the foundation
- Dart (all client surfaces) — the interface
- SQL (PostgreSQL) — the data

**Two primary languages.** One for the server, one for everything the user touches.

---

## 5. Server Architecture

### 5.1 The Core

The core is the minimum viable server. It owns:

- **Plugin registry and lifecycle** — load, enable, disable, uninstall plugins
- **Plugin runtime** — native `cdylib` + `libloading` today; the WASM host API
  (wasmtime) is planned and mirrors the same `Arc<dyn Host…>` boundary (§14-R1)
- **Database connection and schema management** — connection pooling, migrations, schema isolation per plugin
- **HTTP server and route dispatching** — Axum router, middleware stack, request handling
- **Configuration** — server config, plugin config, environment variables
- **Event bus** — publish/subscribe for inter-plugin communication
- **Logging** — structured logging with tracing

The core does NOT contain:
- Business logic for any feature
- Authentication or authorization logic (that's the auth plugin)
- Payment processing (that's the Stripe plugin)
- Any domain-specific data models (that's each feature plugin)

### 5.2 Plugin System

**Architecture:** Interface-based registration. On the native path the
`manifest.json` described below is **folded into the trait implementation** —
`id()`/`version()`/`permissions_granted()` are the manifest, in code the compiler
checks (`server/src/cli.rs:4-7`). The JSON form is retained here as the shape a
future WASM/out-of-tree plugin loader would consume; nothing reads a
`manifest.json` today, and plugin discovery is a scan for `*.so`
(`server/src/plugin_runtime.rs:201-213`).

Each plugin is a directory containing:

```
plugins/
  missions/
    manifest.json
    src/
      mod.rs
      routes/
      models/
      migrations/
    Cargo.toml
```

**Plugin manifest:**

```json
{
  "name": "missions",
  "version": "1.0.0",
  "description": "Six-stage mission lifecycle for scout projects",
  "author": "Adjutant Contributors",
  "dependencies": [],
  "permissions_required": ["missions:read", "missions:write"],
  "permissions_granted": ["missions:create", "missions:read", "missions:update", "missions:approve"],
  "routes": ["/api/missions"],
  "ui": {
    "nav": [{"label": "Missions", "path": "/missions", "icon": "compass"}]
  }
}
```

**Plugin lifecycle:**

1. **Discovery** — Core scans the plugins directory for manifest files
2. **Validation** — Core checks dependencies, permissions, and schema compatibility
3. **Registration** — Plugin registers routes, hooks, UI components, and database migrations with the core
4. **Initialization** — Plugin runs its init function (set up state, connect to services)
5. **Running** — Plugin handles requests, publishes/subscribes to events
6. **Disable** — Plugin stops receiving requests, but data persists
7. **Uninstall** — Plugin's data is archived (not deleted), routes are removed

**Plugin API (Rust traits):**

```rust
#[async_trait]
pub trait AdjutantPlugin: Send + Sync {
    /// Unique identifier
    fn id(&self) -> &str;
    
    /// Version string
    fn version(&self) -> &str;
    
    /// Initialize the plugin with access to the core context
    async fn init(&mut self, ctx: PluginContext) -> Result<()>;
    
    /// Register HTTP routes
    fn routes(&self) -> Vec<RouteDefinition>;
    
    /// Register event subscriptions
    fn subscriptions(&self) -> Vec<EventSubscription>;
    
    /// Register database migrations
    fn migrations(&self) -> Vec<Migration>;
    
    /// Graceful shutdown
    async fn shutdown(&mut self) -> Result<()>;
}

/// Context provided to each plugin by the core
pub struct PluginContext {
    pub db: DbPool,              // Connection pool (scoped to plugin's schema)
    pub config: PluginConfig,    // Plugin-specific configuration
    pub events: EventBus,        // Publish/subscribe for inter-plugin communication
    pub permissions: PermissionService, // Check user permissions
    pub audit: AuditService,     // Write to audit log
}
```


### 5.2a The SDK (adjutant-sdk)

The SDK is the contract between the core and every plugin. It is the primary development interface for Adjutant — first-party and third-party plugins alike.

**Principle:** We eat our own dogfood. The auth, membership, and missions plugins are built using the exact same SDK API that third-party developers will use. If the SDK is awkward for us, it's awkward for everyone.

**Constraint (proven in Milestone 1):** a plugin is a `cdylib` with its own copy
of every dependency, so it cannot call `sqlx`/`tokio` directly — plugin-side DB
calls resolve the plugin's *own* runtime thread-local, panic, and abort the
process (`Rust cannot catch foreign exceptions`). All database, event,
permission, and audit access therefore crosses the boundary as `Arc<dyn Host…>`
trait objects implemented in the core. **The SDK links neither `sqlx` nor
`tokio`.** This host API is the shape the future WASM host API must mirror.

**What the SDK provides:**

| Component | Purpose |
|---|---|
| `AdjutantPlugin` trait | The core interface every plugin implements |
| `PluginContext` struct | Runtime services (DB, events, permissions, audit) |
| `RouteDefinition` | Type-safe Axum route registration |
| `EventSubscription` | Subscribe to event bus topics |
| `Migration` | Database migration declarations |
| `Permission` | Permission declaration and checking |
| `adjutant new-plugin` | CLI scaffolding — generates a plugin project with manifest, routes, models, migrations |
| `adjutant test-plugin` | Test harness — spins up a test server with the plugin loaded, mock permissions, test database |
| Path captures | `{name}` segments in `RouteDefinition::path` (`/api/missions/{id}`); captures arrive in `PluginRequest::params`, literal routes are matched first (landed ahead of SDK v0.2 because M4 needs it) |
| `adjutant validate-plugin` | **Planned, not implemented.** Validation happens at load time (id/route-namespace/duplicate/permission-parity checks in `plugin_runtime.rs`), which fails the boot loudly rather than validating offline |

**SDK versioning:** The SDK version is pinned to the core version. Breaking changes to the SDK require a major version bump. The SDK changelog is the contract changelog.

**SDK development order:** The SDK is built incrementally, not up front. The core defines the traits it needs; the SDK wraps them into a developer-facing API. Each plugin built with the SDK is a validation pass — if a plugin can't be built cleanly, the SDK needs revision before we write more plugins.

```
SDK v0.1  → core traits + PluginContext + scaffolding CLI
SDK v0.2  → route helpers, event helpers, test harness (after auth plugin validates it)
SDK v0.3  → permission macros, migration helpers (after membership plugin validates it)
SDK v1.0  → stable, documented, ready for third-party plugins
```

**Schema isolation:** Each plugin gets its own PostgreSQL schema (`missions.*`, `governance.*`, etc.). The plugin's database handle is scoped to its own schema — it cannot read or write to other plugins' schemas unless the core explicitly grants cross-schema access. The boundary is the identity of the connection: each plugin runs on a pool authenticated as its own `adjutant_plugin_<id>` `LOGIN` role, which owns that schema (design: [`docs/design/plugin-isolation.md`](docs/design/plugin-isolation.md)).

**WASM sandboxing (optional mode):** Plugins can be compiled to WASM and loaded into the wasmtime runtime. In WASM mode:
- No filesystem access
- No network access (unless explicitly granted via host API)
- Resource limits (CPU time, memory)
- Deterministic execution
- Sandboxed from other plugins

For most plugins, native Rust compilation is fine. WASM mode is for third-party or untrusted plugins.

### 5.3 Axum Server

**Framework:** Axum (Tokio ecosystem)

**Middleware stack (bottom to top):**

1. **Request ID + logging** — one layer: assigns `x-request-id` and emits one
   structured line per request (`middleware.rs:32-62`)
2. **CORS** — tower-http layer, outermost so its headers survive 429s and errors
3. **Rate limiting** — fixed window **per client IP** (`middleware.rs:98-176`).
   `x-forwarded-for` is honoured only when the direct peer is listed in
   `ADJUTANT_TRUSTED_PROXIES` (empty by default) — the header is client-controlled
4. **Authentication** — plugin identity providers first (auth sessions via cookie
   or Bearer); the `x-dev-user`/`x-dev-role` stub only as a gated fallback
   (`server/src/server.rs:63-87`). The core owns this resolution so the permission
   gate, the admin gate and audit attribution cannot drift apart
5. **Authorization** — permission checked by the core against
   `core.role_permissions` (never by the plugin)
6. **Plugin routing** — resolve `METHOD path` against the live registry and
   dispatch, releasing the registry lock before the handler runs

**Route structure:**

```
/                           — Health check
/api/plugins                — Plugin registry + route table (core:admin)
/api/plugins/{name}         — DELETE = uninstall, data archived (core:admin)
/api/plugins/reload         — Hot reload (core:admin)
/api/events/recent          — Event replay, ?since=&limit= (core:admin)
/api/audit/verify           — Hash-chain verification (core:admin)
/api/mcp/{messages,sse}     — MCP endpoint: planned (M5), no code yet
/api/{plugin}/*             — Plugin-specific routes. A segment may be a
                              capture: `/api/missions/{id}` delivers
                              `PluginRequest::params["id"]`
```

### 5.4 Event Bus

The event bus is the nervous system of Adjutant. Plugins publish events, other plugins subscribe.

**Event format:**

```rust
pub struct Event {
    pub event_type: String,      // e.g., "mission.completed"
    pub payload: serde_json::Value,
    pub source: String,          // Plugin ID that emitted the event
    pub timestamp: DateTime<Utc>,
}
```

**Built-in event types:**

```
user.registered          — New user created
user.role_changed        — User's role or scope changed
mission.created          — New mission proposed
mission.approved         — Mission approved by Lodge Commander
mission.completed        — Mission debrief submitted
motion.proposed          — New motion proposed
motion.passed            — Motion approved by vote
motion.failed            — Motion rejected by vote
conflict.escalated       — Conflict moved to next stage
payment.received         — Stripe payment confirmed
member.joined            — New member registered
member.left              — Member departed
event.created            — New calendar event
```

Plugins can define custom event types for their domain. The event bus doesn't validate event type names — it's a simple pub/sub transport.

---

## 6. Client Architecture

### 6.1 Flutter (All Platforms)

**Single codebase.** One Flutter project produces:
- Android APK/AAB
- iOS IPA (via TestFlight or direct)
- Web PWA (compiled to WASM via Flutter's web target)
- Windows, macOS, Linux desktop apps

**Why Flutter:**
- Native performance on every platform
- Offline-first with local storage (Hive, Isar, or SQLite)
- PWA support with service workers
- Responsive layout system (MediaQuery, LayoutBuilder)
- One language (Dart), one codebase, six platforms

### 6.2 Client Architecture

```
lib/
├── core/                    # Shared client logic
│   ├── api/                 # API client (HTTP + WebSocket)
│   ├── auth/                # Authentication state management
│   ├── storage/             # Local storage (offline)
│   ├── sync/                # Online/offline sync engine
│   └── models/              # Shared data models
├── features/                # Feature screens
│   ├── dashboard/           # Main dashboard
│   ├── missions/            # Mission screens
│   ├── governance/          # Governance screens
│   ├── finance/             # Finance screens
│   ├── membership/          # Membership screens
│   ├── calendar/            # Calendar screens
│   └── settings/            # Settings screens
├── plugins/                 # Plugin-provided UI components
│   └── {plugin_id}/         # Each plugin registers its screens
├── widgets/                 # Shared UI components
├── app.dart                 # App entry point
└── main.dart                # Main function
```

### 6.3 Offline-First Design

The client maintains a local database (SQLite via `sqflite`) that mirrors relevant server data. When the device is online, changes sync bidirectionally. When offline:

- **Reads** serve from local cache
- **Writes** queue locally and sync when connectivity returns
- **Conflict resolution** uses last-write-wins for most data, with manual resolution for governance votes and financial transactions

The sync engine handles:
- Delta synchronization (only changed data)
- Conflict detection and resolution
- Offline queue management
- Background sync on connectivity change

### 6.4 Real-Time Updates

**WebSocket connection** for bidirectional real-time:
- Live mission status updates
- Governance vote results
- Meshcore position sharing
- Emergency alerts

**Server-Sent Events (SSE)** for one-way push:
- Notifications
- Calendar updates
- Membership changes

The client maintains a persistent WebSocket connection when online, falling back to polling when offline.

---

## 7. Plugin Specifications

> **Which of these ship, in what order, and when 1.0 is earned:**
> [`docs/plugin-roadmap.md`](docs/plugin-roadmap.md). This section defines each
> plugin's behaviour; the roadmap owns the inventory and the release gate.

### 7.1 Auth Plugin

**Purpose:** Authentication, session management, role enforcement.

**Responsibilities:**
- OIDC provider integration (Google, Apple, Microsoft, Nextcloud, Proton, any OIDC-compliant provider)
- Local username/password fallback
- Session token management (JWT or database-backed)
- Role loading and permission checking
- Multi-device session management

**Database schema:** `core.users`, `core.sessions`, `core.roles`, `core.permissions`, `core.role_permissions`, `core.user_roles`

**OIDC as Source (Identity Provider):**
Scouts authenticate with existing accounts. No new passwords. The auth plugin handles the OIDC flow and populates `core.users` with the authenticated identity.

**OIDC as Client (Relying Party):**
Adjutant can act as an identity provider for other troop systems. A separate plugin or configuration within the auth plugin handles this.

### 7.2 Membership Plugin

**Purpose:** Member management, OSG sync, proficiency tracking, lodge/patrol structure.

**Responsibilities:**
- Pull membership roster from OSG (CSV import or API sync)
- Display OSG background check status (compliance dashboard)
- Track local troop data (trail names, patrol assignments, leadership positions)
- Proficiency tracking (OSG curriculum + local proficiencies)
- Lodge and patrol management
- Steward appointments

**Database schema:** `membership.members`, `membership.proficiencies`, `membership.patrols`, `membership.lodges`, `membership.stewards`

**OSG Integration:** The membership plugin pulls data from OSG's upstream systems. It does not duplicate OSG's registration or background check functionality. It consumes that data and layers troop-specific functionality on top.

### 7.3 Missions Plugin

**Purpose:** Six-stage mission lifecycle.

**Stages:** Request → Review → Approval → Execution → Debrief → Report

**Responsibilities:**
- Mission proposal form (guided, structured)
- Mentor matching and relationship tracking
- Lodge Commander approval workflow (approve, reject with guidance, appeal to Troop Council)
- Progress tracking during execution
- Debrief and impact recording
- Roll-up into cumulative Impact Report

**Database schema:** `missions.missions`, `missions.milestones`, `missions.mentorships`

### 7.4 Governance Plugin

**Purpose:** Motions, votes, amendments, Accords versioning.

**Responsibilities:**
- Motion lifecycle (propose → second → debate → vote → implement)
- Amendment tracking (friendly and formal)
- Accords versioning (every Congress adoption creates a new version)
- Quorum tracking (real-time display during meetings)
- Vote recording (voice, show of hands, ballot, roll call)
- Minutes generation (auto-draft from motion records)

**Database schema:** `governance.motions`, `governance.amendments`, `governance.votes`, `governance.accords_versions`

### 7.5 Finance Plugin

**Purpose:** Fund management, transactions, budgets, dues.

**Responsibilities:**
- Six fund tracking (General, Scholarship, Equipment, Expedition, Impact, Commencement)
- Transaction recording (income, expense, transfer)
- Budget vs. actuals tracking
- Sliding scale dues administration
- **The Scholarship fund as the funding source for anything free, deducted or waived.** A comp, a sliding-scale reduction, a scholarship award or a waived due is *drawn from* `scholarship` — recorded as a balanced transfer into the fund that would otherwise have received the money — so the subsidy is visible in the ledger and the Annual Financial Report instead of being expressed as a price of zero. A zero price hides who paid; a draw names them.
- **Donations and allocations addressed to a named fund** — a scout or troop management may give to the troop, or direct money into a fund (Scholarship above all), and the gift is income to that fund like any other
- **A receipt for any money received** — a donation, a dues payment, a shop purchase, an event fee. The receipt is issued from the **ledger record** (finance owns the money, so finance owns the receipt), it is **numbered and immutable** — a correction is a new receipt that references the one it supersedes, never an edit — and it is re-issuable and printable. It carries **no tax-deductibility language unless the troop has declared that status**, because a receipt claiming a deduction the troop cannot substantiate is a liability for the troop rather than a courtesy to the giver; where a status is declared, the wording is the troop's own and configurable, never invented by the software.
- Annual Financial Report generation
- Lodge dues tracking

**Database schema:** `finance.funds`, `finance.transactions`, `finance.budgets`, `finance.dues`

### 7.6 Equipment Plugin

**Purpose:** Gear inventory and checkout management.

**Responsibilities:**
- Inventory catalog (items, condition, location)
- Checkout/checkin tracking
- Maintenance schedules
- Replacement flagging
- Availability view for mission planning

**Database schema:** `equipment.items`, `equipment.checkouts`

### 7.7 Calendar Plugin

**Purpose:** Event management and scheduling.

**Responsibilities:**
- Troop-wide and Lodge-level events
- Recurring event support (iCal RRULE)
- RSVP tracking
- Quorum calculation for Congress
- Seasonal awareness (proactive task surfacing)

**Database schema:** `calendar.events`, `calendar.rsvps`

### 7.8 Archive Plugin

**Purpose:** Historical record keeping and search.

**Responsibilities:**
- Store Congress proceedings, Troop Council minutes, mission reports
- Full-text search (PostgreSQL GIN index)
- Timeline view of troop history
- Relationship tracking (decisions → policies → missions → impact)

**Database schema:** `archive.records`

### 7.9 Conflicts Plugin

**Purpose:** Conflict resolution pathway tracking.

**Stages:** Direct conversation → Facilitation → Arbitration → Troop Council

**Responsibilities:**
- Conflict case management (private, visible only to parties and facilitators)
- Stage transition tracking
- Anti-dropout mechanism (nudge facilitators if a stage stalls)
- Resolution recording

**Database schema:** `conflicts.cases`, `conflicts.stage_log`

### 7.10 Hermes MCP Plugin

**Purpose:** Permissions-aware MCP server for Hermes agent integration.

**Responsibilities:**
- Expose Adjutant functionality as MCP tools
- Filter tools based on authenticated user's permissions
- Check permissions on every tool invocation
- Execute through the same plugin API as the UI
- Log all MCP invocations for audit

**Database schema:** `mcp.connections`, `mcp.invocations`

**Permission model:**
- One permission system, same as UI and API
- Tools filtered by user's role and scope
- Every invocation logged with user identity, tool, parameters, and result
- No privilege escalation possible

### 7.11 Meshcore Plugin

**Purpose:** LoRa mesh networking integration.

**Responsibilities:**
- LoRa node registration and management
- Position tracking and map display
- Message relay over mesh
- Emergency beacon propagation
- Mesh health monitoring

**Database schema:** `meshcore.nodes`, `meshcore.positions`, `meshcore.messages`

### 7.12 ATAK Plugin

**Purpose:** Android Tactical Awareness Kit integration.

**Responsibilities:**
- Export Adjutant data as GeoJSON/KML for ATAK
- Display Adjutant overlays in ATAK (mission boundaries, rally points, patrol positions)
- Import ATAK markers into Adjutant
- Incident marker coordination

**Note:** ATAK is Android-native. The plugin generates standard geospatial formats that other mapping tools can consume.

### 7.13 Stripe Plugin

**Purpose:** Payment processing.

**Responsibilities:**
- Dues collection via Stripe Checkout
- Fundraising donation pages
- Event fee collection
- Payment webhook handling
- Transaction recording (writes to `finance.transactions`)

### 7.14 Announcements Plugin

**Purpose:** Internal troop communication.

**Responsibilities:**
- Announcement creation and distribution
- Read receipts
- Categorization (urgent, informational, event)
- Push notifications

### 7.15 Community Plugins (Google, Apple, Microsoft, Nextcloud, Proton)

**Purpose:** Integration with external productivity suites.

**Each plugin handles:**
- OIDC authentication (if used as identity provider)
- File sync (Drive, OneDrive, iCloud, Nextcloud, Proton Drive)
- Calendar sync (bidirectional)
- Email notifications (Gmail, Outlook, Proton Mail)
- Contact sync

These are optional — troops choose which integrations they need.

### 7.16 Store Plugin

**Purpose:** The troop's shop — what it sells, to whom, and at what price.

**Responsibilities:**
- A catalogue of what a troop sells: uniforms, patches, insignia, camp gear, event merchandise
- Prices per item, with a **sliding scale** — the same principle as dues, so cost never decides who belongs; a reduction is funded by a draw on the Scholarship fund (§7.5), never by a lower recorded price
- **Equipment rentals** as a priced product: the fee is the shop's, the custody and condition stay `equipment`'s (§7.6)
- Order placement and completion, with the record of what was sold and to whom
- **Comp sales** — a commander-and-above authority to sell to a member at no charge, with the reason and the authority recorded. The amount is **not zero**: it is **drawn from the Scholarship fund**, so who paid for it is visible in the ledger and the Annual Financial Report rather than hidden in a waived price.

**Depends on:** `stripe` (§7.13) to take money and `finance` (§7.5) to record it.
The shop holds no money and keeps no books of its own: a paid order is completed by
calling `stripe` as the caller, and the ledger entry remains finance's (§3.3 of
`docs/design/plugin-to-plugin.md`).

**Not in scope:** payment processing (§7.13), equipment custody and its checkout
state machine (§7.6), and dues (§7.5).

---

## 8. Database Schema

### 8.1 Core Schema

```sql
CREATE SCHEMA core;

-- Users (identity only — auth plugin fills)
CREATE TABLE core.users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    external_id     TEXT UNIQUE,
    email           TEXT UNIQUE,
    display_name    TEXT NOT NULL,
    avatar_url      TEXT,
    is_active       BOOLEAN DEFAULT true,
    created_at      TIMESTAMPTZ DEFAULT now(),
    updated_at      TIMESTAMPTZ DEFAULT now()
);

-- Sessions
CREATE TABLE core.sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    token_hash      TEXT NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    device_info     JSONB DEFAULT '{}',
    created_at      TIMESTAMPTZ DEFAULT now()
);

-- Plugin registry
CREATE TABLE core.plugins (
    id              TEXT PRIMARY KEY,
    version         TEXT NOT NULL,
    enabled         BOOLEAN DEFAULT true,
    config          JSONB DEFAULT '{}',
    installed_at    TIMESTAMPTZ DEFAULT now(),
    updated_at      TIMESTAMPTZ DEFAULT now()
);

-- Roles
CREATE TABLE core.roles (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    description     TEXT,
    created_at      TIMESTAMPTZ DEFAULT now()
);

-- Permissions
CREATE TABLE core.permissions (
    id              TEXT PRIMARY KEY,
    description     TEXT,
    created_at      TIMESTAMPTZ DEFAULT now()
);

-- Role → Permission mapping
CREATE TABLE core.role_permissions (
    role_id         TEXT NOT NULL REFERENCES core.roles(id) ON DELETE CASCADE,
    permission_id   TEXT NOT NULL REFERENCES core.permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

-- User → Role mapping (scoped to troop, Lodge or Patrol).
-- scope_id is opaque TEXT owned by the plugin; NULL means troop-wide. A
-- non-troop scope requires a scope_id and a troop scope requires NULL (CHECK).
-- The uniqueness is a unique index over COALESCE(scope_id,'') rather than a
-- PRIMARY KEY, because a PK column cannot be nullable.
CREATE TABLE IF NOT EXISTS core.user_roles (
    user_id         UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    role_id         TEXT NOT NULL REFERENCES core.roles(id) ON DELETE CASCADE,
    scope_type      TEXT NOT NULL DEFAULT 'troop',
    scope_id        TEXT,
    granted_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    granted_by      UUID REFERENCES core.users(id)
);
-- CREATE UNIQUE INDEX idx_user_roles_unique
--   ON core.user_roles (user_id, role_id, COALESCE(scope_id, ''));

-- Event bus
CREATE TABLE core.events (
    id              BIGSERIAL PRIMARY KEY,
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    source_plugin   TEXT NOT NULL,
    created_at      TIMESTAMPTZ DEFAULT now()
);

-- Audit log (append-only)
CREATE TABLE core.audit_log (
    id              BIGSERIAL PRIMARY KEY,
    user_id         UUID REFERENCES core.users(id),
    action          TEXT NOT NULL,
    resource_type   TEXT NOT NULL,
    resource_id     TEXT NOT NULL,
    details         JSONB DEFAULT '{}',
    source          TEXT NOT NULL,
    created_at      TIMESTAMPTZ DEFAULT now()
);
```

### 8.2 Plugin Schemas

Each plugin creates its own PostgreSQL schema. Full schema definitions are in the plugin directories. See the database design document for complete DDL.

**Plugin schema list:**
- `membership.*` — Members, proficiencies, patrols, lodges, stewards
- `missions.*` — Missions, milestones, mentorships
- `governance.*` — Motions, amendments, votes, Accords versions
- `finance.*` — Funds, transactions, budgets, dues
- `equipment.*` — Items, checkouts
- `calendar.*` — Events, RSVPs
- `conflicts.*` — Cases, stage log
- `archive.*` — Historical records
- `mcp.*` — MCP connections, invocations
- `meshcore.*` — Nodes, positions, messages

---

## 9. Permission Model

### 9.1 Permission Taxonomy

```
missions:
  missions:read          View missions
  missions:create        Propose new missions
  missions:update        Update own missions
  missions:approve       Approve/reject missions (Lodge Commander+)
  missions:appeal        Appeal mission rejection to Troop Council

governance:
  governance:read        View motions and votes
  governance:propose     Propose motions
  governance:vote        Vote on motions (when eligible)
  governance:amend       Propose amendments to motions

membership:
  membership:read        View own profile
  membership:read_lodge  View Lodge membership (Lodge Commander+)
  membership:read_all    View all membership (Chief+)
  membership:manage      Manage membership (Chief+)

finance:
  finance:read           View fund balances
  finance:read_all       View detailed financial data (Finance Subcouncil+)
  finance:manage         Manage finances (Finance Subcouncil+)

equipment:
  equipment:read         View inventory
  equipment:checkout     Check out gear
  equipment:manage       Manage inventory (Quartermaster+)

calendar:
  calendar:read          View events
  calendar:create        Create events (Chief+)

archive:
  archive:read           Search historical records

conflicts:
  conflicts:initiate     Flag a conflict
  conflicts:facilitate   Serve as facilitator
  conflicts:council      Review at Troop Council level

mcp:
  mcp:connect            Connect Hermes agent
```

### 9.2 Scoped Permissions

A role grant carries a scope:
- **Troop-wide** — The Chief, Troop Council members. Covers everything.
- **Lodge** — Lodge Commanders, scoped to one Lodge.
- **Patrol** — Patrol Captains, scoped to one Patrol.

`scope_id` is opaque text owned by the plugin (a bigint, UUID or slug); the core
never interprets it. `NULL` means troop-wide. Scope is **enforced**, not merely
declared:

- The core gate checks the route's declared reach. Ordinary routes
  (`get_protected` and friends) require a grant **covering troop**; object routes
  (`*_protected_any_scope`) require the permission at *some* scope and the
  handler checks the specific object with `PermissionService::has_in_scope`.
  Coverage is strict — a troop grant covers every scope; a lodge grant covers
  only that lodge; nothing else covers anything.
- `delete` requires a troop-covering grant from every constructor.
- **Personal** scope is not a scope: a caller reading their own record is an
  ownership check, not a grant.

See [`docs/design/scoped-permissions.md`](docs/design/scoped-permissions.md) and
`docs/plugin-development.md`.

---

## 10. OSG Integration

OSG handles:
- Member registration and roster
- Background check processing
- Curriculum and proficiency standards
- Youth safety policy
- Scouter training requirements

Adjutant consumes this data via:
- **Baseline:** CSV import (manual, works today)
- **Upgrade:** API integration (when OSG provides programmatic access)

The membership plugin pulls OSG data, displays it in Adjutant's compliance dashboard, and layers troop-specific functionality on top. Adjutant does not duplicate OSG's infrastructure.

---

## 11. Deployment

### 11.1 Docker Compose

# NOTE (planned, not yet implemented - M7): no Redis. SPEC 2.6 says one
# server, one database, one binary; nothing in the code uses a cache layer.
# NOTE (planned, not yet implemented - M7): no Redis. SPEC 2.6 says one
# server, one database, one binary; nothing in the code uses a cache layer.
```yaml
services:
  adjutant:
    build: .
    ports: ["8787:8787"]
    environment:
      DATABASE_URL: postgresql://adjutant:secret@postgres:5432/adjutant
      REDIS_URL: redis://redis:6379
    depends_on: [postgres, redis]
  
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: adjutant
      POSTGRES_USER: adjutant
      POSTGRES_PASSWORD: secret
    volumes: ["pgdata:/var/lib/postgresql/data"]
  

  minio:
    image: minio/minio
    command: server /data --console-address ":9001"
    ports: ["9000:9000", "9001:9001"]
    volumes: ["minio:/data"]

volumes:
  pgdata:
  minio:
```

### 11.2 Bare Metal

```bash
# Build
cargo build --release

# Run (binary name is `adjutant`; env vars are ADJUTANT_*-prefixed)
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5432/adjutant \
ADJUTANT_PLUGIN_DIR=/etc/adjutant/plugins \
ADJUTANT_DEV_HEADERS=false \
ADJUTANT_TRUSTED_PROXIES=127.0.0.1 \
  ./target/release/adjutant --config /etc/adjutant/config.toml
```

Single binary plus PostgreSQL. Plugin `cdylibs` live in the plugin directory, so
the deployment unit is the binary plus its `.so` files. Release builds still need
`pgcrypto` available in PostgreSQL (created by core migration 2).

### 11.3 Minimum Requirements

- **CPU:** 1 core (2+ recommended)
- **RAM:** 256MB minimum (512MB+ recommended)
- **Disk:** 1GB (scales with data)
- **OS:** Linux, macOS, Windows
- **PostgreSQL:** 14+ (`pgcrypto` extension available)

Runs on a Raspberry Pi, a cheap VPS, or a production server.

---

## 12. Repository Structure

```
adjutant/
├── server/                     # Rust (Axum)
│   ├── src/
│   │   ├── core/               # Core server logic
│   │   │   ├── config.rs
│   │   │   ├── db.rs
│   │   │   ├── events.rs
│   │   │   ├── plugin_runtime.rs
│   │   │   ├── permissions.rs
│   │   │   └── server.rs
│   │   ├── plugins/            # Built-in plugins
│   │   │   ├── auth/
│   │   │   ├── membership/
│   │   │   ├── missions/
│   │   │   ├── governance/
│   │   │   ├── finance/
│   │   │   ├── equipment/
│   │   │   ├── calendar/
│   │   │   ├── archive/
│   │   │   ├── conflicts/
│   │   │   ├── hermes-mcp/
│   │   │   ├── meshcore/
│   │   │   ├── atak/
│   │   │   ├── stripe/
│   │   │   └── announcements/
│   │   └── main.rs
│   ├── Cargo.toml
│   └── Dockerfile
├── client/                     # Flutter (Dart)
│   ├── lib/
│   │   ├── core/               # Shared client logic
│   │   ├── features/           # Feature screens
│   │   ├── plugins/            # Plugin-provided UI
│   │   └── main.dart
│   ├── pubspec.yaml
│   └── Dockerfile              # For web build
├── plugins/                    # Plugin SDK and examples
│   ├── sdk/                    # Rust traits and types for plugin development
│   ├── examples/               # Example plugins
│   └── README.md
├── docs/                       # Documentation
│   ├── architecture.md
│   ├── plugin-development.md
│   ├── deployment.md
│   └── api-reference.md
├── docker-compose.yml
├── LICENSE
└── README.md
```

---

## 13. Resolved Decisions

The following were open questions in earlier versions of this spec. They are resolved.

1. **Axum** — Confirmed. Axum is the server framework. Tokio ecosystem, proven at scale, good WASM integration via wasmtime.
2. **License** — MIT. Permissive, simple, maximally forkable. No patent clause complexity.
3. **Hosting** — Docker Compose primary, bare metal supported. One `docker compose up` to deploy.
4. **OSG API** — Assume none. CSV import is the baseline. If OSG provides an API later, the membership plugin adapts.
5. **Plugin WASM** — WASM is optional mode for third-party/untrusted plugins. First-party plugins compile natively.
6. **Mobile distribution** — APK direct + F-Droid. Google Play deferred until post-MVP.
7. **Web push** — Firebase Cloud Messaging for web push. Self-hosted alternative via ntfy (plugin).
8. **Multi-tenancy** — Single troop per instance. Multi-tenancy is a future community plugin, not core.
9. **Versioning** — SemVer for core and SDK. Plugins version independently, pinned to minimum core version.
10. **Contributing** — CONTRIBUTING.md, code of conduct, PR process — created at Milestone 6.

---

## 14. Risk Register

| # | Risk | Severity | Likelihood | Mitigation |
|---|---|---|---|---|
| R1 | **WASM plugin host API is an unsolved design problem.** Plugins need to register routes, query DB, publish events, check permissions — all through a WASM host API. This is the hardest technical piece. | HIGH | HIGH | **Resolved for now: native-only.** Milestone 1 proved the host-mediated I/O boundary (`Arc<dyn Host…>`, the SDK links no sqlx/tokio) and deferred WASM; first-party plugins are native `cdylibs`. That host API is the shape a WASM host must mirror; sandboxing third-party plugins remains open. |
| R2 | **SDK ergonomics.** If writing a plugin with the SDK is painful, the ecosystem dies. The SDK is the product; everything else is infrastructure. | HIGH | MEDIUM | Build the SDK incrementally. Auth and membership plugins are dogfooding — if they can't be built cleanly, redesign the SDK before writing more plugins. CLI scaffolding (`adjutant new-plugin`) enforces good patterns. |
| R3 | **Offline sync protocol.** The client must work without connectivity. Conflict resolution, delta sync, and queue management are non-trivial. | HIGH | MEDIUM | Design the sync protocol before Phase 3 (client). Use CRDTs for simple conflict resolution (last-write-wins for most fields, merge for lists). Prototype the sync layer as a standalone library. |
| R4 | **Scope creep.** AI makes it easy to add "just one more feature." A large codebase that nobody fully understands is a liability. | MEDIUM | HIGH | Ship the MVP (Milestone 5), get it in front of scouts, iterate based on real feedback. The milestone structure enforces hard gates — don't start the next milestone until the current one's exit criteria are met. |
| R5 | **Concurrent AI agents.** Multiple agents working the same codebase simultaneously produce conflicting code. | MEDIUM | MEDIUM | One agent at a time per worktree. Lock the worktree before starting. Small commits, clear interfaces, human review at every phase boundary. |
| R6 | **App store submission.** Google Play and Apple App Store have review processes, developer accounts, and compliance requirements. | LOW | HIGH | Defer to Milestone 6. APK direct + F-Droid for initial distribution. App store submission is a separate workstream with its own timeline. |
| R7 | **Axum doesn't deliver.** The framework has gaps we haven't found yet (middleware limitations, WASM integration issues, ecosystem holes). | LOW | LOW | Mitigated by the Milestone 1 prototype. If Axum fails, alternatives (Actix-web, Warp) are drop-in replacements at the trait level — the SDK insulates plugins from the framework choice. |

---

## 15. Development Milestones

Milestones are ordered by dependency. Each milestone has hard exit criteria — don't start the next one until the current one passes. Time estimates are deliberately absent; the milestones define *what* is done, not *when*.

> **Numbering note:** `docs/milestones/M4-core-sdk-stabilization.md` is an
> **inserted stabilization gate (M3-S)** that landed between M3 and this
> section's M4; SPEC numbering remains canonical. See
> [`docs/plugin-roadmap.md`](docs/plugin-roadmap.md) §2.

### Milestone 1: Prototype Validation

**Goal:** Prove the core architectural bets are sound before writing application code.

**Status: PASSED (2026-09-21).** Evidence: `docs/milestones/M1-prototype-validation.md`
(10/10 live probes, 18 unit tests). Three findings reshaped the design — see that
document: host-mediated I/O, plugin libraries must outlive the router, and a
SPEC §8.1 DDL error (COALESCE in PRIMARY KEY).

**Exit criteria:**
- [x] Axum server compiles and responds to HTTP requests
- [x] A plugin can register routes and handle requests through the core router
- [x] A plugin can query PostgreSQL through the core's connection pool *(via host API — plugin links no sqlx)*
- [x] A plugin can publish and subscribe to events through the event bus
- [x] Permissions can be declared by a plugin and enforced by the core middleware
- [x] Database migrations run automatically when a plugin loads
- [x] The WASM plugin path works (or the fallback: native-only) — one of these must be proven *(native-only; `.so` has 0 tokio/sqlx symbols)*
- [x] A "hello world" plugin compiles, loads, and responds

**Deliverable:** A working prototype that validates every core subsystem. If any subsystem fails, redesign before proceeding.

---

### Milestone 2: Core Server

**Goal:** Build the production core using the SDK.

**Status: PASSED (2026-09-22).** Evidence: `docs/milestones/M2-core-server.md`
(**67/67** live probes via the committed `scripts/probes.py`, transcript in
`docs/evidence/m2_probes.json`; 28 unit tests at commit `0cb6fff`; clippy 0
warnings). *Correction (audit, 2026-09-22): this line previously claimed "52/52
live probes" from four gitignored JSON transcripts that in fact recorded 50 passes
and 7 failures. The probes were re-run through a committed harness; the defects
behind those failures (broken reinstall, anonymous plugin inventory, fail-open
audit verifier, actorless lifecycle audit rows, unconditional XFF trust) are fixed
and listed in the milestone document.* One bug dominated the
milestone — `sqlx::raw_sql`'s `async fn` wrapper is unprovable as `Send`, which
made every handler awaiting migrations fail axum's `Handler` bound; fixed by
calling `Executor::execute` directly. Also found: a `libloading::Symbol` or a
`slice::Iter` left alive across an await poisons the generator's `Send` proof.

**Exit criteria:**
- [x] Plugin registry: load, enable, disable, uninstall, hot-reload
- [x] Database connection pool with per-plugin schema isolation
- [x] Axum server with full middleware stack (request ID, logging, CORS, rate limiting, auth, authorization, plugin routing)
- [x] Event bus with pub/sub, persistence, and replay
- [x] Configuration management (file, environment, CLI)
- [x] Structured logging with tracing *(pretty + json)*
- [x] Audit log (append-only, tamper-evident) *(SHA-256 chain + append-only trigger; superuser bypass detected by verifier)*
- [x] All core traits compiled as the `adjutant-sdk` crate
- [x] Core compiles, all tests pass, `cargo clippy` clean *(0 warnings, 28/28)*

**Deliverable:** A production-ready core server that plugins can target. The SDK is built alongside — the core defines the traits it needs.

---

### Milestone 3: SDK v0.1 + First Plugins

**Goal:** Build the SDK to v0.1 and validate it by building the auth and membership plugins with it.

**Exit criteria:**
- [ ] `adjutant-sdk` crate published (v0.1) — *versioned 0.1.0 in-workspace; crates.io publication pending a registry token decision*
- [x] `adjutant new-plugin` CLI scaffolds a plugin project with manifest, routes, models, migrations
- [x] `adjutant test-plugin` runs a test server with mock permissions and test database
- [x] Auth plugin: OIDC login, session management, role enforcement — all built with the SDK
- [x] Membership plugin: member roster, OSG CSV import, proficiency tracking — all built with the SDK
- [x] Both plugins can be loaded, enabled, disabled, and uninstalled through the core
- [x] Both plugins' routes respond correctly with proper permission checks
- [x] Both plugins' database schemas are isolated and migrations run cleanly
- [x] The SDK API feels good. If building auth or membership is painful, redesign the SDK before proceeding.

**Evidence:** `docs/milestones/M3-sdk-and-plugins.md` — build/test/clippy gates,
**34/34 + 4 skipped** `test-plugin` probes, and **52/52** live end-to-end probes
with `ADJUTANT_DEV_HEADERS=false` (the harness now asserts that flag rather than
assuming it; every identity comes from a real session; OIDC exercised against a mock
IdP). SDK verdict: API held up; no redesign needed. All ten listed bugs are now verified
fixed in the code — the tenth (a claimed 409 for duplicate usernames) described
behaviour that did not exist when written and has since been implemented
(`auth/src/lib.rs`, unique violation → 409). The systemic
one — the documented per-plugin `search_path` contract was never applied at runtime,
which had silently hidden every unqualified plugin table — is fixed and now has a
regression test. The audit pass also fixed the harness's `elif` body-assertion bug,
an unverifiable re-import assertion, missing lifecycle probes, an anonymous
`/api/plugins`, a fail-open audit verifier, and four unit tests that could not fail.

**Deliverable:** SDK v0.1 with two validated plugins. The SDK is the product — this milestone proves it works.

---

### Milestone 4: Missions + Governance + SDK v0.2

**Goal:** Expand the SDK and build the core mission/governance plugins.

**Exit criteria:**
- [ ] Missions plugin: full 6-stage lifecycle, mentor matching, Lodge Commander approval
- [ ] Governance plugin: motion lifecycle, voting, amendments, Accords versioning
- [ ] SDK v0.2: route helpers, event helpers, test harness improvements
- [ ] Event bus integration: missions publish `mission.completed`, governance publishes `motion.passed`
- [ ] Both plugins built with the SDK, validating the API

**Deliverable:** Four plugins (auth, membership, missions, governance) all built with the SDK.

---

### Milestone 5: MCP Server + Flutter Client MVP + Calendar

**Goal:** Ship the permissions-aware MCP server and a working Flutter client, with
the calendar scouts actually plan their lives in.

**Exit criteria:**
- [ ] MCP server plugin: tools exposed, permissions filtered, invocations logged
- [ ] Hermes agent can connect and interact through the MCP server
- [ ] Flutter client: login screen, mission list, membership roster, calendar, basic navigation
- [ ] Flutter client works on Android, iOS, and Web (PWA)
- [ ] Offline mode: client caches data locally, syncs when online
- [ ] Calendar plugin: events, RSVPs, recurring events, quorum tracking
- [ ] Basic UI/UX review — is the navigation intuitive? Are the screens useful?
- [ ] Deployed to The House for real use by the 161st

**Deliverable:** A working MVP that scouts can actually use. This is the first release to real users. Calendar moved here from M6 because a troop without a calendar stays in a group chat — see [`docs/plugin-roadmap.md`](docs/plugin-roadmap.md) §4.

---

### Milestone 6: Finance + Equipment + Archive + Conflicts + Announcements

**Goal:** Build the operational plugins that scouts use daily, and the two that
carry the Accords — searchable history and the conflict pathway.

**Exit criteria:**
- [ ] Finance plugin: fund tracking, transaction recording, budget vs. actuals, sliding scale dues
- [ ] Equipment plugin: inventory, checkout/checkin, maintenance schedules
- [ ] Archive plugin: Congress proceedings, minutes, full-text search, timeline, decision→policy→mission tracking
- [ ] Conflicts plugin: staged pathway, case management, stage transitions, anti-dropout nudges
- [ ] Announcements plugin: creation, read receipts, categories, push notifications
- [ ] SDK v0.3: permission macros, migration helpers
- [ ] All plugins built with the SDK, validating the API

**Deliverable:** Eleven plugins covering the full operational scope of a scout troop. Archive, conflicts and announcements were unassigned before this revision; they are in scope for 1.0, not post-1.0 — see [`docs/plugin-roadmap.md`](docs/plugin-roadmap.md) §3.

---

### Milestone 7: Production Hardening

**Goal:** Make Adjutant production-ready for self-hosting.

**Exit criteria:**
- [ ] Docker Compose deployment (one command)
- [ ] HTTPS/TLS configuration
- [ ] Backup and restore procedures
- [ ] Performance testing (100+ concurrent users)
- [ ] Security audit (OWASP Top 10, dependency scanning)
- [ ] Documentation: admin guide, user guide, API reference
- [ ] CONTRIBUTING.md, code of conduct, PR process
- [ ] CI/CD pipeline (GitHub Actions)

**Deliverable:** Production-ready deployment with documentation.

---

### Milestone 8: v1.0 Release

**Goal:** Stable, documented, ready for community adoption.

**Exit criteria:**
- [ ] Every Tier A plugin in [`docs/plugin-roadmap.md`](docs/plugin-roadmap.md) §3 has passed all three tests in §1 — exists through the blessed path, stable, and usable by a scout without SQL
- [ ] SDK v1.0 published with stable API
- [ ] Documentation site live
- [ ] F-Droid listing
- [ ] Community plugin guide published
- [ ] 3+ troops using Adjutant in production
- [ ] 1+ community plugin contributed by someone outside The House

**Deliverable:** Adjutant v1.0. The project is real.

---

## 16. AI-Assisted Development Workflow

Adjutant is built by humans and AI agents working together. The human makes design decisions. The AI generates code. The SPEC is the contract between them.

### 16.1 What the AI Agent Does

| Task | AI Suitability | Notes |
|---|---|---|
| Core server boilerplate | HIGH | Plugin registry, DB pool, HTTP routes, event bus — standard Rust patterns |
| Database migrations | HIGH | Schema is defined in the SPEC; AI writes the DDL |
| Plugin implementations | HIGH | Once SDK traits are defined, plugins follow predictable patterns |
| Test generation | HIGH | Unit tests, integration tests, API tests — AI generates from the spec |
| Documentation | HIGH | API docs, admin guides, user guides — AI generates from code and spec |
| CI/CD setup | HIGH | GitHub Actions workflows, Docker builds — standard patterns |
| SDK scaffolding | HIGH | CLI tools, project templates — mechanical generation |
| Bug fixes | MEDIUM | AI can diagnose and fix if the bug is well-defined |
| Performance optimization | MEDIUM | AI can identify hot paths but needs human judgment on tradeoffs |

### 16.2 What the Human Does

| Task | Why Human | Notes |
|---|---|---|
| SDK API design | Ergonomics are subjective | The SDK must feel good to use. This is a design decision, not a code generation task. |
| UX decisions | Scouts are the users | Layout, navigation, what goes where — AI can suggest, humans decide |
| Phase boundary review | Quality gate | Before starting the next milestone, review what was built. Does it work? Does it feel right? |
| Error handling strategy | UX and safety implications | How errors surface to users, how failures propagate — human judgment |
| Naming and API surface | Long-term maintainability | Names stick. Humans choose them. |
| Scope enforcement | AI says yes, humans say no | The milestone structure is the scope gate. Don't add features outside the current milestone. |

### 16.3 The Contract

The SPEC is the contract. AI generates code that satisfies the SPEC. Humans validate the SPEC and the output.

```
Human writes/validates SPEC
        ↓
AI generates code from SPEC
        ↓
Human reviews output, course-corrects
        ↓
If output fails: redesign SPEC, then regenerate
        ↓
If output passes: commit, move to next milestone
```

**Phase boundaries are hard stops.** At the end of each milestone, the human reviews:
1. Does the code compile and pass tests?
2. Does the SDK API feel right for building plugins?
3. Are there any design decisions the AI made that need human input?
4. Is the scope still under control?

Only after all four are satisfied does the next milestone begin.

### 16.4 Agent Constraints

- **One agent at a time.** Don't run concurrent agents on the same worktree. Lock the worktree before starting.
- **Small commits.** Each commit should be reviewable in isolation. No mega-commits.
- **Tests first.** AI generates tests from the SPEC before generating implementation. The SPEC defines expected behavior; tests verify it.
- **No scope creep.** If the AI suggests "while I'm here, I could also add X" — reject it. X goes in the next milestone, not this one.
- **Human review at every milestone boundary.** The AI does not self-certify. Humans review and approve.

---

