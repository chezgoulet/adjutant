# Releasing & publishing

How to cut an Adjutant release and publish the crates. See
[`sdk-compatibility.md`](sdk-compatibility.md) for what a version promises, and
[`release-path.md`](release-path.md) for what has to be true before the next tag —
in particular the interim `v0.3.0` recommendation and the v1.0 gate.

## What a release contains

- A Git tag `vX.Y.Z` on `main`, which triggers
  [`.github/workflows/release.yml`](../.github/workflows/release.yml): it builds
  the workspace and the WASM guest, and publishes a tarball with the `adjutant`
  binary, the bundled plugin `.so` files, the `hello_wasm.wasm` guest,
  `README.md`, and `LICENSE`.
- Published crates: **`adjutant-sdk` first**, then **`adjutant-server`** (the
  server depends on the SDK by version, so the SDK must exist on crates.io
  before the server can be published).

## One-time setup (repository owner)

1. Create or use a crates.io account and generate an API token.
2. Add the token as a repository secret (e.g. `CARGO_REGISTRY_TOKEN`) for the
   publish workflow, or run the publish steps locally with `cargo login`.
3. Confirm the crate names `adjutant-sdk` and `adjutant-server` are available.

## Version bump

1. Update `version` in [`Cargo.toml`](../Cargo.toml) `[workspace.package]`
   (the core and SDK share a version; plugins version independently).
2. If the SDK contract changed in a breaking way, bump `SDK_ABI_VERSION` in
   `plugins/sdk/src/lib.rs` and note it in [`CHANGELOG.md`](../CHANGELOG.md).
3. Move the `[Unreleased]` changelog section to the new version.

## Verify before publishing

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check

# Package both crates without uploading; the SDK has no path dependencies and
# can be verified end to end. The server resolves `adjutant-sdk` from crates.io,
# so its dry run succeeds only after the SDK is published.
cargo publish --dry-run -p adjutant-sdk
cargo publish --dry-run -p adjutant-server
```

## Publish

```bash
cargo publish -p adjutant-sdk
# wait for crates.io to index it, then:
cargo publish -p adjutant-server
cargo publish --dry-run -p adjutant-gear_locker   # only if you publish plugins
```

## Tag the release

```bash
git checkout main && git pull
git tag -a vX.Y.Z -m "Adjutant vX.Y.Z"
git push origin vX.Y.Z
```

The release workflow attaches the tarball and generated notes to the GitHub
release.

## Plugin authors: depending on the SDK

Prefer crates.io:

```toml
adjutant-sdk = "0.2"
```

To track an unreleased commit, pin a Git tag (not a branch):

```toml
adjutant-sdk = { git = "https://github.com/chezgoulet/adjutant", tag = "v0.2.0" }
```

Rebuild and `adjutant validate-plugin <so>` after any core upgrade — a plugin
built against a different SDK ABI is refused at load.
