#!/usr/bin/env python3
"""Adjutant's MCP stdio bridge — the JSON-RPC facade an MCP host speaks to.

## Why this exists

`adjutant-mcp` exposes the MCP *surface* as HTTP + JSON: `POST /api/mcp/connect`
mints a connection, `GET /api/mcp/tools` is the `tools/list` analogue and
`POST /api/mcp/invoke` the `tools/call` analogue. That plugin's own documentation
says the rest is a client concern:

> a JSON-RPC facade over these two (for a stdio/SSE transport) is a client
> concern, which is why the routes are named for what they do rather than for a
> protocol revision.

This file **is** that client concern, and it is the thing that lets a real MCP
host — Hermes, Claude Desktop, any `stdio` client — see Adjutant's tools without
that host knowing anything about Adjutant's HTTP shape.

## What it does

It speaks MCP over `stdio` (newline-delimited JSON-RPC 2.0) and relays to the
HTTP surface with **the caller's own credentials**:

* on the first call it runs the MCP `initialize` handshake against
  `/api/mcp/connect` and keeps the connection token the server mints;
* `tools/list` → `GET /api/mcp/tools` with `mcp-session-id: <token>`;
* `tools/call` → `POST /api/mcp/invoke` with the same token.

No credential is invented, upgraded or substituted here: the bridge holds the
session it was given and relays it. Adjutant re-checks the tool's own permission
on the forwarded request, so the agent path and the UI path meet at one
authorization decision, and every invocation is logged in `mcp.invocations`
(`ok`, `denied` and `error` alike).

## Configuration (environment)

| Variable | Required | Meaning |
|---|---|---|
| `ADJUTANT_BASE_URL` | no | Adjutant's address; default `http://127.0.0.1:8787`. Behind a proxy give the **internal** address. |
| `ADJUTANT_SESSION` | one of these | A real session token — the `token` from `POST /api/auth/login`, or the value of the session cookie. Sent as `Authorization: Bearer …`. |
| `ADJUTANT_SESSION_FILE` | one of these | **Preferred.** A file holding that token. The MCP host then keeps no credential in its own configuration — the credential lives in a file this bridge reads (mode 0600), which is the same pattern the House uses for its other MCP servers. |
| `ADJUTANT_CLIENT` | no | The client name recorded on the connection row; default `adjutant-mcp-bridge`. |
| `ADJUTANT_MCP_TIMEOUT` | no | Per-request timeout in seconds; default 30. |
| `ADJUTANT_MCP_VERBOSE` | no | `1` logs relayed calls to stderr (never stdout — stdout is the protocol). |

**The dev-header stub cannot be used here.** `x-dev-user`/`x-dev-role` are
deliberately not forwarded by the plugin: a stub identity is not a credential.
Give the bridge a session the server issued.

## Why stdlib and not the `mcp` SDK

The Python SDK's 2.x release removed `mcp.server.fastmcp`, which is what every
Python MCP server is written against; a bridge built on it would break on
whatever version the host happens to have. The stdio transport is
newline-delimited JSON-RPC 2.0, and the four methods an MCP host needs to list
and call tools (`initialize`, `notifications/initialized`, `tools/list`,
`tools/call`) are a small, stable subset. Standard library only means this runs
under any Python 3 on the host, in the host's own environment, with no install
step and nothing to pin.

Run it by hand (the file form is preferred, and is what the config block in
`docs/mcp-hermes.md` uses):

    printf '%s' "$SESSION_TOKEN" > ~/.config/adjutant/mcp-session
    chmod 600 ~/.config/adjutant/mcp-session
    ADJUTANT_SESSION_FILE=~/.config/adjutant/mcp-session \
      ADJUTANT_BASE_URL=http://127.0.0.1:8787 \
      python3 tools/mcp-bridge/adjutant_mcp_bridge.py

`scripts/mcp-live-harness.sh` boots a server, registers it with a real MCP host
and proves the whole path.
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
import urllib.error
import urllib.request

PROTOCOL_VERSION = "2025-06-18"
SERVER_NAME = "adjutant"
SERVER_VERSION = "0.3.0"


def _log(message: str) -> None:
    """Progress goes to stderr — stdout is the protocol stream."""
    print(message, file=sys.stderr, flush=True)


class AdjutantError(RuntimeError):
    """A relayed call failed in a way the model should see."""


class Bridge:
    def __init__(self) -> None:
        self.base_url = os.environ.get("ADJUTANT_BASE_URL", "http://127.0.0.1:8787").rstrip("/")
        self.session = self._resolve_session()
        self.client_name = os.environ.get("ADJUTANT_CLIENT", "adjutant-mcp-bridge")
        self.timeout = float(os.environ.get("ADJUTANT_MCP_TIMEOUT", "30"))
        self.verbose = os.environ.get("ADJUTANT_MCP_VERBOSE") == "1"
        self.connection_token: str | None = None
        self.protocol_version = PROTOCOL_VERSION
        self.server_info = {"name": SERVER_NAME, "version": SERVER_VERSION}
        if not self.session:
            raise SystemExit(
                "a session is required: set ADJUTANT_SESSION, or ADJUTANT_SESSION_FILE to a "
                "0600 file holding the token from POST /api/auth/login. The dev-header stub is "
                "not a credential and is not forwarded by the plugin."
            )

    @staticmethod
    def _resolve_session() -> str:
        """The caller's session, inline or from a file.

        The file form is preferred: it keeps the credential out of whatever
        configuration the MCP host keeps, which is the same reasoning the House
        applies to its other MCP servers.
        """
        inline = (os.environ.get("ADJUTANT_SESSION") or "").strip()
        if inline:
            return inline
        path = (os.environ.get("ADJUTANT_SESSION_FILE") or "").strip()
        if not path:
            return ""
        try:
            return pathlib.Path(path).expanduser().read_text(encoding="utf-8").strip()
        except OSError as error:
            raise SystemExit(f"cannot read ADJUTANT_SESSION_FILE {path}: {error}") from error

    # --- transport -------------------------------------------------------

    def _request(self, method: str, path: str, body: dict | None = None) -> tuple[int, dict]:
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base_url + path, data=data, method=method)
        request.add_header("Authorization", f"Bearer {self.session}")
        request.add_header("Accept", "application/json")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        if self.connection_token:
            # The header an MCP client sends naturally, per the plugin's own
            # `connection_token()`: body first, then this.
            request.add_header("mcp-session-id", self.connection_token)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read().decode() or "{}"
                return response.status, json.loads(raw)
        except urllib.error.HTTPError as error:
            raw = error.read().decode()
            try:
                payload = json.loads(raw)
            except json.JSONDecodeError:
                payload = {"error": raw.strip() or error.reason}
            if self.verbose:
                _log(f"  ← {error.code} {path}: {payload.get('error')}")
            return error.code, payload
        except urllib.error.URLError as error:
            raise AdjutantError(f"cannot reach Adjutant at {self.base_url}: {error.reason}") from error

    # --- relayed surface -------------------------------------------------

    def connect(self) -> None:
        """Mint an MCP connection, the way a handshake would."""
        status, payload = self._request(
            "POST",
            "/api/mcp/connect",
            {
                "client": self.client_name,
                "protocolVersion": PROTOCOL_VERSION,
            },
        )
        if status >= 300 or "token" not in payload:
            raise AdjutantError(
                f"POST /api/mcp/connect answered {status}: {payload.get('error', payload)} "
                "(the session may lack mcp:connect, or the server may be unreachable)"
            )
        self.connection_token = payload["token"]
        self.protocol_version = payload.get("protocolVersion", PROTOCOL_VERSION)
        self.server_info = payload.get("server", self.server_info)
        if self.verbose:
            _log(f"  connected as {self.client_name} (connection {payload.get('connection_id')})")

    def tools(self) -> list[dict]:
        if not self.connection_token:
            self.connect()
        status, payload = self._request("GET", "/api/mcp/tools")
        if status >= 300:
            raise AdjutantError(f"GET /api/mcp/tools answered {status}: {payload.get('error', payload)}")
        tools = payload.get("tools", [])
        # MCP's tool shape is name/description/inputSchema. Adjutant adds
        # requiredPermission and scope, which are harmless to a strict client and
        # useful to a human reading the transcript — so they are passed through.
        return tools

    def call(self, name: str, arguments: dict) -> dict:
        if not self.connection_token:
            self.connect()
        status, payload = self._request(
            "POST",
            "/api/mcp/invoke",
            {"tool": name, "arguments": arguments},
        )
        if status >= 300:
            # A refusal is the model's to read, not a transport failure: it is
            # reported as a tool error with the server's own words.
            raise AdjutantError(f"{status}: {payload.get('error', payload)}")
        return payload

    # --- MCP -------------------------------------------------------------

    def handle(self, message: dict) -> dict | None:
        method = message.get("method")
        identifier = message.get("id")

        if method == "initialize":
            return self._result(
                identifier,
                {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                    "instructions": (
                        "Adjutant, a scout troop's administration server. The tools here are the "
                        "troop's API, filtered to what this caller's role and scope already allow: "
                        "an agent can reach exactly what the same person reaches in the app."
                    ),
                },
            )

        if method in ("notifications/initialized", "initialized"):
            return None  # a notification: no reply, ever

        if method == "ping":
            return self._result(identifier, {})

        if method == "tools/list":
            try:
                tools = self.tools()
            except AdjutantError as error:
                return self._error(identifier, -32000, str(error))
            return self._result(identifier, {"tools": tools})

        if method == "tools/call":
            params = message.get("params") or {}
            name = params.get("name", "")
            arguments = params.get("arguments") or {}
            try:
                payload = self.call(name, arguments)
            except AdjutantError as error:
                # The server's own refusal, as a tool result the model reads.
                return self._result(
                    identifier,
                    {"content": [{"type": "text", "text": str(error)}], "isError": True},
                )
            text = json.dumps(payload, indent=2, sort_keys=True) if not isinstance(payload, str) else payload
            return self._result(
                identifier,
                {"content": [{"type": "text", "text": text}], "isError": False},
            )

        return self._error(identifier, -32601, f"method not found: {method!r}")

    @staticmethod
    def _result(identifier: object, result: dict) -> dict:
        return {"jsonrpc": "2.0", "id": identifier, "result": result}

    @staticmethod
    def _error(identifier: object, code: int, message: str) -> dict:
        return {"jsonrpc": "2.0", "id": identifier, "error": {"code": code, "message": message}}


def main() -> int:
    bridge = Bridge()
    if bridge.verbose:
        _log(f"adjutant-mcp-bridge → {bridge.base_url}")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            # A parse error has no id to answer, and a stdio host must not be
            # killed by one bad line.
            continue
        try:
            response = bridge.handle(message)
        except Exception as error:  # noqa: BLE001 — one bad message must not end the session
            response = Bridge._error(message.get("id"), -32603, f"bridge error: {error}")
        if response is not None:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
