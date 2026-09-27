# The v0.3.0 cut — what it is, and what it is not

**Owner decision, 2026-09-27.** `testing` is merged to `main` and tagged
**`v0.3.0`**. This page is written *before* the tag and is carried by it, so the
tag's own tree says what the tag is; the run ids and the release URL are added
beside it afterwards (`plugin-roadmap.md` §7: a later figure is added, never
substituted).

## What the tag carries

Every artifact the release workflow produces, from this tree:

- `adjutant-v0.3.0-x86_64-unknown-linux-gnu.tar.gz` — the server binary and **all
  fourteen plugin libraries** (derived from the workspace, not listed), the WASM
  guest, README and LICENSE, with a `.sha256`;
- `adjutant-client-v0.3.0-web.tar.gz` — the client, built for the web;
- `adjutant-client-v0.3.0-android.apk` — the client as an APK for a phone
  (Obtainium is how the 161st receives it), **release mode, signed with Flutter's
  throwaway debug key**, with a `.sha256`. Nothing we publish is signed until
  beta, and the APK is the one artifact whose platform refuses an unsigned
  install — so it is debug-signed so it installs at all, and a properly signed
  build later may not upgrade over it cleanly.

## What the tag does **not** claim

The plan of record said v0.3.0 would be cut **"when it is actually deployable"**
— Stage 1's boxes plus the two deployment proofs — and described the gate as
*deployment-shaped*: the 161st runs the tag they can also host. **Two of M5's
eight boxes are open at this cut, and both are the deployment-shaped half:**

- **box 7 — the owner's UI/UX review.** Authored by Christopher; not written yet.
- **box 8 — deployed to The House for real use by the 161st.** The deployment is
  *proven* (proxy-fronted, `deploy/verify.sh`, a restore drill from a destroyed
  volume) but no instance serves the troop, and the host decision (hardware the
  161st owns or is given) is a decision rather than a running instance.

So this is a **code freeze, not the deployment-shaped gate the plan described.**
The distinction is the whole reason this page exists: a tag is easy to read as
"shipped", and on this tag two of the eight exit criteria for the milestone it
closes are still being worked. Everything else in M5 is met and green — six boxes,
with the evidence in [`milestones/M5-mcp-and-flutter-mvp.md`](milestones/M5-mcp-and-flutter-mvp.md).

## What changed under the release in this session

- **#119** — the `client` job compiles the client for web and Android instead of
  only analyzing it.
- **#121** — offline mode (M5 box 5): a durable read cache and a queued write
  list, replay ordered, refusals parked rather than dropped.
- **#120** — the MCP↔Hermes bridge and its two-identity proof (M5 box 2).
- **#125** — the client run on a real Android emulator against a real server
  (M5 box 4): installed, signed in, rendered server data, and walked the offline
  path on the device.
- **#126** — the tag attaches the APK, and the release workflow gains a
  `workflow_dispatch` entry so its own steps can be exercised without cutting a
  release (Stage 3.10 below).
- **the Android device gate** — `scripts/android-device-gate.sh` plus
  `client/integration_test/app_test.dart`: the hand-run above, as code, runnable
  by a person or by CI. **It is inert in CI** until this repository is granted
  the House's `android` runner (see below), because a `runs-on: [self-hosted, …]`
  job with no matching runner queues forever and would block every PR.

## Open, and why each one is not this tag's problem

- **The Android device gate is not yet a gate in CI.** The runner exists and is
  online (`sasquatch-runner`, labels `self-hosted, linux, x64, android`) but
  `/repos/chezgoulet/adjutant/actions/runners` reports zero: it is not in a runner
  group that includes this repository. A `runs-on: [self-hosted, …]` job with no
  matching runner does not fail — it **queues forever**. So the job ships behind
  the repository variable `ANDROID_DEVICE_GATE`, and switching it on is two
  clicks: add the repo to the runner group, set the variable to `true`.
- **The APK is not attached to a *signed* release**, and will not be until beta.
- **Three findings from the device run** (#122 the wizard's *"Skip for now"*
  silently switching twelve plugins off at runtime; #123 an API 404 rendered as
  "Cannot reach the server"; #124 no screen re-reads after a queued write
  replays) are **fixed in the PRs merged with this release** — where they are not,
  the individual PR says so.
- **#127** — a push to `release/*` or `hotfix/*` runs no CI until a PR opens;
  found while cutting this release.
