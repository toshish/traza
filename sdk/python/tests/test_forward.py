"""``traza forward`` against a local HTTPS server.

The upstream is a TLS server on loopback with a throwaway certificate
authority made at test time with the ``openssl`` command (the suite skips,
saying why, when there is none). The forwarder runs as its command does, in
a child process, and is handed that authority the only way it accepts one:
``SSL_CERT_FILE``, which OpenSSL reads for the default verify paths — so a
test that hands it an unrelated authority shows the verification is on. The
last test sends a real ``traza-server import`` through it.
"""

from __future__ import annotations

import hashlib
import http.client
import http.server
import json
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Dict, List, Optional

SRC = Path(__file__).resolve().parents[1] / "src"
REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(SRC))

from _server import server_binary  # noqa: E402

from traza.forward import parse_listen, parse_upstream  # noqa: E402

OPENSSL_CONFIG = """\
[req]
distinguished_name = dn
prompt = no
[dn]
CN = traza forward test
[ca_ext]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[leaf_ext]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, IP:127.0.0.1
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
"""


class Authority:
    """A certificate authority and a leaf it signed for 127.0.0.1, in a
    temporary directory, or a reason there is none."""

    def __init__(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="traza-forward-tls-")
        self.reason: Optional[str] = None
        openssl = shutil.which("openssl")
        if openssl is None:
            self.reason = "no openssl command to make a throwaway certificate with"
            return
        with open(os.path.join(self.dir, "openssl.cnf"), "w") as handle:
            handle.write(OPENSSL_CONFIG)

        def run(*args: str) -> None:
            subprocess.run([openssl, *args], cwd=self.dir, check=True, stdout=subprocess.DEVNULL,
                           stderr=subprocess.PIPE)

        try:
            for name in ("ca", "other"):
                run("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{name}.key", "-out",
                    f"{name}.pem", "-days", "2", "-config", "openssl.cnf", "-extensions", "ca_ext",
                    "-subj", f"/CN=traza forward test {name}")
            run("req", "-newkey", "rsa:2048", "-nodes", "-keyout", "leaf.key", "-out", "leaf.csr",
                "-config", "openssl.cnf", "-subj", "/CN=127.0.0.1")
            run("x509", "-req", "-in", "leaf.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
                "-out", "leaf.pem", "-days", "2", "-extfile", "openssl.cnf", "-extensions", "leaf_ext")
        except subprocess.CalledProcessError as error:
            self.reason = f"openssl could not make a throwaway certificate: {error.stderr.decode(errors='replace')}"

    def path(self, name: str) -> str:
        return os.path.join(self.dir, name)

    def close(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)


class Seen:
    """What the upstream was asked, in order."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.requests: List[Dict[str, object]] = []
        self.release = threading.Event()

    def add(self, request: Dict[str, object]) -> None:
        with self.lock:
            self.requests.append(request)

    def all(self) -> List[Dict[str, object]]:
        with self.lock:
            return list(self.requests)


class UpstreamHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: "Upstream"

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass

    def _body(self) -> Optional[bytes]:
        """The request body, or None for one that was abandoned unfinished
        (the forwarder gives up a chunked body over its cap mid-way)."""
        try:
            return self._read_body()
        except ValueError:
            self.close_connection = True
            return None

    def _read_body(self) -> bytes:
        if (self.headers.get("Transfer-Encoding") or "").lower() == "chunked":
            out = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    return out
                out += self.rfile.read(size)
                self.rfile.readline()
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def _send(self, status: int, body: bytes, headers: Optional[Dict[str, str]] = None) -> None:
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle(self) -> None:
        body = self._body()
        if body is None:
            return
        seen = self.server.seen
        seen.add({"method": self.command, "path": self.path, "headers": self.headers.items(), "body": body})
        path = self.path.split("?")[0]
        if path.endswith("/stream"):
            # A live stream: one event, then the rest only once the test has
            # read the first through the forwarder.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            first = b"event: one\ndata: {}\n\n"
            self.wfile.write(b"%x\r\n%s\r\n" % (len(first), first))
            self.wfile.flush()
            seen.release.wait(10)
            last = b"event: two\ndata: {}\n\n"
            self.wfile.write(b"%x\r\n%s\r\n0\r\n\r\n" % (len(last), last))
        elif path.endswith("/redirect"):
            self._send(302, b"", {"Location": "http://elsewhere.invalid/v1/spans"})
        elif path.endswith("/slow"):
            time.sleep(4)
            self._send(200, b"late")
        elif path.endswith("/until-close"):
            self.send_response(200)
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(b"read until the connection closes")
            self.close_connection = True
        elif path.endswith("/v1/whoami"):
            self._send(200, json.dumps({"auth": "token", "scope": "rw", "tenant": None, "operator": True}).encode(),
                       {"Content-Type": "application/json"})
        elif path.endswith("/v1/spans"):
            self._send(200, json.dumps({"accepted": len(json.loads(body))}).encode(),
                       {"Content-Type": "application/json"})
        else:
            echo = {"method": self.command, "path": self.path, "headers": [list(h) for h in self.headers.items()],
                    "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()}
            self._send(201, json.dumps(echo).encode(), {
                "Content-Type": "application/json", "X-Upstream": "yes", "Keep-Alive": "timeout=5",
            })

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = _handle


class Upstream(http.server.ThreadingHTTPServer):
    """An HTTPS server on 127.0.0.1 with the authority's leaf certificate."""

    daemon_threads = True

    def __init__(self, authority: Authority) -> None:
        super().__init__(("127.0.0.1", 0), UpstreamHandler)
        self.seen = Seen()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(authority.path("leaf.pem"), authority.path("leaf.key"))
        self.socket = context.wrap_socket(self.socket, server_side=True)
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"https://127.0.0.1:{self.server_address[1]}"

    def stop(self) -> None:
        self.seen.release.set()
        self.shutdown()
        self.server_close()


class Forwarder:
    """``python -m traza.forward`` in a child process, on an ephemeral port."""

    def __init__(self, upstream: str, ca_file: str, *args: str) -> None:
        env = {key: value for key, value in os.environ.items() if not key.startswith(("SSL_CERT", "TRAZA_"))}
        env["SSL_CERT_FILE"] = ca_file
        env["PYTHONPATH"] = str(SRC)
        self.process = subprocess.Popen(
            [sys.executable, "-m", "traza.forward", "--upstream", upstream, "--listen", "127.0.0.1:0", "--quiet",
             *args],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
        )
        assert self.process.stderr is not None
        line = self.process.stderr.readline().decode()
        if "http://127.0.0.1:" not in line:
            self.process.kill()
            raise RuntimeError(f"traza forward did not start: {line}{self.process.stderr.read().decode()}")
        self.port = int(line.split("http://127.0.0.1:")[1].split(" ")[0])
        self.url = f"http://127.0.0.1:{self.port}"
        self.connections: List[http.client.HTTPConnection] = []

    def connect(self, timeout: float = 15) -> http.client.HTTPConnection:
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        self.connections.append(connection)
        return connection

    def stop(self) -> None:
        for connection in self.connections:
            connection.close()
        self.process.terminate()
        try:
            self.process.wait(5)
        except subprocess.TimeoutExpired:  # pragma: no cover
            self.process.kill()
        if self.process.stderr:
            self.process.stderr.close()


AUTHORITY: Optional[Authority] = None


def setUpModule() -> None:  # noqa: N802 - unittest's name
    global AUTHORITY
    AUTHORITY = Authority()


def tearDownModule() -> None:  # noqa: N802
    if AUTHORITY is not None:
        AUTHORITY.close()


class ForwardTestCase(unittest.TestCase):
    def setUp(self) -> None:
        assert AUTHORITY is not None
        if AUTHORITY.reason:
            self.skipTest(AUTHORITY.reason)
        self.upstream = Upstream(AUTHORITY)
        self.addCleanup(self.upstream.stop)

    def forwarder(self, *args: str, upstream: Optional[str] = None, ca: str = "ca.pem") -> Forwarder:
        assert AUTHORITY is not None
        forwarder = Forwarder(upstream or self.upstream.url, AUTHORITY.path(ca), *args)
        self.addCleanup(forwarder.stop)
        return forwarder


class ForwardsOverTls(ForwardTestCase):
    def test_method_path_query_and_end_to_end_headers_pass_and_hop_by_hop_ones_do_not(self) -> None:
        conn = self.forwarder().connect()
        body = b'{"spans": []}'
        conn.request("PATCH", "/v1/things/a%20b?x=1&y=two%20words", body=body, headers={
            "Authorization": "Bearer tzk_forward_test",
            "Content-Type": "application/json",
            "X-Keep": "kept",
            "Connection": "X-Drop-Me",
            "X-Drop-Me": "named by Connection",
            "Keep-Alive": "timeout=5",
            "TE": "trailers",
            "Proxy-Authorization": "Basic c2VjcmV0",
            "Upgrade": "h2c",
        })
        response = conn.getresponse()
        self.assertEqual(response.status, 201)
        echo = json.loads(response.read())
        headers = {name.lower(): value for name, value in echo["headers"]}
        self.assertEqual(echo["method"], "PATCH")
        self.assertEqual(echo["path"], "/v1/things/a%20b?x=1&y=two%20words")
        self.assertEqual(headers["authorization"], "Bearer tzk_forward_test")
        self.assertEqual(headers["x-keep"], "kept")
        self.assertEqual(headers["host"], f"127.0.0.1:{self.upstream.server_address[1]}")
        self.assertEqual(echo["sha256"], hashlib.sha256(body).hexdigest())
        for dropped in ("x-drop-me", "keep-alive", "te", "proxy-authorization", "upgrade"):
            self.assertNotIn(dropped, headers)
        # The response's own headers come back; its hop-by-hop ones do not.
        self.assertEqual(response.getheader("X-Upstream"), "yes")
        self.assertIsNone(response.getheader("Keep-Alive"))

    def test_an_upstream_path_prefix_leads_every_path(self) -> None:
        conn = self.forwarder(upstream=self.upstream.url + "/traza/").connect()
        conn.request("GET", "/v1/whoami?tenant=")
        response = conn.getresponse()
        response.read()
        self.assertEqual(self.upstream.seen.all()[-1]["path"], "/traza/v1/whoami?tenant=")

    def test_a_chunked_request_body_streams_through(self) -> None:
        conn = self.forwarder().connect()
        pieces = [b"a" * 70_000, b"b" * 10, b"c" * 3]
        conn.request("POST", "/echo", body=iter(pieces), encode_chunked=True)
        response = conn.getresponse()
        echo = json.loads(response.read())
        self.assertEqual(response.status, 201)
        self.assertEqual(echo["bytes"], 70_013)
        self.assertEqual(echo["sha256"], hashlib.sha256(b"".join(pieces)).hexdigest())

    def test_a_response_streams_as_it_arrives_not_once_it_ends(self) -> None:
        conn = self.forwarder().connect(timeout=8)
        conn.request("GET", "/v1/runs/live/stream")
        response = conn.getresponse()
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        # The upstream holds its second event until this read returns: a
        # forwarder that buffered the body would never hand over the first.
        first = response.read1(65536)
        self.assertIn(b"event: one", first)
        self.upstream.seen.release.set()
        rest = response.read()
        self.assertIn(b"event: two", first + rest)

    def test_a_body_that_ends_with_the_connection_is_passed_on_that_way(self) -> None:
        conn = self.forwarder().connect()
        conn.request("GET", "/until-close")
        response = conn.getresponse()
        self.assertEqual(response.read(), b"read until the connection closes")
        self.assertEqual(response.getheader("Connection"), "close")

    def test_one_connection_carries_several_requests(self) -> None:
        conn = self.forwarder().connect()
        for index in range(3):
            conn.request("POST", f"/echo/{index}", body=b"x" * index)
            response = conn.getresponse()
            self.assertEqual(json.loads(response.read())["path"], f"/echo/{index}")


class RefusesWhatItShouldNot(ForwardTestCase):
    def test_a_redirect_is_neither_followed_nor_passed_on(self) -> None:
        conn = self.forwarder().connect()
        conn.request("POST", "/v1/redirect", body=b"[]", headers={"Authorization": "Bearer tzk_forward_test"})
        response = conn.getresponse()
        error = json.loads(response.read())["error"]
        self.assertEqual(response.status, 502)
        self.assertIsNone(response.getheader("Location"))
        self.assertIn("answered 302 with a redirect to http://elsewhere.invalid/v1/spans", error)
        self.assertEqual(len(self.upstream.seen.all()), 1)

    def test_a_body_over_the_cap_never_reaches_the_upstream(self) -> None:
        forwarder = self.forwarder("--max-body-bytes", "1000")
        conn = forwarder.connect()
        conn.request("POST", "/echo", body=b"x" * 1001)
        response = conn.getresponse()
        self.assertEqual(response.status, 413)
        self.assertIn("over the forwarder's 1000-byte cap", json.loads(response.read())["error"])
        conn = forwarder.connect()
        conn.request("POST", "/echo", body=iter([b"x" * 600, b"y" * 600]), encode_chunked=True)
        response = conn.getresponse()
        self.assertEqual(response.status, 413)
        response.read()
        # At the cap exactly is forwarded.
        conn = forwarder.connect()
        conn.request("POST", "/echo", body=b"x" * 1000)
        response = conn.getresponse()
        self.assertEqual(json.loads(response.read())["bytes"], 1000)
        self.assertEqual([r["body"] for r in self.upstream.seen.all()], [b"x" * 1000])

    def test_an_upstream_whose_certificate_is_not_trusted_is_refused(self) -> None:
        # The forwarder is handed an authority that did not sign the
        # upstream's certificate: verification is on, so nothing is sent.
        conn = self.forwarder(ca="other.pem").connect()
        conn.request("GET", "/v1/whoami", headers={"Authorization": "Bearer tzk_forward_test"})
        response = conn.getresponse()
        error = json.loads(response.read())["error"]
        self.assertEqual(response.status, 502)
        self.assertIn("TLS verification of 127.0.0.1", error)
        self.assertIn("certificate", error)
        self.assertEqual(self.upstream.seen.all(), [])

    def test_an_upstream_that_goes_silent_is_given_up(self) -> None:
        conn = self.forwarder("--timeout", "1").connect()
        started = time.monotonic()
        conn.request("GET", "/slow")
        response = conn.getresponse()
        self.assertEqual(response.status, 504)
        self.assertIn("went 1 s without answering", json.loads(response.read())["error"])
        self.assertLess(time.monotonic() - started, 3.5)

    def test_a_url_target_is_not_proxied(self) -> None:
        forwarder = self.forwarder()
        with socket.create_connection(("127.0.0.1", forwarder.port), timeout=5) as sock:
            sock.sendall(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            answer = b""
            while True:
                piece = sock.recv(65536)
                if not piece:
                    break
                answer += piece
        self.assertTrue(answer.startswith(b"HTTP/1.1 400 "), answer[:80])
        self.assertIn(b"is not a proxy", answer)
        conn = forwarder.connect()
        conn.request("CONNECT", "example.com:443")
        self.assertEqual(conn.getresponse().status, 405)
        self.assertEqual(self.upstream.seen.all(), [])


class Addresses(unittest.TestCase):
    def test_only_loopback_is_listened_on(self) -> None:
        for refused in ("0.0.0.0:8089", "[::]:8089", "192.168.1.20:8089", "10.0.0.1:80", "example.com:8089"):
            with self.assertRaisesRegex(ValueError, "must be a loopback address"):
                parse_listen(refused)
        self.assertEqual(parse_listen("8089")[:2], ("127.0.0.1", 8089))
        self.assertEqual(parse_listen("127.0.0.2:9000")[:2], ("127.0.0.2", 9000))
        self.assertEqual(parse_listen("[::1]:9000")[:2], ("::1", 9000))
        host, port, _ = parse_listen("localhost:9000")
        self.assertEqual(port, 9000)
        self.assertIn(host, ("127.0.0.1", "::1"))
        for malformed in ("127.0.0.1", "127.0.0.1:x", "127.0.0.1:70000", "::1:80"):
            with self.assertRaises(ValueError):
                parse_listen(malformed)

    def test_the_command_refuses_a_public_address_before_listening(self) -> None:
        env = dict(os.environ, PYTHONPATH=str(SRC))
        done = subprocess.run([sys.executable, "-m", "traza.cli", "forward", "--upstream", "https://example.com",
                               "--listen", "0.0.0.0:8089"], env=env, capture_output=True, timeout=30)
        self.assertEqual(done.returncode, 2)
        self.assertIn(b"must be a loopback address", done.stderr)

    def test_only_https_is_an_upstream(self) -> None:
        self.assertEqual(parse_upstream("https://api.trazadb.com"),
                         ("api.trazadb.com", 443, "", "api.trazadb.com"))
        self.assertEqual(parse_upstream("https://[::1]:8443/traza/"), ("::1", 8443, "/traza", "[::1]:8443"))
        for refused, why in (("http://api.trazadb.com", "https://"), ("api.trazadb.com", "https://"),
                             ("https://user:pw@api.trazadb.com", "credentials"),
                             ("https://api.trazadb.com/?token=x", "no query"),
                             ("https:///v1", "no host"), ("https://h:99999", "invalid port")):
            with self.assertRaisesRegex(ValueError, why):
                parse_upstream(refused)


class TheImporterReachesTls(ForwardTestCase):
    def test_traza_server_import_sends_through_the_forwarder(self) -> None:
        forwarder = self.forwarder()
        fixture = REPO / "tests" / "fixtures" / "import" / "claude-code"
        env = {key: value for key, value in os.environ.items() if not key.startswith("TRAZA_")}
        done = subprocess.run(
            [str(server_binary()), "import", "claude-code", "--path", str(fixture), "--url", forwarder.url,
             "--token", "tzk_forward_test", "--no-state", "--json"],
            env=env, capture_output=True, timeout=60,
        )
        self.assertEqual(done.returncode, 0, done.stderr.decode() + done.stdout.decode())
        stats = json.loads(done.stdout)
        seen = self.upstream.seen.all()
        self.assertEqual(seen[0]["path"], "/v1/whoami")
        posts = [request for request in seen if request["path"] == "/v1/spans"]
        self.assertTrue(posts)
        self.assertEqual(sum(len(json.loads(request["body"])) for request in posts), stats["spans_sent"])
        for request in seen:
            headers = {name.lower(): value for name, value in request["headers"]}
            self.assertEqual(headers["authorization"], "Bearer tzk_forward_test")
            self.assertNotIn("connection", headers)


if __name__ == "__main__":
    unittest.main()
