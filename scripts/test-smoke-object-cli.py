#!/usr/bin/env python3
"""Prove the artifact smoke fails for bad versions and false configuration success."""

import importlib.util
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("smoke", Path(__file__).with_name("smoke-object-cli.py"))
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)


class ObjectCliSmokeTests(unittest.TestCase):
    def fixture(self, directory, version="0.26.0-preview.1", help_exit=0, missing_exit=1, missing_error="--bucket is required"):
        binary = directory / "fake-object"
        binary.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "assert 'AWS_SECRET_ACCESS_KEY' not in os.environ\n"
            "assert 'AWS_PROFILE' not in os.environ\n"
            "assert 'HTTPS_PROXY' not in os.environ\n"
            "assert os.environ['AWS_EC2_METADATA_DISABLED'] == 'true'\n"
            "assert os.environ['HOME'] == os.getcwd()\n"
            "if sys.argv[1] == '--version':\n"
            f" print('traza-object {version}'); sys.exit(0)\n"
            "if sys.argv[1] == '--help':\n"
            f" print('USAGE: object --bucket NAME'); sys.exit({help_exit})\n"
            f"print({missing_error!r}, file=sys.stderr); sys.exit({missing_exit})\n"
        )
        binary.chmod(0o755)
        return binary

    def test_valid_cli_and_environment_isolation(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            binary = self.fixture(directory)
            with patch.dict(os.environ, {
                "AWS_SECRET_ACCESS_KEY": "SYNTHETIC-DO-NOT-FORWARD",
                "AWS_PROFILE": "synthetic-profile", "HTTPS_PROXY": "http://invalid.example",
            }):
                receipt = SMOKE.smoke([str(binary)], "0.26.0-preview.1", directory)
            self.assertEqual(len(receipt["checks"]), 4)

    def test_wrong_version_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            binary = self.fixture(directory, version="0.25.0")
            with self.assertRaisesRegex(ValueError, "version differs"):
                SMOKE.smoke([str(binary)], "0.26.0-preview.1", directory)

    def test_help_failure_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            binary = self.fixture(directory, help_exit=1)
            with self.assertRaisesRegex(ValueError, "help failed"):
                SMOKE.smoke([str(binary)], "0.26.0-preview.1", directory)

    def test_missing_configuration_cannot_succeed_or_fail_vaguely(self):
        for exit_code, message in [(0, "--bucket is required"), (1, "connection failed")]:
            with self.subTest(exit_code=exit_code, message=message):
                with tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    binary = self.fixture(directory, missing_exit=exit_code, missing_error=message)
                    with self.assertRaisesRegex(ValueError, "reject missing bucket clearly"):
                        SMOKE.smoke([str(binary)], "0.26.0-preview.1", directory)

    def test_container_runs_without_network_or_forwarded_credentials(self):
        command = SMOKE.container_command("traza-release-smoke", "smoke-fixture")
        self.assertIn("--network=none", command)
        self.assertIn("--read-only", command)
        self.assertIn("--entrypoint=/traza-object", command)
        self.assertEqual(command[-1], "traza-release-smoke")
        self.assertFalse(any("AWS_SECRET" in item or "AWS_PROFILE" in item for item in command))

    def test_container_timeout_reaps_only_its_own_name(self):
        with tempfile.TemporaryDirectory() as temporary:
            command = SMOKE.container_command("traza-release-smoke", "smoke-fixture")
            with patch.object(SMOKE.subprocess, "run", side_effect=[
                subprocess.TimeoutExpired("docker", 20),
                subprocess.CompletedProcess([], 0),
            ]) as run:
                with self.assertRaises(subprocess.TimeoutExpired):
                    SMOKE.smoke(command, "0.26.0-preview.1", temporary, "smoke-fixture")
            self.assertEqual(run.call_args_list[1].args[0], ["docker", "rm", "--force", "smoke-fixture"])


if __name__ == "__main__":
    unittest.main()
