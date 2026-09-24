# SDK Compatibility Policy

The SDK is the contract between the core and every plugin (SPEC §5.2a). This
document defines what a version number promises, and how the core enforces it.

## Versioning

- **One version for core + SDK.** `adjutant-sdk` and the core are released
  together and share a version. The SDK version *is* the contract version.
- **SemVer.** While at `0.x`, minor bumps may break (Rust ecosystem convention);
  from `1.0.0` on, breaking changes require a major bump.
- **The changelog is the contract changelog.** Every SDK-visible change lands in
  [`CHANGELOG.md`](../CHANGELOG.md).

## The ABI handshake (what the core actually enforces)

`export_plugin!` emits an `adjutant_sdk_abi()` symbol returning
`adjutant_sdk::SDK_ABI_VERSION`. Before the core calls a plugin's factory, it
resolves that symbol and compares it to its own `SDK_ABI_VERSION`:

- **Symbol missing** — the plugin predates the handshake. The core refuses to
  load it and says to rebuild against the current SDK.
- **Value differs** — the plugin was built against a different ABI. The core
  refuses to load it and reports both values.

This turns the usual "stale `.so` against a newer core" failure (undefined
behaviour through a mismatched vtable) into a clear load error.

`SDK_ABI_VERSION` is bumped on **any** breaking change to:

- the `AdjutantPlugin` trait surface (method signatures, added/removed methods),
- the `HostDb` / `HostEvents` / `HostHttp` traits,
- the exported symbols (`adjutant_plugin_create`, `adjutant_sdk_abi`), or
- the layout of types passed across the boundary (`PluginContext`,
  `PluginRequest`, `PluginResponse`, `SdkError`, `SqlValue`, `Event`).

Additive, non-breaking changes (new `SdkError` variants a plugin need not match,
new helper constructors, new `testing` mocks) do **not** bump the ABI version.

## What a plugin author must do

1. Build against the same SDK version as the core you target.
2. Rebuild after upgrading the core/SDK. `adjutant validate-plugin` catches a
   stale build before it reaches a server:
   ```bash
   cargo build -p adjutant-your_plugin
   adjutant validate-plugin target/debug/libadjutant_your_plugin.so
   ```
3. Pin the SDK in your `Cargo.toml`. For now, plugins in this workspace use a
   path dependency; published plugins should pin a version:
   ```toml
   adjutant-sdk = "0.2"
   ```

## Compatibility matrix

| Core / SDK | Plugin ABI | Notes |
|---|---|---|
| 0.1.0 | pre-handshake (no symbol) | M1–M3; refused by 0.2+ until rebuilt |
| 0.2.0 (M4 target) | 2 | First version with the handshake and scoped identity (`Identity.grants`) |

## Native-only caveat

Plugins are native `cdylib`s: core and plugin must share a toolchain and
dependency versions (the native loading caveat in the SDK docs). This is fine
for first-party and trusted plugins; it is **not** a security boundary.
Sandboxed third-party execution is the WASM path (SPEC §14-R1), prototyped in
Milestone 4 W3.
