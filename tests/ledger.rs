//! The ledger and evidence packs, end to end against the real binary.
//!
//! What is proven here is what an auditor relies on: the chain grows with
//! every published generation and erasure; every head recomputes from the
//! documented formula with an implementation that shares no code with the
//! engine's; verification names the broken entry when a byte of the ledger
//! changes and fails the file check when a byte of a segment changes; an
//! anchor is accepted only for the head it names; an erasure entry carries a
//! digest and never the subject; an evidence pack's digest recomputes and a
//! tenant's pack shows only that tenant; a pin carries the ledger; and the
//! offline `verify --ledger` exits by verdict.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use common::Server;
use serde_json::{json, Value};

const TOKENS: &str = "admin:root,rw:writer,ro:observer,ro@acme:acme-ro,rw@acme:acme-rw,\
                      admin@acme:acme-admin,rw@bigco:bigco-rw";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A scratch directory removed on drop, for the library-built stores the CLI
/// tests verify offline.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "traza-ledger-it-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

/// Canonical JSON written from the documented rule — keys sorted, no
/// whitespace — and nothing of the engine's, so a head that recomputes here
/// recomputes for an auditor too.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key"),
                        canonical(&map[key.as_str()])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => serde_json::to_string(other).expect("scalar"),
    }
}

fn sha256(text: &str) -> String {
    traza::payload::sha256_hex(text.as_bytes())
}

/// The documented head formula, applied to one stored entry.
fn recompute_head(entry: &Value) -> String {
    let mut unsigned = entry.clone();
    let map = unsigned.as_object_mut().expect("entry is an object");
    map.remove("head");
    let prev = map["prev"].as_str().expect("prev").to_owned();
    sha256(&format!("traza-ledger/1\n{prev}\n{}", canonical(&unsigned)))
}

fn span(trace: &str, start_ns: u64, attributes: Value) -> Value {
    json!({
        "trace_id": trace, "span_id": "s1", "name": "chat", "service": "svc",
        "start_time_ns": start_ns, "end_time_ns": start_ns + 1_000_000,
        "attributes": attributes,
    })
}

fn bearer(token: &str) -> [(&'static str, String); 1] {
    [("Authorization", format!("Bearer {token}"))]
}

fn as_json(
    server: &Server,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let [(name, value)] = bearer(token);
    server.json(method, path, body, &[(name, value.as_str())])
}

fn entries(server: &Server) -> Vec<Value> {
    let (status, body) = server.json("GET", "/v1/ledger?limit=1000", None, &[]);
    assert_eq!(status, 200, "{body}");
    body["entries"].as_array().expect("entries").clone()
}

fn kinds(entries: &[Value]) -> Vec<&str> {
    entries
        .iter()
        .map(|entry| entry["kind"].as_str().expect("kind"))
        .collect()
}

/// Every head recomputes, every `prev` is the head before it, seqs count up
/// from 0, and the genesis `prev` is 64 zeros.
fn assert_chain(entries: &[Value]) {
    let mut prev = "0".repeat(64);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry["seq"], json!(index), "seq order: {entry}");
        assert_eq!(entry["prev"], json!(prev), "link at seq {index}");
        let head = entry["head"].as_str().expect("head");
        assert_eq!(head, recompute_head(entry), "head at seq {index}");
        prev = head.to_owned();
    }
}

fn verify(server: &Server) -> Value {
    let (status, body) = server.json("GET", "/v1/ledger/verify", None, &[]);
    assert_eq!(status, 200, "{body}");
    body
}

/// Rewrites one decimal digit of entry `seq`'s `at_ns` in place: the line
/// stays valid JSON and the same length, so only the head check can see it.
fn tamper_at_ns(ledger: &Path, seq: usize) -> Vec<u8> {
    let original = std::fs::read(ledger).expect("ledger");
    let text = String::from_utf8(original.clone()).expect("utf8");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let line = &mut lines[seq];
    let start = line.find("\"at_ns\":").expect("at_ns") + "\"at_ns\":".len();
    let digit_at = start + 5;
    let digit = line.as_bytes()[digit_at];
    let replacement = if digit == b'9' {
        '0'
    } else {
        (digit + 1) as char
    };
    line.replace_range(digit_at..digit_at + 1, &replacement.to_string());
    std::fs::write(ledger, lines.join("\n") + "\n").expect("tamper");
    original
}

#[test]
fn checkpoints_grow_the_chain_and_every_head_recomputes_independently() {
    let mut server = Server::spawn(&["--compaction-fanout", "0"]);

    // Opening a store writes the genesis entry and ledgers its first
    // generation.
    let opened = entries(&server);
    assert_eq!(kinds(&opened), ["genesis", "checkpoint"]);
    let store_id = opened[0]["store_id"].as_str().expect("store_id");
    assert_eq!(store_id.len(), 32);
    assert!(store_id.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let mut generations = Vec::new();
    for round in 0..2 {
        let (status, body) = server.json(
            "POST",
            "/v1/spans",
            Some(&json!([span(&format!("t{round}"), 1_000, json!({}))])),
            &[],
        );
        assert_eq!(status, 200, "{body}");
        let (status, body) = server.json("POST", "/v1/checkpoint", None, &[]);
        assert_eq!(status, 200, "{body}");
        generations.push(body["generation"].as_u64().expect("generation"));
    }
    let all = entries(&server);
    assert_eq!(
        kinds(&all),
        ["genesis", "checkpoint", "checkpoint", "checkpoint"]
    );
    assert_chain(&all);
    assert_eq!(all[2]["generation"], json!(generations[0]));
    assert_eq!(all[3]["generation"], json!(generations[1]));
    // The manifest digest is of the manifest file as stored, so sha256sum
    // reproduces it.
    let manifest = std::fs::read(server.data_dir.join(format!(
        "generations/{}/state-manifest.json",
        generations[1]
    )))
    .expect("live manifest");
    assert_eq!(
        all[3]["manifest_sha256"],
        json!(traza::payload::sha256_hex(&manifest))
    );

    // The head reports the tip; bound or not, it discloses no tenant data.
    let (status, head) = server.json("GET", "/v1/ledger/head", None, &[]);
    assert_eq!(status, 200, "{head}");
    assert_eq!(head["seq"], json!(3));
    assert_eq!(head["head"], all[3]["head"]);
    assert_eq!(head["store_id"], json!(store_id));
    assert_eq!(head["at_ns"], all[3]["at_ns"]);

    // Paging: strictly after `after`, at most `limit`.
    let (status, page) = server.json("GET", "/v1/ledger?after=1&limit=1", None, &[]);
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["entries"].as_array().expect("page").len(), 1);
    assert_eq!(page["entries"][0]["seq"], json!(2));
    assert_eq!(page["head"], json!({"seq": 3, "head": all[3]["head"]}));
    for bad in [
        "/v1/ledger?limit=0",
        "/v1/ledger?limit=1001",
        "/v1/ledger?after=x",
        "/v1/ledger?bogus=1",
        "/v1/ledger/head?bogus=1",
    ] {
        let (status, body) = server.json("GET", bad, None, &[]);
        assert_eq!(status, 400, "{bad}: {body}");
    }

    // A crash and restart neither re-genesises nor re-ledgers the live
    // generation: the chain resumes where it was.
    server.restart();
    let resumed = entries(&server);
    assert_eq!(
        resumed, all,
        "a restart over an unchanged generation appends nothing"
    );
    let report = verify(&server);
    assert_eq!(report["intact"], json!(true), "{report}");
}

#[test]
fn verify_names_a_tampered_entry_and_fails_a_tampered_segment() {
    let server = Server::spawn(&["--compaction-fanout", "0"]);
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span("t1", 1_000, json!({"marker": "payload"}))])),
        &[],
    );
    assert_eq!(status, 200);
    let (status, body) = server.json("POST", "/v1/checkpoint", None, &[]);
    assert_eq!(status, 200, "{body}");
    let live = body["generation"].as_u64().expect("generation");

    let clean = verify(&server);
    assert_eq!(clean["intact"], json!(true), "{clean}");
    assert_eq!(clean["entries"], json!(3));
    assert_eq!(clean["problems"], json!([]));
    assert_eq!(clean["generation"]["id"], json!(live));
    assert_eq!(clean["generation"]["matches_ledger"], json!(true));
    assert_eq!(clean["generation"]["files_verified"], json!(true));
    assert_eq!(clean["head"], entries(&server)[2]["head"]);

    // One digit of seq 1's timestamp: the entry still parses, still links —
    // its head no longer matches its contents, and that entry alone is named.
    let ledger = server.data_dir.join("ledger.jsonl");
    let original = tamper_at_ns(&ledger, 1);
    let broken = verify(&server);
    assert_eq!(broken["intact"], json!(false), "{broken}");
    let problems: Vec<&str> = broken["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .map(|problem| problem.as_str().expect("text"))
        .collect();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("seq 1: head does not match its contents"),
        "{problems:?}"
    );
    assert_eq!(broken["generation"]["files_verified"], json!(true));
    std::fs::write(&ledger, original).expect("restore ledger");
    assert_eq!(verify(&server)["intact"], json!(true));

    // One byte of a manifested segment: same length, different digest.
    let segment = std::fs::read_dir(&server.data_dir)
        .expect("data dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".seg"))
        })
        .expect("a sealed segment");
    let mut bytes = std::fs::read(&segment).expect("segment");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&segment, &bytes).expect("tamper segment");
    let damaged = verify(&server);
    assert_eq!(damaged["intact"], json!(false), "{damaged}");
    assert_eq!(damaged["generation"]["files_verified"], json!(false));
    assert_eq!(
        damaged["generation"]["matches_ledger"],
        json!(true),
        "the chain is fine; the files are not"
    );
    let name = segment
        .file_name()
        .and_then(|name| name.to_str())
        .expect("name");
    assert!(
        damaged["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| problem
                .as_str()
                .is_some_and(|text| text.contains(name) && text.contains("digest mismatch"))),
        "{damaged}"
    );
}

#[test]
fn anchors_are_accepted_only_for_the_head_they_name() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);
    let (status, head) = as_json(&server, "observer", "GET", "/v1/ledger/head", None);
    assert_eq!(status, 200, "{head}");
    let tip_seq = head["seq"].as_u64().expect("seq");
    let tip_head = head["head"].as_str().expect("head").to_owned();
    let anchor = |target_seq: u64, target_head: &str| {
        json!({
            "target_seq": target_seq, "target_head": target_head,
            "authority": "notary.example", "anchored_at_ns": 1_760_000_000_000_000_000u64,
            "signature": "MEUCIQDsig", "public_key": "MCowBQYDK2VwAyEAkey", "note": "q3 close",
        })
    };

    // Only an unbound admin may anchor.
    for token in ["writer", "acme-admin"] {
        let (status, body) = as_json(
            &server,
            token,
            "POST",
            "/v1/ledger/anchors",
            Some(&anchor(tip_seq, &tip_head)),
        );
        assert_eq!(status, 403, "{token}: {body}");
    }

    let wrong = "ab".repeat(32);
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&anchor(tip_seq, &wrong)),
    );
    assert_eq!(status, 409, "a head the ledger never recorded: {body}");
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&anchor(tip_seq + 5, &tip_head)),
    );
    assert_eq!(status, 409, "an entry that does not exist yet: {body}");
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&anchor(tip_seq, "not-a-head")),
    );
    assert_eq!(
        status, 409,
        "a malformed head cannot be the recorded one: {body}"
    );
    assert!(
        !body["error"].as_str().unwrap_or("").contains("not-a-head"),
        "the value is not echoed: {body}"
    );
    let mut smuggled = anchor(tip_seq, &tip_head);
    smuggled["head"] = json!(wrong);
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&smuggled),
    );
    assert_eq!(
        status, 400,
        "chain fields are the ledger's to assign: {body}"
    );
    assert_eq!(
        as_json(&server, "root", "GET", "/v1/ledger/head", None).1["seq"],
        json!(tip_seq),
        "no refused anchor was appended"
    );

    let (status, entry) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&anchor(tip_seq, &tip_head)),
    );
    assert_eq!(status, 201, "{entry}");
    assert_eq!(entry["kind"], "anchor");
    assert_eq!(entry["seq"], json!(tip_seq + 1));
    assert_eq!(entry["prev"], json!(tip_head));
    assert_eq!(entry["target_seq"], json!(tip_seq));
    assert_eq!(entry["target_head"], json!(tip_head));
    assert_eq!(entry["authority"], "notary.example");
    assert_eq!(entry["signature"], "MEUCIQDsig");
    assert_eq!(entry["public_key"], "MCowBQYDK2VwAyEAkey");
    assert_eq!(entry["note"], "q3 close");
    assert_eq!(entry["head"], json!(recompute_head(&entry)));

    // An earlier entry is anchorable too, and the chain still verifies.
    let (status, listed) = as_json(&server, "root", "GET", "/v1/ledger", None);
    assert_eq!(status, 200);
    let genesis_head = listed["entries"][0]["head"]
        .as_str()
        .expect("head")
        .to_owned();
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/ledger/anchors",
        Some(&anchor(0, &genesis_head)),
    );
    assert_eq!(status, 201, "{body}");
    let (status, report) = as_json(&server, "root", "GET", "/v1/ledger/verify", None);
    assert_eq!(status, 200);
    assert_eq!(report["intact"], json!(true), "{report}");
    assert_chain(listed["entries"].as_array().expect("entries"));
}

#[test]
fn erasure_entries_carry_the_subject_digest_never_the_subject() {
    let server = Server::spawn(&["--compaction-fanout", "0"]);
    let secret = "alice-secret-session-7";
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span("t1", 1_000, json!({"session.id": secret}))])),
        &[],
    );
    assert_eq!(status, 200);
    let (status, erased) = server.json(
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "session", "session_id": secret}})),
        &[],
    );
    assert_eq!(status, 200, "{erased}");
    let published_in = erased["settle"]["generation"].clone();

    let all = entries(&server);
    assert_chain(&all);
    let tail = &all[all.len() - 2..];
    assert_eq!(kinds(tail), ["checkpoint", "erasure"]);
    assert_eq!(
        tail[0]["generation"], published_in,
        "the erasure's own generation"
    );
    let entry = &tail[1];
    assert_eq!(entry["erasure_id"], erased["id"]);
    assert_eq!(
        entry["tenant"], "",
        "the default tenant is the empty string"
    );
    assert_eq!(entry["subject_kind"], "session");
    assert_eq!(
        entry["generation"], published_in,
        "the generation that published it"
    );
    assert_eq!(entry["salt"].as_str().map(str::len), Some(32), "{entry}");
    // Recomputable by anyone who knows the subject, and only by them.
    let subject = json!({"kind": "session", "session_id": secret});
    assert_eq!(entry["subject_digest"], json!(sha256(&canonical(&subject))));
    let raw = std::fs::read_to_string(server.data_dir.join("ledger.jsonl")).expect("ledger");
    assert!(
        !raw.contains("alice-secret"),
        "the subject text is not in the ledger: {raw}"
    );

    // The receipt names the ledger among what an erasure leaves behind on
    // purpose, and a stated retention does not cost it conclusiveness.
    let (status, receipt) = server.json(
        "GET",
        &format!("/v1/erasures/{}/verify", erased["id"]),
        None,
        &[],
    );
    assert_eq!(status, 200, "{receipt}");
    let ledger_domain = receipt["domains"]
        .as_array()
        .expect("domains")
        .iter()
        .find(|domain| domain["domain"] == "ledger")
        .expect("a ledger domain");
    assert_eq!(ledger_domain["result"], "retained-by-design");
    assert_eq!(receipt["result"], "erased");
    assert_eq!(receipt["conclusive"], json!(true), "{receipt}");
}

#[test]
fn an_evidence_pack_recomputes_and_a_bound_tenant_sees_only_itself() {
    let server = Server::spawn_with_env(
        &[
            "--compaction-fanout",
            "0",
            "--ttl-seconds",
            "31536000",
            "--tenant-ttl",
            "acme=0",
        ],
        &[("TRAZA_TOKENS", TOKENS)],
    );
    let now = now_ns();
    let hour = 3_600_000_000_000_u64;
    let recent = now - hour;
    let older = now - 49 * hour;
    let acme = json!([
        span(
            "acme-1",
            older,
            json!({"session.id": "acme-chat", "llm.cost_usd": 0.25})
        ),
        span(
            "acme-2",
            recent,
            json!({"session.id": "acme-chat", "llm.cost_usd": 0.5})
        ),
        span("acme-gone", recent, json!({})),
    ]);
    let (status, body) = as_json(&server, "acme-rw", "POST", "/v1/spans", Some(&acme));
    assert_eq!(status, 200, "{body}");
    let mut failing = span("bigco-1", recent, json!({"llm.cost_usd": 2.0}));
    failing["status"] = json!("error");
    let bigco = json!([failing, span("bigco-gone", recent, json!({}))]);
    let (status, body) = as_json(&server, "bigco-rw", "POST", "/v1/spans", Some(&bigco));
    assert_eq!(status, 200, "{body}");
    for (tenant, trace) in [("acme", "acme-gone"), ("bigco", "bigco-gone")] {
        let subject = json!({"subject": {"kind": "trace", "trace_id": trace, "tenant": tenant}});
        let (status, body) = as_json(&server, "root", "POST", "/v1/erasures", Some(&subject));
        assert_eq!(status, 200, "{body}");
    }

    // The whole store, as the operator sees it.
    let (status, pack) = as_json(&server, "root", "GET", "/v1/evidence", None);
    assert_eq!(status, 200, "{pack}");
    let mut keys: Vec<&str> = pack
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "activity",
            "digest",
            "engine_version",
            "erasures",
            "format",
            "generated_at_ns",
            "ledger",
            "retention",
            "store_id",
            "tenant",
            "usage",
            "window"
        ]
    );
    assert_eq!(pack["format"], "traza-evidence/1");
    assert_eq!(pack["engine_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(pack["tenant"], Value::Null);
    // A whole-store pack is nobody's account, so it carries no usage row.
    assert_eq!(pack["usage"], Value::Null);
    assert_eq!(pack["window"]["since_ns"], json!(0));
    assert_eq!(pack["store_id"], pack["ledger"]["head"]["store_id"]);
    assert_eq!(pack["activity"]["spans"], json!(3));
    assert_eq!(pack["activity"]["traces"], json!(3));
    assert_eq!(pack["activity"]["sessions"], json!(1));
    assert_eq!(pack["activity"]["errors"], json!(1));
    assert_eq!(
        pack["retention"],
        json!({"ttl_seconds": 31536000, "source": "global"})
    );
    assert_eq!(pack["erasures"].as_array().expect("erasures").len(), 2);
    assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
    assert_eq!(pack["ledger"]["entries_truncated"], json!(false));
    assert_eq!(pack["ledger"]["entries_unreadable"], json!(0));
    assert_digest(&pack);
    let operator_entries = pack["ledger"]["entries"].as_array().expect("entries");
    assert_chain(operator_entries);
    assert_eq!(
        operator_entries.last().expect("tip")["seq"],
        pack["ledger"]["head"]["seq"]
    );

    // A bound credential: its own tenant, whatever it asks for.
    let (status, mine) = as_json(&server, "acme-ro", "GET", "/v1/evidence", None);
    assert_eq!(status, 200, "{mine}");
    assert_eq!(mine["tenant"], "acme");
    assert_eq!(mine["activity"]["spans"], json!(2));
    assert_eq!(mine["activity"]["traces"], json!(2));
    assert_eq!(mine["activity"]["sessions"], json!(1));
    assert_eq!(mine["activity"]["errors"], json!(0));
    let days = mine["activity"]["by_day"].as_array().expect("by_day");
    assert_eq!(days.len(), 2, "{days:?}");
    let cost: f64 = days
        .iter()
        .map(|day| day["cost_usd"].as_f64().expect("cost"))
        .sum();
    assert!((cost - 0.75).abs() < 1e-9, "{days:?}");
    // The --tenant-ttl override keeps acme forever: `null`, from the flag.
    assert_eq!(
        mine["retention"],
        json!({"ttl_seconds": null, "source": "flag"})
    );
    // Its usage row is GET /v1/usage's, counting what acme sent.
    let (status, usage) = as_json(&server, "acme-ro", "GET", "/v1/usage", None);
    assert_eq!(status, 200, "{usage}");
    assert_eq!(mine["usage"], usage["tenants"][0], "{mine}");
    assert_eq!(mine["usage"]["tenant"], "acme");
    assert_eq!(mine["usage"]["lifetime"]["spans"], json!(3));
    let erasures = mine["erasures"].as_array().expect("erasures");
    assert_eq!(erasures.len(), 1, "only acme's erasure: {erasures:?}");
    assert_eq!(erasures[0]["subject_kind"], "trace");
    assert_eq!(erasures[0]["result"], "erased");
    assert!(erasures[0]["conclusive"].is_boolean());
    assert!(erasures[0]["published_at_ns"].as_u64().is_some());
    // bigco's erasure entry is a stub: followable, not attributable.
    let erasure_entries: Vec<&Value> = mine["ledger"]["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|entry| entry["kind"] == "erasure")
        .collect();
    assert_eq!(erasure_entries.len(), 2);
    let (own, foreign): (Vec<&Value>, Vec<&Value>) = erasure_entries
        .into_iter()
        .partition(|entry| entry["tenant"] == "acme");
    assert_eq!(own.len(), 1);
    assert_eq!(foreign.len(), 1);
    // The tenant's own entry is whole — salt included — so it recomputes.
    assert_eq!(
        own[0]["salt"].as_str().map(str::len),
        Some(32),
        "{}",
        own[0]
    );
    assert_eq!(own[0]["head"], json!(recompute_head(own[0])));
    assert_eq!(foreign[0]["redacted"], json!(true));
    // The salt above all: without it, the stub's head cannot be recomputed
    // for a guessed neighbour.
    for key in [
        "tenant",
        "erasure_id",
        "subject_kind",
        "subject_digest",
        "generation",
        "salt",
    ] {
        assert!(
            foreign[0].get(key).is_none(),
            "{key} leaked: {}",
            foreign[0]
        );
    }
    for key in ["seq", "prev", "head", "at_ns"] {
        assert!(foreign[0].get(key).is_some(), "{key} kept: {}", foreign[0]);
    }
    let pack_text = serde_json::to_string(&mine).expect("text");
    assert!(
        !pack_text.contains("bigco"),
        "nothing of bigco's: {pack_text}"
    );
    assert_digest(&mine);

    // Naming another tenant is refused, not silently narrowed.
    let (status, body) = as_json(&server, "acme-ro", "GET", "/v1/evidence?tenant=bigco", None);
    assert_eq!(status, 403);
    assert_eq!(
        body,
        json!({"error": "this credential is bound to a different tenant", "required": "unbound"})
    );
    let (status, _) = as_json(&server, "acme-ro", "GET", "/v1/evidence?tenant=acme", None);
    assert_eq!(status, 200);

    // An unbound caller may name a tenant. A runtime retention override in
    // bigco's limits document outranks the global window, and the pack says
    // so — it states the policy expiry acts on, not the flags alone.
    let limits = json!({"monthly_spans": null, "monthly_bytes": null,
                        "ingest_paused": false, "ttl_seconds": 7776000});
    let (status, body) = as_json(
        &server,
        "root",
        "POST",
        "/v1/admin/tenants/bigco/limits",
        Some(&limits),
    );
    assert_eq!(status, 200, "{body}");
    let (status, theirs) = as_json(
        &server,
        "observer",
        "GET",
        "/v1/evidence?tenant=bigco",
        None,
    );
    assert_eq!(status, 200, "{theirs}");
    assert_eq!(theirs["tenant"], "bigco");
    assert_eq!(theirs["activity"]["spans"], json!(1));
    assert_eq!(theirs["activity"]["errors"], json!(1));
    assert_eq!(
        theirs["retention"],
        json!({"ttl_seconds": 7776000, "source": "limits"})
    );
    assert_eq!(theirs["usage"]["tenant"], "bigco");
    assert_eq!(theirs["usage"]["lifetime"]["spans"], json!(2));
    assert_eq!(theirs["usage"]["limits"]["ttl_seconds"], json!(7776000));
    assert_digest(&theirs);

    // The window bounds activity by span start.
    let since = now - 24 * hour;
    let (status, windowed) = as_json(
        &server,
        "acme-ro",
        "GET",
        &format!("/v1/evidence?since={since}"),
        None,
    );
    assert_eq!(status, 200, "{windowed}");
    assert_eq!(windowed["window"]["since_ns"], json!(since));
    assert_eq!(windowed["activity"]["spans"], json!(1));
    assert_eq!(
        windowed["activity"]["by_day"]
            .as_array()
            .expect("days")
            .len(),
        1
    );
    let (status, body) = as_json(
        &server,
        "acme-ro",
        "GET",
        "/v1/evidence?since=10&until=5",
        None,
    );
    assert_eq!(status, 400, "{body}");
    let (status, body) = as_json(&server, "acme-ro", "GET", "/v1/evidence?bogus=1", None);
    assert_eq!(status, 400, "{body}");

    // The chain and its verification are operator surfaces; the head is not.
    for path in ["/v1/ledger", "/v1/ledger/verify"] {
        let (status, body) = as_json(&server, "acme-ro", "GET", path, None);
        assert_eq!(status, 403, "{path}: {body}");
    }
    let (status, head) = as_json(&server, "acme-ro", "GET", "/v1/ledger/head", None);
    assert_eq!(status, 200, "{head}");
    assert_eq!(head["seq"], pack["ledger"]["head"]["seq"]);
}

/// The pack's digest is the SHA-256 of the canonical JSON of every other
/// field — recomputed here from the documented rule.
fn assert_digest(pack: &Value) {
    let mut unsigned = pack.clone();
    let claimed = unsigned
        .as_object_mut()
        .expect("object")
        .remove("digest")
        .expect("digest");
    assert_eq!(claimed, json!(sha256(&canonical(&unsigned))));
}

#[test]
fn a_pin_carries_the_ledger_and_a_restore_ledgers_what_it_installed() {
    let server = Server::spawn(&["--compaction-fanout", "0"]);
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span("t1", 1_000, json!({}))])),
        &[],
    );
    assert_eq!(status, 200);
    let (status, pinned) = server.json("POST", "/v1/backups/nightly", None, &[]);
    assert_eq!(status, 201, "{pinned}");
    assert_eq!(
        pinned["verified"],
        json!(true),
        "the copy does not disturb pin verification"
    );
    let pin = server.data_dir.join("pins/nightly");

    let copied = std::fs::read_to_string(pin.join("ledger.jsonl")).expect("pinned ledger");
    let copied: Vec<Value> = copied
        .lines()
        .map(|line| serde_json::from_str(line).expect("entry"))
        .collect();
    assert_chain(&copied);
    let last = copied.last().expect("an entry");
    assert_eq!(
        last["kind"], "checkpoint",
        "the pin's own checkpoint ends its copy"
    );
    assert_eq!(last["generation"], pinned["generation"]);
    let manifest = std::fs::read(pin.join("state-manifest.json")).expect("pin manifest");
    assert_eq!(
        last["manifest_sha256"],
        json!(traza::payload::sha256_hex(&manifest))
    );

    // A restore into an empty directory starts its own chain — a new store
    // id — and ledgers the installed generation at its first open.
    let pin_arg = pin.to_str().expect("utf8 path").to_owned();
    let restored = Server::spawn(&["--compaction-fanout", "0", "--restore", &pin_arg]);
    let chain = entries(&restored);
    assert_eq!(kinds(&chain), ["genesis", "checkpoint"]);
    assert_chain(&chain);
    assert_ne!(chain[0]["store_id"], copied[0]["store_id"]);
    let report = verify(&restored);
    assert_eq!(report["intact"], json!(true), "{report}");
    assert_eq!(chain[1]["generation"], report["generation"]["id"]);
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .args(args)
        .output()
        .expect("runs traza-server")
}

#[test]
fn verify_ledger_offline_exits_by_verdict() {
    let dir = TempDir::new("cli");
    {
        let store = traza::Store::open(&dir.0, traza::Config::default()).expect("open");
        let span: traza::Span = serde_json::from_value(span("t1", 1_000, json!({}))).expect("span");
        store.ingest(span).expect("ingest");
        store.checkpoint().expect("checkpoint");
    }
    let path = dir.0.to_str().expect("utf8 path");

    let intact = cli(&["verify", "--ledger", "--data-dir", path, "--json"]);
    assert_eq!(intact.status.code(), Some(0), "{intact:?}");
    let report: Value = serde_json::from_slice(&intact.stdout).expect("json report");
    assert_eq!(report["intact"], json!(true));
    assert_eq!(report["entries"], json!(3));
    assert_eq!(report["generation"]["files_verified"], json!(true));
    let text = cli(&["verify", "--ledger", "--data-dir", path]);
    assert_eq!(text.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&text.stdout).contains("result: intact"));

    // Read-only: verifying neither appends nor heals.
    let before = std::fs::read(dir.0.join("ledger.jsonl")).expect("ledger");
    let _ = cli(&["verify", "--ledger", "--data-dir", path]);
    assert_eq!(
        std::fs::read(dir.0.join("ledger.jsonl")).expect("ledger"),
        before
    );

    tamper_at_ns(&dir.0.join("ledger.jsonl"), 1);
    let broken = cli(&["verify", "--ledger", "--data-dir", path]);
    assert_eq!(broken.status.code(), Some(2), "{broken:?}");
    let stdout = String::from_utf8_lossy(&broken.stdout);
    assert!(stdout.contains("seq 1: head does not match"), "{stdout}");
    assert!(stdout.contains("result: BROKEN"), "{stdout}");

    // Not a data directory, two verdicts at once: errors, not verdicts.
    let empty = TempDir::new("cli-empty");
    let refused = cli(&[
        "verify",
        "--ledger",
        "--data-dir",
        empty.0.to_str().expect("utf8"),
    ]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no published generation"));
    let both = cli(&["verify", "--ledger", "--erasure", "1", "--data-dir", path]);
    assert_eq!(both.status.code(), Some(1), "{both:?}");

    // A directory a live server owns is that server's to verify.
    let live = Server::spawn(&[]);
    let owned = cli(&[
        "verify",
        "--ledger",
        "--data-dir",
        live.data_dir.to_str().expect("utf8"),
    ]);
    assert_eq!(owned.status.code(), Some(1), "{owned:?}");
    assert!(
        String::from_utf8_lossy(&owned.stderr).contains("GET /v1/ledger/verify"),
        "{owned:?}"
    );
}

#[test]
fn verify_ledger_offline_names_an_undone_erasure_and_the_crash_window() {
    let dir = TempDir::new("cli-rollback");
    let path = dir.0.to_str().expect("utf8 path").to_owned();
    let open = || traza::Store::open(&dir.0, traza::Config::default()).expect("open");
    let erase = |store: &traza::Store| {
        store
            .erase(traza::erasure::Subject::Trace {
                trace_id: "t1".to_owned(),
                tenant: String::new(),
            })
            .expect("erase")
    };
    {
        let store = open();
        let span: traza::Span = serde_json::from_value(span("t1", 1_000, json!({}))).expect("span");
        store.ingest(span).expect("ingest");
        store.pin_generation("before").expect("pin");
        erase(&store);
    }
    // Offline, the erasure's entry is checked against the tombstone log read
    // straight from disk: it holds.
    let verdict = cli(&["verify", "--ledger", "--data-dir", &path]);
    assert_eq!(verdict.status.code(), Some(0), "{verdict:?}");

    // A restore of the pin from before the erasure, in place: the data and
    // the pre-erasure tombstone log come back, the ledger stays — and the
    // verdict says the ledgered erasure no longer holds.
    drop(
        traza::Store::restore(&dir.0, dir.0.join("pins/before"), traza::Config::default())
            .expect("restore"),
    );
    let verdict = cli(&["verify", "--ledger", "--data-dir", &path]);
    assert_eq!(verdict.status.code(), Some(2), "{verdict:?}");
    let stdout = String::from_utf8_lossy(&verdict.stdout);
    assert!(
        stdout.contains("holds no settled erasure of that subject")
            && stdout.contains("re-apply the erasure"),
        "{stdout}"
    );

    // Re-applied, it holds again.
    erase(&open());
    let verdict = cli(&["verify", "--ledger", "--data-dir", &path]);
    assert_eq!(verdict.status.code(), Some(0), "{verdict:?}");

    // The crash window — `CURRENT` moved, its checkpoint entry not yet
    // appended — reads as not intact, and says what it likely is and how it
    // heals rather than only that it is broken.
    let ledger = dir.0.join("ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).expect("ledger");
    let lines: Vec<&str> = text.lines().collect();
    let last_checkpoint = lines
        .iter()
        .rposition(|line| line.contains("\"kind\":\"checkpoint\""))
        .expect("a checkpoint entry");
    assert_eq!(last_checkpoint, lines.len() - 2, "checkpoint, then erasure");
    std::fs::write(&ledger, lines[..last_checkpoint].join("\n") + "\n").expect("truncate");
    let verdict = cli(&["verify", "--ledger", "--data-dir", &path]);
    assert_eq!(verdict.status.code(), Some(2), "{verdict:?}");
    let stdout = String::from_utf8_lossy(&verdict.stdout);
    assert!(
        stdout.contains("is newer and not ledgered yet") && stdout.contains("opening the store"),
        "{stdout}"
    );
    // And opening the store does heal it.
    drop(open());
    let verdict = cli(&["verify", "--ledger", "--data-dir", &path]);
    assert_eq!(verdict.status.code(), Some(0), "{verdict:?}");
}

/// The `python` blocks of `docs/guide/ledger.md`, in order.
fn documented_scripts() -> Vec<String> {
    let guide =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/guide/ledger.md"))
            .expect("the ledger guide");
    let fence = "```";
    guide
        .split(&format!("{fence}python\n"))
        .skip(1)
        .map(|block| block.split(fence).next().expect("closed block").to_owned())
        .collect()
}

/// The guide promises an auditor can check the chain and a pack with nothing
/// but `python3`; this runs the guide's own scripts, verbatim, so the promise
/// cannot drift from the code. Skipped (loudly) on a machine without
/// `python3`; CI has it.
#[test]
fn the_guides_python_scripts_recompute_the_heads_and_the_pack_digest() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("SKIPPED: python3 is not on PATH; the guide's scripts were not exercised");
        return;
    }
    let scripts = documented_scripts();
    assert_eq!(scripts.len(), 2, "the head script, then the digest script");
    assert!(
        scripts[0].lines().count() <= 10,
        "the guide calls it a ten-line script:\n{}",
        scripts[0]
    );
    let work = TempDir::new("scripts");
    let heads = work.0.join("check_ledger.py");
    let digest = work.0.join("check_pack.py");
    std::fs::write(&heads, &scripts[0]).expect("script");
    std::fs::write(&digest, &scripts[1]).expect("script");
    let python = |script: &Path, input: &Path| {
        Command::new("python3")
            .arg("-I")
            .arg(script)
            .arg(input)
            .output()
            .expect("python3 runs")
    };

    let server = Server::spawn(&["--compaction-fanout", "0"]);
    // Fractional costs in the shapes Python would re-print differently
    // (1e-05, 1.5e+20): the digest script must keep the server's text.
    let spans = json!([
        span(
            "t1",
            1_000,
            json!({"session.id": "s", "llm.cost_usd": 0.00001})
        ),
        span("t2", 90_000_000_000_000, json!({"llm.cost_usd": 1.5e20})),
    ]);
    let (status, _) = server.json("POST", "/v1/spans", Some(&spans), &[]);
    assert_eq!(status, 200);
    let (status, _) = server.json("POST", "/v1/checkpoint", None, &[]);
    assert_eq!(status, 200);
    let (status, _) = server.json(
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "trace", "trace_id": "t1"}})),
        &[],
    );
    assert_eq!(status, 200);

    let ledger = server.data_dir.join("ledger.jsonl");
    let clean = python(&heads, &ledger);
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");
    let report = String::from_utf8_lossy(&clean.stdout).into_owned();
    assert_eq!(report.lines().count(), entries(&server).len(), "{report}");
    assert!(report.lines().all(|line| line.ends_with(" ok")), "{report}");

    let (status, _, pack) = server.request("GET", "/v1/evidence", None, &[]);
    assert_eq!(status, 200);
    assert!(
        pack.contains("e+20"),
        "the exponent shape is exercised: {pack}"
    );
    let pack_path = work.0.join("pack.json");
    std::fs::write(&pack_path, &pack).expect("pack");
    let verified = python(&digest, &pack_path);
    assert_eq!(verified.status.code(), Some(0), "{verified:?}");
    // A check, not a rubber stamp: one changed count fails it.
    std::fs::write(&pack_path, pack.replacen("\"spans\":1", "\"spans\":7", 1)).expect("pack");
    assert_eq!(python(&digest, &pack_path).status.code(), Some(2));

    tamper_at_ns(&ledger, 1);
    let broken = python(&heads, &ledger);
    assert_eq!(broken.status.code(), Some(2), "{broken:?}");
    let report = String::from_utf8_lossy(&broken.stdout).into_owned();
    let flagged: Vec<&str> = report
        .lines()
        .filter(|line| line.ends_with("BROKEN"))
        .collect();
    assert_eq!(flagged.len(), 1, "{report}");
    assert!(flagged[0].starts_with("1 checkpoint "), "{report}");
}
