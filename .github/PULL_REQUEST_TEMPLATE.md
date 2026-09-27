<!--
  Delete the sections that do not apply. The checklist below is not ceremony:
  every box is a gate CI actually runs (see .github/workflows/ci.yml), written
  here so you can run the same commands locally and find the disagreement
  before CI does. The mechanics — branches, style, where things go — are in
  CONTRIBUTING.md; this file is about the pull request itself.
-->

## What and why

<!-- One sentence on the change, one on the motivation. The diff says what;
     only you can say why, and that is the part a reviewer cannot recover. -->

Closes #

## Gates

Run what your change touches. CI runs all of it on every push and pull
request, and a local result that disagrees with CI is worth a line in
"Notes for the reviewer" below.

**Rust — any change under `server/`, `plugins/`, `wasm/`, or a workspace
`Cargo.toml`:**

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --workspace`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
- [ ] `cargo deny check`
- [ ] If you touched a DB-backed suite (`server/tests/host_db.rs`,
      `pool_lifecycle`, anything `#[ignore]`d): it needs a test database —
      `ADJUTANT_TEST_DATABASE_URL`, pointed at a database whose name ends in
      `_test` — and a bare `cargo test` reports those as *ignored*, not passed.
      CI runs them explicitly, and so should you:
      `cargo test -p adjutant-server --test host_db --test pool_lifecycle -- --ignored --nocapture`

**Flutter client — any change under `client/`:**

- [ ] `flutter analyze --no-pub` reports zero findings
- [ ] `flutter test --no-pub`
- [ ] Your Flutter matches the pin in `client/pubspec.yaml`
      (`environment.flutter`) — that exact version is what CI installs, so a
      green run on a different one is a coincidence, not a result.

`CONTRIBUTING.md` §Gates has the "when relevant" extras (the probe ladder,
the `test-plugin` harnesses, the WASM guest) for when your change reaches them.

## Target branch

- [ ] This PR targets `testing`, not `main`. `testing` is the integration
      branch; `main` moves only to cut a release.

## What review expects

- [ ] One logical change per commit, small enough to read in one sitting.
- [ ] Comments explain **why**, not what.
- [ ] No new dependency without a stated reason.

## Notes for the reviewer

<!-- Where should they look hardest? What are you unsure of? What did you
     deliberately leave out of scope? -->
