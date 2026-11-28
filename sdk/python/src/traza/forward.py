"""``traza forward``: plain HTTP on loopback, forwarded to an ``https://`` Traza.

Some of Traza's own tools speak plain HTTP only — ``traza-server import
claude-code --watch``, ``traza-server blame``, the MCP bridge — and refuse an
``https://`` URL. A server on this machine needs nothing more; a server
reached over a network (Traza Cloud, or your own behind TLS) does. This
forwarder bridges the two without a reverse proxy to configure::

    traza forward --upstream https://api.trazadb.com
    traza-server import claude-code --watch --url http://127.0.0.1:8089 --token tzk_...

It listens on loopback only — a request reaches it in plain text, token and
all, and only the loopback interface keeps that on this machine — and sends
each request on to the upstream over TLS, verified against the system's
certificate authorities (there is no flag to turn verification off). Method,
path, query and headers pass through, the hop-by-hop ones excepted, with
``Host`` set to the upstream's; request and response bodies stream through in
pieces, never held whole, so a live stream (``/v1/tail``) stays live. A
request body is capped (``--max-body-bytes``, 64 MiB by default, the
server's own cap); a request with a larger one is answered ``413`` and never
reaches the upstream. A redirect from the upstream is neither followed nor
passed on: it is answered ``502`` naming where it pointed, so a credential
goes only where ``--upstream`` says. The forwarder adds no credential of its
own and keeps none; a token is whatever the local tool sends.

Errors the forwarder answers itself are JSON, ``{"error": "..."}``, like the
server's: ``400`` for a request it will not forward, ``413`` for a body over
the cap, ``502`` for an upstream that could not be reached, failed TLS
verification, or redirected, and ``504`` for one that went silent for
``--timeout`` seconds. Standard library only.

Also runnable as ``python -m traza.forward``.
"""

from __future__ import annotations

import argparse
import http.client
import http.server
import ipaddress
import json
import socket
import socketserver
import ssl
import sys
import urllib.parse
from typing import BinaryIO, Iterator, List, NamedTuple, Optional, Sequence, Tuple

from . import __version__

#: Where the forwarder listens when ``--listen`` names nothing else.
DEFAULT_LISTEN = "127.0.0.1:8089"
#: The largest request body forwarded: the server's own cap.
DEFAULT_MAX_BODY_BYTES = 64 * 1024 * 1024
#: Seconds an upstream exchange, or a client's request, may go without
#: progress — connecting, or between two reads or writes — before it is given
#: up. A live stream's heartbeat (15 s) keeps it well inside this.
DEFAULT_TIMEOUT = 120.0
#: The most read or written in one piece.
CHUNK_BYTES = 64 * 1024

#: Headers that describe one connection, not the request (RFC 9110 §7.6.1),
#: never forwarded in either direction. ``Expect`` is answered here (a
#: ``100 Continue`` to the local tool), and ``Host`` and ``Content-Length``
#: are set by the forwarder itself.
HOP_BY_HOP = frozenset({
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
})
_SET_HERE = frozenset({"host", "content-length", "expect"})


class Upstream(NamedTuple):
    """Where requests go: an ``https://`` origin and an optional path prefix."""

    host: str
    port: int
    #: A path the upstream lives under (``/traza``), without a trailing
    #: slash; empty for one at the root.
    prefix: str
    #: The ``Host`` header the upstream is sent.
    host_header: str

    @property
    def url(self) -> str:
        """The upstream as a URL, for messages."""
        return f"https://{self.host_header}{self.prefix}"


class Listen(NamedTuple):
    """A loopback address to listen on."""

    host: str
    port: int
    family: int

    @property
    def url(self) -> str:
        """The address as a URL, for messages."""
        host = f"[{self.host}]" if self.family == socket.AF_INET6 else self.host
        return f"http://{host}:{self.port}"


def parse_upstream(url: str) -> Upstream:
    """``https://HOST[:PORT][/PREFIX]``, or ``ValueError`` saying what is wrong."""
    parts = urllib.parse.urlsplit(url.strip())
    if parts.scheme.lower() != "https":
        raise ValueError(
            f"--upstream must be an https:// URL (got {url!r}): the forwarder exists to add TLS; "
            "a plain-HTTP server needs no forwarder"
        )
    if not parts.hostname:
        raise ValueError(f"--upstream has no host: {url!r}")
    if parts.username is not None or parts.password is not None:
        raise ValueError("--upstream must not carry credentials; the local tool sends its own token")
    if parts.query or parts.fragment:
        raise ValueError(f"--upstream takes no query or fragment: {url!r}")
    try:
        port = parts.port or 443
    except ValueError:
        raise ValueError(f"--upstream has an invalid port: {url!r}") from None
    host = parts.hostname
    shown = f"[{host}]" if ":" in host else host
    return Upstream(
        host=host,
        port=port,
        prefix=parts.path.rstrip("/"),
        host_header=shown if port == 443 else f"{shown}:{port}",
    )


def parse_listen(text: str) -> Listen:
    """``HOST:PORT``, ``[::1]:PORT`` or a bare ``PORT`` (on 127.0.0.1), refused
    with ``ValueError`` unless every address it names is loopback."""
    text = text.strip()
    if text.isdigit():
        host, port_text = "127.0.0.1", text
    elif text.startswith("["):
        host, _, rest = text[1:].partition("]")
        if not rest.startswith(":"):
            raise ValueError(f"--listen must be HOST:PORT, [::1]:PORT or PORT (got {text!r})")
        port_text = rest[1:]
    elif text.count(":") == 1:
        host, port_text = text.split(":")
    else:
        raise ValueError(f"--listen must be HOST:PORT, [::1]:PORT or PORT (got {text!r})")
    if not port_text.isdigit() or not 0 <= int(port_text) <= 65535:
        raise ValueError(f"--listen has an invalid port: {text!r}")
    port = int(port_text)
    refusal = (
        f"--listen must be a loopback address (127.0.0.1, ::1 or localhost), not {host!r}: a request "
        "reaches the forwarder in plain text, token and all, and only the loopback interface keeps "
        "that on this machine"
    )
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        address = None
    if address is not None:
        if not address.is_loopback:
            raise ValueError(refusal)
        family = socket.AF_INET6 if address.version == 6 else socket.AF_INET
        return Listen(str(address), port, family)
    if host.lower() != "localhost":
        raise ValueError(refusal)
    # A name: every address it resolves to must be loopback, or a hosts file
    # could send it elsewhere. Bound to the first.
    try:
        found = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except OSError as error:
        raise ValueError(f"--listen {host!r} does not resolve: {error}") from None
    addresses = [(info[0], info[4][0]) for info in found]
    if not addresses or not all(ipaddress.ip_address(a.split("%")[0]).is_loopback for _, a in addresses):
        raise ValueError(refusal)
    family, first = addresses[0]
    return Listen(first, port, family)


class ForwardServer(http.server.ThreadingHTTPServer):
    """The forwarder: a threading HTTP server on a loopback address.

    ``context`` is the TLS context the upstream is verified with; by default
    ``ssl.create_default_context()`` — the system's authorities, hostname
    checked. Serve with ``serve_forever()``; ``server_port`` is the port bound
    (useful with port 0)."""

    daemon_threads = True

    def __init__(
        self,
        listen: Listen,
        upstream: Upstream,
        *,
        max_body_bytes: int = DEFAULT_MAX_BODY_BYTES,
        timeout: float = DEFAULT_TIMEOUT,
        context: Optional[ssl.SSLContext] = None,
        quiet: bool = False,
    ) -> None:
        if max_body_bytes < 0:
            raise ValueError("--max-body-bytes must be 0 or more")
        if timeout <= 0:
            raise ValueError("--timeout must be above 0")
        self.address_family = listen.family
        self.upstream = upstream
        self.max_body_bytes = max_body_bytes
        self.timeout_seconds = timeout
        self.context = context or ssl.create_default_context()
        self.quiet = quiet
        super().__init__((listen.host, listen.port), ForwardHandler)

    def server_bind(self) -> None:
        # HTTPServer.server_bind asks for the address's fully qualified name,
        # a reverse lookup that can stall startup on a misconfigured resolver;
        # nothing here uses it.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class _Refusal(Exception):
    """A request the forwarder answers itself."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


class ForwardHandler(http.server.BaseHTTPRequestHandler):
    """Forwards one connection's requests to the server's upstream."""

    protocol_version = "HTTP/1.1"
    server_version = f"traza-forward/{__version__}"
    server: ForwardServer

    def setup(self) -> None:
        # A client that goes quiet mid-request, or idles on a kept-alive
        # connection, is let go after the same time an upstream would be.
        self.timeout = self.server.timeout_seconds
        super().setup()

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002 - the base class's name
        if not self.server.quiet:
            super().log_message(format, *args)

    def do_CONNECT(self) -> None:  # noqa: N802 - http.server's naming
        self._answer(405, "traza forward is not a proxy: send requests to it as to the server itself")

    def _dispatch(self) -> None:
        try:
            self._forward()
        except _Refusal as refusal:
            self._answer(refusal.status, refusal.message)
        except (BrokenPipeError, ConnectionResetError, socket.timeout):
            # The local tool went away before its answer was written.
            self.close_connection = True

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = _dispatch

    # ------------------------------------------------------------ request

    def _forward(self) -> None:
        upstream = self.server.upstream
        if not self.path.startswith("/"):
            raise _Refusal(
                400,
                "send origin-form requests (GET /v1/...), not a URL: traza forward sends everything "
                f"to {upstream.url} and is not a proxy",
            )
        length, chunked = self._body_framing()
        headers = _end_to_end(self.headers.items(), self.headers.get_all("Connection") or [])
        connection = http.client.HTTPSConnection(
            upstream.host, upstream.port, timeout=self.server.timeout_seconds, context=self.server.context
        )
        try:
            try:
                connection.putrequest(self.command, upstream.prefix + self.path, skip_host=True,
                                      skip_accept_encoding=True)
                connection.putheader("Host", upstream.host_header)
                for name, value in headers:
                    connection.putheader(name, value)
            except (http.client.InvalidURL, ValueError) as error:
                raise _Refusal(400, f"the request cannot be forwarded as it is: {error}") from None
            if chunked:
                connection.putheader("Transfer-Encoding", "chunked")
            elif length is not None:
                connection.putheader("Content-Length", str(length))
            try:
                connection.endheaders()
                if chunked:
                    self._send_chunked(connection)
                elif length:
                    self._send_exactly(connection, length)
                response = connection.getresponse()
            except _Refusal:
                raise
            except Exception as error:  # noqa: BLE001 - every failure is answered, none escapes
                raise _Refusal(*_upstream_failure(error, upstream, self.server.timeout_seconds)) from None
            self._relay(response)
        finally:
            connection.close()

    def _body_framing(self) -> Tuple[Optional[int], bool]:
        """``(Content-Length, chunked)`` of the request, refused when the body
        cannot be framed or is declared over the cap."""
        codings = ",".join(self.headers.get_all("Transfer-Encoding") or [])
        if codings.strip():
            names = [name.strip().lower() for name in codings.split(",") if name.strip()]
            if names != ["chunked"]:
                self.close_connection = True
                raise _Refusal(501, f"only the chunked transfer coding is forwarded (got {codings!r})")
            return None, True
        values = self.headers.get_all("Content-Length") or []
        if not values:
            return None, False
        lengths = {value.strip() for value in values}
        if len(lengths) != 1 or not next(iter(lengths)).isdigit():
            self.close_connection = True
            raise _Refusal(400, "the request's Content-Length is not one number")
        length = int(next(iter(lengths)))
        if length > self.server.max_body_bytes:
            # The body is never read, so the connection cannot carry another
            # request after it.
            self.close_connection = True
            raise _Refusal(
                413,
                f"the request body is {length} bytes, over the forwarder's {self.server.max_body_bytes}-byte "
                "cap (--max-body-bytes); nothing was sent upstream",
            )
        return length, False

    def _read_client(self, read, *args):  # type: ignore[no-untyped-def]
        """One read of the request body: a client that stops sending is
        refused as such, never taken for an upstream that stopped answering."""
        try:
            return read(*args)
        except (socket.timeout, ConnectionError) as error:
            self.close_connection = True
            raise _Refusal(408, f"the request body stopped arriving: {error}") from None

    def _send_exactly(self, connection: http.client.HTTPSConnection, length: int) -> None:
        left = length
        while left:
            piece = self._read_client(self.rfile.read, min(CHUNK_BYTES, left))
            if not piece:
                self.close_connection = True
                raise _Refusal(400, f"the request body ended {left} bytes short of its Content-Length")
            connection.send(piece)
            left -= len(piece)

    def _send_chunked(self, connection: http.client.HTTPSConnection) -> None:
        total = 0
        for piece in _read_chunks(self.rfile, self._read_client):
            total += len(piece)
            if total > self.server.max_body_bytes:
                self.close_connection = True
                raise _Refusal(
                    413,
                    f"the request body passed the forwarder's {self.server.max_body_bytes}-byte cap "
                    "(--max-body-bytes); the upstream request was abandoned unfinished",
                )
            connection.send(b"%x\r\n%s\r\n" % (len(piece), piece))
        connection.send(b"0\r\n\r\n")

    # ------------------------------------------------------------ response

    def _relay(self, response: http.client.HTTPResponse) -> None:
        upstream = self.server.upstream
        status = response.status
        if 300 <= status < 400 and status != 304:
            location = response.getheader("Location") or "(no Location)"
            raise _Refusal(
                502,
                f"{upstream.url} answered {status} with a redirect to {location}; traza forward follows "
                "no redirect and passes none on, so a credential goes only to --upstream. Point "
                "--upstream at the address the server answers on.",
            )
        headers = _end_to_end(response.getheaders(), response.headers.get_all("Connection") or [])
        bodiless = self.command == "HEAD" or status in (204, 304) or 100 <= status < 200
        declared = response.getheader("Content-Length")
        self.log_request(status)
        self.send_response_only(status, response.reason or None)
        for name, value in headers:
            self.send_header(name, value)
        if bodiless:
            if declared is not None and declared.strip().isdigit():
                self.send_header("Content-Length", declared.strip())
            self._end_headers_for_close()
            return
        if response.length is not None:
            # Content-Length known (http.client never sets it for a chunked
            # body): stream exactly that many bytes.
            self.send_header("Content-Length", str(response.length))
            mode = "length"
        elif response.chunked and self.request_version != "HTTP/1.0":
            self.send_header("Transfer-Encoding", "chunked")
            mode = "chunked"
        else:
            # Neither: the body ends when the connection does.
            self.close_connection = True
            mode = "close"
        self._end_headers_for_close()
        self._stream(response, mode)

    def _end_headers_for_close(self) -> None:
        if self.close_connection:
            self.send_header("Connection", "close")
        self.end_headers()

    def _stream(self, response: http.client.HTTPResponse, mode: str) -> None:
        try:
            while True:
                try:
                    piece = response.read1(CHUNK_BYTES)
                except Exception:  # noqa: BLE001 - an upstream that broke mid-body
                    # The status line is gone already; ending the connection
                    # without a clean end is the only way left to say the
                    # body is incomplete.
                    self.close_connection = True
                    return
                if not piece:
                    break
                if mode == "chunked":
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(piece), piece))
                else:
                    self.wfile.write(piece)
            if mode == "chunked":
                self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError, socket.timeout):
            # The local tool went away, or stopped reading.
            self.close_connection = True

    def _answer(self, status: int, message: str) -> None:
        body = json.dumps({"error": message}).encode()
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            if self.command != "HEAD":
                self.send_header("Content-Length", str(len(body)))
            if self.close_connection:
                self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, socket.timeout):
            self.close_connection = True


def _end_to_end(items: Sequence[Tuple[str, str]], connection_values: Sequence[str]) -> List[Tuple[str, str]]:
    """The headers of `items` that describe the message, not the connection:
    the hop-by-hop ones dropped, with every header `Connection` names."""
    named = {name.strip().lower() for value in connection_values for name in value.split(",") if name.strip()}
    return [
        (name, value)
        for name, value in items
        if name.lower() not in HOP_BY_HOP and name.lower() not in _SET_HERE and name.lower() not in named
    ]


def _read_chunks(stream: BinaryIO, guard) -> Iterator[bytes]:  # type: ignore[no-untyped-def]
    """The data of a chunked body read from `stream`, chunk by chunk (large
    chunks in pieces), trailers read and dropped. Every read goes through
    `guard(read, *args)`."""
    while True:
        line = guard(stream.readline, 1024)
        if not line.endswith(b"\n"):
            raise _Refusal(400, "the chunked request body is malformed (a chunk-size line)")
        size_text = line.split(b";", 1)[0].strip()
        try:
            size = int(size_text, 16)
        except ValueError:
            raise _Refusal(400, "the chunked request body is malformed (a chunk size)") from None
        if size == 0:
            while True:
                trailer = guard(stream.readline, 8192)
                if trailer in (b"\r\n", b"\n", b""):
                    return
        left = size
        while left:
            piece = guard(stream.read, min(CHUNK_BYTES, left))
            if not piece:
                raise _Refusal(400, "the chunked request body ended inside a chunk")
            left -= len(piece)
            yield piece
        if guard(stream.readline, 3) not in (b"\r\n", b"\n"):
            raise _Refusal(400, "the chunked request body is malformed (a chunk's end)")


def _upstream_failure(error: BaseException, upstream: Upstream, timeout: float) -> Tuple[int, str]:
    """The status and message a failed upstream exchange is answered with."""
    if isinstance(error, ssl.SSLCertVerificationError):
        why = getattr(error, "verify_message", None) or str(error)
        return 502, (
            f"TLS verification of {upstream.host_header} failed: {why}. The forwarder verifies the "
            "upstream against the system's certificate authorities and cannot be told not to."
        )
    if isinstance(error, ssl.SSLError):
        return 502, f"the TLS handshake with {upstream.host_header} failed: {error}"
    if isinstance(error, socket.timeout):
        return 504, f"{upstream.url} went {timeout:g} s without answering (--timeout)"
    if isinstance(error, socket.gaierror):
        return 502, f"{upstream.host} does not resolve: {error}"
    if isinstance(error, (OSError, http.client.HTTPException)):
        return 502, f"{upstream.url} could not be reached: {error}"
    return 502, f"forwarding to {upstream.url} failed: {error}"


def add_arguments(parser: argparse.ArgumentParser) -> argparse.ArgumentParser:
    """Adds ``traza forward``'s options to `parser`."""
    parser.add_argument("--upstream", required=True, metavar="URL",
                        help="the https:// server to forward to, for example https://api.trazadb.com")
    parser.add_argument("--listen", default=DEFAULT_LISTEN, metavar="HOST:PORT",
                        help=f"the loopback address to listen on (default {DEFAULT_LISTEN}); "
                        "127.0.0.1, ::1 or localhost only")
    parser.add_argument("--max-body-bytes", type=int, default=DEFAULT_MAX_BODY_BYTES, metavar="N",
                        help=f"the largest request body forwarded (default {DEFAULT_MAX_BODY_BYTES}, "
                        "the server's own cap)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, metavar="SECONDS",
                        help=f"seconds without progress before an exchange is given up (default {DEFAULT_TIMEOUT:g})")
    parser.add_argument("--quiet", action="store_true", help="log nothing per request")
    return parser


def build_server(args: argparse.Namespace) -> ForwardServer:
    """The server `args` describe, bound but not yet serving; ``ValueError``
    for an address or URL refused."""
    upstream = parse_upstream(args.upstream)
    listen = parse_listen(args.listen)
    try:
        return ForwardServer(listen, upstream, max_body_bytes=args.max_body_bytes, timeout=args.timeout,
                             quiet=args.quiet)
    except OSError as error:
        raise ValueError(f"cannot listen on {listen.url}: {error}") from None


def run(args: argparse.Namespace) -> int:
    """Serves until interrupted; returns the exit status."""
    server = build_server(args)
    shown = Listen(server.server_address[0], server.server_port, server.address_family)
    print(f"traza forward: {shown.url} -> {server.upstream.url} (TLS verified); Ctrl-C to stop",
          file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    """``python -m traza.forward``: the same as ``traza forward``."""
    parser = add_arguments(argparse.ArgumentParser(
        prog="traza forward", description="Forward plain HTTP on loopback to an https:// Traza server."))
    args = parser.parse_args(argv)
    try:
        return run(args)
    except ValueError as error:
        print(f"traza forward: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
