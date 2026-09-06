#!/usr/bin/env python3
"""Preserve packaged Rust license texts for the locked release feature graph."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile


TARGETS = (
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
)
NOTICE_NAME = re.compile(r"^(?:licen[sc]e|notice|copying|copyright)(?:[-._]|$)", re.I)
ROOT = Path(__file__).resolve().parent.parent


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def reachable(metadata):
    """Follow the target-filtered graph, including build and macro dependencies."""
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [metadata["resolve"]["root"]]
    found = set()
    while pending:
        package = pending.pop()
        if package in found:
            continue
        found.add(package)
        pending.extend(dep["pkg"] for dep in nodes[package]["deps"])
    return found


def lock_checksums(lock):
    """Read Cargo's canonical generated lockfile package checksums."""
    result = {}
    for block in re.split(r"(?m)^\[\[package\]\]\s*$", lock):
        fields = dict(re.findall(r'(?m)^(name|version|source|checksum) = "([^"\n]+)"$', block))
        if "checksum" in fields:
            result[(fields["name"], fields["version"], fields["source"])] = fields["checksum"]
    return result


def license_files(package, checksum, archive=None):
    """Read texts directly from the checksum-verified published crate archive."""
    root = Path(package["manifest_path"]).parent
    prefix = f"{package['name']}-{package['version']}"
    if archive is None:
        archive = root.parent.parent.parent / "cache" / root.parent.name / (prefix + ".crate")
    with archive.open("rb") as source:
        hasher = hashlib.sha256()
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
        digest = hasher.hexdigest()
    if digest != checksum:
        raise ValueError(f"crate archive differs from Cargo.lock: {package['name']} {package['version']}")
    explicit = package.get("license_file")
    if explicit and Path(explicit).is_absolute():
        explicit = Path(explicit).relative_to(root).as_posix()
    result = []
    with tarfile.open(archive, "r:gz") as source:
        for member in sorted(source.getmembers(), key=lambda member: member.name):
            path = Path(member.name)
            if not (NOTICE_NAME.match(path.name) or (explicit and member.name == f"{prefix}/{explicit}")):
                continue
            if member.isdir():
                continue
            if path.is_absolute() or ".." in path.parts or path.parts[0] != prefix or not member.isfile():
                raise ValueError(f"license file escapes package or is not a regular file: {member.name}")
            if member.size > 2 * 1024 * 1024:
                raise ValueError(f"oversized license file needs review: {member.name}")
            data = source.extractfile(member).read()
            if not data.strip() or b"\0" in data:
                raise ValueError(f"empty or binary license file: {member.name}")
            result.append((path.relative_to(prefix).as_posix(), sha256(data), data.decode("utf-8")))
    if not result:
        raise ValueError(f"no packaged license texts: {package['name']} {package['version']}")
    if explicit and not any(path == explicit for path, _, _ in result):
        raise ValueError(f"declared license file is absent from crate: {explicit}")
    return result


def generate(root):
    packages = {}
    checksums = lock_checksums((root / "Cargo.lock").read_text())
    for target in TARGETS:
        raw = subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--features",
             "object-storage", "--filter-platform", target], cwd=root,
        )
        metadata = json.loads(raw)
        included = reachable(metadata)
        for package in metadata["packages"]:
            if package["id"] not in included or not package["source"]:
                continue
            if package["source"] != "registry+https://github.com/rust-lang/crates.io-index" or not package.get("license"):
                raise ValueError(f"dependency needs explicit notice review: {package['id']}")
            entry = packages.setdefault(package["id"], {"package": package, "targets": []})
            entry["targets"].append(target)
    inventory = {
        "schema": 1,
        "cargo_lock_sha256": sha256((root / "Cargo.lock").read_bytes()),
        "features": ["object-storage"],
        "targets": list(TARGETS),
        "scope": "Reachable release dependencies, including build tools and proc macros; not a claim that every crate is linked.",
        "packages": [],
    }
    texts = {}
    for entry in sorted(packages.values(), key=lambda item: (item["package"]["name"], item["package"]["version"])):
        package = entry["package"]
        checksum = checksums[(package["name"], package["version"], package["source"])]
        files = license_files(package, checksum)
        record = {
            "name": package["name"], "version": package["version"],
            "license": package["license"], "crate_sha256": checksum,
            "source": f"https://crates.io/crates/{package['name']}/{package['version']}",
            "targets": entry["targets"], "license_files": [],
        }
        for path, digest, contents in files:
            record["license_files"].append({"path": path, "sha256": digest})
            texts.setdefault(digest, {"text": contents, "uses": []})["uses"].append(
                f"{package['name']} {package['version']}: {path}"
            )
        inventory["packages"].append(record)
    lines = [
        "# Rust dependency license texts", "",
        "Generated by `python3 scripts/generate-rust-notices.py` from the locked",
        "`object-storage` release feature graph for the three supported archive targets.",
        "Includes build tools and proc macros conservatively; this inventory does not",
        "claim every crate is statically linked. Upstream license and notice texts are",
        "preserved below, with identical files displayed once. Metadata expressions",
        "are reproduced as supplied by upstream; component-specific terms still apply.",
        "The exact crate and license-file SHA256 values are in `THIRD_PARTY_RUST_INVENTORY.json`.",
        "", "| Crate | Upstream license expression | Texts |", "|---|---|---|",
    ]
    for package in inventory["packages"]:
        references = ", ".join(
            f"[{file['path']}](#text-{file['sha256']})" for file in package["license_files"]
        )
        lines.append(f"| [{package['name']} {package['version']}]({package['source']}) | {package['license']} | {references} |")
    for digest, item in sorted(texts.items()):
        lines.extend(["", f'<a id="text-{digest}"></a>', "", f"## Text {digest}", ""])
        lines.extend(f"- {use}" for use in item["uses"])
        # Indented literal blocks preserve upstream Markdown/backticks safely.
        lines.extend(["", *(("    " + line.rstrip()) if line.strip() else "" for line in item["text"].splitlines()), ""])
    return {
        "THIRD_PARTY_RUST_INVENTORY.json": json.dumps(inventory, indent=2, ensure_ascii=False) + "\n",
        "THIRD_PARTY_RUST_NOTICES.md": "\n".join(lines) + "\n",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    outputs = generate(ROOT)
    for name, contents in outputs.items():
        path = ROOT / name
        if args.check:
            if not path.is_file() or path.read_text(encoding="utf-8") != contents:
                raise SystemExit(f"{name} is stale; run python3 scripts/generate-rust-notices.py")
        else:
            path.write_text(contents, encoding="utf-8")
    print("Rust dependency notices match Cargo.lock" if args.check else "Generated Rust dependency notices")


if __name__ == "__main__":
    main()
