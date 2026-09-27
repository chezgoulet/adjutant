# Wiring an MCP host to Adjutant

Adjutant exposes its functionality to an agent as MCP tools through the `mcp`
plugin. This document is how you connect a host to it, what the host then sees,
and what that has been proven to do.

Read [`api-reference.md`](api-reference.md) for the routes themselves and the
plugin's own module documentation (`plugins/mcp/src/lib.rs`) for the catalogue,
the permission model and the audit table. This file is the *client* half.

---

## The shape, and why there is a bridge

`adjutant-mcp` speaks **HTTP + JSON**, not JSON-RPC:

| Route | MCP analogue |
|---|---|
| `POST /api/mcp/connect` | opening a session — mints a connection token |
| `GET /api/mcp/tools` | `tools/list` |
| `POST /api/mcp/invoke` | `tools/call` |
| `GET /api/mcp/invocations` | the audit trail (own rows, or all with `mcp:audit`) |

That is deliberate, and the plugin says so: a JSON-RPC facade over those routes
"is a client concern, which is why the routes are named for what they do rather
than for a protocol revision."

**`tools/mcp-bridge/adjutant_mcp_bridge.py` is that concern, delivered.** It is a
stdio MCP server: your host launches it, it speaks newline-delimited JSON-RPC 2.0
on stdin/stdout, and it relays `tools/list` and `tools/call` to the routes above
**with the caller's own credentials**. Nothing is minted, upgraded or substituted
on the way through.

It is **standard library only** on purpose. The Python `mcp` SDK's 2.x release
removed `mcp.server.fastmcp`, which is what most Python MCP servers are written
against; a bridge built on it would break depending on which version your host
happens to carry. The stdio transport and the four methods a tool-listing host
needs are a small, stable subset, so the bridge has no install step and nothing
to pin.

---

## Configuring Hermes

One credential is needed: a **session** for the identity the agent should act as.
Get it the way the app does — `POST /api/auth/login` — and keep it in a file
rather than in your host's configuration:

```bash
mkdir -p ~/.config/adjutant
printf '%s' "$SESSION_TOKEN" > ~/.config/adjutant/mcp-session
chmod 600 ~/.config/adjutant/mcp-session
```

Then register the bridge. In a Hermes profile (this is the sanctioned
`hermes mcp add` path; the equivalent `mcp_servers:` block is below it):

```bash
HERMES_HOME=~/.hermes/profiles/<profile> hermes mcp add adjutant \
  --env ADJUTANT_BASE_URL=http://127.0.0.1:8787 \
        ADJUTANT_SESSION_FILE=$HOME/.config/adjutant/mcp-session \
        ADJUTANT_CLIENT=hermes \
  --command python3 \
  --args /path/to/adjutant/tools/mcp-bridge/adjutant_mcp_bridge.py
```

`--env` must come **before** `--args`: the args option is greedy and swallows
anything after it, which produces a connection that closes immediately and no
useful error. `hermes mcp test adjutant` should then print "✓ Connected" and the
tool list.

What that writes, for reference:

```yaml
mcp_servers:
  adjutant:
    command: python3
    args: ["/path/to/adjutant/tools/mcp-bridge/adjutant_mcp_bridge.py"]
    env:
      ADJUTANT_BASE_URL: "http://127.0.0.1:8787"
      ADJUTANT_SESSION_FILE: "/home/you/.config/adjutant/mcp-session"
      ADJUTANT_CLIENT: "hermes"
```

**Two deployment notes that will otherwise cost you an afternoon:**

- **`ADJUTANT_BASE_URL` must be an address the *server* can dial.** The plugin
  relays through the core's mediated HTTP client, so behind a reverse proxy you
  give the internal address (`http://127.0.0.1:8787`), never the public hostname.
  If it is wrong, tool calls answer `502 … could not reach the Adjutant API`.
  The same address is what the plugin's own `mcp.base_url` config holds.
- **The dev-header stub is not a credential and is not forwarded.** With
  `ADJUTANT_DEV_HEADERS=true` the MCP pre-check passes and the downstream route
  answers `401`. Give the bridge a real session.

---

## What the agent sees

The tool list is **the caller's own reach**, filtered by the permissions their
role and scope already carry — a tool the caller cannot invoke is not advertised,
and a listing and a permission check share one implementation. A given tool's
own route re-checks its permission on the forwarded request, so the agent path
and the UI path meet at a single authorization decision: an agent can reach
exactly what the same person can reach in the app, and nothing else.

Invoking a tool that was *not* advertised is still refused — by name, since
hiding is not enforcing — and both outcomes are recorded in `mcp.invocations`:
`ok`, `error`, and `denied`, which is the most security-relevant row in the
table. The response to a call carries its `invocation_id`, so a transcript can be
tied back to the audit row it produced.

---

## What has been proven

`scripts/mcp-live-harness.sh` boots a server on a database of its own, registers
the bridge with Hermes in a scratch `HERMES_HOME`, and drives two identities
through it. Run it with a database URL:

```bash
ADJUTANT_TEST_DATABASE_URL=postgres://you@127.0.0.1:5432/adjutant_dev_test \
  scripts/mcp-live-harness.sh
```

It asserts, and fails loudly otherwise:

- a **chief** connects, sees the tools their grants reach (11 at the time of
  writing), and calls one — the API answers and the row is recorded `ok`;
- a **scout** holding only `mcp:connect`, `mcp:invoke` and `membership:read`
  connects through the same bridge and sees **one** tool rather than eleven;
- that scout invoking `missions_create_mission` by name — a tool that was never
  advertised to them — is refused with the permission the route itself requires,
  recorded `denied`;
- and, when `hermes` is on `PATH`, that a real MCP host registers the bridge and
  discovers the tools (`hermes mcp test adjutant`).

The harness's own transcript from the run that closed M5 box 2:

```
==> chief: every tool their grants reach, and a real call
PASS  initialize completes  (scenario=chief)
PASS  tools/list returns tools  (11 visible: calendar_create_event, calendar_get_event, …)
PASS  every listed tool carries an input schema  (11 tools)
PASS  the chief sees the tool it may use  (missions_create_mission present)
PASS  tools/call reaches the API  (membership_list_members -> http_status: 200, invocation_id: 3)
PASS  probe tally  (ran=5 failed=0)
==> scout: one tool visible, and a refusal when they ask for another
PASS  a narrower caller sees fewer tools  (1 < 11 (the chief's count))
PASS  a tool the caller cannot use is not advertised  (missions_create_mission absent from 1 tools)
PASS  invoking a hidden tool by name is refused  (403: requires missions:create at any scope)
PASS  probe tally  (ran=6 failed=0)
PASS  audit trail: membership_list_members ok, missions_create_mission denied
```

**What this does not prove.** The LLM-in-the-loop round trip — an agent deciding
to call a tool and reading its result — is the same protocol path the SDK client
and `hermes mcp test` exercise, but a model's judgement is not what the gate is
for; the harness proves the *plumbing and the permissions*. If you want an agent's
transcript as evidence too, run one against your own profile: the config above is
all it needs.
