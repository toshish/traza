//! Cache Autopsy: why a model call's prompt cache did or did not hold.
//!
//! A provider's prompt cache serves the longest previously-seen PREFIX of a
//! prompt, matched byte for byte, for as long as the cached entry lives. So
//! one call is compared with the call before it in the same conversation —
//! its *predecessor* — chunk by chunk from the start, on each chunk's exact
//! identity (role, kind, call id and text as recorded). The first chunk that
//! differs is where the cache stopped serving; what kind of difference it is
//! names the cause.
//!
//! **The predecessor** is the nearest earlier model call of the same tenant
//! whose context shares at least half of its chunks with this one, measured
//! either way round (so a compaction that kept the system prompt and replaced
//! the history still finds the call it compacted). The first chunks of the
//! two contexts also count as shared when they are near-identical — the same
//! kind, differing in a short span — so a system prompt that embeds the time
//! of day does not hide the conversation it belongs to. Calls of other agents
//! interleaved in the same run share little with each other and are skipped,
//! which is what keeps a multi-agent run's autopsy per conversation.
//!
//! Everything here is computed from recorded text. What it cannot see, it
//! says: the provider's own rules (minimum cacheable length, where cache
//! breakpoints sit, TTL refreshes on a hit) are not modelled, so the reported
//! `cache_read` is shown beside every verdict and a disagreement between the
//! two is counted rather than explained away.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::chunk::{clip, ChunkKind, Piece};

/// Characters each side of a divergence diff shows at most.
pub(crate) const DIFF_CHARS: usize = 80;

/// How far a diff is widened, each way, to show the whole volatile token.
const WIDEN_CHARS: usize = 40;

/// Of what followed the first divergent chunk, the share that must still be
/// present for the change to read as an edit rather than a rewrite.
const SURVIVAL_FOR_EDIT: f64 = 0.5;

/// Why a call's cached prefix held or broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The predecessor's whole context is an unchanged prefix of this one,
    /// within the TTL.
    Stable,
    /// Content inside the prefix changed: a chunk edited in place, or a new
    /// chunk inserted ahead of content the predecessor already had.
    VolatileContent,
    /// The same chunks in a different order, or the same content serialized
    /// into different bytes.
    Reordered,
    /// The prefix was intact, but the TTL had passed since the predecessor.
    TtlExpired,
    /// No predecessor (the first call of a conversation), or the context was
    /// rewritten: chunks removed from inside the prefix, or most of it gone.
    NewContext,
    /// The comparison cannot be made, or its outcome is not knowable from
    /// the trace.
    Unknown,
}

impl Cause {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::VolatileContent => "volatile_content",
            Self::Reordered => "reordered",
            Self::TtlExpired => "ttl_expired",
            Self::NewContext => "new_context",
            Self::Unknown => "unknown",
        }
    }
}

/// What happened at the first divergent chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// The chunk was edited in place; the rest of the context survives.
    Edited,
    /// A new chunk sits where the predecessor's chunk was, and that chunk
    /// follows it.
    Inserted,
    /// The predecessor's chunk is gone and what followed it moved up.
    Removed,
    /// Both chunks exist in both contexts, in other places.
    Moved,
    /// The same content, different bytes (key order, spacing).
    Reserialized,
    /// Most of what followed is gone: compaction or truncation.
    Rewritten,
    /// This context is a strict prefix of the predecessor's.
    Shrunk,
}

/// The kind of token a volatile edit swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Volatile {
    /// A date, a time of day, or an epoch number.
    Timestamp,
    /// A UUID.
    Uuid,
    /// A run of hex digits: a request id, a hash.
    HexId,
    /// A number: a counter, a balance.
    Number,
    /// Anything else.
    Text,
}

/// The first differing stretch of two chunks' text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ByteDiff {
    /// Byte offset into the chunk's text where the shown difference starts.
    pub offset: usize,
    /// What the predecessor had there (at most 80 characters).
    pub before: String,
    /// What this call has there (at most 80 characters).
    pub after: String,
}

/// Where and how a call's prefix diverged from its predecessor's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Divergence {
    /// The chunk position (in this call's context) of the first difference.
    pub index: usize,
    /// The role there: this call's chunk, or the predecessor's when this
    /// context ended first.
    pub role: Option<String>,
    /// The kind there, under the same rule.
    pub kind: ChunkKind,
    /// What happened at that position.
    pub change: Change,
    /// The first differing stretch of text; `null` when a text was too large
    /// to keep for comparison.
    pub diff: Option<ByteDiff>,
    /// For an in-place edit, what kind of token was swapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volatile: Option<Volatile>,
}

/// One chunk as the autopsy keeps it: identities and size, text by key.
#[derive(Clone, Debug)]
pub(crate) struct Kept {
    pub(crate) content: u128,
    pub(crate) exact: u128,
    pub(crate) est_tokens: u64,
    pub(crate) kind: ChunkKind,
    pub(crate) role: Option<String>,
}

/// One model call's context, as the predecessor search and the comparison
/// need it.
#[derive(Clone, Debug)]
pub(crate) struct Context {
    /// Tape index of the call.
    pub(crate) step_index: usize,
    /// The span's tenant, so contexts of two tenants never pair.
    pub(crate) tenant: String,
    pub(crate) start_ns: u64,
    pub(crate) chunks: Vec<Kept>,
    /// Content digests present.
    pub(crate) contents: HashSet<u128>,
    /// Why the context could not be read, when it could not.
    pub(crate) unavailable: Option<&'static str>,
}

impl Context {
    pub(crate) fn new(
        step_index: usize,
        tenant: &str,
        start_ns: u64,
        pieces: &[Piece],
        unavailable: Option<&'static str>,
    ) -> Self {
        let chunks: Vec<Kept> = pieces
            .iter()
            .map(|piece| Kept {
                content: piece.content,
                exact: piece.exact,
                est_tokens: piece.est_tokens(),
                kind: piece.kind,
                role: piece.role.clone(),
            })
            .collect();
        let contents = chunks.iter().map(|chunk| chunk.content).collect();
        Self {
            step_index,
            tenant: tenant.to_owned(),
            start_ns,
            chunks,
            contents,
            unavailable,
        }
    }

    pub(crate) fn est_tokens(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.est_tokens).sum()
    }
}

/// Chunk text kept for diffs and previews, by exact digest, up to a byte
/// ceiling: a run's contexts repeat each other, so keying by identity stores
/// each distinct text once.
pub(crate) struct TextPool {
    texts: HashMap<u128, String>,
    bytes: usize,
    max_bytes: usize,
    /// Whether a text was refused for want of room.
    pub(crate) overflowed: bool,
}

impl TextPool {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            texts: HashMap::new(),
            bytes: 0,
            max_bytes,
            overflowed: false,
        }
    }

    pub(crate) fn keep(&mut self, piece: &Piece) {
        if self.texts.contains_key(&piece.exact) {
            return;
        }
        if self.bytes.saturating_add(piece.text.len()) > self.max_bytes {
            self.overflowed = true;
            return;
        }
        self.bytes += piece.text.len();
        self.texts.insert(piece.exact, piece.text.clone());
    }

    pub(crate) fn get(&self, exact: u128) -> Option<&str> {
        self.texts.get(&exact).map(String::as_str)
    }
}

/// The predecessor of `current` among `recent` (oldest first), and how many
/// of the predecessor's chunks `current` still holds — or `Err` with the
/// step index and the reason when a nearer candidate of the tenant could not
/// be read, so no honest pairing exists.
pub(crate) fn predecessor<'r>(
    current: &Context,
    recent: impl DoubleEndedIterator<Item = &'r Context>,
    pool: &TextPool,
) -> Result<Option<(&'r Context, usize)>, (usize, &'static str)> {
    if current.chunks.is_empty() {
        return Ok(None);
    }
    for candidate in recent.rev() {
        if candidate.tenant != current.tenant || candidate.step_index >= current.step_index {
            continue;
        }
        if let Some(reason) = candidate.unavailable {
            return Err((candidate.step_index, reason));
        }
        if candidate.chunks.is_empty() {
            continue;
        }
        let mut shared = candidate
            .chunks
            .iter()
            .filter(|chunk| current.contents.contains(&chunk.content))
            .count();
        if !current.contents.contains(&candidate.chunks[0].content)
            && near_identical(&candidate.chunks[0], &current.chunks[0], pool)
        {
            shared += 1;
        }
        let of_current = shared as f64 / current.chunks.len() as f64;
        let of_candidate = shared as f64 / candidate.chunks.len() as f64;
        if shared > 0 && (of_current >= 0.5 || of_candidate >= 0.5) {
            return Ok(Some((candidate, shared)));
        }
    }
    Ok(None)
}

/// Same kind, and the texts differ only in a stretch at most a tenth of the
/// longer one: the shape of a volatile token inside a stable chunk.
fn near_identical(left: &Kept, right: &Kept, pool: &TextPool) -> bool {
    if left.kind != right.kind || left.role != right.role {
        return false;
    }
    let (Some(a), Some(b)) = (pool.get(left.exact), pool.get(right.exact)) else {
        return false;
    };
    similar(a, b)
}

/// Whether two texts differ only in a stretch at most a tenth of the longer
/// one — an edit, rather than different content.
pub(crate) fn similar(a: &str, b: &str) -> bool {
    let longer = a.len().max(b.len());
    if longer == 0 {
        return true;
    }
    let (prefix, suffix) = common_ends(a, b);
    (prefix + suffix) * 10 >= longer * 9
}

/// Lengths in bytes of the common prefix and the common suffix of two texts,
/// on character boundaries, never overlapping.
fn common_ends(a: &str, b: &str) -> (usize, usize) {
    let mut prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !(a.is_char_boundary(prefix) && b.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let room = a.len().min(b.len()) - prefix;
    let mut suffix = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take(room)
        .take_while(|(x, y)| x == y)
        .count();
    while !(a.is_char_boundary(a.len() - suffix) && b.is_char_boundary(b.len() - suffix)) {
        suffix -= 1;
    }
    (prefix, suffix)
}

/// The first differing stretch of `before` and `after`, widened to the
/// word it falls in and clipped to [`DIFF_CHARS`] a side, plus the kind of
/// token it swapped.
pub(crate) fn byte_diff(before: &str, after: &str) -> (ByteDiff, Volatile) {
    if before == after {
        let diff = ByteDiff {
            offset: before.len(),
            before: String::new(),
            after: String::new(),
        };
        return (diff, Volatile::Text);
    }
    let (prefix, suffix) = common_ends(before, after);
    let widen = |character: char| character.is_alphanumeric() || "-:._+/".contains(character);
    // Widen the start backwards over the word the difference begins in.
    let mut start = prefix;
    let mut steps = 0;
    while steps < WIDEN_CHARS {
        let Some(character) = before[..start].chars().next_back() else {
            break;
        };
        if !widen(character) {
            break;
        }
        start -= character.len_utf8();
        steps += 1;
    }
    // Punctuation the widening crossed at the edges is sentence, not token.
    let edge = |character: char| ".:-_+/".contains(character);
    while start < prefix && before[start..].starts_with(edge) {
        start += 1;
    }
    // Widen each end forwards over the word the difference ends in.
    let end = |text: &str| {
        let original = text.len() - suffix;
        let mut end = original;
        let mut steps = 0;
        while steps < WIDEN_CHARS {
            let Some(character) = text[end..].chars().next() else {
                break;
            };
            if !widen(character) {
                break;
            }
            end += character.len_utf8();
            steps += 1;
        }
        while end > original && text[..end].ends_with(edge) {
            end -= 1;
        }
        end.max(start)
    };
    let shown_before = &before[start..end(before)];
    let shown_after = &after[start..end(after)];
    let volatile = classify_volatile(shown_before, shown_after);
    (
        ByteDiff {
            offset: start,
            before: clip(shown_before, DIFF_CHARS),
            after: clip(shown_after, DIFF_CHARS),
        },
        volatile,
    )
}

/// What kind of token the two sides of an edit are.
fn classify_volatile(before: &str, after: &str) -> Volatile {
    let both = |test: fn(&str) -> bool| test(before) && test(after);
    if both(has_uuid) {
        Volatile::Uuid
    } else if both(has_timestamp) {
        Volatile::Timestamp
    } else if both(is_number) {
        Volatile::Number
    } else if both(has_hex_id) {
        Volatile::HexId
    } else {
        Volatile::Text
    }
}

fn has_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    let groups = [8, 4, 4, 4, 12];
    (0..bytes.len()).any(|start| {
        let mut at = start;
        for (position, length) in groups.iter().enumerate() {
            if position > 0 {
                if bytes.get(at) != Some(&b'-') {
                    return false;
                }
                at += 1;
            }
            let Some(group) = bytes.get(at..at + length) else {
                return false;
            };
            if !group.iter().all(u8::is_ascii_hexdigit) {
                return false;
            }
            at += length;
        }
        true
    })
}

fn has_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digit = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    // A date: dddd-dd-dd or dddd/dd/dd.
    let date = (0..bytes.len()).any(|at| {
        (0..4).all(|k| digit(at + k))
            && matches!(bytes.get(at + 4), Some(b'-' | b'/'))
            && digit(at + 5)
            && digit(at + 6)
            && matches!(bytes.get(at + 7), Some(b'-' | b'/'))
            && digit(at + 8)
            && digit(at + 9)
    });
    // A time of day: d?d:dd.
    let time = (0..bytes.len())
        .any(|at| digit(at) && bytes.get(at + 1) == Some(&b':') && digit(at + 2) && digit(at + 3))
        || (0..bytes.len()).any(|at| {
            digit(at) && digit(at + 1) && bytes.get(at + 2) == Some(&b':') && digit(at + 3)
        });
    // An epoch: 10, 13, 16 or 19 digits starting with 1.
    let trimmed = text.trim();
    let epoch = trimmed.starts_with('1')
        && matches!(trimmed.len(), 10 | 13 | 16 | 19)
        && trimmed.bytes().all(|byte| byte.is_ascii_digit());
    date || time || epoch
}

fn is_number(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed.bytes().any(|byte| byte.is_ascii_digit())
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b',' | b'-' | b'+'))
}

fn has_hex_id(text: &str) -> bool {
    let mut run = 0;
    let (mut digits, mut letters) = (false, false);
    for byte in text.bytes() {
        if byte.is_ascii_hexdigit() {
            run += 1;
            digits |= byte.is_ascii_digit();
            letters |= byte.is_ascii_alphabetic();
            if run >= 8 && digits && letters {
                return true;
            }
        } else {
            run = 0;
            digits = false;
            letters = false;
        }
    }
    false
}

/// The comparison of one call with its predecessor.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Comparison {
    pub(crate) cause: Cause,
    pub(crate) shared_chunks: usize,
    pub(crate) shared_est_tokens: u64,
    pub(crate) divergence: Option<Divergence>,
    /// Estimated tokens the cache could have served and did not (raw
    /// estimate, before calibration).
    pub(crate) est_tokens_lost: u64,
    pub(crate) explanation: String,
}

/// Compares `current` with its predecessor `previous`, `gap_ns` after the
/// predecessor began, under a TTL of `ttl_ns`. Lost tokens are scaled by
/// `calibration` (the provider's count over the estimate), so the figure
/// the explanation quotes is the one reported.
pub(crate) fn compare(
    previous: &Context,
    current: &Context,
    gap_ns: u64,
    ttl_ns: u64,
    pool: &TextPool,
    calibration: f64,
) -> Comparison {
    let scaled = |tokens: u64| (tokens as f64 * calibration).round() as u64;
    let (p, c) = (&previous.chunks, &current.chunks);
    let shared = p
        .iter()
        .zip(c.iter())
        .take_while(|(a, b)| {
            a.exact == b.exact
                // A digest match is confirmed on the text when both texts
                // are held, so a forged collision cannot hide a divergence.
                && match (pool.get(a.exact), pool.get(b.exact)) {
                    (Some(x), Some(y)) => x == y,
                    _ => true,
                }
        })
        .count();
    let shared_est_tokens: u64 = c[..shared].iter().map(|chunk| chunk.est_tokens).sum();
    // Content of this call the predecessor also had, after the break.
    let carried_after = |from: usize| -> u64 {
        c[from..]
            .iter()
            .filter(|chunk| previous.contents.contains(&chunk.content))
            .map(|chunk| chunk.est_tokens)
            .sum()
    };
    let expired = gap_ns >= ttl_ns;
    let gap = human_duration(gap_ns);
    let ttl = human_duration(ttl_ns);
    let from = previous.step_index;

    if shared == p.len() {
        // The predecessor's whole context is a prefix of this one.
        let (cause, lost, explanation) = if expired {
            (
                Cause::TtlExpired,
                scaled(shared_est_tokens),
                format!(
                    "the prefix is unchanged since step {from} ({} chunks, ~{} tokens), but {gap} \
                     passed since that call began — past the {ttl} TTL",
                    shared,
                    thousands(shared_est_tokens)
                ),
            )
        } else {
            (
                Cause::Stable,
                0,
                format!(
                    "the prefix is unchanged since step {from} ({} chunks, ~{} tokens) and \
                     within the {ttl} TTL",
                    shared,
                    thousands(shared_est_tokens)
                ),
            )
        };
        return Comparison {
            cause,
            shared_chunks: shared,
            shared_est_tokens,
            divergence: None,
            est_tokens_lost: lost,
            explanation,
        };
    }

    if shared == c.len() {
        // This context ended first: a strict prefix of the predecessor's.
        let gone = p.len() - shared;
        let at = &p[shared];
        return Comparison {
            cause: Cause::Unknown,
            shared_chunks: shared,
            shared_est_tokens,
            divergence: Some(Divergence {
                index: shared,
                role: at.role.clone(),
                kind: at.kind,
                change: Change::Shrunk,
                diff: pool.get(at.exact).map(|before| ByteDiff {
                    offset: 0,
                    before: clip(before, DIFF_CHARS),
                    after: String::new(),
                }),
                volatile: None,
            }),
            est_tokens_lost: 0,
            explanation: format!(
                "this context is the first {shared} chunks of step {from}'s — its last {gone} \
                 were removed (a retry or a rollback); whether the provider serves a shorter \
                 prompt from its cache depends on where its cache breakpoints sat, which the \
                 trace does not record"
            ),
        };
    }

    let (before, after) = (&p[shared], &c[shared]);
    let position_in = |chunks: &[Kept], content: u128| {
        chunks
            .iter()
            .skip(shared + 1)
            .any(|chunk| chunk.content == content)
    };
    let before_later_here = position_in(c, before.content);
    let after_later_there = position_in(p, after.content);
    let after_anywhere_there = previous.contents.contains(&after.content);
    let before_anywhere_here = current.contents.contains(&before.content);
    let rest = &p[shared + 1..];
    let survived = if rest.is_empty() {
        1.0
    } else {
        rest.iter()
            .filter(|chunk| current.contents.contains(&chunk.content))
            .count() as f64
            / rest.len() as f64
    };

    let (cause, change) = if before.content == after.content {
        (Cause::Reordered, Change::Reserialized)
    } else if before_later_here && after_later_there {
        (Cause::Reordered, Change::Moved)
    } else if survived < SURVIVAL_FOR_EDIT {
        (Cause::NewContext, Change::Rewritten)
    } else if !after_anywhere_there && before_later_here {
        (Cause::VolatileContent, Change::Inserted)
    } else if after_anywhere_there && !before_anywhere_here {
        (Cause::NewContext, Change::Removed)
    } else {
        (Cause::VolatileContent, Change::Edited)
    };

    let texts = (pool.get(before.exact), pool.get(after.exact));
    let (diff, volatile) = match texts {
        (Some(old), Some(new)) => {
            let (diff, volatile) = byte_diff(old, new);
            (Some(diff), (change == Change::Edited).then_some(volatile))
        }
        _ => (None, None),
    };
    let mut lost = carried_after(shared);
    if expired {
        lost += shared_est_tokens;
    }
    let lost = scaled(lost);
    let role = after
        .role
        .clone()
        .unwrap_or_else(|| after.kind.as_str().to_owned());
    let swapped = diff.as_ref().map_or_else(String::new, |diff| {
        format!(": \"{}\" → \"{}\"", diff.before, diff.after)
    });
    let token = match volatile {
        Some(Volatile::Timestamp) => " (a timestamp)",
        Some(Volatile::Uuid) => " (a UUID)",
        Some(Volatile::HexId) => " (a hex id)",
        Some(Volatile::Number) => " (a number)",
        _ => "",
    };
    let reprocessed = format!(
        "~{} tokens of content step {from} already had were re-processed",
        thousands(lost)
    );
    let expired_note = if expired {
        format!("; {gap} had also passed since step {from} began, past the {ttl} TTL")
    } else {
        String::new()
    };
    let relabelled = before.role != after.role || before.kind != after.kind;
    let explanation = match change {
        Change::Reserialized if relabelled => format!(
            "chunk {shared} holds the same content as in step {from} under another role or kind \
             ({role} now); {reprocessed}{expired_note}"
        ),
        Change::Reserialized => format!(
            "chunk {shared} ({role}) holds the same content as in step {from} but different \
             bytes — re-serialized (key order or spacing){swapped}; {reprocessed}{expired_note}"
        ),
        Change::Moved => format!(
            "chunk {shared} ({role}) moved: step {from} had other content at this position, \
             and both chunks are still present elsewhere; {reprocessed}{expired_note}"
        ),
        Change::Rewritten => format!(
            "the context was rewritten after chunk {shared}: only {:.0}% of what followed it in \
             step {from} survives (compaction or truncation){expired_note}",
            survived * 100.0
        ),
        Change::Inserted => format!(
            "a new chunk ({role}) was inserted at position {shared}, ahead of content step \
             {from} already had; {reprocessed}{expired_note}"
        ),
        Change::Removed => format!(
            "chunk {shared} of step {from} ({}) was removed from inside the prefix (pruning); \
             {reprocessed}{expired_note}",
            before
                .role
                .clone()
                .unwrap_or_else(|| before.kind.as_str().to_owned())
        ),
        Change::Edited | Change::Shrunk => format!(
            "chunk {shared} ({role}) changed in place since step {from}{swapped}{token}; \
             {reprocessed}{expired_note}"
        ),
    };
    Comparison {
        cause,
        shared_chunks: shared,
        shared_est_tokens,
        divergence: Some(Divergence {
            index: shared,
            role: after.role.clone(),
            kind: after.kind,
            change,
            diff,
            volatile,
        }),
        est_tokens_lost: lost,
        explanation,
    }
}

/// `1,234,567`.
pub(crate) fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, character) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// `7 min 12 s`, `45 s`, `850 ms`.
pub(crate) fn human_duration(nanos: u64) -> String {
    let millis = nanos / 1_000_000;
    if millis < 1_000 {
        return format!("{millis} ms");
    }
    let seconds = millis / 1_000;
    if seconds < 60 {
        return format!("{seconds} s");
    }
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    if minutes < 60 {
        return format!("{minutes} min {seconds} s");
    }
    format!("{} h {} min", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::super::chunk::context_pieces;
    use super::*;
    use serde_json::{json, Value};

    const TTL: u64 = 300_000_000_000;

    fn context(step: usize, messages: Value, pool: &mut TextPool) -> Context {
        let pieces = context_pieces(Some(&messages), &[], 100).list;
        for piece in &pieces {
            pool.keep(piece);
        }
        Context::new(step, "", step as u64 * 1_000_000_000, &pieces, None)
    }

    fn msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": text})
    }

    #[test]
    fn an_extended_prefix_is_stable_and_an_old_one_expired() {
        let mut pool = TextPool::new(1 << 20);
        let p = context(
            0,
            json!([msg("system", "sys"), msg("user", "q")]),
            &mut pool,
        );
        let c = context(
            1,
            json!([
                msg("system", "sys"),
                msg("user", "q"),
                msg("assistant", "a")
            ]),
            &mut pool,
        );
        let stable = compare(&p, &c, 1_000, TTL, &pool, 1.0);
        assert_eq!(stable.cause, Cause::Stable);
        assert_eq!(stable.shared_chunks, 2);
        assert_eq!(stable.est_tokens_lost, 0);
        assert!(stable.divergence.is_none());
        let expired = compare(&p, &c, TTL, TTL, &pool, 1.0);
        assert_eq!(expired.cause, Cause::TtlExpired);
        assert_eq!(expired.est_tokens_lost, stable.shared_est_tokens);
        assert!(
            expired.explanation.contains("past the 5 min 0 s TTL"),
            "{}",
            expired.explanation
        );
    }

    #[test]
    fn a_timestamp_in_the_system_prompt_is_named_as_the_volatile_token() {
        let mut pool = TextPool::new(1 << 20);
        let p = context(
            0,
            json!([
                msg("system", "You help. Now: 2026-10-17T09:14:03Z. Be brief."),
                msg("user", "long question here")
            ]),
            &mut pool,
        );
        let c = context(
            1,
            json!([
                msg("system", "You help. Now: 2026-10-17T09:19:44Z. Be brief."),
                msg("user", "long question here"),
                msg("assistant", "x")
            ]),
            &mut pool,
        );
        let comparison = compare(&p, &c, 1_000, TTL, &pool, 1.0);
        assert_eq!(comparison.cause, Cause::VolatileContent);
        let divergence = comparison.divergence.expect("diverged");
        assert_eq!(divergence.index, 0);
        assert_eq!(divergence.change, Change::Edited);
        assert_eq!(divergence.volatile, Some(Volatile::Timestamp));
        let diff = divergence.diff.expect("diff");
        assert_eq!(diff.before, "2026-10-17T09:14:03Z");
        assert_eq!(diff.after, "2026-10-17T09:19:44Z");
        assert_eq!(diff.offset, "You help. Now: ".len());
        // The user chunk after it was carried but re-processed.
        assert!(comparison.est_tokens_lost > 0);
        // And the near-identical first chunk keeps the pairing.
        let recent = [p.clone()];
        let (found, shared) = predecessor(&c, recent.iter(), &pool)
            .expect("readable")
            .expect("paired");
        assert_eq!(found.step_index, 0);
        assert_eq!(shared, 2);
    }

    #[test]
    fn insertions_removals_moves_and_rewrites_are_told_apart() {
        let mut pool = TextPool::new(1 << 20);
        let base = json!([
            msg("system", "s"),
            msg("user", "a"),
            msg("assistant", "b"),
            msg("user", "c"),
            msg("assistant", "d")
        ]);
        let p = context(0, base, &mut pool);
        let case = |messages: Value, pool: &mut TextPool| {
            let c = context(1, messages, pool);
            compare(&p, &c, 0, TTL, pool, 1.0)
        };
        let inserted = case(
            json!([
                msg("system", "s"),
                msg("user", "RAG doc"),
                msg("user", "a"),
                msg("assistant", "b"),
                msg("user", "c"),
                msg("assistant", "d")
            ]),
            &mut pool,
        );
        assert_eq!(inserted.cause, Cause::VolatileContent);
        assert_eq!(inserted.divergence.unwrap().change, Change::Inserted);

        let removed = case(
            json!([
                msg("system", "s"),
                msg("assistant", "b"),
                msg("user", "c"),
                msg("assistant", "d")
            ]),
            &mut pool,
        );
        assert_eq!(removed.cause, Cause::NewContext);
        assert_eq!(removed.divergence.unwrap().change, Change::Removed);

        let moved = case(
            json!([
                msg("system", "s"),
                msg("assistant", "b"),
                msg("user", "a"),
                msg("user", "c"),
                msg("assistant", "d")
            ]),
            &mut pool,
        );
        assert_eq!(moved.cause, Cause::Reordered);
        assert_eq!(moved.divergence.unwrap().change, Change::Moved);

        let rewritten = case(
            json!([msg("system", "s"), msg("user", "summary of a b c d")]),
            &mut pool,
        );
        assert_eq!(rewritten.cause, Cause::NewContext);
        assert_eq!(rewritten.divergence.unwrap().change, Change::Rewritten);

        let shrunk = case(json!([msg("system", "s"), msg("user", "a")]), &mut pool);
        assert_eq!(shrunk.cause, Cause::Unknown);
        assert_eq!(shrunk.divergence.unwrap().change, Change::Shrunk);
    }

    #[test]
    fn reserialized_json_is_reordered_not_new() {
        let mut pool = TextPool::new(1 << 20);
        let p = context(
            0,
            json!([msg("system", "{\"a\": 1, \"b\": 2}"), msg("user", "q")]),
            &mut pool,
        );
        let c = context(
            1,
            json!([msg("system", "{\"b\": 2, \"a\": 1}"), msg("user", "q")]),
            &mut pool,
        );
        let comparison = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(comparison.cause, Cause::Reordered);
        assert_eq!(comparison.divergence.unwrap().change, Change::Reserialized);
    }

    #[test]
    fn the_prefix_is_compared_on_exact_identity_not_content() {
        // The same text moved from the user's turn into the system prompt is
        // the same content but different prompt bytes: a cache miss.
        let mut pool = TextPool::new(1 << 20);
        let p = context(
            0,
            json!([msg("user", "shared words"), msg("user", "q")]),
            &mut pool,
        );
        let c = context(
            1,
            json!([msg("system", "shared words"), msg("user", "q")]),
            &mut pool,
        );
        let comparison = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(comparison.shared_chunks, 0);
        assert_eq!(comparison.cause, Cause::Reordered);
        let divergence = comparison.divergence.expect("diverged");
        assert_eq!(divergence.change, Change::Reserialized);
        assert!(
            comparison.explanation.contains("under another role"),
            "{}",
            comparison.explanation
        );
    }

    #[test]
    fn the_predecessor_skips_other_conversations_and_refuses_unreadable_ones() {
        let mut pool = TextPool::new(1 << 20);
        let parent = context(
            0,
            json!([msg("system", "parent"), msg("user", "task")]),
            &mut pool,
        );
        let child = context(
            1,
            json!([msg("system", "child agent"), msg("user", "sub task")]),
            &mut pool,
        );
        let parent_again = context(
            2,
            json!([
                msg("system", "parent"),
                msg("user", "task"),
                msg("tool", "sub result")
            ]),
            &mut pool,
        );
        let recent = [parent.clone(), child.clone()];
        let (found, _) = predecessor(&parent_again, recent.iter(), &pool)
            .unwrap()
            .unwrap();
        assert_eq!(found.step_index, 0, "the interleaved child call is skipped");
        assert!(predecessor(&child, [parent.clone()].iter(), &pool)
            .unwrap()
            .is_none());

        let mut unreadable = child.clone();
        unreadable.unavailable = Some("offloaded and not resolved");
        assert_eq!(
            predecessor(&parent_again, [parent, unreadable].iter(), &pool).unwrap_err(),
            (1, "offloaded and not resolved")
        );
    }

    #[test]
    fn volatile_tokens_are_classified() {
        assert_eq!(
            classify_volatile(
                "550e8400-e29b-41d4-a716-446655440000",
                "650e8400-e29b-41d4-a716-446655440000"
            ),
            Volatile::Uuid
        );
        assert_eq!(classify_volatile("12:01", "12:07"), Volatile::Timestamp);
        assert_eq!(
            classify_volatile("1760692443", "1760692999"),
            Volatile::Timestamp
        );
        assert_eq!(classify_volatile("41", "42"), Volatile::Number);
        assert_eq!(
            classify_volatile("req_9f8e7d6c5b", "req_1a2b3c4d5e"),
            Volatile::HexId
        );
        assert_eq!(classify_volatile("apples", "pears"), Volatile::Text);
        let (diff, _) = byte_diff("same", "same");
        assert_eq!((diff.before.as_str(), diff.after.as_str()), ("", ""));
        // Multi-byte text never splits a character.
        let (diff, _) = byte_diff("é1é", "é2é");
        assert_eq!((diff.before.as_str(), diff.after.as_str()), ("é1é", "é2é"));
    }

    #[test]
    fn helpers_render_plainly() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(human_duration(850_000_000), "850 ms");
        assert_eq!(human_duration(432_000_000_000), "7 min 12 s");
        assert_eq!(human_duration(7_500_000_000_000), "2 h 5 min");
    }
}
