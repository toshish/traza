//! Bearer-auth matrix, process-level: loopback is open by default; with
//! TRAZA_TOKENS, 401/403/200 behavior across ingest, OTLP, and flush endpoints.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, tokens: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .env_remove("TRAZA_TOKENS")
            .stderr(Stdio::piped());
        if let Some(tokens) = tokens {
            command.env("TRAZA_TOKENS", tokens);
        }
        let mut child = command.spawn().expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines.next().expect("port line").expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn request(
        &self,
        method: &str,
        target: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, String, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", self.port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 50 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect: {error}"),
                }
            }
        };
        let auth_header = bearer
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let body_len = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth_header}Content-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body");
        }
        let mut response = Vec::new();
        if let Err(error) = stream.read_to_end(&mut response) {
            // Auth is intentionally decided from headers before the body is
            // buffered, so the server can close with body bytes still unread.
            // The invariant that matters is that a COMPLETE response arrived;
            // how the peer tears the socket down afterwards is OS noise, and
            // pinning it to ECONNRESET made this test flaky under load.
            assert!(
                complete_http_response(&response),
                "incomplete response after {:?}: {error}",
                error.kind()
            );
        }
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let payload = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        (status, text, payload)
    }

    fn kill(self) {
        drop(self);
    }
}

fn complete_http_response(response: &[u8]) -> bool {
    let Some(header_end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&response[..header_end]) else {
        return false;
    };
    let Some(content_length) = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    }) else {
        return false;
    };
    response.len() >= header_end + 4 + content_length
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
    let dir =
        std::env::temp_dir().join(format!("traza-auth-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn one_span() -> Value {
    json!([{"trace_id": "t-auth", "span_id": "s1", "name": "op", "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64}])
}

#[test]
fn unset_tokens_means_open() {
    let dir = test_dir("open");
    let server = Server::spawn(&dir, None);
    let (status, _, _) = server.request("POST", "/v1/spans", None, Some(&one_span()));
    assert_eq!(status, 200, "auth disabled by default");
    let (status, _, _) = server.request("GET", "/v1/stats", None, None);
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn token_matrix_across_endpoints() {
    let dir = test_dir("matrix");
    let server = Server::spawn(&dir, Some("rw:writer-token,ro:reader-token"));

    // No header -> 401 with the Bearer challenge.
    let (status, raw, body) = server.request("POST", "/v1/spans", None, Some(&one_span()));
    assert_eq!(status, 401, "{body}");
    assert!(
        raw.contains("WWW-Authenticate: Bearer"),
        "challenge advertised: {raw}"
    );
    assert_eq!(body["error"], "unauthorized", "raw: {raw}");

    // Unknown token -> 401.
    let (status, _, _) = server.request("GET", "/v1/stats", Some("nope"), None);
    assert_eq!(status, 401);

    // ro can GET.
    let (status, _, _) = server.request("GET", "/v1/stats", Some("reader-token"), None);
    assert_eq!(status, 200);

    // ro cannot POST — 403 on every write endpoint.
    for (target, body) in [
        ("/v1/spans", Some(one_span())),
        ("/v1/flush", None),
        ("/v1/traces", Some(json!({"resourceSpans": []}))),
    ] {
        let (status, _, payload) =
            server.request("POST", target, Some("reader-token"), body.as_ref());
        assert_eq!(status, 403, "{target}: {payload}");
        assert_eq!(payload["error"], "forbidden", "{target}");
    }

    // rw can POST everywhere, and reads back.
    let (status, _, _) =
        server.request("POST", "/v1/spans", Some("writer-token"), Some(&one_span()));
    assert_eq!(status, 200);
    let (status, _, _) = server.request("POST", "/v1/flush", Some("writer-token"), None);
    assert_eq!(status, 200);
    let (status, _, body) = server.request("GET", "/v1/traces/t-auth", Some("writer-token"), None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(1));
    server.kill();
}

#[test]
fn invalid_token_config_refuses_startup() {
    // A SET but invalid TRAZA_TOKENS must not silently run open.
    let dir = test_dir("badcfg");
    let output = Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .arg("--data-dir")
        .arg(&dir)
        .arg("--port")
        .arg("0")
        .env("TRAZA_TOKENS", "not-a-valid-entry")
        .output()
        .expect("runs");
    assert!(
        !output.status.success(),
        "invalid auth config must refuse startup"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auth"),
        "startup error names auth: {stderr}"
    );
}

#[test]
fn unauthenticated_non_loopback_bind_requires_explicit_opt_in() {
    let dir = test_dir("unsafe-bind");
    let output = Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .arg("--data-dir")
        .arg(&dir)
        .arg("--host")
        .arg("0.0.0.0")
        .arg("--port")
        .arg("0")
        .env_remove("TRAZA_TOKENS")
        .output()
        .expect("runs");
    assert!(!output.status.success(), "unsafe default must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing unauthenticated non-loopback bind"),
        "{stderr}"
    );
    assert!(
        stderr.contains("--allow-unauthenticated-non-loopback"),
        "error names the deliberate override: {stderr}"
    );
}
