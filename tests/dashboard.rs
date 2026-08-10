//! The dashboard is served from disk (`--ui-dir`), not compiled in: the built
//! SPA is handed out at the shell routes, assets keep their content types,
//! path traversal is refused, the shell loads without credentials while the
//! API stays gated, and a missing build degrades to a helpful 404.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn spawn(data_dir: &Path, ui_dir: &Path, tokens: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--ui-dir")
            .arg(ui_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .stderr(Stdio::piped());
        match tokens {
            Some(value) => command.env("TRAZA_TOKENS", value),
            None => command.env_remove("TRAZA_TOKENS"),
        };
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

    /// Sends a raw request line target verbatim (so traversal attempts reach
    /// the server unnormalized) and returns (status, headers, body).
    fn raw_get(&self, target: &str, authorization: Option<&str>) -> (u16, String, String) {
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
        let auth_header = authorization
            .map(|value| format!("Authorization: Bearer {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: x\r\n{auth_header}Connection: close\r\n\r\n"
        )
        .expect("writes");
        let response = read_until_close(&mut stream);
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let (headers, body) = text
            .split_once("\r\n\r\n")
            .map(|(head, body)| (head.to_owned(), body.to_owned()))
            .unwrap_or((text.clone(), String::new()));
        (status, headers, body)
    }
}

/// Reads until the server closes the socket, tolerating a close delivered as
/// RST once the response is complete — a loaded kernel turns the server's
/// post-response close into a reset rather than a FIN-drain, and `read_to_end`
/// then errors AFTER handing over every byte (the lesson of `tests/auth.rs`).
/// An INCOMPLETE response still panics.
fn read_until_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        assert!(
            complete_http_response(&response),
            "incomplete response after {:?}: {error}",
            error.kind()
        );
    }
    response
}

/// True once `response` holds a full header block plus the `Content-Length`
/// bytes it declares.
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

/// A stand-in for `ui/dist`: the single-file index plus one asset.
fn built_ui(label: &str) -> PathBuf {
    let dist = test_dir(label).join("dist");
    std::fs::create_dir_all(dist.join("assets")).expect("dist");
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>Traza</title><div id=root></div>",
    )
    .expect("index");
    std::fs::write(dist.join("assets/app.js"), "export const x = 1;").expect("asset");
    dist
}

#[test]
fn serves_the_built_spa_from_the_ui_directory() {
    let dist = built_ui("serve");
    let server = Server::spawn(&test_dir("serve-data"), &dist, None);

    for route in ["/", "/dashboard", "/dashboard/"] {
        let (status, headers, body) = server.raw_get(route, None);
        assert_eq!(status, 200, "{route} serves the page: {headers}");
        assert!(body.contains("<title>Traza</title>"), "{route}: {body}");
        assert!(
            headers.contains("Content-Type: text/html; charset=utf-8"),
            "{route} headers: {headers}"
        );
        assert!(
            headers.contains("X-Content-Type-Options: nosniff"),
            "{route} headers: {headers}"
        );
    }

    // Assets are served with their own content type, so a multi-file build
    // (should the single-file plugin ever be dropped) works unchanged.
    let (status, headers, body) = server.raw_get("/assets/app.js", None);
    assert_eq!(status, 200);
    assert!(body.contains("export const x"));
    assert!(
        headers.contains("Content-Type: text/javascript; charset=utf-8"),
        "{headers}"
    );

    // A rebuilt UI is picked up without restarting the server.
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>Traza</title>rebuilt",
    )
    .expect("rewrite");
    let (_, _, body) = server.raw_get("/", None);
    assert!(
        body.contains("rebuilt"),
        "serves from disk each time: {body}"
    );
}

#[test]
fn refuses_to_serve_outside_the_ui_directory() {
    let dist = built_ui("traversal");
    // A file next to dist/ that must never be reachable through the server.
    let secret = dist.parent().expect("parent").join("traza-dash-secret.txt");
    std::fs::write(&secret, "top secret").expect("write");
    let server = Server::spawn(&test_dir("traversal-data"), &dist, None);

    for attack in [
        "/../traza-dash-secret.txt",
        "/assets/../../traza-dash-secret.txt",
        "/%2e%2e/traza-dash-secret.txt",
        "/../../etc/passwd",
    ] {
        let (status, _, body) = server.raw_get(attack, None);
        assert_ne!(status, 200, "{attack} must not be served: {body}");
        assert!(
            !body.contains("top secret"),
            "{attack} leaked the file: {body}"
        );
    }
}

#[test]
fn the_shell_is_open_while_the_api_stays_gated() {
    let dist = built_ui("auth");
    let server = Server::spawn(&test_dir("auth-data"), &dist, Some("rw:secret-token"));

    // The page itself carries no data and must load without credentials, so
    // it can prompt for a token on its first 401.
    let (status, _, body) = server.raw_get("/", None);
    assert_eq!(status, 200, "shell loads unauthenticated: {body}");
    assert!(body.contains("<title>Traza</title>"));

    // Every API call it makes stays gated.
    let (status, headers, _) = server.raw_get("/v1/stats", None);
    assert_eq!(status, 401, "API is gated: {headers}");
    assert!(headers.contains("WWW-Authenticate: Bearer"), "{headers}");

    let (status, _, _) = server.raw_get("/v1/stats", Some("secret-token"));
    assert_eq!(status, 200, "a valid token reaches the API");
}

#[test]
fn a_missing_build_degrades_to_a_helpful_404() {
    let absent = test_dir("absent").join("dist");
    let server = Server::spawn(&test_dir("absent-data"), &absent, None);

    let (status, _, body) = server.raw_get("/", None);
    assert_eq!(status, 404, "no build, no page: {body}");
    assert!(
        body.contains("npm run build"),
        "404 should say how to build the UI: {body}"
    );

    // The API is unaffected by a missing dashboard.
    let (status, _, _) = server.raw_get("/v1/stats", None);
    assert_eq!(status, 200, "the API runs without a dashboard build");
}
