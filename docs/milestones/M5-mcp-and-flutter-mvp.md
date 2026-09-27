# Milestone 5 — MCP Server + Flutter Client MVP + Calendar

SPEC §15 M5 is the milestone that puts the product in a scout's hand: the
permissions-aware MCP server, the Flutter client, and the calendar they actually
plan their lives in. It is the first release to real users, so its exit criteria
are the ones a troop can feel rather than a developer can assert.

**Status (2026-09-27, `testing` at `75981e5`):** **three of the eight exit boxes
are met, one is partial, four are open.** The two halves that were missing at the
last reading have landed: the client is now exercised against a **live server in
CI** (`scripts/client-live-harness.sh`, a step inside `verify`), and the plugin
set is something an operator chooses rather than something the server decides.
What remains is the MCP proof against a real Hermes agent, offline mode, the
owner's UI/UX review, and the deployment to the 161st — whose host decision is
now taken (troop-owned hardware, `plugin-roadmap.md` §6, 2026-09-27).

This record exists because M5 had none. Its boxes are SPEC §15's; the ordered work
against them is [`../release-path.md`](../release-path.md) Stage 1, which owns
what to do next. Where a number below is a run, it is that run's number and not a
claim about today (`plugin-roadmap.md` §7: a record's figures are dated, and a
later figure is added beside the old one rather than replacing it).

## Goal

> Ship the permissions-aware MCP server and a working Flutter client, with the
> calendar scouts actually plan their lives in. (SPEC §15)

## Exit criteria (SPEC §15 M5)

- [x] **MCP server plugin: tools exposed, permissions filtered, invocations
  logged.** `adjutant-mcp` declares four routes (`POST /api/mcp/connect`,
  `GET /api/mcp/tools`, `POST /api/mcp/invoke`, and the connection read) and
  twenty-two tests cover the three clauses by name:
  `tools_are_filtered_by_the_callers_permissions`,
  `an_unheld_permission_hides_its_tool_entirely`,
  `a_lodge_scoped_grant_sees_only_the_tools_it_can_reach`,
  `the_calendar_tools_are_invisible_until_their_plugin_is_installed`,
  `invoke_runs_the_tool_through_the_api_and_logs_it`, and
  `invoke_refuses_a_tool_the_caller_was_not_granted_and_logs_the_refusal`. An
  MCP tool *is* an API call: each tool names the permission its own route
  requires, the caller's credentials are forwarded rather than substituted, and
  the core's route gate checks the same permission a second time.
- [ ] **Hermes agent can connect and interact through the MCP server. — NOT
  MET.** The plugin is proven through its own handlers; nothing in this
  repository, and no open pull request, shows a real Hermes agent connecting,
  listing the tools its identity permits, and invoking one. This is the box the
  evidence for the *server* cannot stand in for, and Stage 1.2 carries it.
- [x] **Flutter client: login screen, mission list, membership roster, calendar,
  basic navigation.** The named screens exist and the app has outgrown them —
  announcements, dues, equipment, governance, store and the admin screens are in
  it too. The `client` job runs `flutter analyze` and **131 tests**, and since
  2026-09-27 the live harness proves the app's own `ApiClient` against a booted
  server rather than a mock (see the evidence table).
- [~] **Flutter client works on Android and Web (PWA). — PARTIAL.** Web builds:
  `flutter build web --release` succeeds with the pinned toolchain (exit 0, a
  41 MB bundle), and the release workflow now attaches that bundle to a tag.
  Android is how the app reaches the 161st (Obtainium), but **no CI step builds
  it for any platform** — that is #111, and it is a gate defect rather than a
  client defect: nothing here proves the APK a scout installs is the tree we
  tagged. **iOS is deferred entirely** by owner decision (2026-09-26) — no
  runner, no distribution path, revisit after 1.0 — and SPEC's box says so.
- [ ] **Offline mode: client caches data locally, syncs when online. — NOT
  MET.** The client's `OfflineException` is an error path, not a cache: nothing
  is stored on the device and nothing is queued for later. The storage decision
  that had to precede the work is taken (owner, 2026-09-27): a plain durable read
  cache plus a queued write list, with drift/SQLite held in reserve for offline
  *queries* rather than adopted up front.
- [x] **Calendar plugin: events, RSVPs, recurring events, quorum tracking.**
  `adjutant-calendar` has all four, with thirty-eight tests in
  `plugins/calendar/tests/calendar.rs`: events at troop or Lodge scope on the
  scoped-permission system, iCal `RRULE` recurrence expanded on the event's own
  timezone, RSVPs per occurrence with `EXDATE`, and `compute_quorum` — the same
  arithmetic governance uses for a Congress quorum, so the RSVP projection and
  the attendance-based number agree by construction rather than by convention.
- [ ] **Basic UI/UX review. — NOT MET.** Christopher authors it, as with the
  other deep review docs. The 48 dp finding that put Governance in Settings
  rather than a ninth navigation destination is its first entry.
- [ ] **Deployed to The House for real use by the 161st. — NOT MET.** The
  deployment is proven (proxy-fronted, `deploy/verify.sh`, and a restore drill
  from a destroyed volume) but no instance serves the troop. The host decision is
  taken (owner, 2026-09-27): hardware the 161st owns or is given. One question
  follows it and is not answered here — how that box is reached from outside,
  tunnel or port-forward — because it belongs with the troop's network.

**Deliverable:** a working MVP scouts can use. Four boxes still stand between the
tree and that sentence; none of them is the application's shape, which is what
three met boxes and the M6 batch already demonstrate.

## Evidence

Read **2026-09-27** on `testing` at `75981e5` — CI run
[`36288096546`](https://github.com/chezgoulet/adjutant/actions/runs/36288096546),
`verify`, `client` and `msrv` all success. PostgreSQL **18**.

| Gate | Result |
|---|---|
| `verify` — build, clippy `-D warnings`, docs `-D warnings`, `cargo-deny` | green on this head |
| `cargo test --workspace` | **573 passed / 0 failed / 65 ignored** (57 suites; the ignored set is the DB-gated probes, issue #25) |
| Route ladder (`adjutant test-plugin`) | **145/145** probes, 14 libraries staged |
| Live client harness (`scripts/client-live-harness.sh`, inside `verify`) | **10 passed**, 9 probes, `ran=9 expected=9 failed=0` |
| `client` job (`flutter analyze` + `flutter test`) | green, **131 tests** |
| `msrv` | green (1.96 floor) |

The live harness is the evidence this record exists for. Its own transcript, from
the run above:

```
bootstrapped 14 plugin role(s): announcements, archive, auth, calendar, conflicts,
  equipment, finance, governance, hello, mcp, membership, missions, store, stripe
==> booting the server on http://127.0.0.1:8790 (dev headers OFF)
==> server is listening (pid 9601)
==> running the live client gate
PASS  server health  (GET / -> 200 {"service":"adjutant","status":"ok"})
PASS  dev-header stub is OFF  (spoofed x-dev-user refused with 401)
PASS  login issues a session  (POST /api/auth/login -> token of 64 chars)
PASS  me reflects the server identity  (username=client-harness-chief roles=[chief])
PASS  members carries the stored roster row
PASS  lodges carries the created lodge  (names=[Harness Lodge])
PASS  motions carries the server stage  (id=1 rows=1 stage=proposed)
PASS  plugins lists the loaded registry
PASS  dead session refused  (bogus token -> 401 session expired or invalid)
PASS  probe tally  (ran=9 expected=9 failed=0)
🎉 10 tests passed.
==> live client gate passed against http://127.0.0.1:8790
```

Read what it is: the Flutter app's **own client code** — `ApiClient` and its
parsing, not a mock — talking to a real server on a real socket, with the
spoofable identity stub refused so the session it uses is one the server issued.
Before this step every client test drove a mocked `http.Client`, which proves the
client renders the shapes it *expects* and cannot prove those are the shapes the
server *sends*.

## How to reproduce the evidence

```bash
export PATH="$HOME/flutter/bin:$PATH"
export ADJUTANT_TEST_DATABASE_URL=postgres://$USER@127.0.0.1:55432/adjutant_dev_test
export ADJUTANT_ALLOW_SUPERUSER=true

cargo build --workspace
python3 scripts/stage-plugins.py                      # 14 libraries from 14 cdylib crates

# The MCP plugin's own proof (the three clauses of box 1)
cargo test -p adjutant-mcp
# The calendar's four features
cargo test -p adjutant-calendar

# The client against a live server, the same step CI runs (box 4's web half and
# box 3's live half). Needs psql and flutter on PATH.
scripts/client-live-harness.sh

# The web artifact a tag now carries (box 4's web half)
cd client && flutter build web --release && ls -la build/web/main.dart.js
```

## Not delivered at this milestone

Three boxes are open and one is partial, and each is a *thing to do* rather than a
thing to decide — except where the decision is named:

1. **The Hermes connect-and-interact proof (box 2).** Nothing here can stand in
   for it: the server is proven, the agent's path is not.
2. **Offline mode (box 5).** Storage decided 2026-09-27; the implementation is not
   started.
3. **The UI/UX review (box 7).** Authored by the owner.
4. **The deployment to the 161st (box 8).** Host decided 2026-09-27 (troop-owned
   hardware); the outside-reach question follows it.
5. **Android's artifact (box 4's other half).** Deferred to #111 rather than
   faked: the release workflow attaches the web bundle today and grows the APK
   when a CI step actually builds one.
