//! Loads a realistic scenario corpus (see [`traza::seed`]) into a store, for
//! UI work, demos, and load testing.
//!
//! Two modes, because a data directory has exactly one writer:
//!
//! - `--data-dir DIR` writes directly through the engine. The server must not
//!   be running against that directory; start it afterwards.
//! - `--url http://host:port` POSTs batches to a server that is already
//!   running, over the same public `/v1/spans` and `/v1/annotations` API any
//!   client uses.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use traza::seed::{corpus, SeedOptions};
use traza::{Config, Store};

fn main() {
    if let Err(error) = run() {
        eprintln!("seed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut options = SeedOptions::default();
    let mut batch = 500_usize;
    let mut payload_threshold = 256 * 1024_usize;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--data-dir needs a value")?,
                ));
            }
            "--url" => {
                i += 1;
                url = Some(args.get(i).ok_or("--url needs a value")?.clone());
            }
            "--scale" => {
                i += 1;
                options.scale = args.get(i).ok_or("--scale needs a value")?.parse()?;
            }
            "--seed" => {
                i += 1;
                options.seed = args.get(i).ok_or("--seed needs a value")?.parse()?;
            }
            "--start-time-ns" => {
                i += 1;
                options.start_time_ns = args
                    .get(i)
                    .ok_or("--start-time-ns needs a value")?
                    .parse()?;
            }
            "--batch" => {
                i += 1;
                batch = args
                    .get(i)
                    .ok_or("--batch needs a value")?
                    .parse::<usize>()?
                    .max(1);
            }
            "--payload-threshold-bytes" => {
                i += 1;
                payload_threshold = args
                    .get(i)
                    .ok_or("--payload-threshold-bytes needs a value")?
                    .parse()?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: seed (--data-dir DIR | --url http://host:port) [--scale N] [--seed N] [--batch N] [--payload-threshold-bytes N]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }

    let generated = corpus(&options);
    eprintln!(
        "seed: generated {} spans, {} annotations (scale {})",
        generated.spans.len(),
        generated.annotations.len(),
        options.scale
    );

    match (data_dir, url) {
        (Some(directory), None) => {
            let store = Store::open(
                &directory,
                Config {
                    flush_spans: 10_000,
                    ttl_seconds: None,
                    payload_threshold: (payload_threshold > 0).then_some(payload_threshold),
                },
            )?;
            store.ingest_batch(generated.spans)?;
            for annotation in generated.annotations {
                store.annotate(annotation)?;
            }
            store.flush()?;
            let stats = store.stats()?;
            eprintln!(
                "seed: wrote {} records into {} ({} segments, {} bytes)",
                stats.total_records,
                directory.display(),
                stats.segment_count,
                stats.disk_bytes
            );
        }
        (None, Some(endpoint)) => {
            let (host, port, _) = split_url(&endpoint)?;
            let mut sent = 0;
            for chunk in generated.spans.chunks(batch) {
                let body = serde_json::to_vec(chunk)?;
                post(&host, port, "/v1/spans", &body)?;
                sent += chunk.len();
            }
            for annotation in &generated.annotations {
                let body = serde_json::to_vec(annotation)?;
                post(&host, port, "/v1/annotations", &body)?;
            }
            eprintln!("seed: posted {sent} spans to {endpoint}");
        }
        (Some(_), Some(_)) => return Err("pass either --data-dir or --url, not both".into()),
        (None, None) => return Err("pass --data-dir DIR or --url http://host:port".into()),
    }
    Ok(())
}

fn split_url(url: &str) -> Result<(String, u16, String), Box<dyn std::error::Error>> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("--url must start with http:// (TLS is reverse-proxy territory)")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = authority
        .split_once(':')
        .map(|(host, port)| (host.to_owned(), port.parse::<u16>()))
        .unwrap_or((authority.to_owned(), Ok(80)));
    Ok((host, port?, format!("/{path}")))
}

/// A deliberately tiny HTTP/1.1 POST — the crate ships no HTTP client, and
/// seeding needs nothing more than "send a body, check the status".
fn post(host: &str, port: u16, path: &str, body: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect((host, port))?;
    let token = std::env::var("TRAZA_TOKEN").unwrap_or_default();
    let authorization = if token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {token}\r\n")
    };
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or("malformed HTTP response")?;
    if !(200..300).contains(&status) {
        return Err(format!("POST {path} failed with {status}: {text}").into());
    }
    Ok(())
}
