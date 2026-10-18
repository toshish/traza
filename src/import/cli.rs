//! The `traza-server import` and `traza-server blame` subcommands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::sink::{error_text, http_request, DrySink, HttpSink, HttpTarget, Sink, StoreSink};
use super::{run, summary_line, ImportOptions, Source};

/// The default server a client subcommand talks to.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// Payload threshold for `--data-dir` imports: the server's own default, so
/// a store filled offline holds what one filled over HTTP would.
const DATA_DIR_PAYLOAD_THRESHOLD: usize = 256 * 1024;

const IMPORT_USAGE: &str = "Usage: traza-server import claude-code|codex [--path DIR] [--watch] \
[--url URL (default http://127.0.0.1:8080)] [--token TOKEN (default $TRAZA_TOKEN)] \
| [--data-dir DIR (ingest straight into a data directory no server holds)] | [--dry-run] \
[--tenant TENANT] [--since DURATION (e.g. 7d, 12h: only files modified that recently)] \
[--project-filter SUBSTR (only sessions whose file path or working directory contains it)] \
[--state FILE (default ~/.traza/import-state.json) | --no-state (send everything)] \
[--interval-ms N (watch poll interval; default 1000)] [--json]";

const BLAME_USAGE: &str = "Usage: traza-server blame FILE:LINE [--url URL (default \
http://127.0.0.1:8080)] [--token TOKEN (default $TRAZA_TOKEN)] [--tenant TENANT] [--limit N] \
[--json]";

/// Runs `traza-server import ARGS…`. Returns the process exit code.
pub fn import_main(args: &[String]) -> Result<i32, String> {
    let Some(first) = args.first() else {
        return Err(IMPORT_USAGE.to_owned());
    };
    if first == "--help" || first == "-h" {
        println!("{IMPORT_USAGE}");
        return Ok(0);
    }
    let source = Source::parse(first).ok_or_else(|| {
        format!("unknown source {first:?}: expected claude-code or codex\n{IMPORT_USAGE}")
    })?;
    let mut path = None;
    let mut watch = false;
    let mut url = None;
    let mut token = std::env::var("TRAZA_TOKEN").ok().filter(|t| !t.is_empty());
    let mut data_dir: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut tenant = None;
    let mut since = None;
    let mut project_filter = None;
    let mut state: Option<Option<PathBuf>> = None;
    let mut interval = Duration::from_millis(1000);
    let mut as_json = false;
    let mut index = 1;
    let value = |index: usize, flag: &str| -> Result<String, String> {
        args.get(index)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value\n{IMPORT_USAGE}"))
    };
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--path" => {
                index += 1;
                path = Some(PathBuf::from(value(index, flag)?));
            }
            "--watch" => watch = true,
            "--url" => {
                index += 1;
                url = Some(value(index, flag)?);
            }
            "--token" => {
                index += 1;
                token = Some(value(index, flag)?);
            }
            "--data-dir" => {
                index += 1;
                data_dir = Some(PathBuf::from(value(index, flag)?));
            }
            "--dry-run" => dry_run = true,
            "--tenant" => {
                index += 1;
                let name = value(index, flag)?;
                if !crate::valid_tenant(&name) {
                    return Err(format!(
                        "--tenant {name:?} is not a valid tenant ([a-z0-9][a-z0-9._-]{{0,63}})"
                    ));
                }
                tenant = Some(name);
            }
            "--since" => {
                index += 1;
                since = Some(parse_duration(&value(index, flag)?)?);
            }
            "--project-filter" => {
                index += 1;
                project_filter = Some(value(index, flag)?);
            }
            "--state" => {
                index += 1;
                state = Some(Some(PathBuf::from(value(index, flag)?)));
            }
            "--no-state" => state = Some(None),
            "--interval-ms" => {
                index += 1;
                let millis: u64 = value(index, flag)?
                    .parse()
                    .map_err(|_| "--interval-ms must be a number of milliseconds".to_owned())?;
                interval = Duration::from_millis(millis.clamp(50, 60_000));
            }
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{IMPORT_USAGE}");
                return Ok(0);
            }
            other => return Err(format!("unknown argument: {other}\n{IMPORT_USAGE}")),
        }
        index += 1;
    }
    let destinations =
        usize::from(url.is_some()) + usize::from(data_dir.is_some()) + usize::from(dry_run);
    if destinations > 1 {
        return Err("pass one of --url, --data-dir or --dry-run".to_owned());
    }
    let home = home_dir();
    let root = match path {
        Some(path) => path,
        None => source.default_root(
            home.as_deref()
                .ok_or("HOME is not set; pass --path to say where the transcripts are")?,
        ),
    };
    if !root.is_dir() {
        return Err(format!(
            "{} is not a directory; pass --path to say where the {} transcripts are",
            root.display(),
            source.as_str()
        ));
    }
    // A dry run sends nothing, so recording offsets would make the next real
    // import skip what it never sent.
    let state = if dry_run {
        None
    } else {
        match state {
            Some(choice) => choice,
            None => Some(
                home.as_deref()
                    .ok_or("HOME is not set; pass --state FILE or --no-state")?
                    .join(".traza")
                    .join("import-state.json"),
            ),
        }
    };
    let mut log = std::io::stderr();
    let mut options = ImportOptions {
        source,
        root,
        tenant: tenant.clone(),
        since,
        project_filter,
        state,
        destination_key: String::new(),
        watch,
        interval,
    };
    let never = || false;
    let stats = if dry_run {
        options.destination_key = "dry-run".to_owned();
        run(&options, &mut DrySink, &mut log, &never)?
    } else if let Some(dir) = data_dir {
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("--data-dir {}: {error}", dir.display()))?;
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        options.destination_key = format!(
            "dir:{}|tenant:{}",
            canonical.display(),
            tenant.as_deref().unwrap_or_default()
        );
        let config = crate::Config {
            payload_threshold: Some(DATA_DIR_PAYLOAD_THRESHOLD),
            ..crate::Config::default()
        };
        let store = crate::Store::open(&dir, config).map_err(|error| {
            format!(
                "cannot open --data-dir {} ({error}); if a server is running on it, import \
                 with --url instead",
                dir.display()
            )
        })?;
        let mut sink = StoreSink::new(store);
        let stats = run(&options, &mut sink, &mut log, &never);
        sink.finish()?;
        stats?
    } else {
        let target = HttpTarget::parse(url.as_deref().unwrap_or(DEFAULT_URL), token)?;
        options.destination_key = format!(
            "{}|tenant:{}",
            target.key(),
            tenant.as_deref().unwrap_or_default()
        );
        let mut sink = HttpSink::new(target);
        run(&options, &mut sink as &mut dyn Sink, &mut log, &never)?
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&stats).map_err(|error| error.to_string())?
        );
    } else {
        println!("{}", summary_line(&stats, false));
        for error in &stats.errors {
            println!("  error: {error}");
        }
    }
    Ok(if stats.spans_refused > 0 || stats.files_failed > 0 {
        3
    } else {
        0
    })
}

/// Runs `traza-server blame FILE:LINE …`. Exit code 0 when an agent edit
/// introduced the line, 1 when none did.
pub fn blame_main(args: &[String]) -> Result<i32, String> {
    let mut target_arg = None;
    let mut url = None;
    let mut token = std::env::var("TRAZA_TOKEN").ok().filter(|t| !t.is_empty());
    let mut tenant = None;
    let mut limit = None;
    let mut as_json = false;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || -> Result<String, String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value\n{BLAME_USAGE}"))
        };
        match flag {
            "--url" => url = Some(value()?),
            "--token" => token = Some(value()?),
            "--tenant" => tenant = Some(value()?),
            "--limit" => {
                limit = Some(
                    value()?
                        .parse::<usize>()
                        .map_err(|_| "--limit must be a positive integer".to_owned())?,
                )
            }
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{BLAME_USAGE}");
                return Ok(0);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown argument: {other}\n{BLAME_USAGE}"))
            }
            other => target_arg = Some(other.to_owned()),
        }
        index += 1;
    }
    let target_arg = target_arg.ok_or_else(|| BLAME_USAGE.to_owned())?;
    let (file, number) = target_arg
        .rsplit_once(':')
        .ok_or_else(|| format!("expected FILE:LINE, got {target_arg:?}\n{BLAME_USAGE}"))?;
    let number: usize = number
        .parse()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("line number must be a positive integer, got {number:?}"))?;
    let text = std::fs::read_to_string(file).map_err(|error| format!("{file}: {error}"))?;
    let line = text.lines().nth(number - 1).ok_or_else(|| {
        format!(
            "{file} has {} lines; there is no line {number}",
            text.lines().count()
        )
    })?;
    if line.trim().is_empty() {
        return Err(format!(
            "{file}:{number} is blank; blame a line with text on it"
        ));
    }
    let query_path = repository_relative(Path::new(file));
    let target = HttpTarget::parse(url.as_deref().unwrap_or(DEFAULT_URL), token)?;
    let mut query = format!(
        "/v1/blame?path={}&line={}",
        percent_encode(&query_path),
        percent_encode(line)
    );
    if let Some(limit) = limit {
        query.push_str(&format!("&limit={limit}"));
    }
    if let Some(tenant) = &tenant {
        query.push_str(&format!("&tenant={}", percent_encode(tenant)));
    }
    let (status, _, body) = http_request(&target, "GET", &query, None)?;
    if status != 200 {
        return Err(format!(
            "the server answered {status}: {}",
            error_text(&body)
        ));
    }
    let answer: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    let found = answer
        .get("matches")
        .and_then(Value::as_array)
        .is_some_and(|matches| !matches.is_empty());
    if as_json {
        println!("{answer}");
    } else {
        print!("{}", render_blame(file, number, line, &answer));
    }
    Ok(if found { 0 } else { 1 })
}

/// The human rendering of a blame answer.
fn render_blame(file: &str, number: usize, line: &str, answer: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{file}:{number}  {}", line.trim());
    let matches = answer
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let searched = answer
        .get("searched_spans")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if matches.is_empty() {
        let _ = writeln!(
            out,
            "  no recorded agent edit introduced this line ({searched} candidate spans checked)"
        );
        return out;
    }
    for (position, found) in matches.iter().enumerate() {
        // Every stored string is producer text: control characters are
        // neutralised so a transcript cannot drive the terminal.
        let text =
            |key: &str| crate::mcp::sanitize(found.get(key).and_then(Value::as_str).unwrap_or("?"));
        let at = found.get("at_ns").and_then(Value::as_u64).unwrap_or(0);
        let lead = if position == 0 {
            "introduced"
        } else {
            "also introduced"
        };
        let step = found
            .get("step_index")
            .and_then(Value::as_u64)
            .map_or(String::new(), |step| format!(", step {step}"));
        let model = found
            .get("model")
            .and_then(Value::as_str)
            .map_or(String::new(), |model| {
                format!(", {}", crate::mcp::sanitize(model))
            });
        let agent = found
            .get("agent")
            .and_then(Value::as_str)
            .map_or(String::new(), |agent| {
                format!(" (sub-agent {})", crate::mcp::sanitize(agent))
            });
        let _ = writeln!(
            out,
            "  {lead} {} by {} in run {}{agent}  [{}{model}{step}]",
            crate::mcp::rfc3339(at),
            text("tool"),
            text("run"),
            text("service"),
        );
        if let Some(prompt) = found.get("prompt").and_then(Value::as_str) {
            let _ = writeln!(out, "    prompt: {}", crate::mcp::sanitize(prompt));
        }
        let _ = writeln!(
            out,
            "    file {}  trace {}  span {}",
            text("file"),
            text("trace_id"),
            text("span_id")
        );
    }
    if answer.get("truncated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            out,
            "  (truncated: more candidates existed than one search verifies; an earlier \
             introduction may exist)"
        );
    }
    out
}

/// The path to send for `file`: relative to its repository root (the nearest
/// ancestor holding `.git`), so edits made in another checkout of the same
/// repository match; else the canonical absolute path.
fn repository_relative(file: &Path) -> String {
    let absolute = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    for dir in absolute.ancestors().skip(1) {
        if dir.join(".git").exists() {
            if let Ok(relative) = absolute.strip_prefix(dir) {
                return relative.to_string_lossy().replace('\\', "/");
            }
        }
    }
    absolute.to_string_lossy().into_owned()
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `7d`, `12h`, `30m`, `90s`, `2w`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => 0,
    };
    match number.parse::<u64>() {
        Ok(count) if seconds > 0 => Ok(Duration::from_secs(count.saturating_mul(seconds))),
        _ => Err(format!(
            "--since takes a duration like 30m, 12h, 7d or 2w (got {text:?})"
        )),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_garbage_is_refused() {
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("1y").is_err());
    }

    #[test]
    fn percent_encoding_keeps_paths_readable_and_escapes_the_rest() {
        assert_eq!(percent_encode("src/a b.rs"), "src/a%20b.rs");
        assert_eq!(percent_encode("x=1&y"), "x%3D1%26y");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
