#!/usr/bin/env python3
"""Run the traza object-storage integration test against a throwaway SeaweedFS.

This harness provisions a fresh, authenticated SeaweedFS 4.45 S3 endpoint bound
only to loopback, hands the Rust integration test synthetic (test-only)
credentials via the usual AWS environment variables, runs the test, and tears
everything down again -- even on failure or interruption.

Nothing about a production store or real credentials is ever read or written.
The SeaweedFS binary is downloaded from the official GitHub release, pinned by
version *and* SHA-256, and only a single ``weed`` member is extracted.

Note for the Rust integration test:
    The default bucket (``traza-test``) is created by SeaweedFS asynchronously
    at startup. The test's *first* authenticated ``HEAD Bucket`` may therefore
    briefly observe a 404 while the default bucket is still being initialised;
    the test must retry that initial HEAD for a few seconds before failing.

Python 3.10+ standard library only. No third-party dependencies (no boto3).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from collections import deque
from pathlib import Path
from threading import Thread

# --------------------------------------------------------------------------- #
# Pinned SeaweedFS release. Digests are the exact SHA-256 of each official
# 4.45 platform archive; the binary is refused if it does not match.
# --------------------------------------------------------------------------- #
SEAWEEDFS_VERSION = "4.45"
RELEASE_URL = "https://github.com/seaweedfs/seaweedfs/releases/download/{version}/{archive}"
ARCHIVE_DIGESTS = {
    "darwin_arm64.tar.gz": "e38ed55f9b9d59d926befcba05088010454551677547835ba154dbc2a2d8d4a4",
    "linux_amd64.tar.gz": "c408894668aeaa74d4f251e20b350fd72195cbe596ddc3f48658709714f7be36",
    "linux_arm64.tar.gz": "8b40d7f2c72765a0f2fef785e51bee186475526d01415276e641164652209269",
}

MAX_ARCHIVE_BYTES = 64 * 1024 * 1024   # 64 MiB download ceiling
MAX_BINARY_BYTES = 256 * 1024 * 1024   # 256 MiB extracted-binary ceiling
MAX_LOG_BYTES = 64 * 1024 * 1024       # 64 MiB per captured log file
DOWNLOAD_TIMEOUT = 30                   # seconds, connect+read for urllib
READINESS_TIMEOUT = 30                  # seconds to wait for authenticated S3
CHUNK = 64 * 1024

# --------------------------------------------------------------------------- #
# Synthetic, test-only credentials. These are NOT secret: they exist purely to
# exercise the S3 auth path against a disposable loopback backend. Never reuse
# them for anything real.
# --------------------------------------------------------------------------- #
TEST_ACCESS_KEY = "traza-test-access-key"
TEST_SECRET_KEY = "traza-synthetic-test-secret-DO-NOT-USE-0000000000"
TEST_REGION = "us-east-1"
TEST_BUCKET = "traza-test"

# Order matters: this is the argument name paired with each reserved port.
PORT_FLAGS = [
    "master.port",
    "master.port.grpc",
    "volume.port",
    "volume.port.grpc",
    "filer.port",
    "filer.port.grpc",
    "s3.port",
    "s3.port.grpc",
]
S3_HTTP_INDEX = PORT_FLAGS.index("s3.port")

# AWS credential-provider env vars that could send the child looking at an
# external identity source. Strip them so the test only sees our loopback creds.
AWS_LOOKUP_VARS = [
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
]


class HarnessError(RuntimeError):
    """A recoverable failure that should be reported cleanly, not traced."""


# --------------------------------------------------------------------------- #
# Platform / archive selection
# --------------------------------------------------------------------------- #
def select_archive() -> str:
    """Return the pinned archive name for this platform, or fail loudly."""
    system = platform.system()
    machine = platform.machine().lower()
    if system == "Darwin" and machine in ("arm64", "aarch64"):
        return "darwin_arm64.tar.gz"
    if system == "Linux" and machine in ("x86_64", "amd64"):
        return "linux_amd64.tar.gz"
    if system == "Linux" and machine in ("arm64", "aarch64"):
        return "linux_arm64.tar.gz"
    raise HarnessError(
        f"unsupported platform {system}/{machine}: this harness supports "
        "Darwin arm64, Linux x86_64, and Linux arm64 only"
    )


# --------------------------------------------------------------------------- #
# Download + digest verification
# --------------------------------------------------------------------------- #
def sha256_of_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(CHUNK), b""):
            digest.update(block)
    return digest.hexdigest()


def download_archive(url: str, dest: Path) -> str:
    """Stream ``url`` to ``dest`` over HTTPS, enforcing the size cap.

    Returns the hex SHA-256 of the downloaded bytes.
    """
    if not url.lower().startswith("https://"):
        raise HarnessError(f"refusing non-HTTPS download URL: {url}")
    request = urllib.request.Request(url, headers={"User-Agent": "traza-object-s3-test"})
    # Disable any proxy for the actual transfer? No -- the release host may sit
    # behind a corporate proxy. We only bypass proxies for loopback readiness.
    digest = hashlib.sha256()
    total = 0
    try:
        with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT) as response:
            with open(dest, "wb") as out:
                while True:
                    chunk = response.read(CHUNK)
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > MAX_ARCHIVE_BYTES:
                        raise HarnessError(
                            f"archive exceeded {MAX_ARCHIVE_BYTES} bytes; aborting download"
                        )
                    digest.update(chunk)
                    out.write(chunk)
    except urllib.error.URLError as error:
        raise HarnessError(f"failed to download {url}: {error}") from error
    return digest.hexdigest()


def obtain_archive(archive_name: str, work_dir: Path, provided: Path | None) -> Path:
    """Return a path to a verified archive, downloading if none was provided."""
    expected = ARCHIVE_DIGESTS[archive_name]
    if provided is not None:
        provided = provided.resolve()
        if not provided.is_file():
            raise HarnessError(f"--archive path is not a file: {provided}")
        if provided.stat().st_size > MAX_ARCHIVE_BYTES:
            raise HarnessError(f"provided archive exceeds {MAX_ARCHIVE_BYTES} bytes")
        actual = sha256_of_file(provided)
        if actual != expected:
            raise HarnessError(
                f"provided archive digest mismatch for {archive_name}\n"
                f"  expected {expected}\n  actual   {actual}"
            )
        print(f"Using provided archive {provided} (sha256 verified)", flush=True)
        return provided

    url = RELEASE_URL.format(version=SEAWEEDFS_VERSION, archive=archive_name)
    dest = work_dir / archive_name
    print(f"Downloading {url}", flush=True)
    actual = download_archive(url, dest)
    if actual != expected:
        raise HarnessError(
            f"downloaded archive digest mismatch for {archive_name}\n"
            f"  expected {expected}\n  actual   {actual}"
        )
    print("Download sha256 verified", flush=True)
    return dest


# --------------------------------------------------------------------------- #
# Extraction (single regular member named "weed" only -- never extractall)
# --------------------------------------------------------------------------- #
def extract_weed(archive_path: Path, dest_dir: Path) -> Path:
    """Extract only the single regular ``weed`` member into ``dest_dir``."""
    with tarfile.open(archive_path, "r:gz") as tar:
        member = None
        for candidate in tar.getmembers():
            if candidate.name in ("weed", "./weed"):
                member = candidate
                break
        if member is None:
            raise HarnessError("archive does not contain a 'weed' member")
        if not member.isreg():
            raise HarnessError("'weed' member is not a regular file")
        if member.issym() or member.islnk():
            raise HarnessError("'weed' member is a link; refusing to extract")
        if member.size > MAX_BINARY_BYTES:
            raise HarnessError(
                f"'weed' member is {member.size} bytes (> {MAX_BINARY_BYTES}); refusing"
            )

        weed_path = dest_dir / "weed"
        source = tar.extractfile(member)
        if source is None:
            raise HarnessError("could not read the 'weed' member from the archive")
        written = 0
        with source, open(weed_path, "wb") as out:
            while True:
                chunk = source.read(CHUNK)
                if not chunk:
                    break
                written += len(chunk)
                if written > MAX_BINARY_BYTES:
                    raise HarnessError("extracted binary exceeded size cap")
                out.write(chunk)
    os.chmod(weed_path, 0o755)
    return weed_path


def verify_weed_version(weed_path: Path) -> str:
    """Run ``weed version`` once and confirm it reports 4.45."""
    try:
        result = subprocess.run(
            [str(weed_path), "version"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise HarnessError(f"could not run 'weed version': {error}") from error
    output = (result.stdout or "") + (result.stderr or "")
    if result.returncode != 0 or SEAWEEDFS_VERSION not in output:
        raise HarnessError(
            f"weed did not report version {SEAWEEDFS_VERSION}; got: {output.strip()!r}"
        )
    return output.strip().splitlines()[0] if output.strip() else ""


# --------------------------------------------------------------------------- #
# Ports
# --------------------------------------------------------------------------- #
def reserve_ports(count: int = 8):
    """Bind ``count`` free loopback TCP ports; keep the sockets held open."""
    sockets = []
    ports = []
    try:
        for _ in range(count):
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.bind(("127.0.0.1", 0))
            sockets.append(sock)
            ports.append(sock.getsockname()[1])
    except OSError:
        for sock in sockets:
            sock.close()
        raise
    return sockets, ports


# --------------------------------------------------------------------------- #
# Backend lifecycle
# --------------------------------------------------------------------------- #
def write_s3_config(path: Path) -> None:
    """Write the SeaweedFS identity config enabling authenticated S3 access."""
    config = {
        "identities": [
            {
                "name": "traza-test",
                "credentials": [
                    {"accessKey": TEST_ACCESS_KEY, "secretKey": TEST_SECRET_KEY}
                ],
                "actions": ["Admin", "Read", "Write", "List", "Tagging"],
            }
        ]
    }
    path.write_text(json.dumps(config, indent=2) + "\n")


def build_weed_args(weed_path: Path, data_dir: Path, config_path: Path, ports) -> list[str]:
    args = [
        str(weed_path),
        "mini",
        f"-dir={data_dir}",
        "-ip=127.0.0.1",
        "-ip.bind=127.0.0.1",
        "-master.telemetry=false",
        "-webdav=false",
        "-admin.ui=false",
        "-s3.port.iceberg=0",
        "-s3.port.lance=0",
        "-master.volumeSizeLimitMB=64",
        f"-bucket={TEST_BUCKET}",
        f"-s3.config={config_path}",
    ]
    for flag, port in zip(PORT_FLAGS, ports):
        args.append(f"-{flag}={port}")
    return args


def _loopback_opener() -> urllib.request.OpenerDirector:
    """An opener with proxies disabled -- loopback must never be proxied."""
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def s3_auth_enforced(endpoint: str, opener: urllib.request.OpenerDirector) -> bool:
    """True once the S3 gateway rejects an *unauthenticated* request with 403.

    This proves both that the gateway is listening and that authentication is
    actually being enforced -- a healthy-but-open process is not "ready".
    """
    request = urllib.request.Request(endpoint + "/", method="GET")
    try:
        with opener.open(request, timeout=2):
            pass
        return False  # anonymous access accepted -> auth NOT enforced
    except urllib.error.HTTPError as error:
        if error.code == 403:
            body = error.read().decode("utf-8", "replace")
            return "AccessDenied" in body or "InvalidAccessKeyId" in body
        return False
    except urllib.error.URLError:
        return False  # not listening yet


def wait_for_backend(server: subprocess.Popen, endpoint: str, log_path: Path) -> None:
    opener = _loopback_opener()
    deadline = time.monotonic() + READINESS_TIMEOUT
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise HarnessError(
                f"SeaweedFS exited early with code {server.returncode}; see {log_path}"
            )
        if s3_auth_enforced(endpoint, opener):
            if server.poll() is not None:
                raise HarnessError(f"SeaweedFS exited during readiness; see {log_path}")
            return
        time.sleep(0.2)
    raise HarnessError(
        f"SeaweedFS S3 endpoint did not enforce auth within {READINESS_TIMEOUT}s; "
        f"see {log_path}"
    )


def stop_process(proc: subprocess.Popen | None, use_group: bool, grace: float) -> int | None:
    """Stop and reap a child we own. SIGINT/SIGTERM, bounded wait, then kill."""
    if proc is None:
        return None
    if proc.poll() is not None and not use_group:
        return proc.returncode
    try:
        if use_group:
            os.killpg(proc.pid, signal.SIGTERM)
        else:
            proc.send_signal(signal.SIGINT)
    except (ProcessLookupError, OSError):
        pass
    try:
        proc.wait(grace)
    except subprocess.TimeoutExpired:
        try:
            if use_group:
                os.killpg(proc.pid, signal.SIGKILL)
            else:
                proc.kill()
        except (ProcessLookupError, OSError):
            pass
        try:
            proc.wait(grace)
        except subprocess.TimeoutExpired:
            pass
    if use_group:
        # The session leader may exit while a compiler or test descendant still
        # holds stdout open. The group ID was allocated by start_new_session.
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    return proc.returncode


# --------------------------------------------------------------------------- #
# Child test environment + execution
# --------------------------------------------------------------------------- #
def build_child_env(endpoint: str) -> dict:
    env = os.environ.copy()
    for var in AWS_LOOKUP_VARS:
        env.pop(var, None)
    env.update(
        AWS_ACCESS_KEY_ID=TEST_ACCESS_KEY,
        AWS_SECRET_ACCESS_KEY=TEST_SECRET_KEY,
        AWS_DEFAULT_REGION=TEST_REGION,
        AWS_REGION=TEST_REGION,
        AWS_EC2_METADATA_DISABLED="true",
        AWS_ENDPOINT_URL=endpoint,
        AWS_ENDPOINT_URL_S3=endpoint,
        AWS_ALLOW_HTTP="true",
        AWS_BUCKET=TEST_BUCKET,
        S3_BUCKET=TEST_BUCKET,
        S3_ENDPOINT=endpoint,
        TRAZA_TEST_S3_ENDPOINT=endpoint,
        TRAZA_TEST_S3_BUCKET=TEST_BUCKET,
        # Keep loopback off any inherited proxy.
        NO_PROXY="127.0.0.1,localhost,::1",
        no_proxy="127.0.0.1,localhost,::1",
    )
    return env


def build_cargo_command(cargo: str, test_name: str) -> list[str]:
    return [
        cargo,
        "test",
        "--locked",
        "--features",
        "object-storage",
        "--test",
        test_name,
        "--",
        "--ignored",
        "--nocapture",
    ]


class SummaryParser:
    """Track cargo integration-test summaries as output streams by."""

    _PATTERN = None

    def __init__(self):
        import re

        if SummaryParser._PATTERN is None:
            SummaryParser._PATTERN = re.compile(
                r"test result:\s+(ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed"
            )
        self.summaries = []

    def feed(self, line: str) -> None:
        match = SummaryParser._PATTERN.search(line)
        if match:
            self.summaries.append(
                {
                    "status": match.group(1),
                    "passed": int(match.group(2)),
                    "failed": int(match.group(3)),
                }
            )

    @property
    def total_passed(self) -> int:
        return sum(s["passed"] for s in self.summaries)

    @property
    def total_failed(self) -> int:
        return sum(s["failed"] for s in self.summaries)

    @property
    def saw_result(self) -> bool:
        return bool(self.summaries)


def run_test(cmd: list[str], env: dict, cwd: Path, timeout: int, log_path: Path):
    """Run the cargo test, mirroring output to console and a bounded log file.

    Returns ``(exit_code, timed_out, parser, tail)``.
    """
    parser = SummaryParser()
    tail = deque(maxlen=60)
    proc = subprocess.Popen(
        cmd,
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,  # own process group so we can reap the test binary
    )

    written = [0]

    def pump() -> None:
        assert proc.stdout is not None
        with open(log_path, "wb") as log:
            for raw in iter(proc.stdout.readline, b""):
                sys.stdout.buffer.write(raw)
                sys.stdout.buffer.flush()
                if written[0] < MAX_LOG_BYTES:
                    log.write(raw)
                    written[0] += len(raw)
                    log.flush()
                text = raw.decode("utf-8", "replace").rstrip("\n")
                tail.append(text)
                parser.feed(text)

    reader = Thread(target=pump, daemon=True)
    reader.start()

    timed_out = False
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        stop_process(proc, use_group=True, grace=15)
    except BaseException:
        # Cargo runs in its own session, so terminal interrupts do not reach it.
        stop_process(proc, use_group=True, grace=15)
        reader.join(10)
        raise
    # Closing the pipe (process is gone) unblocks the reader; bound the join so
    # a stalled reader can never hang cleanup.
    reader.join(10)
    if reader.is_alive():
        stop_process(proc, use_group=True, grace=15)
        reader.join(10)
        raise HarnessError("test output remained open after the command exited")
    return proc.returncode, timed_out, parser, tail


# --------------------------------------------------------------------------- #
# Evidence
# --------------------------------------------------------------------------- #
def preserve_evidence(evidence_dir: Path, files: list[Path]) -> None:
    for src in files:
        if src.exists():
            try:
                shutil.copy2(src, evidence_dir / src.name)
            except OSError as error:
                print(f"warning: could not preserve {src.name}: {error}", flush=True)


# --------------------------------------------------------------------------- #
# Orchestration
# --------------------------------------------------------------------------- #
def run(args) -> int:
    repo_root = Path(__file__).resolve().parent.parent
    archive_name = select_archive()

    evidence_dir = None
    if args.evidence_dir is not None:
        evidence_dir = args.evidence_dir.resolve()
        # Must be a brand-new directory so we never clobber an unrelated path.
        evidence_dir.mkdir(parents=True, exist_ok=False)

    receipt = {
        "backend": "SeaweedFS",
        "version": SEAWEEDFS_VERSION,
        "archive": archive_name,
        "started_at": time.time(),
    }

    tmp = tempfile.TemporaryDirectory(prefix="traza-object-s3-")
    work_dir = Path(tmp.name)
    server_log = work_dir / "server.log"
    test_log = work_dir / "test.log"
    receipt_path = work_dir / "receipt.json"
    data_dir = work_dir / "data"
    bin_dir = work_dir / "bin"
    config_path = work_dir / "s3-config.json"
    data_dir.mkdir()
    bin_dir.mkdir()

    server = None
    exit_code = 1
    sockets = []

    def finalize(status_note: str) -> None:
        """Best-effort: stop the backend, write the receipt, preserve evidence."""
        backend_code = stop_process(server, use_group=False, grace=15)
        for sock in sockets:
            try:
                sock.close()
            except OSError:
                pass
        receipt["finished_at"] = time.time()
        receipt["backend_exit_code"] = backend_code
        receipt["cleanup"] = {
            "note": status_note,
            "backend_stopped": server is None or server.poll() is not None,
        }
        try:
            receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
        except OSError as error:
            print(f"warning: could not write receipt: {error}", flush=True)
        if evidence_dir is not None:
            # Never copy the S3 config -- it holds the (synthetic) secret key.
            preserve_evidence(evidence_dir, [server_log, test_log, receipt_path])

    try:
        archive_path = obtain_archive(archive_name, work_dir, args.archive)
        receipt["archive_sha256"] = ARCHIVE_DIGESTS[archive_name]

        weed_path = extract_weed(archive_path, bin_dir)
        version_line = verify_weed_version(weed_path)
        receipt["weed_version_line"] = version_line
        print(f"Verified SeaweedFS binary: {version_line}", flush=True)

        write_s3_config(config_path)

        sockets, ports = reserve_ports(8)
        endpoint = f"http://127.0.0.1:{ports[S3_HTTP_INDEX]}"
        receipt["endpoint"] = endpoint
        receipt["bucket"] = TEST_BUCKET
        receipt["ports"] = dict(zip(PORT_FLAGS, ports))

        weed_args = build_weed_args(weed_path, data_dir, config_path, ports)
        receipt["server_command"] = weed_args

        # Release the reserved ports immediately before handing them to weed.
        with open(server_log, "wb") as log:
            for sock in sockets:
                sock.close()
            sockets = []
            server = subprocess.Popen(
                weed_args, env=build_child_env(endpoint),
                stdout=log, stderr=subprocess.STDOUT,
            )

        wait_for_backend(server, endpoint, server_log)
        receipt["backend_pid"] = server.pid
        print(f"SeaweedFS {SEAWEEDFS_VERSION} ready (authenticated) at {endpoint}", flush=True)
        print(f"Synthetic loopback bucket: {TEST_BUCKET}", flush=True)

        child_env = build_child_env(endpoint)
        cargo_cmd = build_cargo_command(args.cargo, args.test_name)
        receipt["test_command"] = cargo_cmd
        print(f"Running: {' '.join(cargo_cmd)} (cwd={repo_root})", flush=True)

        code, timed_out, parser, tail = run_test(
            cargo_cmd, child_env, repo_root, args.timeout, test_log
        )
        receipt["test_exit_code"] = code
        receipt["test_timed_out"] = timed_out
        receipt["tests_passed"] = parser.total_passed
        receipt["tests_failed"] = parser.total_failed

        if timed_out:
            raise HarnessError(f"test timed out after {args.timeout}s")
        if code != 0:
            raise HarnessError(f"cargo test exited with code {code}")
        if not parser.saw_result:
            raise HarnessError(
                "cargo produced no 'test result:' summary -- no tests executed "
                f"(is the '{args.test_name}' test target present?)"
            )
        if parser.total_failed != 0:
            raise HarnessError(f"{parser.total_failed} test(s) failed")
        if parser.total_passed <= 0:
            raise HarnessError("cargo executed 0 tests; refusing to report success")

        exit_code = 0
        receipt["result"] = "ok"
        print(
            f"\nOK: {parser.total_passed} test(s) passed, 0 failed against "
            f"SeaweedFS {SEAWEEDFS_VERSION}",
            flush=True,
        )
        finalize("success")
        return exit_code

    except HarnessError as error:
        receipt["result"] = "failed"
        receipt["error"] = str(error)
        print(f"\nFAILED: {error}", file=sys.stderr, flush=True)
        finalize(f"failed: {error}")
        return 1
    except KeyboardInterrupt:
        receipt["result"] = "interrupted"
        print("\nInterrupted; shutting down backend...", file=sys.stderr, flush=True)
        finalize("interrupted")
        return 130
    except Exception as error:  # noqa: BLE001 - ensure the backend is always reaped
        receipt["result"] = "error"
        receipt["error"] = repr(error)
        print(f"\nUNEXPECTED ERROR: {error!r}", file=sys.stderr, flush=True)
        finalize(f"error: {error!r}")
        return 1
    finally:
        # Always remove the temp tree, even if evidence copy or cleanup raised.
        try:
            tmp.cleanup()
        except OSError:
            pass


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        prog="test-object-s3.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "The Rust integration test should retry its first authenticated "
            "HEAD Bucket for a few seconds: the default bucket is created "
            "asynchronously at SeaweedFS startup and may briefly 404."
        ),
    )
    parser.add_argument(
        "--archive",
        type=Path,
        default=None,
        help="Use a predownloaded SeaweedFS archive (SHA-256 still verified) "
        "instead of downloading it.",
    )
    parser.add_argument(
        "--evidence-dir",
        type=Path,
        default=None,
        help="New directory in which to preserve server.log, test.log and "
        "receipt.json (no credentials or data) on exit or failure.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Path to the cargo executable (default: cargo).",
    )
    parser.add_argument(
        "--test-name",
        default="object_storage_s3",
        help="Integration test target to run (default: object_storage_s3).",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=1800,
        help="Seconds before the test run is terminated (default: 1800).",
    )
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)

    # Translate SIGTERM into the same clean-shutdown path as Ctrl-C so that our
    # own children are always reaped. We never scan or signal foreign processes.
    def _on_term(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, _on_term)

    try:
        return run(args)
    except HarnessError as error:
        print(f"FAILED: {error}", file=sys.stderr, flush=True)
        return 1
    except FileExistsError:
        print(
            f"FAILED: --evidence-dir must be a new directory: {args.evidence_dir}",
            file=sys.stderr,
            flush=True,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
