//! Where imported spans go: a running server over HTTP, or a data directory
//! opened in-process.
//!
//! The HTTP client is deliberately minimal — std sockets, HTTP/1.1, one
//! request per connection, plain HTTP only (like the MCP stdio bridge: TLS is
//! a reverse proxy's job). It only ever connects to the URL the operator
//! named on the command line; the engine itself makes no outbound requests.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::{Error, Span, Store};

/// Why a send failed.
#[derive(Debug)]
pub enum SendError {
    /// The destination refused these spans for a reason that is the spans'
    /// own (an admission cap, an invalid tenant): splitting the batch can
    /// isolate them, and the rest can still be sent.
    Refused(String),
    /// Nothing will get through (a bad credential, an unreachable server, a
    /// quota): the import stops.
    Fatal(String),
}

/// Something spans can be sent to.
pub trait Sink {
    /// Sends one batch.
    fn send(&mut self, spans: &[Span]) -> Result<(), SendError>;
}

/// A server's address, parsed from `--url`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpTarget {
    /// Host name or address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Path prefix (empty for a server at the root).
    pub prefix: String,
    /// Bearer token, if any.
    pub token: Option<String>,
}

impl HttpTarget {
    /// Parses `http://host[:port][/prefix]`. `https` is refused: put TLS
    /// termination in front and point at the plaintext side.
    pub fn parse(url: &str, token: Option<String>) -> Result<Self, String> {
        let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
        if scheme.eq_ignore_ascii_case("https") {
            return Err(
                "the importer speaks plain HTTP; put TLS termination in front of the server \
                 and point --url at the plaintext side"
                    .to_owned(),
            );
        }
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(format!(
                "--url must be http://HOST:PORT (got scheme {scheme:?})"
            ));
        }
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, String::new()), |(a, p)| (a, format!("/{p}")));
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| format!("--url has an invalid port: {port:?}"))?,
            ),
            None => (authority.to_owned(), 80),
        };
        if host.is_empty() {
            return Err("--url needs a host".to_owned());
        }
        Ok(Self {
            host,
            port,
            prefix: path.trim_end_matches('/').to_owned(),
            token,
        })
    }

    /// The destination's identity in the state file (never the token).
    pub fn key(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.prefix)
    }
}

/// One HTTP exchange: `(status, Retry-After seconds if any, body)`.
pub fn http_request(
    target: &HttpTarget,
    method: &str,
    path_and_query: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Option<u64>, Vec<u8>), String> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .map_err(|error| format!("cannot reach {}: {error}", target.key()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let body = body.unwrap_or_default();
    let authorization = target.token.as_deref().map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let head = format!(
        "{method} {}{path_and_query} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
         {authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        target.prefix,
        target.host,
        target.port,
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|error| format!("sending to {}: {error}", target.key()))?;
    let mut raw = Vec::new();
    // A server that answers and closes may reset a connection whose request
    // it did not read in full; what arrived before the reset still counts.
    let read = stream.read_to_end(&mut raw);
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(match read {
            Err(error) => format!("reading from {}: {error}", target.key()),
            Ok(_) => format!("malformed HTTP response from {}", target.key()),
        });
    };
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("malformed HTTP status from {}", target.key()))?;
    let mut retry_after = None;
    let mut length = None;
    let mut chunked = false;
    for line in head.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("retry-after") {
                retry_after = value.parse().ok();
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.eq_ignore_ascii_case("chunked");
            }
        }
    }
    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)
    } else {
        match length {
            Some(length) => rest[..length.min(rest.len())].to_vec(),
            None => rest.to_vec(),
        }
    };
    Ok((status, retry_after, body))
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(end) = raw.windows(2).position(|window| window == b"\r\n") {
        let size = std::str::from_utf8(&raw[..end])
            .ok()
            .and_then(|text| usize::from_str_radix(text.split(';').next()?.trim(), 16).ok())
            .unwrap_or(0);
        raw = &raw[end + 2..];
        if size == 0 || raw.len() < size {
            break;
        }
        out.extend_from_slice(&raw[..size]);
        raw = raw.get(size + 2..).unwrap_or_default();
    }
    out
}

/// The `error` field of a JSON error body, or the body itself.
pub(crate) fn error_text(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| match value.get("error") {
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(500).collect())
}

/// Sends batches to `POST /v1/spans` on a running server.
pub struct HttpSink {
    target: HttpTarget,
    /// Retries for a transient failure (503, a dropped connection) or a
    /// quota refusal before the import gives up.
    retries: u32,
}

impl HttpSink {
    /// A sink for `target`.
    pub fn new(target: HttpTarget) -> Self {
        Self { target, retries: 5 }
    }
}

impl Sink for HttpSink {
    fn send(&mut self, spans: &[Span]) -> Result<(), SendError> {
        let body =
            serde_json::to_vec(spans).map_err(|error| SendError::Fatal(error.to_string()))?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let exhausted = attempt > self.retries;
            match http_request(&self.target, "POST", "/v1/spans", Some(&body)) {
                Ok((200, _, _)) => return Ok(()),
                Ok((400 | 409 | 413, _, body)) => {
                    return Err(SendError::Refused(error_text(&body)))
                }
                Ok((status @ (401 | 403), _, body)) => {
                    return Err(SendError::Fatal(format!(
                        "the server refused the credential ({status}): {}; pass --token (or \
                         TRAZA_TOKEN) with rw scope",
                        error_text(&body)
                    )))
                }
                Ok((429, retry_after, body)) if !exhausted => {
                    let wait = retry_after.unwrap_or(5).clamp(1, 30);
                    let _ = body;
                    std::thread::sleep(Duration::from_secs(wait));
                }
                Ok((status, _, body)) if status >= 500 && !exhausted => {
                    let _ = body;
                    std::thread::sleep(Duration::from_millis(250 << attempt.min(5)));
                }
                Ok((status, _, body)) => {
                    return Err(SendError::Fatal(format!(
                        "the server answered {status}: {}",
                        error_text(&body)
                    )))
                }
                Err(_) if !exhausted => {
                    std::thread::sleep(Duration::from_millis(250 << attempt.min(5)));
                }
                Err(error) => return Err(SendError::Fatal(error)),
            }
        }
    }
}

/// Ingests batches straight into a store opened in this process.
pub struct StoreSink {
    store: Store,
}

impl StoreSink {
    /// A sink over an open store.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Seals what was ingested and closes the store.
    pub fn finish(self) -> Result<(), String> {
        self.store.flush().map_err(|error| error.to_string())
    }
}

impl Sink for StoreSink {
    fn send(&mut self, spans: &[Span]) -> Result<(), SendError> {
        match self.store.ingest_batch(spans.to_vec()) {
            Ok(_) => Ok(()),
            Err(Error::InvalidSpan(reason)) => Err(SendError::Refused(reason.to_owned())),
            Err(Error::Invalid(reason)) => Err(SendError::Refused(reason)),
            Err(Error::Conflict(reason)) => Err(SendError::Refused(reason)),
            Err(other) => Err(SendError::Fatal(other.to_string())),
        }
    }
}

/// Converts and counts, sending nothing (`--dry-run`).
#[derive(Default)]
pub struct DrySink;

impl Sink for DrySink {
    fn send(&mut self, _spans: &[Span]) -> Result<(), SendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_to_a_plaintext_target_and_tls_is_refused() {
        let target = HttpTarget::parse("http://127.0.0.1:8080", Some("t".into())).expect("parses");
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.prefix, "");
        assert_eq!(
            target.key(),
            "http://127.0.0.1:8080",
            "the token is not the identity"
        );
        let prefixed = HttpTarget::parse("localhost:9/traza/", None).expect("parses");
        assert_eq!((prefixed.port, prefixed.prefix.as_str()), (9, "/traza"));
        assert!(HttpTarget::parse("https://x:1", None).is_err());
        assert!(HttpTarget::parse("http://:80", None).is_err());
        assert!(HttpTarget::parse("http://h:notaport", None).is_err());
    }

    #[test]
    fn chunked_bodies_decode() {
        assert_eq!(dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"), b"abcde");
        assert_eq!(error_text(br#"{"error":"nope"}"#), "nope");
        assert_eq!(error_text(b"plain"), "plain");
    }
}
