//! Static serving of the built dashboard from a directory on disk.
//!
//! The trace browser is a standalone React app in `ui/`; `npm run build`
//! emits a self-contained `ui/dist`. The server serves that directory when
//! `--ui-dir` points at it — nothing is compiled into the binary, so building
//! `traza-server` still needs no Node toolchain, and a rebuilt UI is picked up
//! without rebuilding the server.
//!
//! `GET /`, `/dashboard`, and `/dashboard/` serve `index.html`; any other path
//! maps to a file beneath the root. The SHELL is served before the auth gate —
//! it carries no data, and every `/v1` call the page makes stays gated — so
//! the page can load and prompt for a bearer token on its first 401.
//!
//! Path handling is the security-sensitive part: request paths are matched
//! against the CANONICALIZED root, so `..` segments, absolute paths, and
//! symlinks that escape the root are refused rather than served.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// A file resolved beneath the UI root, ready to write to a response.
pub struct UiFile {
    /// File contents.
    pub bytes: Vec<u8>,
    /// Value for the `Content-Type` header.
    pub content_type: &'static str,
}

/// A directory of built UI assets (the Vite `dist` output).
#[derive(Clone, Debug)]
pub struct UiRoot {
    root: PathBuf,
    searched: Vec<PathBuf>,
}

impl UiRoot {
    /// Binds to `directory`, resolved against the working directory so logs
    /// and errors name the ABSOLUTE path that was searched. The default
    /// `./ui/dist` is relative, and "no dashboard at ./ui/dist" does not tell
    /// an operator which `./` the server meant. Existence is still checked per
    /// request, so a UI built after startup is served without a restart.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let root = directory.into();
        let root = if root.is_absolute() {
            root
        } else {
            // Drop `.` segments so the logged path reads as a real location
            // rather than "/private/tmp/./ui/dist".
            let trimmed: PathBuf = root
                .components()
                .filter(|part| !matches!(part, Component::CurDir))
                .collect();
            std::env::current_dir().map_or(root.clone(), |cwd| cwd.join(&trimmed))
        };
        Self {
            root,
            searched: Vec::new(),
        }
    }

    /// Locates the built dashboard for a server that was given no explicit
    /// `--ui-dir`.
    ///
    /// A CWD-relative default alone only works when the server is launched
    /// from a checkout: a `cargo install`ed or otherwise packaged binary has
    /// no `./ui/dist` and would serve nothing. So the search also looks
    /// BESIDE THE EXECUTABLE, which is where a package can put the build:
    ///
    /// 1. `TRAZA_UI_DIR`, for operators who place it anywhere else;
    /// 2. `<exe dir>/ui`, the packaging convention;
    /// 3. `<exe dir>/../share/traza/ui`, the Unix prefix convention;
    /// 4. `./ui/dist`, the working copy after `npm run build`.
    ///
    /// Returns the first candidate that holds an `index.html`, else the last
    /// one, so diagnostics name a concrete path. [`Self::searched`] lists
    /// everything tried, which is what a "no dashboard" message should show.
    pub fn discover() -> Self {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(configured) = std::env::var_os("TRAZA_UI_DIR") {
            candidates.push(PathBuf::from(configured));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("ui"));
                candidates.push(dir.join("../share/traza/ui"));
            }
        }
        candidates.push(PathBuf::from("./ui/dist"));

        let mut searched = Vec::with_capacity(candidates.len());
        let mut found: Option<UiRoot> = None;
        for candidate in candidates {
            let root = Self::new(candidate);
            searched.push(root.root.clone());
            if found.is_none() && root.is_available() {
                found = Some(root);
            }
        }
        let mut root = found.unwrap_or_else(|| {
            Self::new(
                searched
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "./ui/dist".into()),
            )
        });
        root.searched = searched;
        root
    }

    /// The configured root directory.
    pub fn directory(&self) -> &Path {
        &self.root
    }

    /// Every directory [`Self::discover`] looked in, for diagnostics. Empty
    /// when the root was configured explicitly.
    pub fn searched(&self) -> &[PathBuf] {
        &self.searched
    }

    /// True when the root exists and holds an `index.html` to serve.
    pub fn is_available(&self) -> bool {
        self.root.join("index.html").is_file()
    }

    /// Resolves a request path to a file beneath the root, or `None` when the
    /// path is not a UI route, escapes the root, or does not exist.
    ///
    /// `path` is the decoded URL path with any query string already removed.
    pub fn resolve(&self, path: &str) -> Option<UiFile> {
        let relative = match path {
            "/" | "/dashboard" | "/dashboard/" => "index.html",
            // The API owns /v1; never let a file shadow it.
            other if other.starts_with("/v1/") || other == "/v1" => return None,
            other => other.strip_prefix('/')?,
        };
        if relative.is_empty() {
            return None;
        }
        // Reject anything that is not a plain relative chain of names before
        // touching the filesystem: no `..`, no root/prefix components.
        let candidate = Path::new(relative);
        if !candidate
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        let root = fs::canonicalize(&self.root).ok()?;
        let target = fs::canonicalize(root.join(candidate)).ok()?;
        // Canonicalization resolves symlinks; the result must still be inside
        // the root, or a link inside dist could hand out arbitrary files.
        if !target.starts_with(&root) || !target.is_file() {
            return None;
        }
        let bytes = fs::read(&target).ok()?;
        Some(UiFile {
            bytes,
            content_type: content_type_for(&target),
        })
    }
}

/// Content type for a built asset, by extension. Unknown types are served as
/// `application/octet-stream` rather than being guessed.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("traza-ui-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn serves_index_for_the_shell_routes() {
        let dir = temp_root("index");
        fs::write(
            dir.join("index.html"),
            "<!doctype html><title>Traza</title>",
        )
        .expect("write");
        let ui = UiRoot::new(&dir);
        assert!(ui.is_available());
        for route in ["/", "/dashboard", "/dashboard/"] {
            let file = ui.resolve(route).expect("index served");
            assert_eq!(file.content_type, "text/html; charset=utf-8");
            assert!(String::from_utf8_lossy(&file.bytes).contains("<title>Traza</title>"));
        }
    }

    #[test]
    fn serves_assets_with_their_content_type() {
        let dir = temp_root("assets");
        fs::write(dir.join("index.html"), "x").expect("write");
        fs::create_dir_all(dir.join("assets")).expect("dir");
        fs::write(dir.join("assets/app.js"), "console.log(1)").expect("write");
        fs::write(dir.join("assets/f.woff2"), [0_u8, 1, 2]).expect("write");
        let ui = UiRoot::new(&dir);
        assert_eq!(
            ui.resolve("/assets/app.js").expect("js").content_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            ui.resolve("/assets/f.woff2").expect("font").content_type,
            "font/woff2"
        );
    }

    #[test]
    fn refuses_traversal_and_absolute_paths() {
        let dir = temp_root("traversal");
        fs::write(dir.join("index.html"), "x").expect("write");
        let secret = dir.parent().expect("parent").join("traza-ui-secret.txt");
        fs::write(&secret, "top secret").expect("write");
        let ui = UiRoot::new(&dir);
        for attack in [
            "/../traza-ui-secret.txt",
            "/assets/../../traza-ui-secret.txt",
            "/./../traza-ui-secret.txt",
            "//etc/passwd",
            "/etc/passwd",
        ] {
            assert!(
                ui.resolve(attack).is_none(),
                "must refuse to serve {attack}"
            );
        }
        let _ = fs::remove_file(&secret);
    }

    #[test]
    fn never_shadows_the_api_or_missing_files() {
        let dir = temp_root("api");
        fs::write(dir.join("index.html"), "x").expect("write");
        fs::create_dir_all(dir.join("v1")).expect("dir");
        fs::write(dir.join("v1/spans"), "not the api").expect("write");
        let ui = UiRoot::new(&dir);
        assert!(ui.resolve("/v1/spans").is_none(), "API owns /v1");
        assert!(ui.resolve("/v1").is_none());
        assert!(ui.resolve("/nope.js").is_none(), "missing file");
        assert!(ui.resolve("/assets").is_none(), "a directory is not a file");
    }

    #[test]
    fn reports_unavailable_without_a_built_ui() {
        let dir = temp_root("empty");
        let ui = UiRoot::new(&dir);
        assert!(!ui.is_available());
        assert!(ui.resolve("/").is_none());
        assert!(UiRoot::new(dir.join("absent")).resolve("/").is_none());
    }
}
