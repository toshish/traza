#!/usr/bin/env python3
"""Classify release tags before any build or registry mutation."""

import argparse
import json
import re
from pathlib import Path


NUMBER = r"(?:0|[1-9][0-9]*)"
TAG = re.compile(
    rf"v({NUMBER}\.{NUMBER}\.{NUMBER}(?:-(?:preview|alpha|beta|rc)\.[1-9][0-9]*)?)"
)


def classify(tag):
    match = TAG.fullmatch(tag)
    if not match:
        raise ValueError(
            "release tag must be vX.Y.Z or vX.Y.Z-{preview,alpha,beta,rc}.N "
            "with canonical numbers and N >= 1"
        )
    version = match.group(1)
    prerelease = "-" in version
    return {
        "version": version,
        "prerelease": prerelease,
        "container_alias": "preview" if prerelease else "latest",
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args(argv)
    try:
        result = classify(args.tag)
    except ValueError as error:
        parser.error(str(error))
    # Validate the complete tag before opening a caller-supplied output file.
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as output:
            for key, value in result.items():
                if isinstance(value, bool):
                    value = str(value).lower()
                output.write(f"{key}={value}\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
