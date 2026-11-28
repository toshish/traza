//! Names that arrive from outside never leave the directory they name into.
//!
//! Three kinds of string become part of a filesystem path without the engine
//! having written them: a backup label (from a request path), a payload
//! reference (from a span attribute or a dataset example a client sent), and
//! a file path in a manifest (from a backup directory handed to `--restore`).
//! Each was once joined onto a directory unchecked:
//!
//! - `POST /v1/backups/../release` removed `pins/..`, which is the data
//!   directory, and an ordinary read-write token was enough to send it;
//! - a span carrying `{"$payload": "sha256/../x"}` made the erasure that
//!   later removed that span unlink `x.bin` outside the store;
//! - a restored manifest naming `../x` copied a file from beside the backup
//!   to beside the store.
//!
//! Every test here is written against the DAMAGE, not the error. A call that
//! destroys the store and then fails on the directory it just removed
//! "returns an error" on the broken code, so each case asserts that the thing
//! outside the boundary is still there afterwards. The process-level cases
//! also restart the server before reading back, because a server that has
//! lost its directory keeps answering from the handles it already holds.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::erasure::Subject;
use traza::{Config, Durability, Error, Span, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-confine-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// Offloading on at a low threshold, so a test states its own precondition:
/// `payloads/` exists only once something was offloaded, and a path through
/// `payloads/..` resolves only once that directory does.
fn config() -> Config {
    Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        payload_threshold: Some(1024),
        ..Config::default()
    }
}

fn span(trace: &str, id: &str, attributes: Value) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace, "span_id": id, "name": "op", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": attributes,
    }))
    .expect("span")
}

fn marker_count(store: &Store, marker: &str) -> usize {
    store
        .query(&SpanFilter {
            attributes: vec![("marker".into(), marker.into())],
            ..SpanFilter::default()
        })
        .expect("query")
        .len()
}

/// Recursively copies a directory tree: the "copy the pin" half of a backup.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

// ------------------------------------------------------------ backup labels

/// Labels that are not one plain name. `..` is the one that reached the data
/// directory; the rest are the same mistake at other distances.
const NOT_ONE_NAME: [&str; 8] = [
    "..",
    ".",
    "",
    "../..",
    "nightly/..",
    "nightly/../..",
    "..\\..",
    ".hidden",
];

#[test]
fn releasing_a_pin_cannot_delete_outside_the_pins_directory() {
    let dir = test_dir("release");
    let store = Store::open(&dir, config()).expect("open");
    store
        .ingest_batch(vec![span("t", "a", json!({"marker": "kept"}))])
        .expect("ingest");
    store.flush().expect("flush");
    store.pin_generation("nightly").expect("pin");

    for label in NOT_ONE_NAME {
        let outcome = store.release_pin(label);
        // The damage first: `pins/..` is the data directory and `pins/.` is
        // every pin, and removing either still ends in an error.
        assert!(
            dir.join("CURRENT").is_file(),
            "release_pin({label:?}) removed the store's CURRENT"
        );
        assert!(
            dir.join("pins/nightly").is_dir(),
            "release_pin({label:?}) removed a pin it did not name"
        );
        assert!(
            matches!(outcome, Err(Error::Invalid(_))),
            "release_pin({label:?}) must be refused as the caller's error, got {outcome:?}"
        );
    }

    // Reopening is what proves the directory survived: the live handle would
    // go on answering from open files and memory either way.
    drop(store);
    let store = Store::open(&dir, config()).expect("reopen");
    assert_eq!(
        marker_count(&store, "kept"),
        1,
        "the span is still stored after every refused release"
    );

    // The real pin still releases, and releasing is still idempotent.
    store.release_pin("nightly").expect("release");
    assert!(!dir.join("pins/nightly").exists(), "the named pin is gone");
    store
        .release_pin("nightly")
        .expect("a second release is a no-op");
    store
        .release_pin("never-made")
        .expect("an unknown label is a no-op");
}

#[test]
fn a_pin_label_is_one_plain_name_on_every_call_that_takes_one() {
    let dir = test_dir("label-rule");
    let store = Store::open(&dir, config()).expect("open");
    store
        .ingest_batch(vec![span("t", "a", json!({"marker": "kept"}))])
        .expect("ingest");
    store.flush().expect("flush");
    store.pin_generation("nightly").expect("pin");
    assert!(
        store.verify_pin("nightly").expect("verify").is_empty(),
        "the pin verifies under its own name"
    );

    for label in NOT_ONE_NAME {
        let pinned = store.pin_generation(label);
        assert!(
            matches!(pinned, Err(Error::Invalid(_))),
            "pin_generation({label:?}) must be the caller's error, got {pinned:?}"
        );
        let verified = store.verify_pin(label);
        assert!(
            matches!(verified, Err(Error::Invalid(_))),
            "verify_pin({label:?}) must be the caller's error, got {verified:?}"
        );
    }
    // The spelling that walks out and back in names the same directory, and
    // that is exactly why it is refused: one pin, one name.
    let roundabout = store.verify_pin("../pins/nightly");
    assert!(
        matches!(roundabout, Err(Error::Invalid(_))),
        "a pin is verified only under its own name, got {roundabout:?}"
    );
}

// -------------------------------------------------------- payload references

#[test]
fn an_erasure_does_not_follow_a_forged_payload_reference_out_of_the_store() {
    let root = test_dir("forged-ref");
    let data = root.join("data");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("mkdir");
    let victim = outside.join("victim.bin");
    fs::write(&victim, b"not the store's to delete").expect("victim");

    let store = Store::open(&data, config()).expect("open");
    store
        .ingest_batch(vec![span("real", "a", json!({"prompt": "x".repeat(4096)}))])
        .expect("ingest");
    assert!(
        data.join("payloads").is_dir(),
        "precondition: the store has offloaded a payload"
    );

    // `payloads/<first two characters>/<hash>.bin` with this "hash" is
    // `payloads/../../outside/victim.bin`, which is the file above.
    store
        .ingest_batch(vec![span(
            "forged",
            "a",
            json!({"note": {"$payload": "sha256/../outside/victim", "bytes": 25}}),
        )])
        .expect("ingest");
    store.flush().expect("flush");

    let status = store
        .erase(Subject::Trace {
            trace_id: "forged".into(),
            tenant: String::new(),
        })
        .expect("erase");
    assert!(
        victim.is_file(),
        "erasing a span deleted a file outside the data directory, along the \
         path its forged payload reference spelled"
    );
    // The string under `$payload` is the client's attribute, not a reference:
    // it is not recorded as one when the erasure begins...
    assert!(
        status.erase.payload_refs.is_empty(),
        "the erasure recorded a forged string as a payload the trace held: {:?}",
        status.erase.payload_refs
    );
    // ...and it is not reported as a file the erasure removed.
    let settle = status.settle.expect("the erasure settled");
    assert_eq!(settle.spans_removed, 1, "the span itself is erased");
    assert!(
        settle.payloads_removed.is_empty(),
        "the receipt reports payload files removed for a trace that held \
         none: {:?}",
        settle.payloads_removed
    );
}

#[test]
fn a_recorded_erasure_cannot_carry_a_forged_reference_into_the_purge() {
    // The collectors stopped reporting malformed references, but an erasure
    // can be RESUMED from a record written before they did: a store that
    // crashed mid-purge under an older build has the forged string in its
    // tombstone log already. The purge has to refuse it on its own.
    let root = test_dir("recorded-ref");
    let data = root.join("data");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("mkdir");
    let victim = outside.join("victim.bin");
    fs::write(&victim, b"not the store's to delete").expect("victim");
    {
        let store = Store::open(&data, config()).expect("open");
        store
            .ingest_batch(vec![
                span("real", "a", json!({"prompt": "x".repeat(4096)})),
                span("txp", "s1", json!({})),
            ])
            .expect("ingest");
        store.flush().expect("flush");
        assert!(
            data.join("payloads").is_dir(),
            "precondition: the store has offloaded a payload"
        );
    }
    // The crash state: intent recorded with the forged reference, purge
    // never ran.
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]],
        "payload_refs": ["sha256/../outside/victim"],
    });
    fs::write(data.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(&data, config()).expect("reopen");
    assert_eq!(store.resume_erasures().expect("settles"), 1);
    assert!(
        victim.is_file(),
        "resuming a recorded erasure deleted a file outside the data directory"
    );
    let settle = store
        .erasure_status(1)
        .expect("status")
        .expect("the erasure exists")
        .settle
        .expect("and settled");
    assert_eq!(settle.spans_removed, 1, "the span itself is erased");
    assert!(
        settle.payloads_removed.is_empty(),
        "the receipt reports a file removed that is not a payload: {:?}",
        settle.payloads_removed
    );
}

#[test]
fn an_erasure_does_not_reach_a_live_payload_through_another_spelling_of_its_reference() {
    let dir = test_dir("alias");
    let store = Store::open(&dir, config()).expect("open");
    store
        .ingest_batch(vec![span(
            "owner",
            "a",
            json!({"marker": "owner", "prompt": "y".repeat(4096)}),
        )])
        .expect("ingest");
    let owner = store
        .query(&SpanFilter {
            attributes: vec![("marker".into(), "owner".into())],
            ..SpanFilter::default()
        })
        .expect("query");
    let reference = owner[0].attributes["prompt"]["$payload"]
        .as_str()
        .expect("the prompt was offloaded")
        .to_owned();

    // The same hash in upper case is a DIFFERENT string, so the check that
    // keeps a payload a live span still references does not see it — and on
    // a case-insensitive filesystem (macOS by default) it is the same file.
    let shouted = format!(
        "sha256/{}",
        reference["sha256/".len()..].to_ascii_uppercase()
    );
    assert_ne!(
        shouted, reference,
        "precondition: the hash has a letter in it"
    );
    store
        .ingest_batch(vec![span(
            "forged",
            "a",
            json!({"note": {"$payload": shouted, "bytes": 4096}}),
        )])
        .expect("ingest");
    store.flush().expect("flush");

    let status = store
        .erase(Subject::Trace {
            trace_id: "forged".into(),
            tenant: String::new(),
        })
        .expect("erase");
    assert_eq!(
        store
            .payload(&reference)
            .expect("load")
            .map(|bytes| bytes.len()),
        Some(4096),
        "erasing one trace deleted the payload another trace still references"
    );
    // This half goes red on every filesystem: the unlink of a name that does
    // not exist "succeeds", and the receipt then reports it as removed.
    let settle = status.settle.expect("the erasure settled");
    assert!(
        settle.payloads_removed.is_empty(),
        "the receipt reports a payload removed that the erased trace never \
         held: {:?}",
        settle.payloads_removed
    );
}

#[test]
fn a_dataset_example_cannot_claim_a_file_outside_the_store_as_its_payload() {
    let root = test_dir("example-ref");
    let data = root.join("data");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("mkdir");
    let victim = outside.join("victim.bin");
    fs::write(&victim, b"not the store's to answer for").expect("victim");

    let store = Store::open(&data, config()).expect("open");
    store
        .ingest_batch(vec![span("real", "a", json!({"prompt": "x".repeat(4096)}))])
        .expect("ingest");
    assert!(
        data.join("payloads").is_dir(),
        "precondition: the store has offloaded a payload"
    );

    // A version is refused when an example's payload is not in the store.
    // That check asked the filesystem about whatever path the reference
    // spelled, so a file that exists OUTSIDE the store read as "present":
    // the version was recorded, and erasing its tenant later unlinked the
    // file along with the tenant's real payloads.
    let dataset = store.create_dataset("", "curated").expect("dataset");
    let recorded = store.create_dataset_version(
        None,
        dataset,
        None,
        None,
        vec![serde_json::from_value(json!({
            "example_id": "ex-1",
            "input": {"prompt": {"$payload": "sha256/../outside/victim", "bytes": 29}},
        }))
        .expect("example")],
    );
    assert!(
        recorded.is_err(),
        "a version was recorded whose example names a file outside the store \
         as its payload"
    );
    assert!(victim.is_file(), "and the file is untouched");
}

// ------------------------------------------------------------ manifest paths

#[test]
fn a_restored_manifest_cannot_name_a_file_outside_the_backup() {
    let root = test_dir("manifest");
    let source = root.join("source");
    // `backup/../planted.txt` is `stage/planted.txt`, and
    // `restored/../planted.txt` is `target/planted.txt`.
    let backup = root.join("stage/backup");
    let restored = root.join("target/restored");
    fs::create_dir_all(root.join("target")).expect("mkdir");

    let store = Store::open(&source, config()).expect("open");
    store
        .ingest_batch(vec![span("t", "a", json!({"marker": "kept"}))])
        .expect("ingest");
    store.flush().expect("flush");
    store.pin_generation("b").expect("pin");
    copy_tree(&source.join("pins/b"), &backup);
    drop(store);

    // A file beside the backup, and a manifest entry that reaches it with a
    // length and digest that are true — so verification, which only asks
    // whether the named bytes match, has nothing to object to.
    let planted = b"copied from beside the backup to beside the store";
    fs::write(root.join("stage/planted.txt"), planted).expect("plant");
    let manifest_path = backup.join("state-manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("manifest");
    manifest["files"]
        .as_array_mut()
        .expect("files")
        .push(json!({
            "path": "../planted.txt",
            "bytes": planted.len(),
            "sha256": traza::payload::sha256_hex(planted),
        }));
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode"),
    )
    .expect("write");

    let outcome = Store::restore(&restored, &backup, config());
    assert!(
        !root.join("target/planted.txt").exists(),
        "restore wrote a file outside the directory it was restoring into"
    );
    assert!(
        outcome.is_err(),
        "a manifest naming a path outside the backup was installed"
    );

    // Untampered, the same backup restores: the refusal is about the path,
    // not about this backup.
    manifest["files"].as_array_mut().expect("files").pop();
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode"),
    )
    .expect("write");
    let store = Store::restore(&restored, &backup, config()).expect("restore");
    assert_eq!(marker_count(&store, "kept"), 1, "the real backup restores");
}

// ------------------------------------------------------- over HTTP, for real

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, tokens: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .env("TRAZA_TOKENS", tokens)
            .stderr(Stdio::piped());
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

    /// Sends `target` exactly as written. That is the point: an HTTP client
    /// library would normalize `/v1/backups/../release` before sending it,
    /// and the request that matters is the one nothing normalized.
    fn request(
        &self,
        method: &str,
        target: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
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
        let body_len = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body");
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads");
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
        (status, payload)
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

// Backups need an unbound admin credential on this line; the read-write
// token is here to show it is refused before its label is even read.
const TOKENS: &str = "admin:operator,rw:writer,ro:reader";

fn stored_span() -> Value {
    json!([{"trace_id": "t-confine", "span_id": "s1", "name": "op", "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64}])
}

#[test]
fn a_crafted_release_cannot_delete_the_store_over_http() {
    let dir = test_dir("http-release");
    let server = Server::spawn(&dir, TOKENS);
    let (status, _) = server.request("POST", "/v1/spans", "writer", Some(&stored_span()));
    assert_eq!(status, 200, "ingest");
    let (status, _) = server.request("POST", "/v1/flush", "writer", None);
    assert_eq!(status, 200, "flush");
    let (status, body) = server.request("POST", "/v1/backups/nightly", "operator", None);
    assert_eq!(status, 201, "a backup is taken first: {body}");

    // An ordinary read-write token — the one an application's exporter holds
    // — is refused by scope; the admin who may release a pin is refused by
    // the label. Either way the store stays where it is.
    for target in [
        "/v1/backups/../release",
        "/v1/backups/./release",
        "/v1/backups//release",
        "/v1/backups/.hidden/release",
        "/v1/backups/nightly/../release",
    ] {
        let (status, body) = server.request("POST", target, "writer", None);
        assert_eq!(
            status, 403,
            "POST {target} needs an admin, got {status}: {body}"
        );
        let (status, body) = server.request("POST", target, "operator", None);
        assert!(
            dir.join("CURRENT").is_file(),
            "POST {target} removed the store's CURRENT"
        );
        assert!(
            dir.join("pins/nightly").is_dir(),
            "POST {target} removed a backup it did not name"
        );
        assert_eq!(
            status, 400,
            "POST {target} is the caller's error, got {status}: {body}"
        );
    }

    // A server that has lost its directory keeps answering from the handles
    // it holds, so the read-back that counts is the one after a restart.
    drop(server);
    let server = Server::spawn(&dir, TOKENS);
    let (status, body) = server.request("GET", "/v1/traces/t-confine", "reader", None);
    assert_eq!(status, 200, "the trace is still stored: {body}");
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(1),
        "with its span: {body}"
    );

    // The real backup still releases, and an unknown label is still a no-op.
    let (status, body) = server.request("POST", "/v1/backups/nightly/release", "operator", None);
    assert_eq!(status, 200, "release: {body}");
    assert!(
        !dir.join("pins/nightly").exists(),
        "the named backup is gone"
    );
    let (status, _) = server.request("POST", "/v1/backups/never-made/release", "operator", None);
    assert_eq!(status, 200, "releasing an unknown backup stays idempotent");
}

#[test]
fn a_malformed_backup_label_is_the_callers_error_on_create_too() {
    let dir = test_dir("http-create");
    let server = Server::spawn(&dir, TOKENS);
    // Something to back up, so that the accepted label below is refused or
    // taken on the label's account and nothing else's.
    let (status, _) = server.request("POST", "/v1/spans", "writer", Some(&stored_span()));
    assert_eq!(status, 200, "ingest");
    let (status, _) = server.request("POST", "/v1/flush", "writer", None);
    assert_eq!(status, 200, "flush");
    for target in [
        "/v1/backups/.hidden",
        "/v1/backups/..",
        "/v1/backups/a/b",
        "/v1/backups/",
    ] {
        let (status, body) = server.request("POST", target, "operator", None);
        assert_eq!(
            status, 400,
            "POST {target} names no backup, got {status}: {body}"
        );
    }
    assert!(
        !dir.join("pins").exists(),
        "and none of them left a pin behind"
    );
    // A taken label is still a conflict, which is a different answer.
    let (status, _) = server.request("POST", "/v1/backups/nightly", "operator", None);
    assert_eq!(status, 201, "a plain label is accepted");
    let (status, body) = server.request("POST", "/v1/backups/nightly", "operator", None);
    assert_eq!(status, 409, "a label already in use is a conflict: {body}");
}
