//! The import state file: how far each transcript has been read and sent,
//! per destination.
//!
//! Keyed by destination because the same transcripts may be imported into a
//! server and into a scratch data directory; sharing one offset between them
//! would leave the second one empty. Written atomically (a temporary file
//! renamed over the old one), so a crash mid-write leaves the previous state,
//! which at worst re-sends spans the destination already holds — harmless,
//! because re-sending is idempotent.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Bytes of a file's head digested to notice a rewritten file.
const HEAD_BYTES: u64 = 4096;

/// The state file's format version.
const VERSION: u32 = 1;

/// One file's progress.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct FileState {
    /// Bytes read and sent: always the end of a complete line.
    pub(crate) offset: u64,
    /// SHA-256 of the file's first `head_len` bytes when last recorded.
    #[serde(default)]
    pub(crate) head: String,
    /// How many bytes `head` covers (the file may have been shorter than
    /// [`HEAD_BYTES`] then).
    #[serde(default)]
    pub(crate) head_len: u64,
    /// The run id the file's spans carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run: Option<String>,
    /// For a compressed transcript, its compressed size (`offset` is into
    /// the decompressed content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stored_size: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct Destination {
    #[serde(default)]
    files: BTreeMap<String, FileState>,
}

/// The whole state file.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct State {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    destinations: BTreeMap<String, Destination>,
}

impl State {
    /// Loads `path`, or an empty state when it does not exist yet.
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes).map_err(|error| {
                    format!(
                        "state file {} is not readable ({error}); move it aside or pass \
                         --no-state to send everything again",
                        path.display()
                    )
                })?;
                if state.version > VERSION {
                    return Err(format!(
                        "state file {} was written by a newer traza (version {}); pass a \
                         different --state",
                        path.display(),
                        state.version
                    ));
                }
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("state file {}: {error}", path.display())),
        }
    }

    /// Writes the state atomically.
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("state directory {}: {error}", parent.display()))?;
            }
        }
        let mut copy = self.clone();
        copy.version = VERSION;
        let bytes = serde_json::to_vec_pretty(&copy).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temporary, &bytes)
            .map_err(|error| format!("state file {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("state file {}: {error}", path.display()))
    }

    /// The recorded progress of `file` for `destination`.
    pub(crate) fn file(&self, destination: &str, file: &str) -> Option<&FileState> {
        self.destinations.get(destination)?.files.get(file)
    }

    /// Whether `path`'s head still matches what was recorded (or nothing
    /// was recorded).
    pub(crate) fn head_matches(&self, destination: &str, file: &str, path: &Path) -> bool {
        let Some(saved) = self.file(destination, file) else {
            return true;
        };
        if saved.head_len == 0 {
            return true;
        }
        head_digest(path, saved.head_len)
            .is_some_and(|(digest, len)| len == saved.head_len && digest == saved.head)
    }

    /// Records a compressed `file`'s progress: `offset` into its content,
    /// `stored_size` its size on disk.
    pub(crate) fn record_compressed(
        &mut self,
        destination: &str,
        file: &str,
        offset: u64,
        run: Option<String>,
        stored_size: u64,
    ) {
        let files = &mut self
            .destinations
            .entry(destination.to_owned())
            .or_default()
            .files;
        let entry = files.entry(file.to_owned()).or_default();
        entry.offset = offset;
        entry.head = String::new();
        entry.head_len = 0;
        entry.stored_size = Some(stored_size);
        if run.is_some() {
            entry.run = run;
        }
    }

    /// Records `file`'s progress.
    pub(crate) fn record(
        &mut self,
        destination: &str,
        file: &str,
        offset: u64,
        run: Option<String>,
        path: &Path,
    ) {
        let (head, head_len) = head_digest(path, HEAD_BYTES.min(offset)).unwrap_or_default();
        let files = &mut self
            .destinations
            .entry(destination.to_owned())
            .or_default()
            .files;
        let entry = files.entry(file.to_owned()).or_default();
        entry.offset = offset;
        entry.head = head;
        entry.head_len = head_len;
        if run.is_some() {
            entry.run = run;
        }
    }
}

/// SHA-256 of `path`'s first `len` bytes, and how many it actually had.
fn head_digest(path: &Path, len: u64) -> Option<(String, u64)> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut bytes).ok()?;
    Some((crate::payload::sha256_hex(&bytes), bytes.len() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "traza-import-state-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn progress_round_trips_per_destination_and_notices_a_rewrite() {
        let dir = scratch("roundtrip");
        let transcript = dir.join("t.jsonl");
        std::fs::write(&transcript, b"{\"a\":1}\n{\"b\":2}\n").expect("write");
        let mut state = State::default();
        state.record("url:a", "t", 16, Some("run-1".into()), &transcript);
        let path = dir.join("nested").join("state.json");
        state.save(&path).expect("save");
        let loaded = State::load(&path).expect("load");
        assert_eq!(loaded.file("url:a", "t").map(|f| f.offset), Some(16));
        assert_eq!(
            loaded.file("url:a", "t").and_then(|f| f.run.clone()),
            Some("run-1".to_owned())
        );
        assert!(
            loaded.file("url:b", "t").is_none(),
            "destinations are separate"
        );
        assert!(loaded.head_matches("url:a", "t", &transcript));

        // Appending keeps the head; rewriting the first bytes does not.
        std::fs::write(&transcript, b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").expect("append");
        assert!(loaded.head_matches("url:a", "t", &transcript));
        std::fs::write(&transcript, b"{\"z\":9}\n{\"b\":2}\n{\"c\":3}\n").expect("rewrite");
        assert!(!loaded.head_matches("url:a", "t", &transcript));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_state_file_is_an_error_not_a_silent_resend() {
        let dir = scratch("garbage");
        let path = dir.join("state.json");
        std::fs::write(&path, b"not json").expect("write");
        let error = State::load(&path).expect_err("refused");
        assert!(error.contains("--no-state"), "{error}");
        assert_eq!(
            State::load(&dir.join("absent.json")).expect("absent is empty"),
            State::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
