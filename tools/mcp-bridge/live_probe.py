#!/usr/bin/env python3
"""Drive the MCP bridge with a real MCP client and report PASS/FAIL.

This is the other half of `scripts/mcp-live-harness.sh`: the harness boots a
server, wires the bridge into an MCP host, and this file speaks the protocol
through the bridge as a client would — `initialize`, `tools/list`, `tools/call`.

It is deliberately a **separate process using a third-party MCP client** (the
`mcp` SDK), not the bridge's own code calling itself: a proof that reuses the
implementation under test proves the implementation agrees with itself.

Two scenarios, because "permissions filtered" is a comparison and not a number:

    live_probe.py chief   # a caller who holds every permission
    live_probe.py scout   # a caller who holds mcp:connect/invoke + membership:read

The scout must see **fewer** tools than the chief, must not see the one it cannot
use, and must be refused (by name, since a hidden tool is still invocable by name)
when it asks for it anyway. The harness asserts the audit rows after each run:
`ok` for the chief's call, `denied` for the scout's refusal.

Environment: ADJUTANT_BRIDGE (path to the bridge), ADJUTANT_BASE_URL,
ADJUTANT_SESSION or ADJUTANT_SESSION_FILE, ADJUTANT_EXPECT_FEWER_THAN.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

# The tool the scout must not see and must be refused: it needs `missions:create`,
# which that role does not hold.
GATED_TOOL = "missions_create_mission"
# A tool the chief both sees and can use (chief holds every permission at boot).
OPEN_TOOL = "membership_list_members"

_failed = 0
_ran = 0


def record(ok: bool, name: str, detail: str) -> None:
    global _failed, _ran
    _ran += 1
    print(f"{'PASS' if ok else 'FAIL'}  {name}  ({detail})", flush=True)
    if not ok:
        _failed += 1


def bridge_env() -> dict:
    """What the bridge needs, forwarded explicitly — the host does not hand over
    a shell environment, and a session is the one thing it must pass."""
    keep = ("ADJUTANT_BASE_URL", "ADJUTANT_SESSION", "ADJUTANT_SESSION_FILE", "ADJUTANT_CLIENT")
    env = {k: os.environ[k] for k in keep if k in os.environ}
    env.setdefault("PATH", os.environ.get("PATH", "/usr/bin:/bin"))
    env["ADJUTANT_MCP_VERBOSE"] = os.environ.get("ADJUTANT_MCP_VERBOSE", "0")
    return env


def first_text(result) -> str:
    """The first text block of a tool result, whatever else it carries.

    The SDK's content list is a union (text, image, audio, resource link,
    embedded resource), so a probe that indexes `content[0].text` blindly breaks
    the moment a tool returns anything else.
    """
    for block in getattr(result, "content", []) or []:
        text = getattr(block, "text", None)
        if isinstance(text, str):
            return text
    return json.dumps(getattr(result, "structured_content", None) or {}, sort_keys=True)[:200]


async def run(scenario: str) -> int:
    bridge = os.environ["ADJUTANT_BRIDGE"]
    described = os.environ.get("ADJUTANT_SCENARIO", scenario)
    params = StdioServerParameters(command=sys.executable, args=[bridge], env=bridge_env())

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            record(True, "initialize completes", f"scenario={described}")

            listed = await session.list_tools()
            names = [tool.name for tool in listed.tools]
            record(len(names) > 0, "tools/list returns tools", f"{len(names)} visible: {', '.join(sorted(names)[:4])}…")
            schemas = [tool for tool in listed.tools if not tool.input_schema]
            record(not schemas, "every listed tool carries an input schema", f"{len(names)} tools")

            if scenario == "chief":
                record(
                    GATED_TOOL in names,
                    "the chief sees the tool it may use",
                    f"{GATED_TOOL} present",
                )
                result = await session.call_tool(OPEN_TOOL, {})
                record(
                    not result.is_error,
                    "tools/call reaches the API",
                    f"{OPEN_TOOL} -> {first_text(result)[:120]}",
                )
            else:
                fewer_than = int(os.environ.get("ADJUTANT_EXPECT_FEWER_THAN", "0"))
                record(
                    fewer_than == 0 or len(names) < fewer_than,
                    "a narrower caller sees fewer tools",
                    f"{len(names)} < {fewer_than} (the chief's count)",
                )
                record(
                    GATED_TOOL not in names,
                    "a tool the caller cannot use is not advertised",
                    f"{GATED_TOOL} absent from {len(names)} tools",
                )
                result = await session.call_tool(
                    GATED_TOOL,
                    {
                        "title": "harness refusal probe",
                        "summary": "this call must be refused",
                        "body": "the scout holds no missions:create",
                    },
                )
                record(
                    bool(result.is_error),
                    "invoking a hidden tool by name is refused",
                    first_text(result)[:160],
                )

    record(_failed == 0, "probe tally", f"ran={_ran} failed={_failed}")
    return 1 if _failed else 0


def main() -> int:
    if len(sys.argv) < 2 or sys.argv[1] not in ("chief", "scout"):
        print("usage: live_probe.py chief|scout", file=sys.stderr)
        return 2
    return asyncio.run(run(sys.argv[1]))


if __name__ == "__main__":
    sys.exit(main())
