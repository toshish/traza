//! Shared process-level harness: the real `traza-server` binary on an
//! ephemeral loopback port, against a private data directory, spoken to over
//! plain HTTP/1.1.
//!
//! Include it with `mod common;` from a test crate. Every test binary compiles
//! its own copy and uses a subset of it, hence the dead-code allowance.
//!
//! Hermetic by construction: the harness supplies `--data-dir`, `--host
//! 127.0.0.1` and `--port 0` itself (pass neither), and strips every
//! inherited `TRAZA_*` variable before applying the test's own, so a
//! developer's exported `TRAZA_TOKENS` cannot turn an open-mode test into an
//! auth test. Dropping a [`Server`] kills the child and removes its data
//! directory — on a panicking test too, which matters: a leaked child holds
//! the test binary's inherited pipes open and hangs `cargo test` long after
//! the failure it should be reporting.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Response headers (and trailers) in arrival order, names as sent.
pub type Headers = Vec<(String, String)>;

/// `(status, headers, body)` of one response.
pub type Response = (u16, Headers, String);

/// How long one request may wait for its response before the test fails. A
/// guard against a hung server wedging the suite, not a performance bound.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// A running `traza-server` and the data directory it owns.
pub struct Server {
    child: Child,
    /// The port the server bound (it was started with `--port 0`).
    pub port: u16,
    /// The server's private data directory, removed on drop.
    pub data_dir: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// Starts the server with extra command-line `args` and no environment
    /// beyond the hermetic default.
    pub fn spawn(args: &[&str]) -> Self {
        Self::spawn_with_env(args, &[])
    }

    /// Starts the server with extra `args` and the environment variables
    /// `env` (for example `("TRAZA_TOKENS", "rw:secret")`).
    pub fn spawn_with_env(args: &[&str], env: &[(&str, &str)]) -> Self {
        let data_dir = fresh_dir();
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let (child, port, stderr) = launch(&data_dir, &args, &env);
        Self {
            child,
            port,
            data_dir,
            args,
            env,
            stderr,
        }
    }

    /// Kills the server — SIGKILL, a crash rather than a shutdown — and
    /// starts it again on the same data directory with the same arguments and
    /// environment. What survives is exactly what was durable. The port
    /// usually changes; read [`Self::port`] afterwards.
    pub fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, port, stderr) = launch(&self.data_dir, &self.args, &self.env);
        self.child = child;
        self.port = port;
        self.stderr = stderr;
    }

    /// An absolute URL for `path` (which starts with `/`) on this server.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Everything the current server process has written to stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr
            .lock()
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }

    /// Sends one request and returns `(status, headers, body)`.
    ///
    /// `Content-Type: application/json` is sent unless `headers` names a
    /// content type. Headers come back in arrival order with their names as
    /// sent; a chunked body is de-chunked and its trailers appended to the
    /// headers, so a trailer such as `X-Traza-Export-Complete` is found by
    /// [`header`] like any other. The body is decoded as UTF-8, lossily.
    ///
    /// Reads until the server closes the connection, so it is not for a
    /// response that never ends (an event stream); drive those with a
    /// `TcpStream` of your own.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Response {
        let body = body.unwrap_or_default();
        let mut stream = self.connect();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.port,
            body.len()
        );
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            head.push_str("Content-Type: application/json\r\n");
        }
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .expect("writes request head");
        // A server may answer from the headers alone (an auth refusal) and
        // close before reading the body, so a failed body write is not the
        // verdict — the response is, and an incomplete one panics below.
        let _ = stream.write_all(body);

        let mut raw = Vec::new();
        let read = stream.read_to_end(&mut raw);
        match parse_response(&raw) {
            Some(response) => response,
            // A close delivered as RST once the response is complete is OS
            // noise (a loaded kernel turns the post-response close into a
            // reset); an INCOMPLETE response is a failure either way.
            None => panic!(
                "incomplete response to {method} {path} ({} bytes, read result {read:?}):\n{}\n--- server stderr ---\n{}",
                raw.len(),
                String::from_utf8_lossy(&raw),
                self.stderr()
            ),
        }
    }

    /// Sends `body` as JSON and parses the response body as JSON (`Null` when
    /// it is empty). Panics, showing the body, when the response is not JSON.
    pub fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let (status, _, text) = self.request(method, path, encoded.as_deref(), headers);
        if text.is_empty() {
            return (status, Value::Null);
        }
        let parsed = serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("{method} {path} answered {status} with a non-JSON body ({error}): {text}")
        });
        (status, parsed)
    }

    fn connect(&self) -> TcpStream {
        let mut attempt = 0;
        let stream = loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(stream) => break stream,
                Err(_) if attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect to 127.0.0.1:{}: {error}", self.port),
            }
        };
        stream
            .set_read_timeout(Some(RESPONSE_TIMEOUT))
            .expect("sets read timeout");
        stream
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// The first header (or trailer) named `name`, compared case-insensitively.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn fresh_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("traza-it-{}-{nonce}-{serial}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creates the data directory");
    dir
}

/// Spawns the binary and waits for its `listening on` line, which carries the
/// port `--port 0` resolved to. A server that exits first fails the test with
/// everything it printed.
fn launch(
    data_dir: &Path,
    args: &[String],
    env: &[(String, String)],
) -> (Child, u16, Arc<Mutex<Vec<String>>>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAZA_") {
            command.env_remove(key);
        }
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawns traza-server");
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut lines = BufReader::new(child.stderr.take().expect("stderr piped")).lines();
    let port = loop {
        let Some(Ok(line)) = lines.next() else {
            let status = child.wait().ok();
            panic!(
                "traza-server exited before listening ({status:?}):\n{}",
                stderr.lock().map(|l| l.join("\n")).unwrap_or_default()
            );
        };
        let port = line
            .strip_prefix("traza-server listening on 127.0.0.1:")
            .map(|rest| rest.trim().parse::<u16>().expect("port parses"));
        stderr.lock().expect("stderr log").push(line);
        if let Some(port) = port {
            break port;
        }
    };
    // Keep draining, or a chatty server blocks on a full pipe.
    let sink = Arc::clone(&stderr);
    std::thread::spawn(move || {
        for line in lines.map_while(Result::ok) {
            if let Ok(mut log) = sink.lock() {
                log.push(line);
            }
        }
    });
    (child, port, stderr)
}

/// Parses a complete HTTP/1.1 response, or `None` when `raw` is not one yet.
fn parse_response(raw: &[u8]) -> Option<Response> {
    let header_end = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let mut headers: Headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let rest = &raw[header_end + 4..];
    let body = if header(&headers, "transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        let (body, trailers) = dechunk(rest)?;
        headers.extend(trailers);
        body
    } else {
        match header(&headers, "content-length") {
            Some(length) => {
                let length: usize = length.parse().ok()?;
                rest.get(..length)?.to_vec()
            }
            // Neither length nor chunking: the body runs to the close.
            None => rest.to_vec(),
        }
    };
    Some((status, headers, String::from_utf8_lossy(&body).into_owned()))
}

/// Decodes a chunked body and its trailers, or `None` when it is truncated.
fn dechunk(mut raw: &[u8]) -> Option<(Vec<u8>, Headers)> {
    let mut body = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|bytes| bytes == b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[..line_end]).ok()?;
        let size_text = size_text.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            break;
        }
        body.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size..)?.strip_prefix(b"\r\n")?;
    }
    let mut trailers = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|bytes| bytes == b"\r\n")?;
        if line_end == 0 {
            return Some((body, trailers));
        }
        let line = std::str::from_utf8(&raw[..line_end]).ok()?;
        if let Some((name, value)) = line.split_once(':') {
            trailers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        raw = &raw[line_end + 2..];
    }
}
