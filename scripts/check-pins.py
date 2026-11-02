#!/usr/bin/env python3
"""Fail when a workflow action or container image is not pinned (#181).

The policy this enforces:

  * every `uses:` in `.github/workflows/` is `owner/repo@<40-hex commit SHA>`,
    with the human-readable version in a trailing comment (Dependabot reads
    that comment when it updates the SHA). Local `./` actions are exempt.
  * every Dockerfile `FROM` carries an `@sha256:` digest (except `scratch`).
  * every `image:` in a Compose file or a workflow service carries an
    `@sha256:` digest.

No third-party YAML parser is needed: the files are checked line by line, and
this script is deliberately dependency-free so it can run in the cheapest CI
job. Exit status is 1 on the first policy violation, with every violation
printed as a GitHub annotation.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

USES_RE = re.compile(r"^\s*(?:-\s+)?uses:\s*(.+?)\s*$")
IMAGE_RE = re.compile(r"^\s*image:\s*(.+?)\s*$")
FROM_RE = re.compile(r"^\s*FROM\s+(.+?)\s*$", re.IGNORECASE)
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"@sha256:[0-9a-f]{64}$")


def strip_quotes(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def rel(path: Path) -> str:
    return str(path.relative_to(ROOT))


def workflow_files() -> list[Path]:
    return sorted(
        p
        for pattern in ("*.yml", "*.yaml")
        for p in (ROOT / ".github" / "workflows").glob(pattern)
    )


def compose_files() -> list[Path]:
    files = sorted(ROOT.glob("docker-compose*.yml")) + sorted(
        ROOT.glob("docker-compose*.yaml")
    )
    for pattern in ("*.yml", "*.yaml"):
        files += sorted((ROOT / "deploy").rglob(pattern))
    # The same file can match more than one pattern.
    return sorted(set(files))


def dockerfiles() -> list[Path]:
    skip = {".git", "target", "node_modules"}
    return sorted(
        p
        for p in ROOT.rglob("Dockerfile*")
        if not any(part in skip for part in p.relative_to(ROOT).parts)
    )


def check_workflow(path: Path, errors: list[str], ok: list[str]) -> None:
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        match = USES_RE.match(line)
        if not match:
            continue
        raw = match.group(1)
        value = strip_quotes(raw.split("#", 1)[0].strip())
        where = f"{rel(path)}:{lineno}"
        if value.startswith("./"):
            # A local action is this repository; there is no third party to pin.
            continue
        if "@" not in value:
            errors.append(f"{where}: `uses: {value}` has no @ref; expected a 40-hex commit SHA")
            continue
        ref = value.rsplit("@", 1)[1]
        if not SHA_RE.match(ref):
            errors.append(
                f"{where}: `uses: {value}` is not pinned to a 40-hex commit SHA "
                f"(got `{ref}`)"
            )
            continue
        comment = line.split("#", 1)[1].strip() if "#" in line else ""
        if not comment:
            errors.append(
                f"{where}: `uses: {value}` is pinned but has no trailing version "
                f"comment (Dependabot needs `# vX.Y.Z` to update it safely)"
            )
            continue
        ok.append(f"{where}  {value}  ({comment})")


def check_image_ref(where: str, value: str, errors: list[str], ok: list[str]) -> None:
    image = strip_quotes(value.split("#", 1)[0].strip())
    if not DIGEST_RE.search(image):
        errors.append(
            f"{where}: `{image}` is not digest-pinned; expected "
            f"`image@sha256:<64 hex>` (keep the tag in a comment)"
        )
        return
    ok.append(f"{where}  {image}")


def check_dockerfile(path: Path, errors: list[str], ok: list[str]) -> None:
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        match = FROM_RE.match(line)
        if not match:
            continue
        tokens = match.group(1).split()
        # Skip Dockerfile flags such as `--platform=linux/amd64`.
        while tokens and tokens[0].startswith("--"):
            tokens.pop(0)
        if not tokens:
            errors.append(f"{rel(path)}:{lineno}: `FROM` has no image")
            continue
        image = tokens[0]
        if image.lower() == "scratch":
            continue
        check_image_ref(f"{rel(path)}:{lineno}", image, errors, ok)


def check_images(path: Path, errors: list[str], ok: list[str]) -> None:
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        match = IMAGE_RE.match(line)
        if not match:
            continue
        check_image_ref(f"{rel(path)}:{lineno}", match.group(1), errors, ok)


def main() -> int:
    errors: list[str] = []
    ok: list[str] = []

    for path in workflow_files():
        check_workflow(path, errors, ok)
        check_images(path, errors, ok)
    for path in compose_files():
        check_images(path, errors, ok)
    for path in dockerfiles():
        check_dockerfile(path, errors, ok)

    for line in ok:
        print(f"  ok — {line}")
    if errors:
        print()
        for error in errors:
            # GitHub-specific annotation, plus plain text for a local run.
            print(f"::error::{error}")
            print(f"FAIL: {error}")
        print(f"\n{len(errors)} supply-chain pin(s) are missing")
        return 1

    actions = sum(1 for line in ok if "  (" in line)
    images = len(ok) - actions
    print(
        f"\nall pinned: {actions} workflow action ref(s), "
        f"{images} container image ref(s)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
