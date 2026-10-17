"""The real traza-server on an ephemeral loopback port, for the client tests.

Hermetic in the same way as the engine's own harness (tests/common/mod.rs):
the server gets a private data directory, ``--host 127.0.0.1 --port 0``, and
an environment with every inherited ``TRAZA_*`` variable removed, so a
developer's exported ``TRAZA_TOKENS`` cannot turn an open-mode test into an
auth test. The binary is ``$TRAZA_SERVER_BIN``, else the release build at the
repository root.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Dict, List, Optional

SDK_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BIN = SDK_ROOT / ".." / ".." / "target" / "release" / "traza-server"
LISTENING = re.compile(r"traza-server listening on 127\.0\.0\.1:(\d+)")


def server_binary() -> Path:
    configured = os.environ.get("TRAZA_SERVER_BIN")
    path = Path(configured) if configured else DEFAULT_BIN
    if not path.is_file():
        # A failure, not a skip: a suite that skips itself when the binary is
        # missing would let CI pass without testing anything.
        raise RuntimeError(
            f"traza-server not found at {path}; build it with `cargo build --release` "
            "or point TRAZA_SERVER_BIN at a built binary"
        )
    return path.resolve()


class Server:
    """One running server. Use as a context manager, or call ``stop()``."""

    def __init__(self, args: Optional[List[str]] = None, env: Optional[Dict[str, str]] = None) -> None:
        # The store lives one level down so the stderr log can sit beside it
        # without being a stray file inside the server's data directory.
        self.data_dir = tempfile.mkdtemp(prefix="traza-sdk-py-")
        self._stderr_path = os.path.join(self.data_dir, "server.stderr")
        child_env = {key: value for key, value in os.environ.items() if not key.startswith("TRAZA_")}
        child_env.update(env or {})
        store = os.path.join(self.data_dir, "store")
        # stderr goes to a file rather than a pipe: nothing drains a pipe once
        # the port is read, and a server blocked on a full pipe looks hung.
        self._stderr = open(self._stderr_path, "wb")
        self.process = subprocess.Popen(
            [str(server_binary()), "--data-dir", store, "--host", "127.0.0.1", "--port", "0", *(args or [])],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=self._stderr,
            env=child_env,
        )
        self.port = self._wait_for_port()
        self.url = f"http://127.0.0.1:{self.port}"

    def _wait_for_port(self) -> int:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            text = self.stderr()
            match = LISTENING.search(text)
            if match:
                return int(match.group(1))
            if self.process.poll() is not None:
                self.stop()
                raise RuntimeError(f"traza-server exited before listening:\n{text}")
            time.sleep(0.02)
        self.stop()
        raise RuntimeError("traza-server did not report a port within 15 s")

    def stderr(self) -> str:
        with open(self._stderr_path, "rb") as handle:
            return handle.read().decode("utf-8", "replace")

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self._stderr.close()
        shutil.rmtree(self.data_dir, ignore_errors=True)

    def __enter__(self) -> "Server":
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop()
