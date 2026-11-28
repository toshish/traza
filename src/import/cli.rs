//! The `traza-server import` and `traza-server blame` subcommands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::sink::{error_text, http_request, DrySink, HttpSink, HttpTarget, Sink, StoreSink};
use super::{run, summary_line, ImportOptions, Source};

/// The default server a client subcommand talks to.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// Longest blame query the CLI sends: repository roots past it are left out.
const MAX_QUERY_BYTES: usize = 32 * 1024;

/// Payload threshold for `--data-dir` imports: the server's own default, so
/// a store filled offline holds what one filled over HTTP would.
const DATA_DIR_PAYLOAD_THRESHOLD: usize = 256 * 1024;

const IMPORT_USAGE: &str = "Usage: traza-server import claude-code|codex [--path DIR] [--watch] \
[--url URL (default http://127.0.0.1:8080)] [--token TOKEN (default $TRAZA_TOKEN)] \
| [--data-dir DIR (ingest straight into a data directory no server holds)] | [--dry-run] \
[--tenant TENANT] [--since DURATION (e.g. 7d, 12h: only sessions active that recently)] \
[--project-filter SUBSTR (only sessions whose file path or working directory contains it)] \
[--state FILE (default ~/.traza/import-state.json) | --no-state (send everything)] \
[--interval-ms N (watch poll interval; default 1000)] [--json]";

const BLAME_USAGE: &str = "Usage: traza-server blame FILE:LINE [--url URL (default \
http://127.0.0.1:8080)] [--token TOKEN (default $TRAZA_TOKEN)] [--tenant TENANT] [--limit N] \
[--json] [--anywhere (search every project, not only this repository and its worktrees)]";

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
        let identity = loop {
            match whoami(&target) {
                Ok(identity) => break identity,
                // A watcher started before its server (at login, say) waits
                // for it rather than exiting.
                Err(Whoami::Unreachable(error)) if watch => {
                    eprintln!("traza import: {error}; retrying in 5 s");
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(Whoami::Unreachable(error) | Whoami::Refused(error)) => return Err(error),
            }
        };
        let effective = destination_tenant(&identity, tenant.as_deref())?;
        options.destination_key = format!("{}|tenant:{effective}", target.key());
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

/// What `GET /v1/whoami` said about the import's credential.
#[derive(Debug, PartialEq)]
struct Identity {
    /// The tenant the credential is bound to (`None`: unbound, or open mode).
    tenant: Option<String>,
    /// `ro`, `rw` or `admin`; `None` when the server did not say.
    scope: Option<String>,
}

/// Why the identity could not be had.
enum Whoami {
    /// The server could not be reached (or failed): a watcher waits.
    Unreachable(String),
    /// The server refused the credential, or answered something unusable.
    Refused(String),
}

/// Asks the server who the import's credential is, before anything is
/// read: a refused or read-only credential stops the import up front, and
/// the binding decides the state file's key.
fn whoami(target: &HttpTarget) -> Result<Identity, Whoami> {
    let (status, _, body) =
        http_request(target, "GET", "/v1/whoami", None).map_err(Whoami::Unreachable)?;
    match status {
        200 => {
            let answer: Value = serde_json::from_slice(&body).map_err(|error| {
                Whoami::Refused(format!("{}/v1/whoami answered {error}", target.key()))
            })?;
            Ok(Identity {
                tenant: answer
                    .get("tenant")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                scope: answer
                    .get("scope")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        401 | 403 => Err(Whoami::Refused(format!(
            "the server refused the credential ({status}): {}; pass --token (or TRAZA_TOKEN) \
             with rw scope",
            error_text(&body)
        ))),
        status if status >= 500 => Err(Whoami::Unreachable(format!(
            "{} answered {status}: {}",
            target.key(),
            error_text(&body)
        ))),
        status => Err(Whoami::Refused(format!(
            "{}/v1/whoami answered {status}: {}; is --url a Traza server?",
            target.key(),
            error_text(&body)
        ))),
    }
}

/// The tenant an HTTP import writes, which keys its progress in the state
/// file: a bound credential's own (so two credentials bound to different
/// tenants never share progress, and rotating a credential keeps it), else
/// `--tenant`, else the default tenant (`""`). A read-only credential, or a
/// `--tenant` a bound credential cannot write, is refused here — before a
/// single batch would be.
fn destination_tenant(identity: &Identity, flag: Option<&str>) -> Result<String, String> {
    if identity.scope.as_deref() == Some("ro") {
        return Err(
            "the credential is read-only; importing writes spans, so pass --token (or \
             TRAZA_TOKEN) with rw scope"
                .to_owned(),
        );
    }
    match (&identity.tenant, flag) {
        (Some(bound), Some(flag)) if bound != flag => Err(format!(
            "--tenant {flag} conflicts with the credential, which is bound to tenant {bound}; \
             drop --tenant (a bound credential stamps its own)"
        )),
        (Some(bound), _) => Ok(bound.clone()),
        (None, flag) => Ok(flag.unwrap_or_default().to_owned()),
    }
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
    let mut anywhere = false;
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
            "--anywhere" => anywhere = true,
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
    let (query_path, roots) = repository_scope(Path::new(file));
    let target = HttpTarget::parse(url.as_deref().unwrap_or(DEFAULT_URL), token)?;
    let mut query = format!(
        "/v1/blame?path={}&line={}",
        percent_encode(&query_path),
        percent_encode(line)
    );
    // Scoped to this repository (its checkout and worktrees) unless asked
    // otherwise: a repository-relative path alone matches the same path in
    // every project the store holds.
    if !anywhere {
        for root in &roots {
            let parameter = format!("&project={}", percent_encode(root));
            // Kept well inside the server's 64 KiB request head.
            if query.len() + parameter.len() > MAX_QUERY_BYTES {
                break;
            }
            query.push_str(&parameter);
        }
    }
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
    let scoped = answer
        .get("projects")
        .and_then(Value::as_array)
        .is_some_and(|projects| !projects.is_empty());
    if matches.is_empty() {
        let _ = writeln!(
            out,
            "  no recorded agent edit introduced this line {}({searched} candidate spans checked)",
            if scoped {
                "in this repository or its worktrees "
            } else {
                ""
            }
        );
        if scoped {
            let _ = writeln!(
                out,
                "  (--anywhere searches every project, e.g. sessions recorded at another path)"
            );
        }
        return out;
    }
    for (position, found) in matches.iter().enumerate() {
        // Every stored string is producer text: control characters are
        // neutralised so a transcript cannot drive the terminal.
        let text =
            |key: &str| crate::mcp::sanitize(found.get(key).and_then(Value::as_str).unwrap_or("?"));
        let at = found.get("at_ns").and_then(Value::as_u64).unwrap_or(0);
        // Unconfirmed matches follow every applied one.
        let unconfirmed = found.get("outcome").and_then(Value::as_str) == Some("unconfirmed");
        let lead = if unconfirmed {
            "attempted"
        } else if position == 0 {
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
        if unconfirmed {
            let _ = writeln!(
                out,
                "    no result was recorded for this call: the edit may not have applied"
            );
        }
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
        if !scoped {
            if let Some(project) = found.get("project").and_then(Value::as_str) {
                let _ = writeln!(out, "    project {}", crate::mcp::sanitize(project));
            }
        }
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

/// The path to send for `file`, and the repository roots to scope the
/// search to: the path relative to its checkout's root (the nearest ancestor
/// holding `.git`), with that root, the main checkout, the directories of
/// checkouts nested inside it (as `<dir>/*`) and every other worktree git
/// lists for the repository — a line written in any of them is the same line
/// of the same file. Outside a repository: the canonical absolute path and
/// no roots.
fn repository_scope(file: &Path) -> (String, Vec<String>) {
    let absolute = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    for dir in absolute.ancestors().skip(1) {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            if let Ok(relative) = absolute.strip_prefix(dir) {
                let mut roots = vec![slash(dir)];
                // The root as spelled on the command line too, when a
                // symlink (macOS's /tmp, say) makes it differ from the
                // canonical one: a transcript records the path the agent
                // was given.
                if let Some(given) = std::path::absolute(file)
                    .ok()
                    .and_then(|given| given_root(&given, relative))
                {
                    if !roots.contains(&given) {
                        roots.push(given);
                    }
                }
                let checkouts = repository_checkouts(&dot_git);
                let mut containers: Vec<String> = Vec::new();
                if let Some(main) = &checkouts.main {
                    if !roots.contains(main) {
                        roots.push(main.clone());
                    }
                    // Checkouts nested inside the repository — Claude
                    // Code's `.claude/worktrees/<name>`, or wherever the
                    // listed worktrees live inside it — stand for the
                    // repository even after they are removed and git
                    // forgets them, which is the usual fate of an agent's
                    // worktree.
                    containers.push(format!("{main}/.claude/worktrees"));
                    for linked in &checkouts.linked {
                        if let Some(parent) = linked.rsplit_once('/').map(|(parent, _)| parent) {
                            let inside = parent.len() > main.len()
                                && parent.starts_with(main.as_str())
                                && parent.as_bytes()[main.len()] == b'/';
                            if inside && !containers.iter().any(|known| known == parent) {
                                containers.push(parent.to_owned());
                            }
                        }
                    }
                }
                for container in &containers {
                    roots.push(format!("{container}/*"));
                }
                // Linked worktrees a container already covers need no entry.
                for linked in checkouts.linked {
                    let covered = containers.iter().any(|container| {
                        linked
                            .strip_prefix(container.as_str())
                            .and_then(|rest| rest.strip_prefix('/'))
                            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
                    });
                    if !covered && !roots.contains(&linked) {
                        roots.push(linked);
                    }
                }
                roots.truncate(crate::blame::MAX_BLAME_PROJECTS);
                return (slash(relative), roots);
            }
        }
    }
    (absolute.to_string_lossy().into_owned(), Vec::new())
}

/// A path as the forward-slashed text blame compares.
fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `given` with `relative` taken off its end, when it ends with it.
fn given_root(given: &Path, relative: &Path) -> Option<String> {
    let mut root = given.to_path_buf();
    for _ in relative.components() {
        if !root.pop() {
            return None;
        }
    }
    (root.join(relative) == given).then(|| slash(&root))
}

/// The checkouts git knows for one repository.
#[derive(Debug, Default)]
struct Checkouts {
    /// The main checkout (the directory whose `.git` is the common one).
    main: Option<String>,
    /// Each linked worktree, sorted.
    linked: Vec<String>,
}

/// Every checkout of the repository whose `.git` is `dot_git`: the main
/// checkout and each linked worktree (`<common>/worktrees/*/gitdir` names
/// the worktree's `.git` file). Read from git's own files — no `git` process
/// — and bounded.
fn repository_checkouts(dot_git: &Path) -> Checkouts {
    const MAX_WORKTREES: usize = 256;
    // A linked worktree's `.git` is a file: `gitdir: <common>/worktrees/<name>`.
    let common = if dot_git.is_file() {
        let Some(gitdir) = std::fs::read_to_string(dot_git).ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("gitdir:"))
                .map(|path| PathBuf::from(path.trim()))
        }) else {
            return Checkouts::default();
        };
        let gitdir = if gitdir.is_relative() {
            dot_git
                .parent()
                .map_or(gitdir.clone(), |dir| dir.join(&gitdir))
        } else {
            gitdir
        };
        let common = std::fs::read_to_string(gitdir.join("commondir"))
            .ok()
            .map(|text| PathBuf::from(text.trim()))
            .unwrap_or_else(|| PathBuf::from("../.."));
        let common = if common.is_relative() {
            gitdir.join(common)
        } else {
            common
        };
        common.canonicalize().unwrap_or(common)
    } else {
        dot_git.to_path_buf()
    };
    let mut checkouts = Checkouts::default();
    // The main checkout holds the common directory as its `.git`.
    if common.file_name().and_then(|name| name.to_str()) == Some(".git") {
        checkouts.main = common.parent().map(slash);
    }
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return checkouts;
    };
    for entry in entries.flatten().take(MAX_WORKTREES) {
        let Ok(text) = std::fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let worktree_git = PathBuf::from(text.trim());
        if let Some(root) = worktree_git.parent() {
            let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            checkouts.linked.push(slash(&root));
        }
    }
    checkouts.linked.sort();
    checkouts.linked.dedup();
    checkouts
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
    fn an_unconfirmed_match_is_printed_as_an_attempt() {
        let found = |outcome: &str, span: &str| {
            serde_json::json!({"outcome": outcome, "at_ns": 0, "tool": "Edit", "run": "r",
                               "service": "claude-code", "file": "/r/a.rs",
                               "trace_id": "t", "span_id": span})
        };
        let answer = serde_json::json!({"matches": [found("applied", "s1"),
                                                    found("unconfirmed", "s2")],
                                        "searched_spans": 2});
        let text = render_blame("a.rs", 1, "x", &answer);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with("  introduced "), "{text}");
        let attempt = lines
            .iter()
            .position(|line| line.starts_with("  attempted "))
            .expect("the unconfirmed match");
        assert!(
            lines[attempt + 1].contains("no result was recorded"),
            "{text}"
        );
        assert!(!text.contains("also introduced"), "{text}");
    }

    #[test]
    fn durations_parse_and_garbage_is_refused() {
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("1y").is_err());
    }

    #[test]
    fn a_file_in_a_linked_worktree_is_scoped_to_every_checkout_of_its_repository() {
        let base = std::env::temp_dir().join(format!(
            "traza-blame-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let main = base.join("main");
        let linked = base.join("linked");
        std::fs::create_dir_all(main.join(".git/worktrees/linked")).expect("main");
        std::fs::create_dir_all(linked.join("src")).expect("linked");
        let main = main.canonicalize().expect("canonical");
        let linked = linked.canonicalize().expect("canonical");
        // git's layout: the worktree's `.git` file names its admin
        // directory, which names the common directory and the worktree.
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/linked").display()),
        )
        .expect("worktree .git");
        std::fs::write(main.join(".git/worktrees/linked/commondir"), "../..\n").expect("commondir");
        std::fs::write(
            main.join(".git/worktrees/linked/gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .expect("gitdir");
        let file = linked.join("src/lib.rs");
        std::fs::write(&file, "x\n").expect("file");
        let (path, roots) = repository_scope(&file);
        assert_eq!(path, "src/lib.rs");
        assert!(roots.contains(&slash(&linked)), "{roots:?}");
        assert!(
            roots.contains(&slash(&main)),
            "the main checkout of the same repository: {roots:?}"
        );
        assert!(
            roots.contains(&format!("{}/.claude/worktrees/*", slash(&main))),
            "Claude Code's worktrees, removed ones included: {roots:?}"
        );
        assert_eq!(
            given_root(Path::new("/a/b/src/lib.rs"), Path::new("src/lib.rs")),
            Some("/a/b".to_owned())
        );
        assert_eq!(
            given_root(Path::new("/a/b/x.rs"), Path::new("src/x.rs")),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_bound_credential_decides_the_destination_tenant() {
        let bound = Identity {
            tenant: Some("acme".into()),
            scope: Some("rw".into()),
        };
        assert_eq!(destination_tenant(&bound, None), Ok("acme".to_owned()));
        assert_eq!(
            destination_tenant(&bound, Some("acme")),
            Ok("acme".to_owned())
        );
        assert!(destination_tenant(&bound, Some("beta"))
            .is_err_and(|error| error.contains("conflicts with the credential")));
        let unbound = Identity {
            tenant: None,
            scope: Some("admin".into()),
        };
        assert_eq!(destination_tenant(&unbound, None), Ok(String::new()));
        assert_eq!(
            destination_tenant(&unbound, Some("beta")),
            Ok("beta".to_owned())
        );
        let reader = Identity {
            tenant: None,
            scope: Some("ro".into()),
        };
        assert!(destination_tenant(&reader, None).is_err_and(|error| error.contains("read-only")));
    }

    #[test]
    fn percent_encoding_keeps_paths_readable_and_escapes_the_rest() {
        assert_eq!(percent_encode("src/a b.rs"), "src/a%20b.rs");
        assert_eq!(percent_encode("x=1&y"), "x%3D1%26y");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
