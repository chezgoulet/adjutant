# Adjutant

**Sovereignty-first administration software for democratic scout troops**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Adjutant is an open-source, self-hosted administration platform for scout troops that govern themselves. Built on the principles of the Catamount Accords — authority flows from the ground up, transparency is the default, and the system serves the scouts.

## What It Does

- **Mission Engine** — Six-stage lifecycle from proposal to impact reporting
- **Governance Ledger** — Motions, votes, amendments, and Accords versioning
- **Membership** — OSG sync, proficiency tracking, lodge and patrol management
- **Finance** — Fund tracking, budgets, sliding scale dues
- **Equipment** — Gear inventory and checkout management
- **Calendar** — Events, RSVPs, seasonal awareness
- **Conflict Resolution** — Structured pathway tracking
- **Impact Reporting** — Community service, conservation, and educational outcomes
- **Hermes MCP** — Permissions-aware AI agent integration
- **Meshcore** — LoRa mesh networking for backcountry operations
- **ATAK** — Tactical mapping integration

## Architecture

- **Server:** Rust + Axum (single binary, WASM plugin runtime)
- **Client:** Flutter (Android, iOS, Web, Desktop — one codebase)
- **Database:** PostgreSQL
- **Plugins:** Core + plugin architecture. Everything is a plugin.

## Quick Start

```bash
# Docker (recommended)
docker compose up

# Or bare metal
cargo build --release
./target/release/adjutant-server
```

## Documentation

- [Master Specification](SPEC.md) — Full architecture and design
- [Plugin Development](docs/plugin-development.md) — How to build plugins
- [Deployment](docs/deployment.md) — Installation and configuration
- [API Reference](docs/api-reference.md) — REST and MCP API docs

## License

MIT — use it, fork it, build on it. The code is given freely.
