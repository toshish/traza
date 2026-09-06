#!/usr/bin/env python3
"""Test the publication boundary, including invalid tag output injection."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HELPER = Path(__file__).with_name("release-channel.py")
SPEC = importlib.util.spec_from_file_location("release_channel", HELPER)
CHANNEL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHANNEL)


class ReleaseChannelTests(unittest.TestCase):
    def test_stable(self):
        for version in ("0.0.0", "0.25.0", "1.0.0", "123.456.789"):
            with self.subTest(version=version):
                self.assertEqual(
                    CHANNEL.classify("v" + version),
                    {"version": version, "prerelease": False, "container_alias": "latest"},
                )

    def test_every_prerelease_channel_avoids_latest(self):
        for channel in ("preview", "alpha", "beta", "rc"):
            for sequence in (1, 2, 100):
                version = f"0.26.0-{channel}.{sequence}"
                with self.subTest(version=version):
                    result = CHANNEL.classify("v" + version)
                    self.assertEqual(result["version"], version)
                    self.assertIs(result["prerelease"], True)
                    self.assertEqual(result["container_alias"], "preview")

    def test_reject_noncanonical_or_unsafe_tags(self):
        invalid = (
            "", "v", "0.26.0", "v1.2", "v1.2.3.4", "v01.2.3", "v1.02.3",
            "v1.2.03", "v1.2.3-preview", "v1.2.3-preview.0", "v1.2.3-preview.01",
            "v1.2.3-Preview.1", "v1.2.3-dev.1", "v1.2.3-rc.1.2", "v1.2.3+build",
            "v1.2.3-preview.1+build", " v1.2.3", "v1.2.3 ", "v1.2.3\n",
            "v1.2.3\r", "v1.2.3\ncontainer_alias=latest", "v1.2.3;echo bad",
            "v1.2.3-$(id)", "v1.2.3-rc.-1", "v１.2.3", "v1.2.3\x00",
        )
        for tag in invalid:
            with self.subTest(tag=tag):
                with self.assertRaises(ValueError):
                    CHANNEL.classify(tag)

    def test_cli_json_and_fixed_github_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.write_text("existing=value\n", encoding="utf-8")
            process = subprocess.run(
                [sys.executable, str(HELPER), "v0.26.0-preview.1", "--github-output", str(output)],
                check=True, capture_output=True, text=True,
            )
            self.assertEqual(json.loads(process.stdout), CHANNEL.classify("v0.26.0-preview.1"))
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                "existing=value\nversion=0.26.0-preview.1\nprerelease=true\ncontainer_alias=preview\n",
            )

    def test_invalid_tag_cannot_write_github_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.write_text("existing=value\n", encoding="utf-8")
            process = subprocess.run(
                [sys.executable, str(HELPER), "v1.2.3\ncontainer_alias=latest", "--github-output", str(output)],
                capture_output=True, text=True,
            )
            self.assertEqual(process.returncode, 2)
            self.assertEqual(process.stdout, "")
            self.assertEqual(output.read_text(encoding="utf-8"), "existing=value\n")


if __name__ == "__main__":
    unittest.main()
