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

**Status (2026-09-27, later the same day, PR #121 on `feature/client-offline-mode`):**
**four of the eight exit boxes are met, one is partial, three are open.** The
fourth is **offline mode**, which read NOT MET on the line above and is built,
tested and gated on this branch — see the box's own entry, and its evidence
section, below. Read the two lines together: the first is the branch at
`75981e5`, the second is the branch with the client's offline work on it, and
neither is rewritten (`plugin-roadmap.md` §7: a later figure is added beside the
old one).

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
  it too. The `client` job runs `flutter analyze` and **131 tests** at `75981e5`
  — **150** on PR #121's branch, which is where the offline work's own 19 live
  (see the evidence below). Since 2026-09-27 the live harness proves the app's
  own `ApiClient` against a booted server rather than a mock (see the evidence
  table).
- [~] **Flutter client works on Android and Web (PWA). — PARTIAL.** Web builds:
  `flutter build web --release` succeeds with the pinned toolchain (exit 0, a
  41 MB bundle), and the release workflow now attaches that bundle to a tag.
  Android is how the app reaches the 161st (Obtainium), but **no CI step builds
  it for any platform** — that is #111, and it is a gate defect rather than a
  client defect: nothing here proves the APK a scout installs is the tree we
  tagged. **iOS is deferred entirely** by owner decision (2026-09-26) — no
  runner, no distribution path, revisit after 1.0 — and SPEC's box says so.
- [x] **Offline mode: client caches data locally, syncs when online. — MET**
  (2026-09-27, PR #121). This box read **NOT MET** at `75981e5`, and that reading
  was right about the branch: the client's `OfflineException` was an error path
  with nothing stored and nothing queued, and the sentence there said so. What is
  built now, on the storage decision the box below names: a **durable read
  cache** plus a **queued write list**, both over `shared_preferences`, with **no
  schema on the device** (owner, 2026-09-27; drift/SQLite stays the answer if
  offline *queries* ever need one — `../plugin-roadmap.md` §6.2).

  - **The cache's rule, in one sentence: no TTL, and it is served only when the
    server cannot be reached.** A cached read never stands in front of a live
    answer, so age alone cannot make stale data look live — a screen showing
    cache is showing the offline banner and the hour its data is from
    (`isStale`/`cachedAt`, which the screens already rendered). What invalidates
    it is the end of the session: signing out drops every cached read *and* the
    queue, because a roster that outlives the session is one member's troop shown
    to the next, and a queue that outlives it would replay one person's change as
    whoever signs in next. The shell asks first when writes are still owed.
  - **Replay is ordered, and delivery is at-least-once — stated, not implied.**
    Entries leave the list in the order they were made, an entry behind an unsent
    one never jumps it, and the stored list is rewritten after each entry
    resolves, so an interrupted replay loses nothing and can at worst repeat the
    one entry in flight. That is why only writes the *server* treats as the same
    act when repeated are queued at all: a read receipt (`already_read`),
    forgetting one ("not an error"), a dues self-report (a setter for one member
    and one year), a plugin's enable/disable (a no-op the core states as such).
  - **A write that cannot be replayed is neither lost nor queued dishonestly.**
    A write the server *appends* — placing an order, opening a Checkout session,
    booking a draw — is refused at the call site, in words, while offline. A
    queued write the server answers **4xx** is parked in a second durable list
    with the server's own sentence, surfaced on a strip above the screen, and
    does not hold up the queue behind it; a **5xx** keeps its place and its
    attempt count and is parked the same way past three attempts. Nothing is
    dropped quietly, and nothing blocks the queue forever.
  - **The idempotency key is recorded, not transmitted.** It is a stable digest
    of the request (method, path, body) — which also makes *enqueue* idempotent,
    so the same write queued twice is one entry — and it is deliberately not sent
    as a header, because no route on this client's writing surface accepts one
    and a header the core ignores would be a promise this client cannot keep.
    Duplicate suppression stays the server's rule.
  - **Syncing is caused, not scheduled.** There is no timer: the queue is drained
    when the server has just proved reachable (a read that landed, a sign-in, a
    write that went through) or when the person presses the retry the app bar
    carries beside the pending count.

  `client/test/offline_test.dart` is the box's own proof — 19 tests over
  ordering and replay, the recorded request matching the one the online path
  sends, the refusal path, the cache surviving a window across a relaunch, and a
  live answer never being served from cache. The branch's `client` job reports
  **150 passed** (131 before it).
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

**Deliverable:** a working MVP scouts can use. Three boxes still stand between the
tree and that sentence; none of them is the application's shape, which is what
four met boxes and the M6 batch already demonstrate.

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

### The offline box's own evidence (2026-09-27, PR #121)

Read on `feature/client-offline-mode` at `32970cc` — the client change; the
commit above it touches only this document and `README.md`. CI run
[`36314548805`](https://github.com/chezgoulet/adjutant/actions/runs/36314548805)
is green on that head: `verify` (33 steps, including *Live client harness (real
server, real client)*), `client` and `msrv`, every step success.

| Gate | Result |
|---|---|
| `client` job — `Analyze` | `No issues found! (ran in 11.1s)` |
| `client` job — `Tests` | `🎉 150 tests passed.` — 131 at `75981e5`, 19 of them new (`test/offline_test.dart`) |
| `verify` — *Live client harness (real server, real client)* | `PASS  probe tally  (ran=9 expected=9 failed=0)`, `🎉 10 tests passed.`, `==> live client gate passed against http://127.0.0.1:8790` |
| `verify` (the other 32 steps: build, clippy `-D warnings`, docs `-D warnings`, `cargo-deny`, tests, the ladders) | all success on this head |

The live harness is the half that matters for a cache: it drives the app's own
`ApiClient` against a booted server, so a cache that swallowed a live response
would fail here rather than pass a mock. It was also run locally on this head
(`scripts/client-live-harness.sh`, against a server built from this branch) and
printed the same lines the CI step did:

```
==> staging plugins into plugins-built
14 libraries from 14 cdylib crates
==> creating the harness database
==> bootstrapping plugin roles into it
==> booting the server on http://127.0.0.1:8790 (dev headers OFF)
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

And the client-side commands, from the same reading:

```
$ export PATH="$HOME/flutter/bin:$PATH"; cd client
$ flutter --version
Flutter 3.47.5 • channel stable • revision 6a19cca564      # the pinned toolchain

$ flutter analyze --no-pub
No issues found! (ran in 1.9s)

$ flutter test --no-pub
00:16 +150: All tests passed!

$ flutter test --no-pub test/offline_test.dart
00:01 +19: All tests passed!
```

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

# The offline box (5): the cache, the queue, replay order, and the refusal path.
cd client && flutter test --no-pub test/offline_test.dart

# The web artifact a tag now carries (box 4's web half)
cd client && flutter build web --release && ls -la build/web/main.dart.js
```

## Not delivered at this milestone

Three boxes are open and one is partial, and each is a *thing to do* rather than a
thing to decide — except where the decision is named:

1. **The Hermes connect-and-interact proof (box 2).** Nothing here can stand in
   for it: the server is proven, the agent's path is not.
2. **The UI/UX review (box 7).** Authored by the owner.
3. **The deployment to the 161st (box 8).** Host decided 2026-09-27 (troop-owned
   hardware); the outside-reach question follows it.
4. **Android's artifact (box 4's other half).** Deferred to #111 rather than
   faked: the release workflow attaches the web bundle today and grows the APK
   when a CI step actually builds one.

Offline mode (box 5) was item 2 of this list until 2026-09-27; it left it in PR
#121, and it is the one entry this list has had that was a decision followed by
an implementation rather than a thing still owed.
