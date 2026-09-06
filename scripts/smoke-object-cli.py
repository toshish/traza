#!/usr/bin/env python3
"""Smoke-test the shipped object CLI without credentials, storage, or networking."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid


def container_command(image, name):
    return [
        "docker", "run", "--rm", "--network=none", "--read-only",
        "--cap-drop=ALL", "--security-opt=no-new-privileges",
        "--entrypoint=/traza-object", "--name", name, "--env=HOME=/nonexistent",
        "--env=AWS_EC2_METADATA_DISABLED=true", image,
    ]


def smoke(command, version, directory, container_name=None):
    directory = Path(directory).resolve()
    empty = directory / "empty-aws-config"
    empty.write_text("")
    # Allowlist the child environment: developer/runner credentials, proxies,
    # profiles, and executable-loader overrides must not reach archive tests.
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(directory), "TMPDIR": str(directory), "LC_ALL": "C",
        "AWS_SHARED_CREDENTIALS_FILE": str(empty), "AWS_CONFIG_FILE": str(empty),
        "AWS_EC2_METADATA_DISABLED": "true",
    }
    results = []
    for arguments in (["--version"], ["--help"], ["list"], ["list", "--endpoint", "https://invalid.example"]):
        try:
            completed = subprocess.run(
                [*command, *arguments], cwd=directory, env=environment,
                capture_output=True, text=True, timeout=20,
            )
        finally:
            if container_name:
                # A killed Docker client does not guarantee its container stops.
                # Reap only this smoke run's unique name, even on timeout.
                subprocess.run(
                    ["docker", "rm", "--force", container_name], cwd=directory,
                    env=environment, capture_output=True, timeout=10,
                )
        result = {
            "arguments": arguments, "exit_code": completed.returncode,
            "stdout": completed.stdout, "stderr": completed.stderr,
        }
        results.append(result)
        if arguments == ["--version"]:
            if completed.returncode != 0 or completed.stdout.strip() != f"traza-object {version}":
                raise ValueError(f"object CLI version differs from release {version}: {result}")
        elif arguments == ["--help"]:
            if completed.returncode != 0 or "USAGE:" not in completed.stdout or "--bucket" not in completed.stdout:
                raise ValueError(f"object CLI help failed or lacks backend configuration: {result}")
        elif (completed.returncode == 0 or completed.stdout
              or "--bucket" not in completed.stderr or "required" not in completed.stderr):
            raise ValueError(f"object CLI must reject missing bucket clearly before remote access: {result}")
    return {"version": version, "checks": results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--binary", type=Path)
    source.add_argument("--container")
    parser.add_argument("--version", required=True)
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args()
    container_name = "traza-object-smoke-" + uuid.uuid4().hex if args.container else None
    command = [str(args.binary.resolve())] if args.binary else container_command(args.container, container_name)
    try:
        with tempfile.TemporaryDirectory(prefix="traza-object-cli-smoke-") as directory:
            receipt = smoke(command, args.version, directory, container_name)
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error)) from error
    if args.receipt:
        args.receipt.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"object CLI smoke passed: {args.version}, help, and both missing-bucket errors")


if __name__ == "__main__":
    main()
