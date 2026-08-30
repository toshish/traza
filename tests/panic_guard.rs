//! The connection-slot guard: a panicking handler must give its slot back.
//!
//! Before the guard, `connections_live` was decremented at the end of the
//! handler closure — a line a panic never reaches — so each handler panic
//! consumed a `--max-connections` slot permanently, and enough of them
//! walked the server to a permanent 503. The guard releases in `Drop`, which
//! unwinding runs, so the proof is behavioural: panic three handlers on a
//! two-slot server and the server must still answer.
//!
//! The panic itself is reachable only through `TRAZA_TEST_PANIC`, latched
//! once at startup; the second test pins down that without it the route does
//! not exist.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, extra: &[&str], test_panic: bool) -> Self {
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
            .env_remove("TRAZA_TEST_PANIC")
            .env("TRAZA_SOCKET_TIMEOUT_MS", "30000")
            .stderr(Stdio::piped());
        if test_panic {
            command.env("TRAZA_TEST_PANIC", "1");
        }
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

// A panicking test must never leak its server child: cargo waits on the
// child's inherited pipes, hanging the whole test binary long after the
// failure it should be reporting.
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
        "traza-panic-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn get(server: &Server, target: &str) -> (String, String) {
    let mut stream = server.connect();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    .expect("request writes");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("response reads");
    let text = String::from_utf8_lossy(&response).into_owned();
    match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head.to_owned(), body.to_owned()),
        None => (text, String::new()),
    }
}

fn metric(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .and_then(|value| value.trim().parse().ok())
}

#[test]
fn three_handler_panics_on_a_two_slot_server_leave_it_serving() {
    let dir = test_dir("slots");
    let server = Server::spawn(&dir, &["--max-connections", "2"], true);

    // Three panics against two slots: with the manual accounting this
    // replaced, the first two would each leak a slot and the third request
    // would already be refused. The slot release happens on the handler
    // thread's unwind, which the dying socket does not wait for, so each
    // panic is confirmed in the counter before the next fires — polled
    // under a failsafe rather than raced.
    for expected in 1..=3_u64 {
        let mut stream = server.connect();
        stream
            .write_all(b"GET /v1/test-panic HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .expect("panic request writes");
        // The connection dies mid-response: read until close (or RST) and
        // tolerate an empty reply — the panic beat any bytes out the door.
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (_, body) = get(&server, "/v1/metrics");
            if metric(&body, "traza_http_handler_panics_total") == Some(expected) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "panic {expected} never counted:\n{body:.400}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // (a) The slots came back: a fresh request is served, not 503-refused.
    let (head, _) = get(&server, "/v1/stats");
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "server no longer serves after panics: {head:.80}"
    );

    // (b) The gauge is back at its idle value — exactly the probe's own
    // connection — with all three panics on the books. Polled under a
    // failsafe: a just-closed probe's handler may still hold its slot for
    // a scheduling beat.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, body) = get(&server, "/v1/metrics");
        let live = metric(&body, "traza_http_connections_live");
        let panics = metric(&body, "traza_http_handler_panics_total");
        if live == Some(1) && panics == Some(3) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "slots never settled: live={live:?} panics={panics:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn the_panic_route_does_not_exist_without_the_startup_latch() {
    let dir = test_dir("no-latch");
    let server = Server::spawn(&dir, &[], false);
    let (head, _) = get(&server, "/v1/test-panic");
    assert!(
        head.starts_with("HTTP/1.1 404"),
        "route exists without TRAZA_TEST_PANIC: {head:.80}"
    );
}
