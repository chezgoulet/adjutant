#!/usr/bin/env python3
"""Fail when the places Adjutant states its version disagree (#177).

`Cargo.toml` `[workspace.package].version` is the source of truth. The core and
the SDK inherit it, so `adjutant --version` and `SDK_VERSION` are derived rather
than separate statements. Everything else that names the release version is
checked against it:

  * the `adjutant-sdk` workspace dependency (it has to be republished at the
    same version as the SDK crate);
  * `client/pubspec.yaml` (Android's `versionName` comes from this, so a stale
    value ships an APK that lies about the release);
  * the newest released heading in `CHANGELOG.md` (not `[Unreleased]`);
  * the MCP bridge's advertised `SERVER_VERSION`;
  * the tag, when one is being checked. On CI a tag push supplies it through
    `GITHUB_REF`; locally pass `--tag vX.Y.Z` before creating the tag.

Historical release pages and milestone notes are deliberately not checked: they
record what an old tag was, not what the current tree is. The separate WASM
workspace and the plugin crates version independently (SPEC §13.9).

No third-party dependency: stdlib only, so this runs in the cheapest CI job.
Exit status is 1 when any statement disagrees, with both values in the message.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO = ROOT / "Cargo.toml"
PUBSPEC = ROOT / "client" / "pubspec.yaml"
CHANGELOG = ROOT / "CHANGELOG.md"
MCP_BRIDGE = ROOT / "tools" / "mcp-bridge" / "adjutant_mcp_bridge.py"

# SemVer's core plus an optional pre-release, with build metadata discarded: a
# pubspec `0.3.0+7` is still release 0.3.0, and a tag never carries `+build`.
SEMVER_RE = re.compile(
    r"^(?P<version>\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(?:\+[0-9A-Za-z.-]+)?$"
)
PUBSPEC_VERSION_RE = re.compile(r"^version:\s*(\S+)\s*$", re.MULTILINE)
CHANGELOG_HEADING_RE = re.compile(r"^##\s+\[([^\]]+)\]", re.MULTILINE)
MCP_SERVER_VERSION_RE = re.compile(
    r"^SERVER_VERSION\s*=\s*[\"']([^\"']+)[\"']", re.MULTILINE
)


def normalise(value: str) -> str | None:
    """Return the version without build metadata, or None if it is not SemVer."""
    match = SEMVER_RE.match(value.strip())
    return match.group("version") if match else None


def tag_from_environment() -> str | None:
    ref = os.environ.get("GITHUB_REF", "")
    if ref.startswith("refs/tags/"):
        return ref.removeprefix("refs/tags/")
    if os.environ.get("GITHUB_REF_TYPE") == "tag":
        return os.environ.get("GITHUB_REF_NAME") or None
    return None


def read_cargo() -> tuple[str, str]:
    with CARGO.open("rb") as handle:
        manifest = tomllib.load(handle)
    try:
        version = manifest["workspace"]["package"]["version"]
        sdk_version = manifest["workspace"]["dependencies"]["adjutant-sdk"]["version"]
    except KeyError as missing:
        raise SystemExit(
            f"::error file=Cargo.toml::missing {missing} — cannot establish the "
            "source of truth"
        ) from missing
    return version, sdk_version


def first_match(pattern: re.Pattern[str], path: Path, what: str) -> str:
    match = pattern.search(path.read_text())
    if not match:
        raise SystemExit(f"::error file={path.relative_to(ROOT)}::no {what} found")
    return match.group(1)


def first_released_changelog_version() -> str:
    text = CHANGELOG.read_text()
    for match in CHANGELOG_HEADING_RE.finditer(text):
        version = normalise(match.group(1))
        if version is not None:
            return match.group(1)
    raise SystemExit(
        "::error file=CHANGELOG.md::no released '## [X.Y.Z]' heading found"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tag",
        help="version tag to check (for example v0.3.0); defaults to the tag "
        "in GITHUB_REF, if this is a tag push",
    )
    args = parser.parse_args()

    source_raw, sdk_raw = read_cargo()
    source = normalise(source_raw)
    if source is None:
        raise SystemExit(
            f"::error file=Cargo.toml::[workspace.package].version {source_raw!r} "
            "is not SemVer"
        )

    pubspec_raw = first_match(PUBSPEC_VERSION_RE, PUBSPEC, "top-level version:")
    changelog_raw = first_released_changelog_version()
    bridge_raw = first_match(
        MCP_SERVER_VERSION_RE, MCP_BRIDGE, "SERVER_VERSION assignment"
    )

    statements = [
        ("Cargo.toml", "[workspace.dependencies].adjutant-sdk", sdk_raw),
        ("client/pubspec.yaml", "top-level version", pubspec_raw),
        ("CHANGELOG.md", "newest released heading", changelog_raw),
        ("tools/mcp-bridge/adjutant_mcp_bridge.py", "SERVER_VERSION", bridge_raw),
    ]

    errors: list[str] = []
    for path, label, raw in statements:
        value = normalise(raw)
        if value is None:
            errors.append(f"{path} {label} is {raw!r}, which is not SemVer")
        elif value != source:
            errors.append(
                f"{path} {label} is {raw} (release {value}); Cargo.toml "
                f"[workspace.package].version is {source_raw} (release {source})"
            )

    tag = args.tag or tag_from_environment()
    if tag:
        tag_version = normalise(tag.removeprefix("v"))
        if tag_version is None:
            errors.append(f"tag {tag!r} does not contain a SemVer version")
        elif tag_version != source:
            errors.append(
                f"tag {tag} is release {tag_version}; Cargo.toml "
                f"[workspace.package].version is {source_raw} (release {source})"
            )

    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        print(
            "\nversion consistency FAILED — no release can be cut from this tree "
            "until the values above agree.",
            file=sys.stderr,
        )
        return 1

    print(f"version consistency OK: {source_raw}")
    print(f"  Cargo.toml [workspace.package].version = {source_raw} (source of truth)")
    print(f"  Cargo.toml [workspace.dependencies].adjutant-sdk = {sdk_raw}")
    print(f"  client/pubspec.yaml version = {pubspec_raw}")
    print(f"  CHANGELOG.md newest released heading = {changelog_raw}")
    print(f"  tools/mcp-bridge/adjutant_mcp_bridge.py SERVER_VERSION = {bridge_raw}")
    if tag:
        print(f"  tag = {tag}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
