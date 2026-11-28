#!/usr/bin/env python3
"""Hold the client libraries' package metadata to the engine's.

Two things drift silently when a release is cut, so the gate checks both:

- The version. sdk/python and sdk/typescript carry the engine's version
  number in six places across five files. A version bump touches Cargo.toml,
  Cargo.lock and the dashboard; one that misses a client fails here, naming
  the file, instead of shipping a client that claims another release.
- The license files. Each package ships a copy of the repository's LICENSE
  and NOTICE, because Apache-2.0 4(a) and 4(d) require both to travel with a
  redistribution and neither registry reaches outside the package directory
  for them. A copy that differs from the root fails here.
"""

import argparse
import json
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parent.parent


def grab(root, path, pattern):
    text = (root / path).read_text(encoding="utf-8")
    match = re.search(pattern, text, re.M)
    if match is None:
        raise SystemExit(f"check-sdk-metadata: no version found in {path} (pattern {pattern!r})")
    return match.group(1)


def versions(root):
    """Each place a client states its version, mapped to what it states."""
    lock = json.loads((root / "sdk/typescript/package-lock.json").read_text(encoding="utf-8"))
    return {
        "sdk/python/pyproject.toml": grab(root, "sdk/python/pyproject.toml", r'^version\s*=\s*"([^"]+)"'),
        "sdk/python/src/traza/__init__.py": grab(
            root, "sdk/python/src/traza/__init__.py", r'^__version__\s*=\s*"([^"]+)"'
        ),
        "sdk/typescript/package.json": json.loads(
            (root / "sdk/typescript/package.json").read_text(encoding="utf-8")
        )["version"],
        "sdk/typescript/package-lock.json (version)": lock["version"],
        'sdk/typescript/package-lock.json (packages[""].version)': lock["packages"][""]["version"],
        "sdk/typescript/src/index.ts (VERSION)": grab(
            root, "sdk/typescript/src/index.ts", r'^export const VERSION = "([^"]+)";'
        ),
    }


def problems(root):
    engine = grab(root, "Cargo.toml", r'^version\s*=\s*"([^"]+)"')
    found = []
    for path, version in versions(root).items():
        if version != engine:
            found.append(f"{path} says {version}, but the engine (Cargo.toml) is {engine}")
    for package in ("sdk/python", "sdk/typescript"):
        for name in ("LICENSE", "NOTICE"):
            copy = root / package / name
            if not copy.is_file():
                found.append(f"{package}/{name} is missing; copy it from the repository root")
            elif copy.read_bytes() != (root / name).read_bytes():
                found.append(f"{package}/{name} differs from the repository root's {name}; copy it again")
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=ROOT, help="the repository to check (default: this one)")
    found = problems(parser.parse_args().root)
    if found:
        print("check-sdk-metadata: the client libraries are out of step with the engine:", file=sys.stderr)
        for line in found:
            print(f"  {line}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
