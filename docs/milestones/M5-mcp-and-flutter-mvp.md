# Milestone 5 — MCP Server + Flutter Client MVP + Calendar

SPEC §15 M5 is the milestone that puts the product in a scout's hand: the
permissions-aware MCP server, the Flutter client, and the calendar they actually
plan their lives in. It is the first release to real users, so its exit criteria
are the ones a troop can feel rather than a developer can assert.

**Status (2026-09-27, `testing` at `2e5663e`, updated the same day):** **four of
the eight exit boxes are met, one is partial, three are open.** The three halves
that were missing at the last reading have landed one by one: the client is
exercised against a **live server in CI** (`scripts/client-live-harness.sh`, a
step inside `verify`), the plugin set is something an operator chooses rather than
something the server decides, and the **MCP connect-and-interact proof** now runs
against two identities with a real MCP host in the loop
([`../mcp-hermes.md`](../mcp-hermes.md)). What remains is offline mode, the
Android artifact, the owner's UI/UX review, and the deployment to the 161st —
whose host decision is taken (troop-owned hardware, `plugin-roadmap.md` §6).

**Status (2026-09-27, later the same day, PR #121 on `feature/client-offline-mode`):**
**four of the eight exit boxes are met, one is partial, three are open.** The
fourth is **offline mode**, which read NOT MET on the line above and is built,
tested and gated on this branch — see the box's own entry, and its evidence
section, below. Read the two lines together: the first is the branch at
`75981e5`, the second is the branch with the client's offline work on it, and
neither is rewritten (`plugin-roadmap.md` §7: a later figure is added beside the
old one).

**Status (2026-09-27, `testing` with #119, #120 and #121 merged):** **five of the
eight exit boxes are met, one is partial and two are open.** The fifth is the
**MCP connect-and-interact proof** (box 2, #120), beside offline mode (box 5,
#121); what remains is the **device half** of Android/Web (box 4 — its build half
closed with #119), the owner's UI/UX review (box 7) and the deployment to the
161st (box 8). Three readings, one page: each is dated, and none is rewritten.

**Status (2026-09-27, later still — the emulator run):** **six of the eight exit
boxes are met and two are open.** The sixth is **box 4**: its build half closed
with #119, and its running half was closed on a real Android emulator on this
host's Thelio — the APK built from `testing` signed in to a real server and
rendered real data, then did box 5's offline path on the device too. The evidence
is [`../evidence/m5-emulator-run.json`](../evidence/m5-emulator-run.json) and the
six screenshots beside it; what is still owed there is the APK as a *release*
artifact, not the client working. Four readings, one page.

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
- [x] **Hermes agent can connect and interact through the MCP server. — MET
  (2026-09-27).** The plugin's HTTP surface is what a host speaks through
  `tools/mcp-bridge/adjutant_mcp_bridge.py` — the JSON-RPC facade the plugin's own
  docs name as "a client concern" — and `scripts/mcp-live-harness.sh` proves the
  path with it: a **chief** connects and sees the tools their grants reach, calls
  one and the API answers (`ok` in `mcp.invocations`); a **scout** holding only
  `mcp:connect`, `mcp:invoke` and `membership:read` sees **one** tool rather than
  eleven, and invoking `missions_create_mission` by name is refused with the
  permission the route requires, recorded `denied`. A real MCP host is in the
  loop too: the harness registers the bridge with Hermes in a scratch
  `HERMES_HOME` and `hermes mcp test adjutant` discovers all 11 tools. Wiring and
  notes: [`../mcp-hermes.md`](../mcp-hermes.md).
- [x] **Flutter client: login screen, mission list, membership roster, calendar,
  basic navigation.** The named screens exist and the app has outgrown them —
  announcements, dues, equipment, governance, store and the admin screens are in
  it too. The `client` job runs `flutter analyze` and **131 tests** at `75981e5`
  — **150** on PR #121's branch, which is where the offline work's own 19 live
  (see the evidence below). Since 2026-09-27 the live harness proves the app's
  own `ApiClient` against a booted server rather than a mock (see the evidence
  table).
- [x] **Flutter client works on Android and Web (PWA). — MET** (2026-09-27, the
  emulator run). This box read PARTIAL in the three readings above, for a reason
  that was real: the client had been *compiled* for Android since #119, but never
  *run*, and a build that has never been launched is a compile, not a working app.
  The running half is evidence now, not a claim. A **debug APK built from the
  `testing` tree** (the tree of `406c315` is byte-identical to `d613a34`,
  `git diff --stat` empty; 160,090,535 bytes, sha256 `1c90beeb…31a0`) was installed
  on Thelio's Android emulator, signed in through the app's own login form to a
  real server over the LAN, and rendered that server's dashboard and roster. It was
  then run through the offline story on the device: with the server stopped, the
  Members screen showed **"Offline — showing data from 08:06"** over the cached
  roster; a receipt tapped on a notice was **queued rather than applied locally**
  ("Offline — your receipt is queued and will sync."), and the shell's app bar
  carried the pending-write badge; when the server came back the retry reported
  **"1 sent."** and the server held the receipt with `read_via: flutter` — the one
  the device queued, not a second one. Every step, its on-screen text and the
  server-side answer are in
  [`../evidence/m5-emulator-run.json`](../evidence/m5-emulator-run.json), with the
  six screenshots beside it.

  Three residuals are named rather than glossed: the device is an **x86_64
  emulator**, not a phone; the build is **debug**, because a release build refuses
  cleartext `http` by design (`client/lib/api/server_address.dart`); and **nothing
  in CI installs an APK on a device** — this host has a self-hosted runner with a
  full Android emulation suite, but this repository has no Android job, so the
  running half is re-proved by hand and not by the gate. The web half is unchanged:
  `flutter build web --release` succeeds with the pinned toolchain (exit 0, a 41 MB
  bundle), and the release workflow attaches that bundle to a tag. **iOS is
  deferred entirely** by owner decision (2026-09-26) — no runner, no distribution
  path, revisit after 1.0 — and SPEC's box says so.
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

**Deliverable:** a working MVP scouts can use. Two boxes still stand between the
tree and that sentence; none of them is the application's shape, which is what six
met boxes and the M6 batch already demonstrate.

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
is green on that head: `verify` (41 steps, step 36 being *Live client harness (real
server, real client)*), `client` and `msrv`, every step success.

| Gate | Result |
|---|---|
| `client` job — `Analyze` | `No issues found! (ran in 11.1s)` |
| `client` job — `Tests` | `🎉 150 tests passed.` — 131 at `75981e5`, 19 of them new (`test/offline_test.dart`) |
| `verify` — *Live client harness (real server, real client)* | `PASS  probe tally  (ran=9 expected=9 failed=0)`, `🎉 10 tests passed.`, `==> live client gate passed against http://127.0.0.1:8790` |
| `verify` (the other 32 steps: build, clippy `-D warnings`, docs `-D warnings`, `cargo-deny`, tests, the ladders) | all success on this head |

The live harness is the half that matters for a cache: it drives the app's own
`ApiClient` against a booted server, so a cache that swallowed a live response
would fail here rather than pass a mock. The transcript below is that CI step's
own output, read from the run's log rather than summarised:

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

It was run on this host against this head's own server as well — the same harness,
with `ADJUTANT_CLIENT_LIVE_PORT=8795` because another harness held 8790 — and it
ended the same way:

```
PASS  probe tally  (ran=9 expected=9 failed=0)
🎉 10 tests passed.
==> live client gate passed against http://127.0.0.1:8795
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

### The emulator run — box 4's running half, and box 5 on a device (2026-09-27)

Read on Thelio (`sasquatch`) against the **tree of `testing`**, and recorded in
full in [`../evidence/m5-emulator-run.json`](../evidence/m5-emulator-run.json)
with six screenshots beside it. The short version, as the device printed it:

```
$ adb -s emulator-5554 install -r app-debug.apk
Success                       # 160,090,535 bytes, sha256 1c90beeb…31a0, built
                              # from `406c315` — the tree of `d613a34`

# signed in to a real server at http://192.168.1.7:8787 through the app's own form
Dashboard   Active missions 0 · Members 1 · Upcoming events 0 · Open motions 1
Members     Harness Scout / Harness Patrol · Harness Lodge / Active

# the server stopped:
Members     Offline — showing data from 08:06        (cached roster still shown)
Inbox       Offline — your receipt is queued and will sync.   (badge: 1 pending,
            notice STILL unread — the write was not applied locally)

# the server back, retry tapped:
Inbox       1 sent.                                   (offline icon gone)

# re-entered the inbox:
Inbox       0 unread of 1 in this inbox · Mark unread  (agrees with the server)

$ curl … /api/announcements/announcement/1
is_read=true  read_count=1  my_receipt.read_via="flutter"   # the queued receipt
```

Three findings from the run are filed as issues rather than left as prose, each
with its on-screen evidence in the same file: **#122** — the first-run plugin
wizard's *"Skip for now"* records a choice of the two required plugins and unloads
the other twelve at runtime, while `GET /api/plugins` still lists them; **#123** —
the client renders an API **404 as an offline condition** ("Cannot reach the
server — route not found"); **#124** — after the shell's retry reports "1 sent.",
**no screen is told to re-read**, so a badge can stay stale until the destination
is re-entered. None of the three is in this box's way: the write landed, the
server agreed, and the run is what found them.

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

# The emulator run (box 4's running half, and box 5 on a device). On the Android
# emulator host, which needs the SDK + JDK + Flutter and a booted AVD; the server
# side is any Adjutant with the client harness's database.
~/build-adj-apk.sh <sha>                     # git clone, checkout, flutter build apk --debug
adb -s emulator-5554 install -r client/build/app/outputs/flutter-apk/app-debug.apk

# The web artifact a tag now carries (box 4's web half)
cd client && flutter build web --release && ls -la build/web/main.dart.js
```

## Not delivered at this milestone

Two boxes are open — no partial is left — and each is a *thing to do* rather than
a thing to decide — except where the decision is named:

1. **The UI/UX review (box 7).** Authored by the owner.
2. **The deployment to the 161st (box 8).** Host decided 2026-09-27 (troop-owned
   hardware); the outside-reach question follows it.
3. **The APK as a release artifact (box 4's residual).** The client on Android is
   proven — built by CI (#119), then installed and run on an emulator against a real
   server (the run in the evidence above). What remains is narrower: the release
   workflow still attaches only the **web** bundle, so the APK a tag produces is
   not yet built there; and no CI step installs one on a device, so nothing on the
   branch re-checks the running half if a change breaks it.

Three entries have left this list, and all three left the same way — a decision
first, then an implementation: **offline mode** (box 5) in #121, the **Hermes
connect-and-interact proof** (box 2) in #120, and the **device half of Android**
(box 4) in the emulator run recorded above.
