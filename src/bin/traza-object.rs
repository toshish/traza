//! Operator CLI for the object-storage snapshot archive (preview).
//!
//! Publishes verified pins as immutable remote snapshots, queries them by
//! range reads, restores them, and manages them. Built only with
//! `--features object-storage`.
//!
//! The recommended source of a pin is a RUNNING server's backup API
//! (`POST /v1/backups/<label>` — no lock contention, no second opener); the
//! `pin` subcommand exists for stopped stores and opens the directory
//! exclusively, refusing if a server holds it.
//!
//! Credentials are never flags: the S3 backend reads the standard AWS
//! environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//! `AWS_SESSION_TOKEN`, `AWS_REGION`, web identity/container credentials/IMDS).
//! Shared AWS profiles are not loaded. TLS verifies unless
//! `--allow-http` explicitly opts a local test endpoint out.

use std::process::ExitCode;
use std::time::Duration;

use traza::object_storage::{Backend, Remote, RemoteOptions, S3Options};
use traza::{SpanFilter, SpanSort};

const USAGE: &str = "traza-object — immutable snapshot archives on object storage (preview)

USAGE:
  traza-object <COMMAND> [OPTIONS]

COMMANDS:
  pin       Take a verified archive pin from a STOPPED store
              --data-dir <DIR> --label <LABEL>
  publish   Publish a verified pin as an immutable snapshot
              --pin <DIR> --snapshot <ID> + backend options
  list      List snapshots (complete and debris)
  inspect   Print one snapshot's manifest summary      --snapshot <ID>
  verify    Check objects against the manifest         --snapshot <ID> [--deep]
  query     Query a snapshot's spans (NDJSON)          --snapshot <ID> [filters]
  trace     Fetch one trace                            --snapshot <ID> --trace-id <T> [--tenant <T>]
  payload   Fetch one offloaded payload to stdout      --snapshot <ID> --ref sha256/<hex>
  restore   Restore into a NEW local directory         --snapshot <ID> --into <DIR>
  delete    Permanently delete a snapshot              --snapshot <ID>
            (tombstoned: the id is NEVER reusable; refuses in-flight uploads)
  cleanup   Remove an abandoned upload's debris        --snapshot <ID> [--publisher-quiescent]
            (--publisher-quiescent asserts NO publisher for this id is
             running anywhere; required to sweep a manifest-less upload)
  version   Print `traza-object <version>` (also --version / -V)

BACKEND OPTIONS (everything but `pin`):
  --bucket <NAME>          S3 bucket (required unless --in-memory)
  --region <REGION>        overrides AWS_REGION
  --endpoint <URL>         S3-compatible endpoint (MinIO etc.)
  --allow-http             explicit opt-in to a plain-HTTP endpoint (testing)
  --virtual-hosted         use virtual-hosted addressing (path-style is default)
  --prefix <KEY/PREFIX>    key prefix inside the bucket
  --store-id <NAME>        store identity stamped into / checked against manifests
  --op-timeout-secs <N>    per-request bound (default 60)
  --retries <N>            S3 retry budget (default 3)
  --cache-mb <N>           chunk-cache budget for reads (default 256)
  --expect-manifest-sha <HEX>  refuse any manifest not hashing to this
                           externally retained SHA-256 (from publish output)
  --in-memory              throwaway in-process backend (testing only)

QUERY FILTERS:
  --service <S> --name <N> --status <S> --tenant <T> --content <WORDS>
  --session <ID> --since-ns <N> --until-ns <N> --min-duration-ns <N>
  --limit <N> --sort <duration_desc|duration_asc|start_desc|start_asc>
  --deadline-ms <N>        refuse queries that exceed this compute budget
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("traza-object: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Flag parser: `--key value` and boolean `--key`, no positionals after the
/// command. Unknown flags are errors — a misspelled bound must not silently
/// become an unbounded default.
struct Flags {
    pairs: Vec<(String, Option<String>)>,
}

impl Flags {
    fn parse(args: &[String], booleans: &[&str]) -> Result<Self, String> {
        let mut pairs = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let flag = &args[index];
            let Some(name) = flag.strip_prefix("--") else {
                return Err(format!(
                    "unexpected argument {flag:?} (flags are --name value)"
                ));
            };
            if booleans.contains(&name) {
                pairs.push((name.to_owned(), None));
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("--{name} needs a value"))?;
            pairs.push((name.to_owned(), Some(value.clone())));
            index += 2;
        }
        Ok(Self { pairs })
    }

    fn take(&mut self, name: &str) -> Option<String> {
        let position = self.pairs.iter().position(|(held, _)| held == name)?;
        self.pairs.remove(position).1
    }

    fn has(&mut self, name: &str) -> bool {
        match self
            .pairs
            .iter()
            .position(|(held, value)| held == name && value.is_none())
        {
            Some(position) => {
                self.pairs.remove(position);
                true
            }
            None => false,
        }
    }

    fn finish(self, known: &str) -> Result<(), String> {
        match self.pairs.first() {
            Some((name, _)) => Err(format!("unknown flag --{name} (known: {known})")),
            None => Ok(()),
        }
    }
}

const BOOLEANS: &[&str] = &[
    "allow-http",
    "virtual-hosted",
    "in-memory",
    "deep",
    "publisher-quiescent",
];

fn parse_number<T: std::str::FromStr>(flags: &mut Flags, name: &str) -> Result<Option<T>, String> {
    match flags.take(name) {
        None => Ok(None),
        Some(value) => value
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("--{name} {value:?} is not a number")),
    }
}

fn remote_from(flags: &mut Flags) -> Result<Remote, String> {
    let in_memory = flags.has("in-memory");
    let backend = if in_memory {
        Backend::InMemory
    } else {
        let bucket = flags
            .take("bucket")
            .ok_or("--bucket is required (or --in-memory for testing)")?;
        let endpoint = flags.take("endpoint");
        Backend::S3(S3Options {
            bucket,
            region: flags.take("region"),
            allow_http: flags.has("allow-http"),
            force_path_style: !flags.has("virtual-hosted"),
            endpoint,
        })
    };
    let mut options = RemoteOptions::new(backend);
    if let Some(prefix) = flags.take("prefix") {
        options.prefix = prefix;
    }
    if let Some(identity) = flags.take("store-id") {
        options.store_identity = identity;
    }
    if let Some(seconds) = parse_number::<u64>(flags, "op-timeout-secs")? {
        options.op_timeout = Duration::from_secs(seconds.max(1));
    }
    if let Some(retries) = parse_number::<usize>(flags, "retries")? {
        options.max_retries = retries;
    }
    if let Some(cache_mb) = parse_number::<usize>(flags, "cache-mb")? {
        options.cache_bytes = cache_mb.saturating_mul(1 << 20);
    }
    options.expected_manifest_sha256 = flags.take("expect-manifest-sha");
    Remote::open(options).map_err(|error| error.to_string())
}

fn filter_from(flags: &mut Flags) -> Result<SpanFilter, String> {
    let mut filter = SpanFilter {
        service: flags.take("service"),
        name: flags.take("name"),
        status: flags.take("status"),
        tenant: flags.take("tenant"),
        content: flags.take("content"),
        session: flags.take("session"),
        since_ns: parse_number(flags, "since-ns")?,
        until_ns: parse_number(flags, "until-ns")?,
        min_duration_ns: parse_number(flags, "min-duration-ns")?,
        limit: parse_number(flags, "limit")?,
        ..SpanFilter::default()
    };
    if let Some(sort) = flags.take("sort") {
        filter.sort =
            Some(SpanSort::parse(&sort).ok_or_else(|| format!("unknown --sort {sort:?}"))?);
    }
    Ok(filter)
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return Err("a command is required".to_owned());
    };
    let mut flags = Flags::parse(&args[1..], BOOLEANS)?;
    match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("traza-object {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "pin" => {
            let data_dir = flags.take("data-dir").ok_or("--data-dir is required")?;
            let label = flags.take("label").ok_or("--label is required")?;
            flags.finish("--data-dir --label")?;
            let store = traza::Store::open(&data_dir, traza::Config::default()).map_err(
                |error| match error {
                    traza::Error::AlreadyOpen => format!(
                        "{data_dir}: a live server owns this store — use its backup API \
                         instead: curl -X POST http://<server>/v1/backups/{label}"
                    ),
                    other => other.to_string(),
                },
            )?;
            let generation = store
                .pin_for_object_archive(&label)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "pin": label,
                    "generation": generation,
                    "path": store.pin_path(&label),
                    "verified": true,
                })
            );
            Ok(())
        }
        "publish" => {
            let pin = flags.take("pin").ok_or("--pin <DIR> is required")?;
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--pin --snapshot + backend options")?;
            let receipt = remote
                .publish_pin(std::path::Path::new(&pin), &snapshot)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": receipt.snapshot,
                    "generation": receipt.generation,
                    "files": receipt.files,
                    "objects": receipt.objects,
                    "uploaded_bytes": receipt.uploaded_bytes,
                    "verified_bytes": receipt.verified_bytes,
                    // Retain this OUTSIDE the bucket; hand it back with
                    // --expect-manifest-sha to detect a rewritten manifest.
                    "manifest_sha256": receipt.manifest_sha256,
                })
            );
            Ok(())
        }
        "list" => {
            let remote = remote_from(&mut flags)?;
            flags.finish("backend options")?;
            for summary in remote.list_snapshots().map_err(|error| error.to_string())? {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": summary.id,
                        "complete": summary.complete,
                        "uploading": summary.uploading,
                        "deleted": summary.deleted,
                        "foreign": summary.foreign,
                        "created_unix_ns": summary.created_unix_ns,
                        "generation": summary.source_generation,
                        "files": summary.files,
                        "total_bytes": summary.total_bytes,
                    })
                );
            }
            Ok(())
        }
        "inspect" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot + backend options")?;
            let manifest = remote
                .inspect_snapshot(&snapshot)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": manifest.snapshot,
                    "store": manifest.store,
                    "created_unix_ns": manifest.created_unix_ns,
                    "generation": manifest.source_generation,
                    "segment_format": manifest.segment_format,
                    "chunk_bytes": manifest.chunk_bytes,
                    "files": manifest.files.len(),
                    "objects": manifest.objects.len(),
                    "total_bytes": manifest.files.iter().map(|f| f.bytes).sum::<u64>(),
                })
            );
            Ok(())
        }
        "verify" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let deep = flags.has("deep");
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot [--deep] + backend options")?;
            let receipt = remote
                .verify_snapshot(&snapshot, deep)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": snapshot,
                    "objects": receipt.objects,
                    "verified_bytes": receipt.verified_bytes,
                    "intact": receipt.problems.is_empty(),
                    "problems": receipt.problems,
                })
            );
            if receipt.problems.is_empty() {
                Ok(())
            } else {
                Err("verification found problems".to_owned())
            }
        }
        "query" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let deadline_ms = parse_number::<u64>(&mut flags, "deadline-ms")?;
            let filter = filter_from(&mut flags)?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot [filters] + backend options")?;
            let view = remote
                .open_snapshot(&snapshot)
                .map_err(|error| error.to_string())?;
            let spans = view
                .query_bounded(&filter, None, deadline_ms.map(Duration::from_millis))
                .map_err(|error| error.to_string())?;
            for span in &spans {
                println!(
                    "{}",
                    serde_json::to_string(span).map_err(|error| error.to_string())?
                );
            }
            let stats = view.read_stats();
            eprintln!(
                "{} span(s); fetched {} of {} remote bytes ({} chunk fetches, {} cache hits)",
                spans.len(),
                stats.fetched_bytes,
                stats.remote_total_bytes,
                stats.chunk_fetches,
                stats.cache_hits,
            );
            Ok(())
        }
        "trace" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let trace_id = flags.take("trace-id").ok_or("--trace-id is required")?;
            let tenant = flags.take("tenant");
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot --trace-id [--tenant] + backend options")?;
            let view = remote
                .open_snapshot(&snapshot)
                .map_err(|error| error.to_string())?;
            let spans = view
                .get_trace(tenant.as_deref(), &trace_id)
                .map_err(|error| error.to_string())?;
            for span in &spans {
                println!(
                    "{}",
                    serde_json::to_string(span).map_err(|error| error.to_string())?
                );
            }
            Ok(())
        }
        "payload" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let reference = flags.take("ref").ok_or("--ref sha256/<hex> is required")?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot --ref + backend options")?;
            let view = remote
                .open_snapshot(&snapshot)
                .map_err(|error| error.to_string())?;
            match view
                .payload(&reference)
                .map_err(|error| error.to_string())?
            {
                Some(bytes) => {
                    use std::io::Write;
                    std::io::stdout()
                        .write_all(&bytes)
                        .map_err(|error| error.to_string())?;
                    Ok(())
                }
                None => Err(format!("{reference}: not in this snapshot")),
            }
        }
        "restore" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let into = flags.take("into").ok_or("--into <NEW DIR> is required")?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot --into + backend options")?;
            let receipt = remote
                .restore_snapshot(&snapshot, std::path::Path::new(&into))
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": receipt.snapshot,
                    "generation": receipt.generation,
                    "files": receipt.files,
                    "fetched_bytes": receipt.fetched_bytes,
                    "restored_to": into,
                    "next": "traza-server --data-dir <fresh dir> --restore <restored_to>",
                })
            );
            Ok(())
        }
        "delete" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot + backend options")?;
            let receipt = remote
                .delete_snapshot(&snapshot)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": snapshot,
                    "objects_deleted": receipt.objects_deleted,
                    "tombstoned": true,
                    "note": "this snapshot id is permanently retired and can never be reused",
                })
            );
            Ok(())
        }
        "cleanup" => {
            let snapshot = flags.take("snapshot").ok_or("--snapshot is required")?;
            let quiescent = flags.has("publisher-quiescent");
            let remote = remote_from(&mut flags)?;
            flags.finish("--snapshot [--publisher-quiescent] + backend options")?;
            let receipt = remote
                .cleanup_snapshot(&snapshot, quiescent)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "snapshot": snapshot,
                    "objects_deleted": receipt.objects_deleted,
                    "note": "listed objects only; a crashed publisher's incomplete multipart \
                             parts need the bucket's AbortIncompleteMultipartUpload lifecycle rule",
                })
            );
            Ok(())
        }
        other => {
            eprintln!("{USAGE}");
            Err(format!("unknown command {other:?}"))
        }
    }
}
