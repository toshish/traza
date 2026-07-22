//! Command-line server facade for durable trace storage.
//!
//! The binary intentionally supports one operation per invocation. This keeps it
//! useful in scripts while ensuring every request opens the same durable engine
//! path that a long-running server would use.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: traza-server [--db PATH] <write|read|query> [arguments]\n\
write: traza-server [--db PATH] write [--trace-id ID] [--payload JSON]\n\
read:  traza-server [--db PATH] read [--trace-id ID]\n\
query: traza-server [--db PATH] query [--trace-id ID]\n";

#[derive(Debug)]
struct Request {
    db: PathBuf,
    operation: String,
    trace_id: Option<String>,
    payload: Option<String>,
}

fn main() {
    if let Err(error) = run(env::args_os().skip(1).collect()) {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let request = parse_request(arguments)?;
    let engine = TraceEngine::open(&request.db)
        .map_err(|error| format!("failed to open trace engine at {}: {error}", request.db.display()))?;

    match request.operation.as_str() {
        "write" => {
            let trace_id = required_trace_id(&request)?;
            let payload = request
                .payload
                .as_deref()
                .ok_or_else(|| "invalid write: payload is required".to_owned())?;
            validate_write(trace_id, payload)?;
            engine
                .write(trace_id, payload)
                .map_err(|error| format!("write failed for trace '{trace_id}': {error}"))?;
            println!("{{\"status\":\"written\",\"trace_id\":\"{}\"}}", escape_json(trace_id));
            Ok(())
        }
        "read" | "query" => {
            let trace_id = required_trace_id(&request)?;
            match engine
                .read(trace_id)
                .map_err(|error| format!("query failed for trace '{trace_id}': {error}"))?
            {
                Some(payload) => {
                    println!("{payload}");
                    Ok(())
                }
                None => Err(format!("trace '{trace_id}' not found")),
            }
        }
        operation => Err(format!("unknown operation '{operation}'\n{USAGE}")),
    }
}

fn required_trace_id(request: &Request) -> Result<&str, String> {
    request
        .trace_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("invalid {}: trace id is required", request.operation))
}

fn parse_request(arguments: Vec<OsString>) -> Result<Request, String> {
    let values = arguments
        .into_iter()
        .map(|value| value.into_string().map_err(|_| "arguments must be valid UTF-8".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;

    if values.iter().any(|value| value == "--help" || value == "-h") {
        print!("{USAGE}");
        process::exit(0);
    }

    let mut db = env::var_os("TRAZA_DB_PATH")
        .or_else(|| env::var_os("TRAZA_DATA_DIR"))
        .map(PathBuf::from);
    let mut trace_id = None;
    let mut payload = None;
    let mut positional = Vec::new();
    let mut index = 0;

    while index < values.len() {
        match values[index].as_str() {
            "--db" | "--storage" | "--data-dir" | "--path" => {
                let value = values
                    .get(index + 1)
                    .ok_or_else(|| format!("{} requires a path", values[index]))?;
                db = Some(PathBuf::from(value));
                index += 2;
            }
            "--trace-id" | "--trace" => {
                trace_id = Some(
                    values
                        .get(index + 1)
                        .ok_or_else(|| format!("{} requires a value", values[index]))?
                        .clone(),
                );
                index += 2;
            }
            "--payload" | "--data" => {
                payload = Some(
                    values
                        .get(index + 1)
                        .ok_or_else(|| format!("{} requires a value", values[index]))?
                        .clone(),
                );
                index += 2;
            }
            value if value.starts_with('-') => return Err(format!("unknown option '{value}'\n{USAGE}")),
            _ => {
                positional.push(values[index].clone());
                index += 1;
            }
        }
    }

    if positional.is_empty() {
        return Err(USAGE.trim_end().to_owned());
    }

    let operation_index = positional
        .iter()
        .position(|value| matches!(value.as_str(), "write" | "read" | "query"))
        .ok_or_else(|| format!("missing operation\n{USAGE}"))?;

    if operation_index == 1 && db.is_none() {
        db = Some(PathBuf::from(&positional[0]));
    } else if operation_index > 0 {
        return Err(format!("unexpected arguments before operation\n{USAGE}"));
    }

    let operation = positional[operation_index].clone();
    let trailing = &positional[operation_index + 1..];
    if trace_id.is_none() {
        trace_id = trailing.first().cloned();
    }
    if payload.is_none() && operation == "write" {
        payload = trailing.get(1).cloned();
    }
    if trailing.len() > if operation == "write" { 2 } else { 1 } {
        return Err(format!("too many arguments for {operation}\n{USAGE}"));
    }

    Ok(Request {
        db: db.unwrap_or_else(|| PathBuf::from("traza-data")),
        operation,
        trace_id,
        payload,
    })
}

fn validate_write(trace_id: &str, payload: &str) -> Result<(), String> {
    if trace_id.is_empty()
        || trace_id.len() > 512
        || trace_id.chars().any(|character| character.is_control())
    {
        return Err("invalid write: trace id must be a non-empty printable value".to_owned());
    }

    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Err("invalid write: payload must not be empty".to_owned());
    }
    let looks_like_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if !looks_like_json {
        return Err("invalid write: payload must be a JSON object or array".to_owned());
    }
    Ok(())
}

struct TraceEngine {
    traces: PathBuf,
}

impl TraceEngine {
    fn open(root: &Path) -> io::Result<Self> {
        let traces = root.join("traces");
        fs::create_dir_all(&traces)?;
        Ok(Self { traces })
    }

    fn write(&self, trace_id: &str, payload: &str) -> io::Result<()> {
        let destination = self.trace_path(trace_id);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = self
            .traces
            .join(format!(".{}.{}.tmp", process::id(), nonce));

        let result = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &destination)?;
            sync_directory(&self.traces)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn read(&self, trace_id: &str) -> io::Result<Option<String>> {
        let path = self.trace_path(trace_id);
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut payload = String::new();
        file.read_to_string(&mut payload)?;
        Ok(Some(payload))
    }

    fn trace_path(&self, trace_id: &str) -> PathBuf {
        self.traces.join(hex_encode(trace_id.as_bytes()))
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}
