//! Persistent connections: framing must stay unambiguous across requests.
//!
//! Keep-alive is a throughput feature with a security tail. Once a socket
//! carries more than one request, any disagreement about where a body ends
//! lets a crafted request be split in two, with the remainder attributed to
//! whatever the client sends next — request smuggling. So the tests that
//! matter here are not "does it go faster" but "does the server ever leave
//! bytes on a socket it intends to reuse".
//!
//! The other half is the announcement: a response saying `keep-alive` while
//! the server closes (or the reverse) desynchronizes any client that believes
//! the header. Every case below asserts the header AND the behaviour.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("buffered")
            .env_remove("TRAZA_TOKENS")
            .env("TRAZA_SOCKET_TIMEOUT_MS", "30000")
            .stderr(Stdio::piped());
        for argument in extra {
            command.arg(argument);
        }
        let mut child = command.spawn().expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = BufReader::new(stderr);
        let port = {
            let mut line = String::new();
            let mut startup = String::new();
            loop {
                line.clear();
                // A zero-length read is EOF: the server exited before it
                // listened. Looping on that spins forever and looks exactly
                // like a hung test, so report what it actually said instead.
                if reader.read_line(&mut line).expect("stderr read") == 0 {
                    panic!("traza-server exited before listening:\n{startup}");
                }
                startup.push_str(&line);
                if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                    break rest.trim().parse::<u16>().expect("port parses");
                }
            }
        };
        std::thread::spawn(move || for _ in reader.lines() {});
        Self { child, port }
    }

    fn connect(&self) -> TcpStream {
        let mut attempt = 0;
        loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .expect("timeout");
                    break stream;
                }
                Err(_) if attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect: {error}"),
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-keepalive-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Reads exactly one response: headers, then `Content-Length` bytes. Anything
/// left in the stream stays there — which is the point, since a server that
/// over- or under-delivers would corrupt the NEXT read on this connection.
fn read_response(stream: &mut impl Read) -> (String, String) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = match stream.read(&mut byte) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("header byte: {error}"),
        };
        assert_ne!(read, 0, "connection closed mid-header: {head:?}");
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).expect("utf-8 headers");
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).expect("body");
    (head, String::from_utf8_lossy(&body).into_owned())
}

/// Drains a connection the server has promised to close, returning whatever
/// arrived after the response already read. A close delivered as RST is still
/// a close — under load the kernel turns close-with-unread-input into a reset,
/// which `read_to_end` reports as an error after handing over every byte that
/// preceded it — so reset-shaped errors are tolerated. Any other error (a
/// timeout above all) still panics: a server that neither sends nor closes is
/// exactly the broken announcement these tests exist to catch, and swallowing
/// the timeout would let it pass.
fn drain_after_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut rest = Vec::new();
    if let Err(error) = stream.read_to_end(&mut rest) {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            "read after close failed with {:?}: {error}",
            error.kind()
        );
    }
    rest
}

fn span_body(id: u32) -> String {
    json!([{
        "trace_id": format!("{id:032x}"),
        "span_id": format!("{id:016x}"),
        "name": "op",
        "service": "svc",
        "start_time_ns": 1_000,
        "end_time_ns": 2_000,
    }])
    .to_string()
}

fn post(id: u32) -> String {
    let body = span_body(id);
    format!(
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn many_requests_are_served_on_one_connection() {
    let dir = test_dir("reuse");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    for id in 1..=25 {
        stream.write_all(post(id).as_bytes()).expect("write");
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"), "request {id}: {head}");
        assert!(
            head.to_ascii_lowercase().contains("connection: keep-alive"),
            "request {id} must announce reuse: {head}"
        );
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["accepted"], 1, "request {id}");
    }

    // All 25 landed, so nothing was lost to a framing slip.
    stream
        .write_all(b"GET /v1/stats HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    let (_, body) = read_response(&mut stream);
    let stats: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(stats["record_count"], 25);
}

#[test]
fn a_pipelined_second_request_is_not_swallowed() {
    // Both requests are written before either response is read, so the second
    // one arrives in the same TCP read as the first. A per-request buffer
    // would drop it.
    let dir = test_dir("pipeline");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    let both = format!("{}{}", post(1), post(2));
    stream.write_all(both.as_bytes()).expect("write");

    for expected in 1..=2 {
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"), "response {expected}");
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["accepted"], 1);
    }
}

#[test]
fn connection_close_is_honoured_and_announced() {
    let dir = test_dir("close");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    let body = span_body(1);
    let request = format!(
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "{head}"
    );

    // The server said close, so it must actually close.
    let rest = drain_after_close(&mut stream);
    assert!(rest.is_empty(), "trailing bytes after close: {rest:?}");
}

#[test]
fn an_http_1_0_client_gets_a_closed_connection_by_default() {
    let dir = test_dir("http10");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    stream
        .write_all(b"GET /v1/stats HTTP/1.0\r\nHost: x\r\n\r\n")
        .expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "HTTP/1.0 defaults to close: {head}"
    );
    let rest = drain_after_close(&mut stream);
    assert!(rest.is_empty());
}

#[test]
fn an_http_1_0_client_asking_for_keep_alive_gets_it() {
    let dir = test_dir("http10-ka");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    for _ in 0..2 {
        stream
            .write_all(b"GET /v1/stats HTTP/1.0\r\nHost: x\r\nConnection: keep-alive\r\n\r\n")
            .expect("write");
        let (head, _) = read_response(&mut stream);
        assert!(
            head.to_ascii_lowercase().contains("connection: keep-alive"),
            "{head}"
        );
    }
}

#[test]
fn a_connection_header_list_is_parsed_token_wise() {
    // `Connection: keep-alive, Upgrade` is ONE header with two tokens. A
    // substring match on "close" would also mis-fire on "close-something".
    let dir = test_dir("tokens");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();
    stream
        .write_all(b"GET /v1/stats HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Upgrade\r\n\r\n")
        .expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(
        head.to_ascii_lowercase().contains("connection: keep-alive"),
        "{head}"
    );
}

#[test]
fn a_transfer_encoded_body_is_refused_rather_than_guessed() {
    // The smuggling classic: Content-Length and Transfer-Encoding disagreeing
    // about where the body ends. Traza reads only Content-Length, so a
    // chunked body would leave its chunk framing in the socket to be read as
    // the next request. Refuse and close instead.
    let dir = test_dir("chunked");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    let smuggled = "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /v1/stats HTTP/1.1\r\nHost: x\r\n\r\n";
    stream.write_all(smuggled.as_bytes()).expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "a refused framing must close: {head}"
    );

    // Crucially: the smuggled GET must NOT have been answered.
    let rest = drain_after_close(&mut stream);
    assert!(
        rest.is_empty(),
        "smuggled request was answered: {}",
        String::from_utf8_lossy(&rest)
    );
}

#[test]
fn duplicate_content_length_headers_are_refused() {
    let dir = test_dir("dup-length");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    stream
        .write_all(
            b"POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n[]",
        )
        .expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    assert!(head.to_ascii_lowercase().contains("connection: close"));
}

#[test]
fn an_unauthorized_request_with_a_body_closes_instead_of_reusing() {
    // Auth is rejected BEFORE the body is read, so those bytes are still in
    // the socket. Reusing the connection would parse them as a request — and
    // this one carries a complete GET to prove it.
    let dir = test_dir("auth-close");
    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
    command
        .arg("--data-dir")
        .arg(&dir)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .env("TRAZA_TOKENS", "rw:writer-token")
        .env("TRAZA_SOCKET_TIMEOUT_MS", "30000")
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn");
    let stderr = child.stderr.take().expect("stderr");
    let mut reader = BufReader::new(stderr);
    let port = {
        let mut line = String::new();
        let mut startup = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).expect("read") == 0 {
                panic!("traza-server exited before listening:\n{startup}");
            }
            startup.push_str(&line);
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port");
            }
        }
    };
    std::thread::spawn(move || for _ in reader.lines() {});

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let payload = "GET /v1/stats HTTP/1.1\r\nHost: x\r\n\r\n";
    let request = format!(
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");

    // The server closes with the refused body still unread in its receive
    // queue, and THAT close is the one the kernel most reliably turns into
    // RST — tolerated by the drain, which still surfaces any bytes that
    // preceded it.
    let rest = drain_after_close(&mut stream);
    assert!(
        rest.is_empty(),
        "the unread body was served as a request: {}",
        String::from_utf8_lossy(&rest)
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_connection_limit_refuses_rather_than_queues() {
    // Backpressure must be visible. A server that accepts everything and
    // queues it reports the overload as latency, which is indistinguishable
    // from being slow.
    let dir = test_dir("limit");
    let server = Server::spawn(&dir, &["--max-connections", "2"]);

    // Hold two connections open with a request in flight on each.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut stream = server.connect();
        stream
            .write_all(b"GET /v1/stats HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write");
        let (head, _) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        held.push(stream);
    }

    // The third is refused outright.
    let mut third = server.connect();
    third
        .write_all(b"GET /v1/stats HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    let response = drain_after_close(&mut third);
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "over-limit connection must be refused, got: {response:.80}"
    );

    // And the refusal is counted, so an operator can see it happened. Counted
    // is still the assertion, but two asynchronies sit between the 503 above
    // and the counter being readable: a dropped connection's slot is reaped
    // only when its handler thread notices the close — a probe that outruns
    // the reaper is itself refused — and the counter increments on the accept
    // thread, not in the 503 the client just read. So poll under a failsafe
    // deadline, counting every 503 the probe itself takes, and the comparison
    // stays EXACT: a refusal the counter never absorbs times out red, and an
    // over-counting server dies on the spot.
    drop(held);
    let mut expected = 1_u64; // the third connection above
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut probe = server.connect();
        probe
            .write_all(b"GET /v1/metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write");
        let (head, body) = read_response(&mut probe);
        if head.starts_with("HTTP/1.1 503") {
            expected += 1; // the probe outran the reaper: one more refusal
        } else {
            let counted: u64 = body
                .lines()
                .find_map(|line| line.strip_prefix("traza_http_connections_refused_total "))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            assert!(
                counted <= expected,
                "more refusals counted than happened: {counted} > {expected}\n{body}"
            );
            if counted == expected {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "refusals never fully counted: expected {expected}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn metrics_report_the_stages_the_benchmark_reads() {
    let dir = test_dir("metrics");
    let server = Server::spawn(&dir, &[]);
    let mut stream = server.connect();

    for id in 1..=3 {
        stream.write_all(post(id).as_bytes()).expect("write");
        let (head, _) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"));
    }
    stream
        .write_all(b"GET /v1/metrics HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    let (head, body) = read_response(&mut stream);
    assert!(head.contains("text/plain"), "{head}");
    for name in [
        "traza_spans_admitted_total 3",
        "traza_batches_admitted_total 3",
        "traza_writer_lock_wait_ns_count",
        // The two halves of the log append. The benchmark reads both to tell
        // a contended lock from a slow device, so their names are a contract.
        "traza_wal_lock_wait_ns_count",
        "traza_wal_write_syscall_ns_count",
        "traza_buffer_upsert_ns_count",
        "traza_http_requests_total",
        "traza_http_connections_live",
    ] {
        assert!(body.contains(name), "missing {name} in:\n{body}");
    }
}
