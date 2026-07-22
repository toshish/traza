//! HTTP server binary for Traza.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const HTTP_QUEUE_DEPTH: usize = 256;
const INGEST_QUEUE_DEPTH: usize = 64;

type SharedState = Arc<RwLock<State>>;

struct State {
    spans: Vec<Value>,
    traces: HashMap<String, Vec<usize>>,
    services: HashMap<String, Vec<usize>>,
    names: HashMap<String, Vec<usize>>,
    attributes: HashMap<(String, String), Vec<usize>>,
    bytes_on_disk: u64,
    segments: u64,
}

impl State {
    fn new() -> Self {
        Self {
            spans: Vec::new(),
            traces: HashMap::new(),
            services: HashMap::new(),
            names: HashMap::new(),
            attributes: HashMap::new(),
            bytes_on_disk: 0,
            segments: 0,
        }
    }

    fn insert(&mut self, span: Value) {
        let index = self.spans.len();
        if let Some(trace_id) = text(&span, "trace_id") {
            self.traces
                .entry(trace_id.to_owned())
                .or_default()
                .push(index);
        }
        if let Some(service) = text(&span, "service") {
            self.services
                .entry(service.to_owned())
                .or_default()
                .push(index);
        }
        if let Some(name) = text(&span, "name") {
            self.names.entry(name.to_owned()).or_default().push(index);
        }
        if let Some(attributes) = span.get("attributes").and_then(Value::as_object) {
            for (key, value) in attributes {
                self.attributes
                    .entry((key.clone(), canonical(value)))
                    .or_default()
                    .push(index);
            }
        }
        self.spans.push(span);
    }
}

struct Ingest {
    spans: Vec<Value>,
    reply: mpsc::SyncSender<Result<usize, String>>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("traza-server: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("./data");
    let mut port = 8080_u16;
    let mut ttl_seconds = None;
    let mut workers = thread::available_parallelism()
        .map_or(4, usize::from)
        .max(4);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(args.get(i).ok_or("--data-dir requires a value")?);
            }
            "--port" => {
                i += 1;
                port = args.get(i).ok_or("--port requires a value")?.parse()?;
            }
            "--ttl-seconds" => {
                i += 1;
                ttl_seconds = Some(
                    args.get(i)
                        .ok_or("--ttl-seconds requires a value")?
                        .parse()?,
                );
            }
            "--workers" => {
                i += 1;
                workers = args
                    .get(i)
                    .ok_or("--workers requires a value")?
                    .parse::<usize>()?
                    .max(1);
            }
            "--help" | "-h" => {
                println!("Usage: traza-server --data-dir DIR --port PORT [--ttl-seconds N] [--workers N]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }

    fs::create_dir_all(&data_dir)?;
    expire_old_logs(&data_dir, ttl_seconds)?;
    let log_path = data_dir.join("spans.log");
    let state = Arc::new(RwLock::new(load_state(&log_path)?));
    let (ingest_tx, ingest_rx) = mpsc::sync_channel::<Ingest>(INGEST_QUEUE_DEPTH);
    let writer_state = Arc::clone(&state);
    let writer_path = log_path.clone();
    thread::Builder::new()
        .name("traza-writer".into())
        .spawn(move || {
            if let Err(error) = writer_loop(&writer_path, writer_state, ingest_rx) {
                eprintln!("writer stopped: {error}");
            }
        })?;

    let listener = TcpListener::bind(("0.0.0.0", port))?;
    eprintln!("traza-server listening on 0.0.0.0:{port}");
    let (http_tx, http_rx) = mpsc::sync_channel::<TcpStream>(HTTP_QUEUE_DEPTH);
    let http_rx = Arc::new(Mutex::new(http_rx));
    for number in 0..workers {
        let rx = Arc::clone(&http_rx);
        let tx = ingest_tx.clone();
        let worker_state = Arc::clone(&state);
        thread::Builder::new()
            .name(format!("http-{number}"))
            .spawn(move || loop {
                let stream = match rx.lock().expect("HTTP receiver poisoned").recv() {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                if let Err(error) = handle_connection(stream, &worker_state, &tx) {
                    eprintln!("request error: {error}");
                }
            })?;
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
                if http_tx.send(stream).is_err() {
                    break;
                }
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }
    Ok(())
}

fn load_state(path: &Path) -> io::Result<State> {
    let mut state = State::new();
    if !path.exists() {
        return Ok(state);
    }
    state.bytes_on_disk = fs::metadata(path)?.len();
    state.segments = 1;
    let reader = BufReader::new(File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(span) => state.insert(span),
            Err(_) => break,
        }
    }
    Ok(state)
}

fn writer_loop(path: &Path, state: SharedState, rx: mpsc::Receiver<Ingest>) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    while let Ok(request) = rx.recv() {
        let mut encoded = Vec::new();
        let result = (|| -> Result<usize, String> {
            for span in &request.spans {
                validate_span(span)?;
                serde_json::to_writer(&mut encoded, span).map_err(|e| e.to_string())?;
                encoded.push(b'\n');
            }
            file.write_all(&encoded).map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
            let count = request.spans.len();
            let mut locked = state
                .write()
                .map_err(|_| "state lock poisoned".to_owned())?;
            for span in request.spans {
                locked.insert(span);
            }
            locked.bytes_on_disk += encoded.len() as u64;
            locked.segments = 1;
            Ok(count)
        })();
        let _ = request.reply.send(result);
    }
    Ok(())
}

#[allow(unused_variables)]
fn validate_span(span: &Value) -> Result<(), String> {
    // The datastore parser is the single source of truth for accepted wire aliases.

    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    state: &SharedState,
    ingest: &mpsc::SyncSender<Ingest>,
) -> io::Result<()> {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => return respond(&mut stream, 400, json!({"error": error.to_string()})),
    };
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    match (request.method.as_str(), path) {
        ("POST", "/v1/spans") => {
            let value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => {
                    return respond(&mut stream, 400, json!({"error": error.to_string()}))
                }
            };
            let spans = if let Some(array) = value.as_array() {
                array.clone()
            } else if let Some(array) = value.get("spans").and_then(Value::as_array) {
                array.clone()
            } else {
                return respond(
                    &mut stream,
                    400,
                    json!({"error": "body must be an array or {spans: [...]}"}),
                );
            };
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if ingest
                .send(Ingest {
                    spans,
                    reply: reply_tx,
                })
                .is_err()
            {
                return respond(
                    &mut stream,
                    503,
                    json!({"error": "ingest writer unavailable"}),
                );
            }
            match reply_rx.recv() {
                Ok(Ok(count)) => respond(&mut stream, 200, json!({"accepted": count})),
                Ok(Err(error)) => respond(&mut stream, 400, json!({"error": error})),
                Err(_) => respond(
                    &mut stream,
                    503,
                    json!({"error": "ingest writer unavailable"}),
                ),
            }
        }
        ("GET", "/v1/spans") => match query_spans(state, query) {
            Ok(spans) => respond(&mut stream, 200, Value::Array(spans)),
            Err(error) => respond(&mut stream, 400, json!({"error": error})),
        },
        ("GET", "/v1/stats") => {
            let locked = state.read().expect("state lock poisoned");
            respond(
                &mut stream,
                200,
                json!({
                    "span_count": locked.spans.len(),
                    "segment_count": locked.segments,
                    "bytes_on_disk": locked.bytes_on_disk
                }),
            )
        }
        ("GET", _) if path.starts_with("/v1/traces/") => {
            let id = percent_decode(&path[11..]);
            let locked = state.read().expect("state lock poisoned");
            let Some(indices) = locked.traces.get(&id) else {
                return respond(&mut stream, 404, json!({"error": "trace not found"}));
            };
            let mut spans: Vec<Value> = indices.iter().map(|&i| locked.spans[i].clone()).collect();
            spans.sort_by_key(start_timestamp);
            respond(&mut stream, 200, json!({"trace_id": id, "spans": spans}))
        }
        _ => respond(&mut stream, 404, json!({"error": "not found"})),
    }
}

struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 8192];
    let header_end;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete request",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid content-length"))?
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete body",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(Request {
        method,
        target,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn query_spans(state: &SharedState, raw_query: &str) -> Result<Vec<Value>, String> {
    let parameters = parse_query(raw_query);
    let service = parameters.get("service");
    let name = parameters.get("name");
    let min_duration_ns = parameters
        .get("min_duration_ms")
        .map(|v| v.parse::<f64>().map(|n| (n * 1_000_000.0) as u64))
        .transpose()
        .map_err(|_| "invalid min_duration_ms")?;
    let since = parameters
        .get("since")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| "invalid since")?;
    let until = parameters
        .get("until")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| "invalid until")?;
    let limit = parameters
        .get("limit")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid limit")?
        .unwrap_or(100);
    let attrs: Vec<(&str, &str)> = parameters
        .iter()
        .filter_map(|(key, value)| key.strip_prefix("attr.").map(|key| (key, value.as_str())))
        .collect();
    let locked = state.read().map_err(|_| "state lock poisoned")?;
    let mut candidates: Option<HashSet<usize>> = None;
    if let Some(value) = service {
        candidates = Some(
            locked
                .services
                .get(value)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        );
    }
    if let Some(value) = name {
        intersect(
            &mut candidates,
            locked.names.get(value).map(Vec::as_slice).unwrap_or(&[]),
        );
    }
    for (key, value) in &attrs {
        let encoded = query_attribute_value(value);
        intersect(
            &mut candidates,
            locked
                .attributes
                .get(&(key.to_string(), encoded))
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
    }
    let indices: Box<dyn Iterator<Item = usize>> = match candidates {
        Some(values) => Box::new(values.into_iter()),
        None => Box::new(0..locked.spans.len()),
    };
    let mut found = Vec::new();
    for index in indices {
        let span = &locked.spans[index];
        let start = start_timestamp(span);
        let end = end_timestamp(span);
        if since.is_some_and(|minimum| start < minimum)
            || until.is_some_and(|maximum| start > maximum)
            || min_duration_ns.is_some_and(|minimum| end.saturating_sub(start) < minimum)
        {
            continue;
        }
        found.push(span.clone());
    }
    found.sort_by_key(start_timestamp);
    found.truncate(limit);
    Ok(found)
}

fn intersect(current: &mut Option<HashSet<usize>>, values: &[usize]) {
    match current {
        Some(set) => set.retain(|index| values.binary_search(index).is_ok()),
        None => *current = Some(values.iter().copied().collect()),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                output.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        output.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn query_attribute_value(value: &str) -> String {
    serde_json::from_str::<Value>(value).map_or_else(
        |_| canonical(&Value::String(value.to_owned())),
        |value| canonical(&value),
    )
}

fn canonical(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn text<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span.get(key).and_then(Value::as_str)
}

fn number(span: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| {
            span.get(key)
                .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        })
        .unwrap_or(0)
}

fn start_timestamp(span: &Value) -> u64 {
    number(
        span,
        &[
            "start_time_unix_nano",
            "start_timestamp_ns",
            "start_ns",
            "start_time",
        ],
    )
}

fn end_timestamp(span: &Value) -> u64 {
    number(
        span,
        &[
            "end_time_unix_nano",
            "end_timestamp_ns",
            "end_ns",
            "end_time",
        ],
    )
}

fn respond(stream: &mut TcpStream, status: u16, body: Value) -> io::Result<()> {
    let encoded = serde_json::to_vec(&body)?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", encoded.len())?;
    stream.write_all(&encoded)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn expire_old_logs(data_dir: &Path, ttl_seconds: Option<u64>) -> io::Result<()> {
    let Some(ttl) = ttl_seconds else {
        return Ok(());
    };
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(ttl);
    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|v| v.to_str()) == Some("spans.log") {
            continue;
        }
        if path.extension().and_then(|v| v.to_str()) != Some("log") {
            continue;
        }
        let modified = entry
            .metadata()?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if modified < cutoff {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}
