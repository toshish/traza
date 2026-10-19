"""A canned-answer HTTP server, for answers traza-server never gives.

A redirect, a reverse proxy's sign-in page, a truncated body, a garbage
status line: the client must survive each, and the real server cannot be made
to produce any of them. Each connection's request is read whole (headers,
then ``Content-Length`` bytes of body) and recorded, ``answer`` turns it into
the raw bytes to write back, and the connection is closed — so every answer is
exactly the bytes a test chose, malformed ones included.
"""

from __future__ import annotations

import socket
import threading
import urllib.parse
from typing import Callable, Dict, List, Optional


class Request:
    """One request as the stub received it."""

    def __init__(self, method: str, target: str, headers: Dict[str, str], body: bytes) -> None:
        self.method = method
        self.target = target
        parts = urllib.parse.urlsplit(target)
        self.path = parts.path
        self.query = dict(urllib.parse.parse_qsl(parts.query, keep_blank_values=True))
        #: Header names lowercased.
        self.headers = headers
        self.body = body


def response(status: int, body: bytes = b"", headers: Optional[Dict[str, str]] = None) -> bytes:
    """A well-formed HTTP/1.1 response that closes its connection."""
    lines = [f"HTTP/1.1 {status} Stub", f"Content-Length: {len(body)}", "Connection: close"]
    lines += [f"{name}: {value}" for name, value in (headers or {}).items()]
    return ("\r\n".join(lines) + "\r\n\r\n").encode("latin-1") + body


class Stub:
    """Serves ``answer(request)`` on an ephemeral loopback port until closed."""

    def __init__(self, answer: Callable[[Request], bytes]) -> None:
        self.answer = answer
        self.requests: List[Request] = []
        self._listener = socket.create_server(("127.0.0.1", 0))
        # A short accept timeout rather than closing the socket under a
        # blocked accept(), which does not reliably wake it on every platform.
        self._listener.settimeout(0.05)
        self.port = self._listener.getsockname()[1]
        self.url = f"http://127.0.0.1:{self.port}"
        self._stopping = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self) -> None:
        while not self._stopping.is_set():
            try:
                conn, _ = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with conn:
                conn.settimeout(5)
                try:
                    request = self._read(conn)
                except OSError:
                    continue
                if request is None:
                    continue
                self.requests.append(request)
                try:
                    conn.sendall(self.answer(request))
                except OSError:
                    pass

    @staticmethod
    def _read(conn: socket.socket) -> Optional[Request]:
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(65536)
            if not chunk:
                return None
            data += chunk
        head, _, body = data.partition(b"\r\n\r\n")
        request_line, *header_lines = head.decode("latin-1").split("\r\n")
        method, target, _ = request_line.split(" ", 2)
        headers = {}
        for line in header_lines:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        length = int(headers.get("content-length", "0"))
        while len(body) < length:
            chunk = conn.recv(65536)
            if not chunk:
                break
            body += chunk
        return Request(method, target, headers, body)

    def close(self) -> None:
        self._stopping.set()
        self._thread.join(timeout=5)
        self._listener.close()

    def __enter__(self) -> "Stub":
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()
