#!/usr/bin/env python3
"""Stage every plugin library into `plugins-built/`, derived from the workspace.

Why this exists. The staging step used to be a hand-written `cp` list, and so did
the `validate-plugin` list beside it. Both fell behind: they named five plugins
while the workspace had twelve, so the route ladder — the only gate that catches a
data-layer fault at request time — never probed calendar, mcp or the whole M6
batch. Two routes answered `500` on `testing` and CI stayed green. Issue #55.

The fix is to stop keeping the list. A plugin library is exactly a **publishable**
workspace crate that produces a `cdylib`, which `cargo metadata` knows and nobody
has to remember:

    python3 scripts/stage-plugins.py            # target/debug  -> plugins-built/
    python3 scripts/stage-plugins.py --profile release

"Publishable" is load-bearing, and it was learned the hard way. The rule used to
read "has no hand-written list", which is not the same as "what is a plugin": the
`broken-init-fixture` crate (`server/tests/plugin_load_semantics.rs`'s failed-load
fixture) is a workspace crate that builds a `cdylib` and is *not* a plugin, so it
was staged, discovered, and loaded — and the harness died on
`plugin broken_init init failed: internal error: fixture: init fails on purpose`.
A crate that exists to fail must not be something the loader is handed. `publish =
false` is the manifests' own way of saying "not part of the product", and every
real plugin is publishable, so the rule reads it rather than a name list.

Prints what it staged and fails loudly if a crate built a `cdylib` that is not on
disk, because a missing library is the failure this script exists to prevent.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
OUT = REPO / "plugins-built"


def cdylib_crates() -> list[tuple[str, str]]:
    """`(package name, lib target name)` for every plugin crate that builds a cdylib.

    Three exclusions, each by a rule rather than by a name:

    * the SDK is an `rlib` and exports no plugin symbols;
    * a crate with `publish = false` is a test fixture, not part of the product.
      `broken-init-fixture` builds a cdylib and must never be handed to the loader
      — see the module docstring for what happened when it was.
    """
    raw = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    meta = json.loads(raw)

    found: list[tuple[str, str]] = []
    for package in meta["packages"]:
        # `publish = false` arrives as an empty list; `None` means publishable.
        if package.get("publish") == []:
            continue
        for target in package.get("targets", []):
            kinds = target.get("crate_types") or []
            if "cdylib" in kinds:
                found.append((package["name"], target["name"]))
    return sorted(found)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", default="debug", help="cargo profile dir (debug, release)")
    args = parser.parse_args()

    libdir = REPO / "target" / args.profile
    OUT.mkdir(exist_ok=True)

    crates = cdylib_crates()
    if not crates:
        print("no cdylib crates found — the workspace is not what this expects", file=sys.stderr)
        return 1

    missing: list[str] = []
    staged: list[str] = []
    for _package, lib in crates:
        source = libdir / f"lib{lib}.so"
        if not source.exists():
            missing.append(source.name)
            continue
        shutil.copy2(source, OUT / source.name)
        staged.append(source.name)

    # Stale libraries are removed rather than left behind: a directory holding a
    # library from a previous build is worse than one holding none, because the
    # ABI handshake then refuses it and the load error looks like a plugin bug.
    keep = {name for name in staged}
    for existing in OUT.glob("libadjutant_*.so"):
        if existing.name not in keep:
            existing.unlink()
            print(f"removed stale {existing.name}")

    for name in staged:
        print(f"staged {name}")
    print(f"\n{len(staged)} libraries from {len(crates)} cdylib crates")

    if missing:
        print(
            "\nBUILT BUT NOT ON DISK (run `cargo build --workspace` first): "
            + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
