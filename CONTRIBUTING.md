# Contributing

Adjutant is built by humans and AI agents together; the SPEC is the contract
(SPEC §16). Contributions are welcome — bug fixes, docs, tests, and plugins.

## Prerequisites

- Rust **1.96+** (`rust-toolchain.toml` pins stable + clippy + rustfmt)
- PostgreSQL 14+ (`pgcrypto` available)
- `wasm32-wasip1` target if you work on WASM guests:
  `rustup target add wasm32-wasip1`
- `psql` on `PATH` for the live harnesses

## Getting started

```bash
# database + role (CREATEROLE enables per-plugin schema isolation)
psql -h 127.0.0.1 -p 5433 -U postgres \
  -c "CREATE ROLE adjutant LOGIN CREATEROLE" \
  -c "CREATE DATABASE adjutant_dev OWNER adjutant"

cargo build --workspace
cargo test --workspace
```

See the [README](README.md) for running the server and the harnesses.

## Gates (must pass before review)

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
```

Plus, when relevant:

```bash
python3 scripts/probes.py                     # M1/M2 live ladder (needs a DB)
ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
cargo build --manifest-path wasm/Cargo.toml --release --target wasm32-wasip1
```

CI runs all of these on every push.

## Git flow

House standard: `testing` is the integration target, `main` is releases.

- Branch from `testing` (`feature/*`, `fix/*`, `docs/*`) and PR **into**
  `testing`.
- `testing` is merged into `main` only to cut a release.
- Keep commits small and reviewable; one logical change per commit.
- Never commit secrets; `main` is the deployable state.

## Code style

- `cargo fmt` (default rustfmt) and `clippy -D warnings`.
- Comments explain **why**, not what; match the surrounding file's voice.
- No new dependency without a reason. The server may link sqlx/tokio; the SDK
  and plugins may not (the host-mediated I/O rule).
- AI-generated code is fine, but a human reviews it at every milestone boundary.

## Adding things

| Change | Where |
|---|---|
| Core service, middleware, dispatch | `server/src/` |
| SDK type or helper | `plugins/sdk/src/lib.rs` (+ tests, + changelog) |
| A first-party plugin | `plugins/<name>/` |
| A sandboxed plugin | `wasm/examples/<name>/` + manifest |
| Tests that need a DB | `server/tests/` or the plugin's `#[cfg(test)]` |

## Changing the SDK contract

The SDK and core share a version; breaking changes bump `SDK_ABI_VERSION` and
must be recorded in [`CHANGELOG.md`](CHANGELOG.md). See
[`docs/sdk-compatibility.md`](docs/sdk-compatibility.md).

## Reporting bugs

Open an issue at <https://github.com/chezgoulet/adjutant/issues> with steps to
reproduce, the expected vs actual behavior, and your environment.
