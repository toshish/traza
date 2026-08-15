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

/// Scale units generated per chunk. Bounds peak memory to roughly one chunk's
/// worth of spans regardless of the requested total.
const CHUNK_SCALE: usize = 25;
/// How far each chunk's time window advances (one day), so a large seed spans
/// a realistic range instead of piling onto one instant.
const CHUNK_WINDOW_NS: u64 = 86_400_000_000_000;

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

    // Generate in bounded chunks rather than materializing the whole corpus:
    // one scale unit is ~100 spans but carries oversized prompt bodies, so a
    // single large corpus costs gigabytes of RSS before a byte is written.
    // Each chunk gets its own id namespace and time window, so chunks never
    // collide on the primary key.
    let total_scale = options.scale.max(1);
    let chunk_scale = CHUNK_SCALE.min(total_scale);
    let mut written = 0_usize;
    let mut annotated = 0_usize;

    let store = match &data_dir {
        Some(directory) => Some(Store::open(
            directory,
            Config {
                tenant_ttl_seconds: Default::default(),
                // Seeding is a bulk load into a fresh store; the log would be
                // pure overhead for data that is flushed at the end anyway.
                durability: traza::Durability::Buffered,
                compaction: Some(traza::CompactionConfig::default()),
                flush_spans: 10_000,
                flush_wal_bytes: None,
                // A seeder ingests continuously and flushes explicitly at the
                // end; the trickle bounds have nothing to add but seals.
                max_buffer_age: None,
                shadow_seal: false,
                ttl_seconds: None,
                payload_threshold: (payload_threshold > 0).then_some(payload_threshold),
                wal_commit_window: None,
                content_index: true,
                // Nothing tails a seeder, and its whole job is to admit
                // millions of spans, so retaining the last few thousand would
                // be memory spent on an audience that does not exist.
                tail_ring_spans: 1,
                tail_ring_bytes: 1,
            },
        )?),
        None => None,
    };
    let endpoint = match (&store, &url) {
        (Some(_), Some(_)) => return Err("pass either --data-dir or --url, not both".into()),
        (None, None) => return Err("pass --data-dir DIR or --url http://host:port".into()),
        (None, Some(endpoint)) => Some(split_url(endpoint)?),
        (Some(_), None) => None,
    };

    let mut done = 0_usize;
    let mut chunk_index = 0_u32;
    while done < total_scale {
        let this_scale = chunk_scale.min(total_scale - done);
        let chunk = corpus(&SeedOptions {
            scale: this_scale,
            // Walk the window forward so chunks do not stack on one instant.
            start_time_ns: options.start_time_ns + u64::from(chunk_index) * CHUNK_WINDOW_NS,
            seed: options.seed.wrapping_add(u64::from(chunk_index)),
            namespace: if total_scale > chunk_scale {
                format!("b{chunk_index}")
            } else {
                String::new()
            },
            big_payload_bytes: options.big_payload_bytes,
        });
        written += chunk.spans.len();
        annotated += chunk.annotations.len();

        match (&store, &endpoint) {
            (Some(store), _) => {
                store.ingest_batch(chunk.spans)?;
                for annotation in chunk.annotations {
                    store.annotate(annotation)?;
                }
            }
            (None, Some((host, port, _))) => {
                for slice in chunk.spans.chunks(batch) {
                    post(host, *port, "/v1/spans", &serde_json::to_vec(slice)?)?;
                }
                for annotation in &chunk.annotations {
                    post(
                        host,
                        *port,
                        "/v1/annotations",
                        &serde_json::to_vec(annotation)?,
                    )?;
                }
            }
            (None, None) => unreachable!("a destination was validated above"),
        }
        done += this_scale;
        chunk_index += 1;
        eprintln!("seed: {written} spans ({done}/{total_scale} scale units)");
    }

    if let Some(store) = &store {
        store.flush()?;
        let stats = store.stats()?;
        eprintln!(
            "seed: wrote {} records into {} ({} segments, {} bytes, {annotated} annotations)",
            stats.total_records,
            data_dir.expect("a data directory").display(),
            stats.segment_count,
            stats.disk_bytes,
        );
    } else {
        eprintln!(
            "seed: posted {written} spans and {annotated} annotations to {}",
            url.expect("an endpoint")
        );
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
