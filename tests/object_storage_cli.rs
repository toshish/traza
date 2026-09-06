//! Focused CLI regression: an explicit `--endpoint` must win over an
//! inherited `AWS_ENDPOINT_URL_S3`.
//!
//! Both endpoints are test-owned loopback sockets. Only the explicitly selected
//! endpoint answers; inherited configuration cannot redirect this operation.

#![cfg(feature = "object-storage")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FAKE_ACCESS_KEY: &str = "traza-cli-test-access";

/// What the stub observed on its single served request — request line and the
/// signing marker only. Deliberately no full headers, bodies, or secrets.
#[derive(Default)]
struct Captured {
    request_line: String,
    signed: bool,
}

/// A single-shot loopback S3 stub, stopped and joined on drop.
struct StubServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    seen: Arc<Mutex<Captured>>,
}

impl StubServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr").to_string();
        listener.set_nonblocking(true).expect("nonblocking stub");
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Captured::default()));

        let stop_thread = Arc::clone(&stop);
        let seen_thread = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        serve_once(stream, &seen_thread);
                        return; // one request only
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
            seen,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read up to the end of the request headers (bounded to 32 KiB and a 2s read
/// timeout), record the request line and signing marker, then reply with a
/// valid empty-bucket ListBucketResult.
fn serve_once(mut stream: TcpStream, seen: &Arc<Mutex<Captured>>) {
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() || buf.len() == 32 * 1024 {
            break;
        }
        stream
            .set_read_timeout(Some(remaining))
            .expect("read timeout");
        let available = chunk.len().min(32 * 1024 - buf.len());
        match stream.read(&mut chunk[..available]) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if find_subslice(&buf, b"\r\n\r\n").is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    // Parse only the request line and scan for the signing marker; nothing else
    // from the request is retained.
    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or_default().to_owned();
    let signed = head
        .lines()
        .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        .any(|line| {
            line.contains("AWS4-HMAC-SHA256")
                && line.contains(&format!("Credential={FAKE_ACCESS_KEY}/"))
        });
    if let Ok(mut guard) = seen.lock() {
        *guard = Captured {
            request_line,
            signed,
        };
    }

    let body = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
        "<Name>fixture</Name><Prefix>snapshots/</Prefix><Delimiter>/</Delimiter>",
        "<KeyCount>0</KeyCount><MaxKeys>1000</MaxKeys>",
        "<IsTruncated>false</IsTruncated></ListBucketResult>",
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Kills and reaps the child on any exit path (assertion failure, timeout, or
/// panic), so a stuck CLI process never leaks past the test.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn get(&mut self) -> &mut Child {
        self.0.as_mut().expect("child present")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn explicit_endpoint_overrides_inherited_aws_service_endpoint() {
    let stub = StubServer::start();

    // A second loopback socket owned by this test: bound so the port cannot be
    // reused, but never accepting, so anything that reaches it simply hangs.
    // Preferred over a fixed port or a dropped-and-reused reservation.
    let dummy = TcpListener::bind("127.0.0.1:0").expect("bind dummy");
    let dummy_endpoint = format!("http://{}", dummy.local_addr().expect("dummy addr"));

    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-object"));
    command
        .args([
            "list",
            "--bucket",
            "fixture",
            "--region",
            "us-east-1",
            "--endpoint",
            &stub.endpoint(),
            "--allow-http",
            "--op-timeout-secs",
            "1",
            "--retries",
            "0",
        ])
        .env_clear()
        // Synthetic credentials only — never this environment's real keys.
        .env("AWS_ACCESS_KEY_ID", FAKE_ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", "synthetic-not-secret-value")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        // The inherited service endpoint the explicit flag must override.
        .env("AWS_ENDPOINT_URL_S3", &dummy_endpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn traza-object");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let mut guard = ChildGuard(Some(child));

    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = guard.get().try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            panic!(
                "traza-object list did not finish within 12s; it likely honoured the \
                 inherited AWS_ENDPOINT_URL_S3 dummy instead of the explicit --endpoint"
            );
        }
        thread::sleep(Duration::from_millis(50));
    };

    // An empty listing has small output; the process deadline also catches
    // unexpected pipe-filling output. Bound the diagnostics retained here.
    let mut out = String::new();
    let mut err = String::new();
    stdout
        .take(8192)
        .read_to_string(&mut out)
        .expect("read stdout");
    stderr
        .take(8192)
        .read_to_string(&mut err)
        .expect("read stderr");

    // On failure surface only the bounded CLI stderr — never request headers or
    // the child's environment — and name the contract that broke.
    let context = || {
        format!(
            "explicit --endpoint must override the inherited AWS_ENDPOINT_URL_S3 \
             service default; CLI stderr:\n{}",
            err.trim()
        )
    };

    assert!(
        status.success(),
        "traza-object list should exit 0. {}",
        context()
    );
    assert!(
        out.trim().is_empty(),
        "an empty bucket should print no snapshots, got stdout:\n{out}\n{}",
        context()
    );

    let seen = stub.seen.lock().expect("captured");
    assert!(
        !seen.request_line.is_empty(),
        "the working stub received no request — the child went elsewhere. {}",
        context()
    );
    let line = seen.request_line.to_ascii_lowercase();
    assert!(
        line.starts_with("get ") && line.contains("list-type=2"),
        "expected a ListObjectsV2 GET, got request line {:?}. {}",
        seen.request_line,
        context()
    );
    // Path-style addressing puts the bucket in the path: `/fixture` (optionally
    // with a trailing slash before the query string).
    assert!(
        line.contains("/fixture?") || line.contains("/fixture/?"),
        "expected the request path to target /fixture, got {:?}. {}",
        seen.request_line,
        context()
    );
    assert!(
        seen.signed,
        "the request was not SigV4-signed with the child's synthetic credentials. {}",
        context()
    );
}
