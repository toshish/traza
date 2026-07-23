//! Bundled-dashboard acceptance, process-level: the real server serves the
//! embedded page at `/` and `/dashboard`; the page references no external
//! URLs; with auth enabled the shell stays open while the API stays gated.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DASHBOARD_HTML: &str = include_str!("../src/dashboard.html");

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

    fn get(&self, target: &str, bearer: Option<&str>) -> (u16, String, String) {
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
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: x\r\n{auth_header}Connection: close\r\n\r\n"
        )
        .expect("writes request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads response");
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status code");
        let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        (status, headers.to_ascii_lowercase(), body.to_owned())
    }

    fn kill(mut self) {
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
        std::env::temp_dir().join(format!("traza-dash-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
fn dashboard_route_serves_embedded_asset() {
    let dir = test_dir("route");
    let server = Server::spawn(&dir, None);
    for target in ["/", "/dashboard", "/dashboard/"] {
        let (status, headers, body) = server.get(target, None);
        assert_eq!(status, 200, "{target}");
        assert!(
            headers.contains("content-type: text/html; charset=utf-8"),
            "{target}: {headers}"
        );
        assert!(
            headers.contains("x-content-type-options: nosniff"),
            "{target} omits nosniff"
        );
        assert_eq!(
            body, DASHBOARD_HTML,
            "{target} did not serve the embedded asset"
        );
        assert!(
            body[..30]
                .to_ascii_lowercase()
                .starts_with("<!doctype html>"),
            "{target} body is not an HTML document"
        );
        assert!(
            body.contains("<title>Traza</title>"),
            "{target} page marker missing"
        );
    }
    // Deeper dashboard paths are NOT the page: unknown assets fail loudly.
    let (status, _, _) = server.get("/dashboard/app.js", None);
    assert_eq!(
        status, 404,
        "unknown dashboard assets must not serve the page"
    );
    server.kill();
}

#[test]
fn dashboard_references_no_external_urls() {
    // The page must be self-contained: same-origin API paths only. A grep
    // over the embedded asset is the spec's oracle for this criterion.
    for needle in [
        "http://",
        "https://",
        "src=\"//",
        "href=\"//",
        "@import",
        "srcset",
    ] {
        assert!(
            !DASHBOARD_HTML.contains(needle),
            "dashboard page references an external resource ({needle})"
        );
    }
    assert!(
        DASHBOARD_HTML.contains("/v1/spans") && DASHBOARD_HTML.contains("/v1/traces/"),
        "dashboard page must consume the existing JSON API"
    );
    assert!(
        DASHBOARD_HTML.contains("sessionStorage"),
        "bearer token must live in sessionStorage only"
    );
}

#[test]
fn dashboard_shell_stays_open_when_auth_is_enabled() {
    let dir = test_dir("authopen");
    let server = Server::spawn(&dir, Some("rw:writer-token,ro:reader-token"));

    // The shell loads without credentials...
    let (status, headers, body) = server.get("/", None);
    assert_eq!(status, 200, "shell must stay open under auth");
    assert!(headers.contains("content-type: text/html"));
    assert!(body.contains("<title>Traza</title>"));

    // ...while the API the page consumes stays gated.
    let (status, headers, _) = server.get("/v1/stats", None);
    assert_eq!(status, 401, "API must remain gated under auth");
    assert!(headers.contains("www-authenticate: bearer"));
    let (status, _, _) = server.get("/v1/stats", Some("reader-token"));
    assert_eq!(status, 200, "scoped token still reads the API");
    server.kill();
}
