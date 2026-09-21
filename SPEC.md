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
| Database | PostgreSQL 16+ | JSONB, PostGIS, full-text search, proven reliability |
| Plugin runtime | wasmtime (WASM) | Sandboxed execution, server-side and client-side |
| Client (all platforms) | Flutter (Dart) | Single codebase, native performance, offline-first, PWA support |
| Auth | Plugin (OIDC) | Delegate to upstream identity providers |
| Payments | Plugin (Stripe) | Industry standard, PCI compliant |
| MCP Server | Plugin (permissions-aware) | AI agent integration with scoped access |
| Storage | S3-compatible (plugin) | MinIO self-hosted or Cloudflare R2 |
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
- **WASM runtime** — wasmtime for sandboxed plugin execution
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

**Architecture:** Interface-based registration with WASM sandboxing.

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

**Schema isolation:** Each plugin gets its own PostgreSQL schema (`missions.*`, `governance.*`, etc.). The plugin's database handle is scoped to its own schema — it cannot read or write to other plugins' schemas unless the core explicitly grants cross-schema access.

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

1. **Request ID** — Assign unique ID to every request
2. **Logging** — Structured request/response logging
3. **CORS** — Cross-origin resource sharing for web clients
4. **Rate limiting** — Per-user, per-endpoint rate limits
5. **Authentication** — Validate session token, load user identity (delegated to auth plugin)
6. **Authorization** — Check permissions against the route's required permission (core enforcement)
7. **Plugin routing** — Dispatch to the appropriate plugin's route handler

**Route structure:**

```
/                           — Health check
/api/plugins                — Plugin management (admin)
/api/mcp/messages           — MCP server endpoint (Hermes)
/api/mcp/sse                — MCP server SSE stream
/api/{plugin}/*             — Plugin-specific routes
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

-- User → Role mapping (scoped to troop or Lodge)
CREATE TABLE core.user_roles (
    user_id         UUID NOT NULL REFERENCES core.users(id) ON DELETE CASCADE,
    role_id         TEXT NOT NULL REFERENCES core.roles(id) ON DELETE CASCADE,
    scope_type      TEXT DEFAULT 'troop',
    scope_id        UUID,
    granted_at      TIMESTAMPTZ DEFAULT now(),
    granted_by      UUID REFERENCES core.users(id),
    PRIMARY KEY (user_id, role_id, COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::UUID))
);

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

Permissions are scoped to a level:
- **Troop-wide** — The Chief, Troop Council members
- **Lodge** — Lodge Commanders, scoped to their Lodge
- **Patrol** — Patrol Captains, scoped to their Patrol
- **Personal** — Regular scouts, scoped to their own data

The enforcement layer checks both the permission AND the scope before allowing an action.

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

```yaml
services:
  adjutant:
    build: .
    ports: ["3000:3000"]
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
  
  redis:
    image: redis:7-alpine
  
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

# Run
./target/release/adjutant-server --config /etc/adjutant/config.toml
```

Single binary. No runtime dependencies. `scp` to a server and run.

### 11.3 Minimum Requirements

- **CPU:** 1 core (2+ recommended)
- **RAM:** 256MB minimum (512MB+ recommended)
- **Disk:** 1GB (scales with data)
- **OS:** Linux, macOS, Windows
- **PostgreSQL:** 14+

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

## 13. Open Questions

1. **Axum commitment** — Final validation of Axum as the server framework before implementation begins
2. **License** — MIT vs. Apache 2.0 (both are permissive, different patent clauses)
3. **Hosting** — Primary deployment target: VPS, self-hosted, or both?
4. **OSG API** — Does OSG have a programmatic API? If not, CSV import is the baseline
5. **Plugin WASM** — Should all plugins support WASM mode, or only third-party plugins?
6. **Mobile app distribution** — Google Play, F-Droid, APK direct, or all three?
7. **Web push notifications** — Web Push API requires a push service (Firebase, web-push-protocol)
8. **Multi-tenancy** — Single troop per instance, or support multiple troops on one server?
9. **Versioning** — SemVer for the core? Separate versioning for plugins?
10. **Contributing guidelines** — CONTRIBUTING.md, code of conduct, PR process

---

## 14. Next Steps

1. Validate Axum with a small proof-of-concept (plugin loading, route dispatch, WASM execution)
2. Set up the Flutter project structure
3. Implement the core (plugin registry, WASM runtime, database, HTTP server)
4. Implement the auth plugin (OIDC integration)
5. Implement the membership plugin (OSG sync)
6. Ship the MCP server plugin (Hermes integration)
7. Iterate on feature plugins with troop feedback
