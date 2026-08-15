//! The eval entity model, held to the product-thesis gate: the
//! trace → dataset → experiment → score loop runs END TO END against the
//! built binary, and the awkward parts settled up front — lineage, and the
//! deletion semantics between erasure and promoted copies — hold under test,
//! not in prose.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::{Config, Durability, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-evals-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

fn wal_config() -> Config {
    Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        payload_threshold: Some(64),
        ..Config::default()
    }
}

// The server harness the erasure suite established: spawn the real binary,
// wait for its port, kill it with the handle.
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
    fn spawn(data_dir: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("wal")
            .arg("--flush-spans")
            .arg("1000000")
            .arg("--payload-threshold-bytes")
            .arg("64")
            .env_remove("TRAZA_TOKENS");
        let mut child = command
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
        let port = loop {
            let line = lines.next().expect("port line").expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", self.port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 100 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect: {error}"),
                }
            }
        };
        let length = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body");
        }
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response);
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

/// THE GATE. Promote failing production traces into a dataset version, run
/// an experiment (execution external — this test IS the harness), record
/// scores, and query distributions and an experiment-over-experiment diff —
/// end to end over HTTP against the built binary.
#[test]
fn the_eval_loop_runs_end_to_end_against_the_built_binary() {
    let dir = test_dir("gate");
    let server = Server::spawn(&dir);

    // 1. Production traffic, some of it failing, one prompt big enough to
    //    offload — the promoted example must carry the REFERENCE, not lose
    //    the content.
    let big_prompt = "please summarize the following document ".repeat(8);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {"trace_id": "prod-1", "span_id": "root", "name": "summarize",
             "service": "agent", "start_time_ns": 1000u64, "end_time_ns": 2000u64,
             "status": "error",
             "attributes": {"gen_ai.prompt": big_prompt, "expected": "a summary"}},
            {"trace_id": "prod-2", "span_id": "root", "name": "summarize",
             "service": "agent", "start_time_ns": 3000u64, "end_time_ns": 4000u64,
             "status": "error",
             "attributes": {"gen_ai.prompt": "short prompt two"}},
            {"trace_id": "prod-3", "span_id": "root", "name": "summarize",
             "service": "agent", "start_time_ns": 5000u64, "end_time_ns": 6000u64,
             "attributes": {"gen_ai.prompt": "fine"}},
        ])),
    );
    assert_eq!(status, 200);

    // 2. Find the failures — the query IS the promotion's provenance.
    let (status, found) = server.request("GET", "/v1/spans?status=error", None);
    assert_eq!(status, 200);
    let failing = found["spans"].as_array().expect("spans");
    assert_eq!(failing.len(), 2);
    let offloaded_prompt = failing
        .iter()
        .find(|span| span["trace_id"] == json!("prod-1"))
        .expect("prod-1")["attributes"]["gen_ai.prompt"]
        .clone();
    assert!(
        offloaded_prompt.get("$payload").is_some(),
        "the big prompt offloaded; the example will carry the reference"
    );

    // 3. Promote them into a dataset version.
    let (status, created) = server.request(
        "POST",
        "/v1/datasets",
        Some(&json!({"name": "failing-summaries"})),
    );
    assert_eq!(status, 200, "{created}");
    let dataset_id = created["dataset_id"].as_u64().expect("dataset id");

    let examples: Vec<Value> = failing
        .iter()
        .map(|span| {
            json!({
                "example_id": format!("ex-{}", span["trace_id"].as_str().expect("id")),
                "input": {"prompt": span["attributes"]["gen_ai.prompt"].clone()},
                "expected": span["attributes"].get("expected").cloned(),
                "split": "test",
                "provenance": {
                    "trace_id": span["trace_id"].clone(),
                    "span_id": span["span_id"].clone(),
                },
            })
        })
        .collect();
    let (status, version) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({
            "provenance": {"query": "/v1/spans?status=error"},
            "examples": examples,
        })),
    );
    assert_eq!(status, 200, "{version}");
    let version_id = version["version_id"]
        .as_str()
        .expect("version id")
        .to_owned();
    assert_eq!(version["examples"], json!(2));
    assert_eq!(version["created"], json!(true));

    // Idempotent by content address: the same promotion is the same version.
    let (status, again) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({
            "provenance": {"query": "someone else, later"},
            "examples": examples,
        })),
    );
    assert_eq!(status, 200);
    assert_eq!(again["version_id"].as_str(), Some(version_id.as_str()));
    assert_eq!(again["created"], json!(false));

    // A derived version records its parent — lineage is first-class.
    let child_examples: Vec<Value> = examples[..1].to_vec();
    let (status, child) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({
            "parent": version_id,
            "examples": child_examples,
        })),
    );
    assert_eq!(status, 200, "{child}");
    let child_id = child["version_id"].as_str().expect("child id").to_owned();
    let (status, fetched) = server.request(
        "GET",
        &format!("/v1/datasets/{dataset_id}/versions/{child_id}"),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(fetched["parent"].as_str(), Some(version_id.as_str()));
    assert_eq!(fetched["bodies"].as_array().expect("bodies").len(), 1);

    // 4. Two experiments against the SAME version — the diff's operands.
    let mut experiment_ids = Vec::new();
    for name in ["baseline", "candidate"] {
        let (status, experiment) = server.request(
            "POST",
            "/v1/experiments",
            Some(&json!({
                "dataset_id": dataset_id,
                "dataset_version": version_id,
                "name": name,
                "config": {"model": format!("model-{name}")},
            })),
        );
        assert_eq!(status, 200, "{experiment}");
        experiment_ids.push(experiment["experiment_id"].as_u64().expect("id"));
    }
    let (baseline, candidate) = (experiment_ids[0], experiment_ids[1]);
    assert!(candidate > baseline, "experiment ids are monotonic");

    // 5. Run the tasks (externally — here) and record runs and scores.
    //    Baseline: ex-prod-1 scores 0.4, ex-prod-2 scores 0.9, exactness false.
    //    Candidate: ex-prod-1 improves to 0.8, ex-prod-2 regresses to 0.7.
    let record_run_and_score =
        |experiment: u64, example: &str, trace: &str, score: f64, exact: bool| {
            let (status, ingested) = server.request(
                "POST",
                "/v1/spans",
                Some(&json!([{
                    "trace_id": trace, "span_id": "run", "name": "eval-run",
                    "service": "harness", "start_time_ns": 10u64, "end_time_ns": 20u64,
                }])),
            );
            assert_eq!(status, 200, "{ingested}");
            let (status, run) = server.request(
                "POST",
                &format!("/v1/experiments/{experiment}/runs"),
                Some(&json!({"example_id": example, "trace_id": trace, "span_id": "run"})),
            );
            assert_eq!(status, 200, "{run}");
            for (name, value) in [("accuracy", json!(score)), ("exact", json!(exact))] {
                let (status, recorded) = server.request(
                    "POST",
                    "/v1/annotations",
                    Some(&json!({
                        "experiment_id": experiment, "example_id": example,
                        "trace_id": trace, "span_id": "run",
                        "name": name, "value": value, "source": "eval:harness",
                    })),
                );
                assert_eq!(status, 200, "{recorded}");
            }
        };
    record_run_and_score(baseline, "ex-prod-1", "run-b1", 0.4, false);
    record_run_and_score(baseline, "ex-prod-2", "run-b2", 0.9, true);
    record_run_and_score(candidate, "ex-prod-1", "run-c1", 0.8, true);
    record_run_and_score(candidate, "ex-prod-2", "run-c2", 0.7, true);

    // The runs are the experiment→trace link, recorded and listable.
    let (status, runs) = server.request("GET", &format!("/v1/experiments/{baseline}/runs"), None);
    assert_eq!(status, 200);
    assert_eq!(runs["runs"].as_array().expect("runs").len(), 2);
    let (status, experiment) = server.request("GET", &format!("/v1/experiments/{baseline}"), None);
    assert_eq!(status, 200);
    assert_eq!(experiment["run_count"], json!(2));
    assert_eq!(experiment["dataset_version_deleted"], json!(false));

    // 6. Score distributions.
    let (status, summary) =
        server.request("GET", &format!("/v1/experiments/{baseline}/summary"), None);
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["examples_total"], json!(2));
    let scores = summary["scores"].as_array().expect("scores");
    let accuracy = scores
        .iter()
        .find(|stat| stat["name"] == json!("accuracy"))
        .expect("accuracy stat");
    assert_eq!(accuracy["count"], json!(2));
    let mean = accuracy["mean"].as_f64().expect("mean");
    assert!(
        (mean - 0.65).abs() < 1e-9,
        "mean of 0.4 and 0.9, got {mean}"
    );
    let exact = scores
        .iter()
        .find(|stat| stat["name"] == json!("exact"))
        .expect("exact stat");
    assert_eq!(exact["true_rate"], json!(0.5));

    // 7. Experiment-over-experiment diff, joined on (example, name).
    let (status, diff) = server.request(
        "GET",
        &format!("/v1/experiments/diff?base={baseline}&candidate={candidate}"),
        None,
    );
    assert_eq!(status, 200, "{diff}");
    let accuracy_diff = diff["scores"]
        .as_array()
        .expect("scores")
        .iter()
        .find(|stat| stat["name"] == json!("accuracy"))
        .cloned()
        .expect("accuracy diff");
    assert_eq!(accuracy_diff["improved"], json!(["ex-prod-1"]));
    assert_eq!(accuracy_diff["regressed"], json!(["ex-prod-2"]));
    let delta = accuracy_diff["delta"].as_f64().expect("delta");
    assert!((delta - 0.1).abs() < 1e-9, "0.75 - 0.65, got {delta}");

    // 8. The whole loop's records survive kill -9: identity, not cache.
    let mut server = server;
    server.kill_hard();
    let server = Server::spawn(&dir);
    let (status, fetched) = server.request("GET", &format!("/v1/datasets/{dataset_id}"), None);
    assert_eq!(status, 200, "{fetched}");
    assert_eq!(fetched["versions"].as_array().expect("versions").len(), 2);
    let (status, summary) =
        server.request("GET", &format!("/v1/experiments/{candidate}/summary"), None);
    assert_eq!(status, 200);
    assert_eq!(summary["examples_total"], json!(2));
}

#[test]
fn version_validation_names_each_defect() {
    let dir = test_dir("validation");
    let server = Server::spawn(&dir);
    let (_, created) = server.request("POST", "/v1/datasets", Some(&json!({"name": "d"})));
    let dataset_id = created["dataset_id"].as_u64().expect("id");

    // Duplicate example id in one manifest.
    let (status, body) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({"examples": [
            {"example_id": "e1", "input": "a"},
            {"example_id": "e1", "input": "b"},
        ]})),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap_or("").contains("twice"));

    // Unknown parent.
    let (status, body) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({"parent": "f".repeat(64),
                     "examples": [{"example_id": "e1", "input": "a"}]})),
    );
    assert_eq!(status, 400, "{body}");

    // A payload reference whose bytes are not in the store: the example
    // would be born dangling, and "examples carry their own copies" would
    // be a lie at birth. Refused with the reference named.
    let absent = format!("sha256/{}", "9".repeat(64));
    let (status, body) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({"examples": [
            {"example_id": "e1", "input": {"$payload": absent, "bytes": 9}},
        ]})),
    );
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or("").contains("sha256/9"));

    // Scores validate their address: unknown experiment, then an example
    // outside the version's manifest.
    let (status, body) = server.request(
        "POST",
        "/v1/annotations",
        Some(&json!({"experiment_id": 999, "example_id": "e1",
                     "name": "accuracy", "value": 1})),
    );
    assert_eq!(status, 400, "{body}");
    let (_, version) = server.request(
        "POST",
        &format!("/v1/datasets/{dataset_id}/versions"),
        Some(&json!({"examples": [{"example_id": "e1", "input": "a"}]})),
    );
    let version_id = version["version_id"].as_str().expect("vid");
    let (_, experiment) = server.request(
        "POST",
        "/v1/experiments",
        Some(&json!({"dataset_id": dataset_id, "dataset_version": version_id})),
    );
    let experiment_id = experiment["experiment_id"].as_u64().expect("id");
    let (status, body) = server.request(
        "POST",
        "/v1/annotations",
        Some(
            &json!({"experiment_id": experiment_id, "example_id": "not-there",
                     "name": "accuracy", "value": 1}),
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap_or("").contains("not-there"));
}

#[test]
fn erasing_the_source_trace_leaves_the_dataset_whole_and_the_receipt_says_so() {
    let dir = test_dir("source-erasure");
    let store = Store::open(&dir, wal_config()).expect("opens");

    let big = "the customer's confidential document ".repeat(8);
    store
        .ingest(
            serde_json::from_value(json!({
                "trace_id": "prod-1", "span_id": "root", "name": "op", "service": "svc",
                "start_time_ns": 1000u64, "end_time_ns": 2000u64,
                "attributes": {"prompt": big},
            }))
            .expect("span"),
        )
        .expect("ingests");
    store.flush().expect("seals");
    let stored = store.get_trace("prod-1").expect("trace");
    let reference = stored[0].attributes["prompt"]["$payload"]
        .as_str()
        .expect("offloaded")
        .to_owned();

    // Promote: the example carries the payload REFERENCE and provenance.
    let dataset_id = store.create_dataset("", "curated").expect("dataset");
    let outcome = store
        .create_dataset_version(
            None,
            dataset_id,
            None,
            Some(json!({"query": "manual"})),
            vec![serde_json::from_value(json!({
                "example_id": "ex-1",
                "input": {"prompt": stored[0].attributes["prompt"].clone()},
                "provenance": {"trace_id": "prod-1", "span_id": "root"},
            }))
            .expect("example")],
        )
        .expect("version");

    // Score a run of it, with the run addressed to the source span — that
    // score dies with the trace (the evidence is gone), while the example
    // does not (the copy was deliberate).
    let experiment = store
        .create_experiment(None, dataset_id, &outcome.version_id, "exp", None)
        .expect("experiment");
    store
        .annotate(
            serde_json::from_value(json!({
                "experiment_id": experiment, "example_id": "ex-1",
                "trace_id": "prod-1", "span_id": "root",
                "name": "accuracy", "value": 0.5,
            }))
            .expect("score"),
        )
        .expect("scores");

    let status = store
        .erase(traza::erasure::Subject::Trace {
            trace_id: "prod-1".into(),
            tenant: String::new(),
        })
        .expect("erases");
    let settle = status.settle.expect("settles");
    assert_eq!(settle.spans_removed, 1);
    assert_eq!(
        settle.payloads_retained.len(),
        1,
        "the example's reference keeps the blob alive: {:?}",
        settle.payloads_removed
    );

    // The version is intact, its body still resolves, the payload still
    // serves — deleting source traces must not corrupt dataset versions.
    let version = store
        .dataset_version(None, dataset_id, &outcome.version_id)
        .expect("fetch")
        .expect("present")
        .expect("not tombstoned");
    assert_eq!(version.bodies.len(), 1);
    assert!(
        store.payload(&reference).expect("payload").is_some(),
        "the example's copy is real for offloaded content"
    );

    // The run-addressed score is gone with its trace.
    let scores = store
        .experiment_scores(None, experiment, None)
        .expect("scores")
        .expect("experiment exists");
    assert!(scores.is_empty(), "judgment about erased evidence goes too");

    // And the receipt tells the operator exactly what survived and where.
    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
    let evals_domain = receipt
        .domains
        .iter()
        .find(|domain| domain.domain == "eval-records")
        .expect("eval domain");
    assert_eq!(evals_domain.result, "attention");
    assert!(
        evals_domain.items.iter().any(|item| item.contains("ex-1")),
        "the copy is NAMED: {:?}",
        evals_domain.items
    );
    assert!(
        !receipt.conclusive,
        "a receipt over surviving copies must say it is not conclusive"
    );
}

#[test]
fn payload_erasure_reaches_bytes_but_leaves_addresses_and_stays_conclusive() {
    let dir = test_dir("payload-erasure");
    let store = Store::open(&dir, wal_config()).expect("opens");
    let big = "material someone will demand erased ".repeat(8);
    store
        .ingest(
            serde_json::from_value(json!({
                "trace_id": "t1", "span_id": "s1", "name": "op", "service": "svc",
                "start_time_ns": 1000u64, "end_time_ns": 2000u64,
                "attributes": {"prompt": big},
            }))
            .expect("span"),
        )
        .expect("ingests");
    store.flush().expect("seals");
    let stored = store.get_trace("t1").expect("trace");
    let reference = stored[0].attributes["prompt"]["$payload"]
        .as_str()
        .expect("offloaded")
        .to_owned();

    let dataset_id = store.create_dataset("", "curated").expect("dataset");
    let outcome = store
        .create_dataset_version(
            None,
            dataset_id,
            None,
            None,
            vec![serde_json::from_value(json!({
                "example_id": "ex-1",
                "input": {"prompt": stored[0].attributes["prompt"].clone()},
            }))
            .expect("example")],
        )
        .expect("version");

    let status = store
        .erase(traza::erasure::Subject::Payload {
            reference: reference.clone(),
        })
        .expect("erases");
    assert!(status.settle.is_some());

    assert!(
        store.payload(&reference).expect("payload").is_none(),
        "the bytes are gone — the erasure was COMMANDED"
    );
    let version = store
        .dataset_version(None, dataset_id, &outcome.version_id)
        .expect("fetch")
        .expect("present")
        .expect("not tombstoned");
    assert_eq!(
        version.bodies.len(),
        1,
        "the manifest is structurally intact"
    );

    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
    let evals_domain = receipt
        .domains
        .iter()
        .find(|domain| domain.domain == "eval-records")
        .expect("eval domain");
    assert_eq!(
        evals_domain.result, "retained-by-design",
        "a dangling reference is an address, not content"
    );
    assert!(
        receipt.conclusive,
        "addresses do not make a receipt inconclusive:\n{}",
        receipt.render_text()
    );
}

#[test]
fn tenant_erasure_takes_the_tenants_eval_records_and_ids_are_never_reused() {
    let dir = test_dir("tenant-evals");
    let store = Store::open(&dir, wal_config()).expect("opens");

    let acme_dataset = store.create_dataset("acme", "acme-data").expect("dataset");
    let bigco_dataset = store
        .create_dataset("bigco", "bigco-data")
        .expect("dataset");
    let outcome = store
        .create_dataset_version(
            None,
            acme_dataset,
            None,
            None,
            vec![serde_json::from_value(json!({
                "example_id": "e1", "input": "acme content",
            }))
            .expect("example")],
        )
        .expect("version");
    let experiment = store
        .create_experiment(None, acme_dataset, &outcome.version_id, "exp", None)
        .expect("experiment");
    store
        .annotate(
            serde_json::from_value(json!({
                "tenant": "acme", "experiment_id": experiment, "example_id": "e1",
                "name": "accuracy", "value": 1,
            }))
            .expect("score"),
        )
        .expect("scores");

    let status = store
        .erase(traza::erasure::Subject::Tenant {
            tenant: "acme".into(),
        })
        .expect("erases");
    let settle = status.settle.expect("settles");
    assert!(
        settle.eval_records_removed >= 3,
        "dataset, version, example, experiment leave via the eval log; got {}",
        settle.eval_records_removed
    );
    assert_eq!(
        settle.annotations_removed, 1,
        "the score leaves as an annotation"
    );

    assert!(store.dataset(None, acme_dataset).expect("fetch").is_none());
    assert!(store.dataset(None, bigco_dataset).expect("fetch").is_some());
    assert!(store.experiment(None, experiment).expect("fetch").is_none());

    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
    let evals_domain = receipt
        .domains
        .iter()
        .find(|domain| domain.domain == "eval-records")
        .expect("eval domain");
    assert_eq!(evals_domain.result, "clear", "{}", receipt.render_text());

    // Ids erased with the tenant are NEVER reissued: every external
    // reference to dataset 1 stays a reference to the erased thing.
    let next = store.create_dataset("bigco", "later").expect("dataset");
    assert!(
        next > acme_dataset && next > bigco_dataset,
        "id monotonicity survives the rewrite (counter record)"
    );

    // And survives a restart.
    drop(store);
    let store = Store::open(&dir, wal_config()).expect("reopens");
    let after_restart = store
        .create_dataset("bigco", "even-later")
        .expect("dataset");
    assert!(after_restart > next);
}

#[test]
fn a_version_tombstone_has_exactly_its_defined_effects() {
    let dir = test_dir("tombstone");
    let store = Store::open(&dir, wal_config()).expect("opens");
    let dataset_id = store.create_dataset("", "d").expect("dataset");
    let outcome = store
        .create_dataset_version(
            None,
            dataset_id,
            None,
            None,
            vec![serde_json::from_value(json!({
                "example_id": "e1", "input": "content",
            }))
            .expect("example")],
        )
        .expect("version");
    let experiment = store
        .create_experiment(None, dataset_id, &outcome.version_id, "exp", None)
        .expect("experiment");
    store
        .annotate(
            serde_json::from_value(json!({
                "experiment_id": experiment, "example_id": "e1",
                "name": "accuracy", "value": 1,
            }))
            .expect("score"),
        )
        .expect("scores");

    assert!(store
        .tombstone_dataset_version(None, dataset_id, &outcome.version_id, "superseded")
        .expect("tombstones"));
    assert!(
        !store
            .tombstone_dataset_version(None, dataset_id, &outcome.version_id, "again")
            .expect("idempotent"),
        "tombstoning twice reports already-tombstoned"
    );

    // Effect 1: the version is GONE from fetch, with the tombstone.
    let fetched = store
        .dataset_version(None, dataset_id, &outcome.version_id)
        .expect("fetch")
        .expect("known");
    assert!(fetched.is_err(), "a tombstoned version answers 410-shaped");

    // Effect 2: dependent experiments keep working and say why.
    let view = store
        .experiment(None, experiment)
        .expect("fetch")
        .expect("present");
    assert!(view.dataset_version_deleted);
    let scores = store
        .experiment_scores(None, experiment, None)
        .expect("scores")
        .expect("present");
    assert_eq!(scores.len(), 1, "scores are untouched");

    // Effect 3: NEW experiments against it are refused.
    assert!(matches!(
        store.create_experiment(None, dataset_id, &outcome.version_id, "late", None),
        Err(traza::Error::Conflict(_))
    ));

    // Effect 4: the tombstone survives restart.
    drop(store);
    let store = Store::open(&dir, wal_config()).expect("reopens");
    let fetched = store
        .dataset_version(None, dataset_id, &outcome.version_id)
        .expect("fetch")
        .expect("known");
    assert!(fetched.is_err());
}

#[test]
fn duplicate_scores_resolve_by_latest_write_in_summary_and_diff() {
    let dir = test_dir("lww-scores");
    let store = Store::open(&dir, wal_config()).expect("opens");
    let dataset_id = store.create_dataset("", "d").expect("dataset");
    let outcome = store
        .create_dataset_version(
            None,
            dataset_id,
            None,
            None,
            vec![serde_json::from_value(json!({
                "example_id": "e1", "input": "content",
            }))
            .expect("example")],
        )
        .expect("version");
    let experiment = store
        .create_experiment(None, dataset_id, &outcome.version_id, "exp", None)
        .expect("experiment");
    for (value, timestamp) in [(0.2, 100u64), (0.9, 200u64)] {
        store
            .annotate(
                serde_json::from_value(json!({
                    "experiment_id": experiment, "example_id": "e1",
                    "name": "accuracy", "value": value, "timestamp_ns": timestamp,
                }))
                .expect("score"),
            )
            .expect("scores");
    }
    let summary = store
        .experiment_summary(None, experiment)
        .expect("summary")
        .expect("present");
    let accuracy = summary
        .scores
        .iter()
        .find(|stat| stat.name == "accuracy")
        .expect("stat");
    assert_eq!(
        accuracy.count, 1,
        "a re-score moves a number, never double-counts"
    );
    assert_eq!(accuracy.mean, Some(0.9), "the LATEST write is the number");
}

#[test]
fn a_pending_tenant_erasure_refuses_eval_writes_until_it_settles() {
    let dir = test_dir("pending-barrier");
    // Plant a PENDING tenant erasure — an erase record with no settle — the
    // way a crash mid-purge leaves one, then open the store around it.
    let planted = json!({
        "op": "erase", "schema": 2, "id": 1, "requested_unix_ns": 1,
        "subject": {"kind": "tenant", "tenant": "acme"},
        "span_keys": [], "payload_refs": [],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{planted}\n")).expect("plants");
    let store = Store::open(&dir, wal_config()).expect("opens");

    // The barrier: every eval write for the masked tenant is refused, and
    // other tenants flow.
    assert!(matches!(
        store.create_dataset("acme", "blocked"),
        Err(traza::Error::Conflict(_))
    ));
    let bigco = store.create_dataset("bigco", "flows").expect("dataset");
    assert!(bigco >= 1);

    // Reads hide the masked tenant's eval records too — nothing to hide
    // here, but the surface answers coherently.
    assert!(store.datasets(Some("acme")).expect("datasets").is_empty());

    // Settle it; the barrier lifts.
    let settled = store.resume_erasures().expect("settles");
    assert_eq!(settled, 1);
    let acme = store
        .create_dataset("acme", "post-settle")
        .expect("dataset");
    assert!(acme > bigco, "an erasure is a barrier, not a ban");
}

#[test]
fn concurrent_identical_version_posts_agree_on_one_version() {
    let dir = test_dir("concurrent-version");
    let store = std::sync::Arc::new(Store::open(&dir, wal_config()).expect("opens"));
    let dataset_id = store.create_dataset("", "d").expect("dataset");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let store = std::sync::Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            store.create_dataset_version(
                None,
                dataset_id,
                None,
                None,
                vec![serde_json::from_value(json!({
                    "example_id": "e1", "input": "identical content",
                }))
                .expect("example")],
            )
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("joins").expect("succeeds"))
        .collect();
    let first = &outcomes[0].version_id;
    assert!(outcomes.iter().all(|outcome| outcome.version_id == *first));
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.created).count(),
        1,
        "exactly one writer created it; the rest found it"
    );
    // The log replays to exactly one version.
    drop(store);
    let store = Store::open(&dir, wal_config()).expect("reopens");
    let dataset = store
        .dataset(None, dataset_id)
        .expect("fetch")
        .expect("present");
    assert_eq!(dataset.versions.len(), 1);
}

#[test]
fn a_torn_eval_append_heals_and_the_version_survives_kill() {
    let dir = test_dir("torn");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        let dataset_id = store.create_dataset("", "d").expect("dataset");
        store
            .create_dataset_version(
                None,
                dataset_id,
                None,
                None,
                vec![serde_json::from_value(json!({
                    "example_id": "e1", "input": "durable content",
                }))
                .expect("example")],
            )
            .expect("version");
    }
    // Tear the tail the way a crash mid-append would.
    let path = dir.join("evals.jsonl");
    let mut bytes = std::fs::read(&path).expect("reads");
    let intact = bytes.len();
    bytes.extend_from_slice(b"{\"record\":\"dataset\",\"schema\":1,\"data");
    std::fs::write(&path, &bytes).expect("tears");

    let store = Store::open(&dir, wal_config()).expect("heals the torn tail");
    let datasets = store.datasets(None).expect("datasets");
    assert_eq!(datasets.len(), 1);
    assert_eq!(datasets[0].versions.len(), 1, "the acked version survived");
    assert_eq!(
        std::fs::read(&path).expect("reads").len(),
        intact,
        "the torn bytes were truncated away, not left as interior damage"
    );
}
