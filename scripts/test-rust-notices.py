#!/usr/bin/env python3
"""Exercise notice collection against independent package fixtures."""

import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("notices", Path(__file__).with_name("generate-rust-notices.py"))
NOTICES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(NOTICES)


class RustNoticeTests(unittest.TestCase):
    def package(self, root, files):
        archive = root / "fixture.crate"
        with tarfile.open(archive, "w:gz") as output:
            for name, content in files.items():
                member = tarfile.TarInfo("fixture-1.0.0/" + name)
                member.size = len(content)
                output.addfile(member, io.BytesIO(content))
        package = {"name": "fixture", "version": "1.0.0", "manifest_path": str(root / "Cargo.toml")}
        return package, hashlib.sha256(archive.read_bytes()).hexdigest(), archive

    def test_preserves_nested_notices_and_does_not_copy_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            package, checksum, archive = self.package(Path(directory), {
                "LICENSE-MIT": b"Copyright Example\nPermission text\n",
                "vendor/NOTICE.txt": b"Required upstream attribution\n",
                "src/lib.rs": b"fn main() {}\n",
            })
            files = NOTICES.license_files(package, checksum, archive)
            self.assertEqual([file[0] for file in files], ["LICENSE-MIT", "vendor/NOTICE.txt"])
            self.assertEqual(files[0][2], "Copyright Example\nPermission text\n")

    def test_modified_upstream_text_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package, checksum, archive = self.package(root, {"LICENSE": b"Original terms\n"})
            self.package(root, {"LICENSE": b"Modified terms\n"})
            with self.assertRaisesRegex(ValueError, "differs from Cargo.lock"):
                NOTICES.license_files(package, checksum, archive)

    def test_missing_text_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            package, checksum, archive = self.package(Path(directory), {"src/lib.rs": b"fn main() {}"})
            with self.assertRaisesRegex(ValueError, "no packaged license texts"):
                NOTICES.license_files(package, checksum, archive)

    def test_outside_license_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package, checksum, archive = self.package(root, {"../LICENSE": b"Outside license\n"})
            with self.assertRaisesRegex(ValueError, "escapes package"):
                NOTICES.license_files(package, checksum, archive)

    def test_unreachable_platform_package_is_excluded(self):
        metadata = {"resolve": {"root": "root", "nodes": [
            {"id": "root", "deps": [{"pkg": "a"}]},
            {"id": "a", "deps": [{"pkg": "b"}]},
            {"id": "b", "deps": []},
            {"id": "unrelated-platform", "deps": []},
        ]}}
        self.assertEqual(NOTICES.reachable(metadata), {"root", "a", "b"})


if __name__ == "__main__":
    unittest.main()
