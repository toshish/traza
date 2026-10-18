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
//! **The predecessor** is chosen among the previous 32 model calls of the
//! same tenant (and of the same agent, when both calls name one). A
//! candidate qualifies when its leading chunks match this call's past the
//! tool definitions — the same system prompt, or the same first
//! turn, so a compaction that kept the system prompt and replaced the history
//! still finds the call it compacted — or, failing that, when the two share
//! at least half of either context's chunks (a re-ordered context). Leading
//! chunks also match when they are near-identical — the same kind and role,
//! differing in a short span — so a system prompt that embeds the time of day
//! does not hide the conversation it belongs to. Among qualifying candidates
//! the one with the longest matching lead wins, then the most shared chunks,
//! then the nearest: a sub-agent that opens with its parent's system prompt
//! does not capture the parent's next call, which continues a longer prefix.
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
    /// The chunk was edited in place — the same kind and role, differing in
    /// a stretch at most a tenth of its text; the rest of the context
    /// survives.
    Edited,
    /// A different chunk took its place: not a small edit of it, and not
    /// content the predecessor had elsewhere.
    Replaced,
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
    /// Bytes of its text, so two chunks too different in length to be an
    /// edit of each other are told apart without their texts.
    pub(crate) bytes: usize,
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
    /// The agent the call belongs to, when the trace names one: calls of
    /// two differently named agents never pair.
    pub(crate) agent: Option<String>,
    /// The conversation the call names (`gen_ai.conversation.id`), when it
    /// names one: such a call pairs only within its conversation, and with
    /// the nearest earlier call of it always.
    pub(crate) conversation: Option<String>,
    pub(crate) start_ns: u64,
    pub(crate) chunks: Vec<Kept>,
    /// Content digests present.
    pub(crate) contents: HashSet<u128>,
    /// Why the context could not be read, when it could not.
    pub(crate) unavailable: Option<&'static str>,
    /// The context had more chunks than were split: `chunks` is its first
    /// part, and a chunk missing from it may lie past the cut.
    pub(crate) truncated: bool,
    /// Leading tool-definition chunks.
    pub(crate) leading_tools: usize,
    /// Whether the estimate covers what the provider counted, so their ratio
    /// may calibrate it: the whole context was read and split, holds no
    /// media (estimated at zero), and either recorded its tool definitions
    /// or used no tools.
    pub(crate) calibratable: bool,
    /// The texts this context holds in a [`TextPool`], released when it
    /// leaves the comparison window.
    pub(crate) held: Vec<u128>,
}

impl Context {
    /// `truncated` says the context had more chunks than were split.
    pub(crate) fn new(
        step_index: usize,
        tenant: &str,
        start_ns: u64,
        pieces: &[Piece],
        truncated: bool,
        unavailable: Option<&'static str>,
    ) -> Self {
        let chunks: Vec<Kept> = pieces
            .iter()
            .map(|piece| Kept {
                content: piece.content,
                exact: piece.exact,
                est_tokens: piece.est_tokens(),
                bytes: piece.text.len(),
                kind: piece.kind,
                role: piece.role.clone(),
            })
            .collect();
        let contents = chunks.iter().map(|chunk| chunk.content).collect();
        let is_definition = |piece: &Piece| piece.role.as_deref() == Some(super::chunk::TOOLS_ROLE);
        let leading_tools = pieces
            .iter()
            .take_while(|piece| is_definition(piece))
            .count();
        let uses_tools = pieces
            .iter()
            .any(|piece| matches!(piece.kind, ChunkKind::ToolCall | ChunkKind::ToolResult));
        let calibratable = !truncated
            && unavailable.is_none()
            && !pieces.is_empty()
            && !pieces.iter().any(|piece| piece.media)
            && (!uses_tools || pieces.iter().any(is_definition));
        Self {
            step_index,
            tenant: tenant.to_owned(),
            agent: None,
            conversation: None,
            start_ns,
            chunks,
            contents,
            unavailable,
            truncated,
            leading_tools,
            calibratable,
            held: Vec::new(),
        }
    }

    /// The same context, attributed to `agent`.
    pub(crate) fn with_agent(mut self, agent: Option<String>) -> Self {
        self.agent = agent;
        self
    }

    /// The same context, in `conversation`.
    pub(crate) fn with_conversation(mut self, conversation: Option<String>) -> Self {
        self.conversation = conversation;
        self
    }

    pub(crate) fn est_tokens(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.est_tokens).sum()
    }
}

/// Chunk text kept for diffs and previews, by exact digest, up to a byte
/// ceiling: a run's contexts repeat each other, so keying by identity stores
/// each distinct text once. Each text counts its holders — the contexts in
/// the comparison window that carry it — and is dropped when the last one
/// leaves, so the ceiling is spent on the calls a verdict can still be about,
/// never on history nobody will compare again.
pub(crate) struct TextPool {
    texts: HashMap<u128, (String, usize)>,
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

    /// One more holder of the piece's text; `false` when it was refused for
    /// want of room (and so holds nothing to release).
    pub(crate) fn keep(&mut self, piece: &Piece) -> bool {
        if let Some((_, holders)) = self.texts.get_mut(&piece.exact) {
            *holders += 1;
            return true;
        }
        if self.bytes.saturating_add(piece.text.len()) > self.max_bytes {
            self.overflowed = true;
            return false;
        }
        self.bytes += piece.text.len();
        self.texts.insert(piece.exact, (piece.text.clone(), 1));
        true
    }

    /// Keeps every piece's text, returning the identities kept, for
    /// [`Self::release`].
    pub(crate) fn hold(&mut self, pieces: &[Piece]) -> Vec<u128> {
        pieces
            .iter()
            .filter(|piece| self.keep(piece))
            .map(|piece| piece.exact)
            .collect()
    }

    /// Gives up one hold on each identity; a text nobody holds is dropped.
    pub(crate) fn release(&mut self, held: &[u128]) {
        for exact in held {
            if let Some((text, holders)) = self.texts.get_mut(exact) {
                *holders -= 1;
                if *holders == 0 {
                    self.bytes -= text.len();
                    self.texts.remove(exact);
                }
            }
        }
    }

    pub(crate) fn get(&self, exact: u128) -> Option<&str> {
        self.texts.get(&exact).map(|(text, _)| text.as_str())
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
    // (candidate, lead, shared): the best qualifying candidate so far.
    let mut best: Option<(&'r Context, usize, usize)> = None;
    for candidate in recent.rev() {
        if candidate.tenant != current.tenant || candidate.step_index >= current.step_index {
            continue;
        }
        if let (Some(theirs), Some(ours)) = (&candidate.agent, &current.agent) {
            if theirs != ours {
                continue;
            }
        }
        // A named conversation links its calls outright: the nearest earlier
        // call of it is the predecessor whatever the two share — after a
        // compaction, nothing — and a call of another conversation, or of
        // none, never is.
        if candidate.conversation.is_some() || current.conversation.is_some() {
            if candidate.conversation != current.conversation {
                continue;
            }
            if let Some(reason) = candidate.unavailable {
                return Err((candidate.step_index, reason));
            }
            let (_, bridged) = leading_match(candidate, current, pool);
            let shared = bridged
                + candidate
                    .chunks
                    .iter()
                    .filter(|chunk| current.contents.contains(&chunk.content))
                    .count();
            return Ok(Some((candidate, shared)));
        }
        if let Some(reason) = candidate.unavailable {
            match best {
                // A nearer call already pairs; this one is only a rival.
                Some(_) => continue,
                None => return Err((candidate.step_index, reason)),
            }
        }
        if candidate.chunks.is_empty() {
            continue;
        }
        let (lead, bridged) = leading_match(candidate, current, pool);
        let shared = bridged
            + candidate
                .chunks
                .iter()
                .filter(|chunk| current.contents.contains(&chunk.content))
                .count();
        let anchored = lead > current.leading_tools;
        let of_current = shared as f64 / current.chunks.len() as f64;
        let of_candidate = shared as f64 / candidate.chunks.len() as f64;
        let overlapping = shared > 0 && (of_current >= 0.5 || of_candidate >= 0.5);
        if !(anchored || overlapping) {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_lead, best_shared)) => (lead, shared) > (best_lead, best_shared),
        };
        if better {
            best = Some((candidate, lead, shared));
        }
        // A candidate this call wholly continues cannot be bettered by an
        // older one in any way that matters to a cache or a conversation.
        if lead == candidate.chunks.len() {
            break;
        }
    }
    Ok(best.map(|(candidate, _, shared)| (candidate, shared)))
}

/// How many leading chunks of `candidate` and `current` match — the same
/// content, or near-identical — and how many of those matched only as
/// near-identical.
fn leading_match(candidate: &Context, current: &Context, pool: &TextPool) -> (usize, usize) {
    let mut bridged = 0;
    let mut lead = 0;
    for (theirs, ours) in candidate.chunks.iter().zip(current.chunks.iter()) {
        if theirs.content == ours.content {
            lead += 1;
        } else if near_identical(theirs, ours, pool) {
            lead += 1;
            bridged += 1;
        } else {
            break;
        }
    }
    (lead, bridged)
}

/// Same kind and role, and the texts differ only in a stretch at most a
/// tenth of the longer one: the shape of a volatile token inside a stable
/// chunk.
fn near_identical(left: &Kept, right: &Kept, pool: &TextPool) -> bool {
    if left.kind != right.kind || left.role != right.role || !could_be_edit(left.bytes, right.bytes)
    {
        return false;
    }
    let (Some(a), Some(b)) = (pool.get(left.exact), pool.get(right.exact)) else {
        return false;
    };
    similar(a, b)
}

/// Whether texts of these byte lengths could pass [`similar`] at all: the
/// common ends cannot exceed the shorter text, so a shorter side under nine
/// tenths of the longer rules an edit out without reading either.
pub(crate) fn could_be_edit(a: usize, b: usize) -> bool {
    a.min(b) * 10 >= a.max(b) * 9
}

/// Whether two texts differ only in a stretch at most a tenth of the longer
/// one — an edit, rather than different content. Linear in the texts'
/// common ends, so it is cheap for different texts of any size.
pub(crate) fn similar(a: &str, b: &str) -> bool {
    let longer = a.len().max(b.len());
    if longer == 0 {
        return true;
    }
    if !could_be_edit(a.len(), b.len()) {
        return false;
    }
    let (prefix, suffix) = common_ends(a, b);
    (prefix + suffix) * 10 >= longer * 9
}

/// Lengths in bytes of the common prefix and the common suffix of two texts,
/// on character boundaries, never overlapping.
pub(crate) fn common_ends(a: &str, b: &str) -> (usize, usize) {
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

/// Something the comparison measured past a divergence, over the chunks
/// split, and whether it holds whatever lies past a cut. Content found is
/// found; content not found in a context cut at the chunk bound may lie past
/// the cut, so an absence is sure only in a context split whole — and a
/// verdict resting on an absence that is not sure is not asserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fact {
    holds: bool,
    sure: bool,
}

impl Fact {
    /// Decided by what was compared.
    fn sure(holds: bool) -> Self {
        Self { holds, sure: true }
    }

    /// Whether content was `found` in a context split `whole` or not.
    fn found(found: bool, whole: bool) -> Self {
        Self {
            holds: found,
            sure: found || whole,
        }
    }

    /// Both hold: sure when both surely hold, or when either surely fails.
    fn and(self, other: Self) -> Self {
        let holds = self.holds && other.holds;
        let sure = if holds {
            self.sure && other.sure
        } else {
            (!self.holds && self.sure) || (!other.holds && other.sure)
        };
        Self { holds, sure }
    }
}

impl std::ops::Not for Fact {
    type Output = Self;

    fn not(self) -> Self {
        Self {
            holds: !self.holds,
            sure: self.sure,
        }
    }
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
    // A context cut at the chunk bound is compared as far as it was split:
    // what was found in it is found, but what was not may lie past the cut.
    let (p_whole, c_whole) = (!previous.truncated, !current.truncated);
    let cut = match (p_whole, c_whole) {
        (false, false) => format!("both this context and step {from}'s"),
        (false, true) => format!("step {from}'s context"),
        _ => "this context".to_owned(),
    };
    let bound = thousands(super::MAX_CONTEXT_CHUNKS as u64);

    if shared == p.len() && p_whole {
        // The predecessor's whole context is a prefix of this one — however
        // much of this one lies past a cut.
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

    if shared == p.len() || (shared == c.len() && !c_whole) {
        // Every chunk compared matches, but a cut context ends the
        // comparison before the prefix does: whether the rest held is not
        // in what was split.
        return Comparison {
            cause: Cause::Unknown,
            shared_chunks: shared,
            shared_est_tokens,
            divergence: None,
            est_tokens_lost: 0,
            explanation: format!(
                "the first {} chunks (~{} tokens) match step {from}'s, but {cut} had more than \
                 {bound} chunks and the rest was not compared, so whether the whole prefix held \
                 is not known",
                thousands(shared as u64),
                thousands(shared_est_tokens)
            ),
        };
    }

    if shared == c.len() {
        // This context ended first: a strict prefix of the predecessor's.
        let gone = p.len() - shared;
        let removed = if p_whole {
            format!("its last {gone} were removed")
        } else {
            format!("the rest of it, more than {gone} chunks, was removed")
        };
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
                "this context is the first {shared} chunks of step {from}'s — {removed} (a retry \
                 or a rollback); whether the provider serves a shorter prompt from its cache \
                 depends on where its cache breakpoints sat, which the trace does not record"
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
    // Where the two divergent chunks are found: a find is a fact, an
    // absence one only in a context split whole ([`Fact`]).
    let before_later_here = Fact::found(position_in(c, before.content), c_whole);
    let after_later_there = Fact::found(position_in(p, after.content), p_whole);
    let after_anywhere_there = Fact::found(previous.contents.contains(&after.content), p_whole);
    let before_anywhere_here = Fact::found(current.contents.contains(&before.content), c_whole);
    let rest = &p[shared + 1..];
    let survived = if rest.is_empty() {
        1.0
    } else {
        rest.iter()
            .filter(|chunk| current.contents.contains(&chunk.content))
            .count() as f64
            / rest.len() as f64
    };

    let texts = (pool.get(before.exact), pool.get(after.exact));
    // An edit is a small change to the same kind of chunk, verified on the
    // texts; anything else in that place is a replacement.
    let edited_text = match texts {
        (Some(old), Some(new))
            if before.kind == after.kind && before.role == after.role && similar(old, new) =>
        {
            Some((old, new))
        }
        _ => None,
    };
    let edit = edited_text.is_some();
    // Of an edited chunk, the text after the change is content the
    // predecessor had that the cache can no longer serve.
    let in_chunk = edited_text.map_or(0, |(old, new)| {
        let (_, suffix) = common_ends(old, new);
        super::chunk::est_tokens(&new[new.len() - suffix..])
    });
    let structural = carried_after(shared) + in_chunk;
    // A cut predecessor had more after its last analysed chunk.
    let rest_empty = Fact::sure(rest.is_empty() && p_whole);
    // Nothing the predecessor had follows the divergence here. Content
    // carried is found; its absence needs both contexts whole.
    let nothing_carried = Fact {
        holds: structural == 0,
        sure: structural > 0 || (p_whole && c_whole),
    };
    // Under half of what followed the divergence in the predecessor
    // survives. Of a cut predecessor that is a share of an unknown whole;
    // against a cut context, a floor.
    let rewritten = Fact {
        holds: survived < SURVIVAL_FOR_EDIT,
        sure: p_whole && (c_whole || survived >= SURVIVAL_FOR_EDIT),
    };
    // The predecessor's last chunk gave way to new content and nothing it
    // had follows: a new turn, which leaves the cache nothing to lose.
    let new_turn = rest_empty
        .and(!before_later_here)
        .and(!after_anywhere_there)
        .and(nothing_carried);
    let served = if expired {
        Cause::TtlExpired
    } else {
        Cause::Stable
    };
    let in_place = if edit {
        Change::Edited
    } else {
        Change::Replaced
    };
    // In order, the first rule that holds names the change. The verdict is
    // settled only when that rule, and every rule before it, would come out
    // the same whatever lies past a cut; otherwise it is `unknown`, and the
    // change describes the chunks compared.
    let rules = [
        (
            Fact::sure(before.content == after.content),
            (Cause::Reordered, Change::Reserialized),
        ),
        (
            before_later_here.and(after_later_there),
            (Cause::Reordered, Change::Moved),
        ),
        (new_turn, (served, in_place)),
        (rewritten, (Cause::NewContext, Change::Rewritten)),
        (
            (!after_anywhere_there).and(before_later_here),
            (Cause::VolatileContent, Change::Inserted),
        ),
        (
            after_anywhere_there.and(!before_anywhere_here),
            (Cause::NewContext, Change::Removed),
        ),
    ];
    let mut settled = true;
    let mut outcome = (Cause::VolatileContent, in_place);
    for (fact, named) in rules {
        settled &= fact.sure;
        if fact.holds {
            outcome = named;
            break;
        }
    }
    let (named_cause, change) = outcome;
    let cause = if settled { named_cause } else { Cause::Unknown };
    let new_turn = new_turn.holds;

    let (diff, volatile) = match texts {
        (Some(old), Some(new)) => {
            let (diff, volatile) = byte_diff(old, new);
            (Some(diff), (change == Change::Edited).then_some(volatile))
        }
        _ => (None, None),
    };
    let mut lost = structural;
    if expired {
        lost += shared_est_tokens;
    }
    let lost = if settled { scaled(lost) } else { 0 };
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
        _ if !settled => {
            let matched = if shared == 0 {
                String::new()
            } else {
                format!(
                    "the first {shared} chunks (~{} tokens) match and ",
                    thousands(shared_est_tokens)
                )
            };
            format!(
                "{matched}chunk {shared} ({role}) differs from step {from}'s{swapped}, but {cut} \
                 had more than {bound} chunks and the rest was not compared, so whether this is \
                 a new turn or a break, and why, is not known{expired_note}"
            )
        }
        _ if new_turn => {
            let how = if edit {
                format!("carries it edited at its end{swapped}")
            } else {
                format!("puts new content ({role}) in its place")
            };
            let held = format!(
                "the prefix is unchanged since step {from} up to that call's last chunk ({shared} \
                 chunks, ~{} tokens); this call {how}, and nothing step {from} had follows it — \
                 a new turn, not a break",
                thousands(shared_est_tokens)
            );
            if expired {
                format!("{held}; but {gap} passed since that call began — past the {ttl} TTL")
            } else {
                format!("{held}; within the {ttl} TTL")
            }
        }
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
        Change::Replaced if diff.is_none() => format!(
            "chunk {shared} ({role}) differs from step {from}'s, whose text was not kept, so \
             whether it is an edit is not known; it sits ahead of content step {from} already \
             had; {reprocessed}{expired_note}"
        ),
        Change::Replaced => format!(
            "chunk {shared} ({role}) was replaced with different content since step \
             {from}{swapped}, ahead of content step {from} already had; \
             {reprocessed}{expired_note}"
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
        context_with_tools(step, &[], messages, pool)
    }

    fn context_with_tools(
        step: usize,
        definitions: &[Value],
        messages: Value,
        pool: &mut TextPool,
    ) -> Context {
        let pieces = context_pieces(Some(&messages), definitions, 100).list;
        let mut context = Context::new(step, "", step as u64 * 1_000_000_000, &pieces, false, None);
        context.held = pool.hold(&pieces);
        context
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
    fn a_cut_context_settles_only_what_the_cut_cannot_change() {
        let mut pool = TextPool::new(1 << 20);
        let cut = |step: usize, messages: Value, pool: &mut TextPool| {
            let mut context = context(step, messages, pool);
            context.truncated = true;
            context
        };
        let base = json!([
            msg("system", "s"),
            msg("user", "a"),
            msg("assistant", "b"),
            msg("user", "c"),
            msg("assistant", "d")
        ]);
        let whole = context(0, base.clone(), &mut pool);

        // A whole predecessor that this cut context starts with: stable,
        // whatever lies past the cut.
        let mut longer = base.as_array().expect("list").clone();
        longer.push(msg("user", "e"));
        let extended = cut(1, Value::Array(longer), &mut pool);
        assert_eq!(
            compare(&whole, &extended, 0, TTL, &pool, 1.0).cause,
            Cause::Stable
        );

        // A cut predecessor matched as far as it was split: its rest was
        // not compared.
        let short = cut(0, json!([msg("system", "s"), msg("user", "a")]), &mut pool);
        let matched = compare(&short, &whole, 0, TTL, &pool, 1.0);
        assert_eq!(matched.cause, Cause::Unknown);
        assert_eq!((matched.shared_chunks, matched.est_tokens_lost), (2, 0));
        assert!(matched.divergence.is_none());
        assert!(
            matched
                .explanation
                .contains("step 0's context had more than 2,000 chunks"),
            "{}",
            matched.explanation
        );

        // A new turn needs nothing of the predecessor to follow — which a
        // cut context cannot show.
        let two = context(0, json!([msg("system", "s"), msg("user", "a")]), &mut pool);
        let turn = json!([msg("system", "s"), msg("user", "a different question")]);
        let whole_turn = context(1, turn.clone(), &mut pool);
        assert_eq!(
            compare(&two, &whole_turn, 0, TTL, &pool, 1.0).cause,
            Cause::Stable
        );
        let cut_turn = cut(1, turn, &mut pool);
        let unsure = compare(&two, &cut_turn, 0, TTL, &pool, 1.0);
        assert_eq!(unsure.cause, Cause::Unknown, "{}", unsure.explanation);
        assert_eq!(unsure.est_tokens_lost, 0);
        let divergence = unsure.divergence.expect("diverged");
        assert_eq!(divergence.index, 1, "where it diverged is still said");
        assert!(
            unsure
                .explanation
                .contains("this context had more than 2,000 chunks"),
            "{}",
            unsure.explanation
        );

        // An insertion rests on what was found here and absent from a whole
        // predecessor: settled, counting the chunks compared.
        let inserted = cut(
            1,
            json!([
                msg("system", "s"),
                msg("user", "RAG doc"),
                msg("user", "a"),
                msg("assistant", "b"),
                msg("user", "c")
            ]),
            &mut pool,
        );
        let comparison = compare(&whole, &inserted, 0, TTL, &pool, 1.0);
        assert_eq!(comparison.cause, Cause::VolatileContent);
        assert_eq!(
            comparison.divergence.map(|d| d.change),
            Some(Change::Inserted)
        );
        assert!(comparison.est_tokens_lost > 0);

        // A removal rests on an absence from this context, which a cut
        // cannot prove: the same change read whole is a removal.
        let pruned = json!([msg("system", "s"), msg("assistant", "b"), msg("user", "c")]);
        let whole_pruned = context(1, pruned.clone(), &mut pool);
        assert_eq!(
            compare(&whole, &whole_pruned, 0, TTL, &pool, 1.0)
                .divergence
                .map(|d| d.change),
            Some(Change::Removed)
        );
        let cut_pruned = cut(1, pruned, &mut pool);
        assert_eq!(
            compare(&whole, &cut_pruned, 0, TTL, &pool, 1.0).cause,
            Cause::Unknown
        );

        // A move is two finds: settled with both contexts cut.
        let p = cut(
            0,
            json!([
                msg("system", "s"),
                msg("user", "a"),
                msg("assistant", "b"),
                msg("user", "c")
            ]),
            &mut pool,
        );
        let c = cut(
            1,
            json!([
                msg("system", "s"),
                msg("assistant", "b"),
                msg("user", "a"),
                msg("user", "c")
            ]),
            &mut pool,
        );
        let moved = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(moved.cause, Cause::Reordered);
        assert_eq!(moved.divergence.map(|d| d.change), Some(Change::Moved));
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
    fn a_new_turn_is_not_a_break_and_a_templated_edit_is() {
        let mut pool = TextPool::new(1 << 20);
        let system = msg("system", "You label sentences. Answer with one word.");
        // A stateless pipeline: the same system prompt, a different input.
        let p = context(
            0,
            json!([system.clone(), msg("user", "Classify: the sky is green")]),
            &mut pool,
        );
        let c = context(
            1,
            json!([system.clone(), msg("user", "Classify: water is wet")]),
            &mut pool,
        );
        let turn = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(turn.cause, Cause::Stable, "{}", turn.explanation);
        assert_eq!(turn.est_tokens_lost, 0);
        assert_eq!(turn.shared_chunks, 1);
        let divergence = turn.divergence.expect("where the prefix ended");
        assert_eq!(divergence.change, Change::Replaced);
        assert_eq!(divergence.volatile, None);
        assert!(
            turn.explanation.contains("a new turn, not a break"),
            "{}",
            turn.explanation
        );
        // The same, past the TTL, is an expiry of the shared prefix.
        let late = compare(&p, &c, TTL, TTL, &pool, 1.0);
        assert_eq!(late.cause, Cause::TtlExpired);
        assert_eq!(late.est_tokens_lost, turn.shared_est_tokens);

        // A template whose variable sits ahead of fixed rules: the rules are
        // re-processed every call — a break, priced by the rules' tokens.
        let rules = " Rules: answer yes or no, cite the line, never guess.".repeat(4);
        let p = context(
            2,
            json!([
                system.clone(),
                msg("user", &format!("Check item 123.{rules}"))
            ]),
            &mut pool,
        );
        let c = context(
            3,
            json!([
                system.clone(),
                msg("user", &format!("Check item 124.{rules}"))
            ]),
            &mut pool,
        );
        let templated = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(templated.cause, Cause::VolatileContent);
        let divergence = templated.divergence.expect("diverged");
        assert_eq!(divergence.change, Change::Edited);
        assert_eq!(divergence.volatile, Some(Volatile::Number));
        assert_eq!(
            templated.est_tokens_lost,
            super::super::chunk::est_tokens(&format!(".{rules}"))
        );

        // A chunk replaced wholesale with what followed it surviving.
        let p = context(
            4,
            json!([
                system.clone(),
                msg("user", "doc: alpha beta gamma"),
                msg("user", "q?")
            ]),
            &mut pool,
        );
        let c = context(
            5,
            json!([
                system,
                msg("user", "entirely different retrieved text"),
                msg("user", "q?"),
                msg("assistant", "a")
            ]),
            &mut pool,
        );
        let replaced = compare(&p, &c, 0, TTL, &pool, 1.0);
        assert_eq!(replaced.cause, Cause::VolatileContent);
        assert_eq!(replaced.divergence.unwrap().change, Change::Replaced);
        assert_eq!(replaced.est_tokens_lost, 1, "the carried \"q?\"");
    }

    #[test]
    fn compactions_pair_with_the_call_they_compacted() {
        let mut pool = TextPool::new(1 << 20);
        let rule = msg("user", "Never touch prod. Clean the logs.");
        let l0 = context(0, json!([msg("system", "S"), rule.clone()]), &mut pool);
        let l1 = context(
            1,
            json!([
                msg("system", "S"),
                rule.clone(),
                msg("assistant", "ok"),
                msg("user", "go on")
            ]),
            &mut pool,
        );
        let l2 = context(
            2,
            json!([
                msg("system", "S"),
                msg("user", "Summary: logs cleaned."),
                msg("user", "continue")
            ]),
            &mut pool,
        );
        let recent = [l0.clone(), l1.clone()];
        let (found, shared) = predecessor(&l2, recent.iter(), &pool)
            .expect("readable")
            .expect("paired");
        assert_eq!(found.step_index, 1, "the call compacted, not an older one");
        assert_eq!(shared, 1);
        let verdict = compare(found, &l2, 0, TTL, &pool, 1.0);
        assert_eq!(verdict.cause, Cause::NewContext);
        assert_eq!(verdict.divergence.unwrap().change, Change::Rewritten);

        // A longer summary shares under half of either context.
        let l2b = context(
            2,
            json!([
                msg("system", "S"),
                msg("user", "Summary: logs cleaned."),
                msg("user", "continue"),
                msg("user", "and report")
            ]),
            &mut pool,
        );
        let only_l1 = [l1.clone()];
        let found = predecessor(&l2b, only_l1.iter(), &pool).expect("readable");
        assert_eq!(found.map(|(c, _)| c.step_index), Some(1));

        // Tool definitions lead both contexts; the lead must reach past them.
        let tools = [json!([{"name": "grep"}])];
        let t1 = context_with_tools(
            3,
            &tools,
            json!([
                msg("system", "S"),
                rule,
                msg("assistant", "a"),
                msg("user", "b"),
                msg("assistant", "c"),
                msg("user", "d")
            ]),
            &mut pool,
        );
        let t2 = context_with_tools(
            4,
            &tools,
            json!([
                msg("system", "S"),
                msg("user", "Summary."),
                msg("user", "next"),
                msg("user", "more")
            ]),
            &mut pool,
        );
        assert_eq!(t2.leading_tools, 1);
        let only_t1 = [t1.clone()];
        let found = predecessor(&t2, only_t1.iter(), &pool).expect("readable");
        assert_eq!(found.map(|(c, _)| c.step_index), Some(3));
        // Sharing only the tool catalog is not a conversation.
        let other = context_with_tools(
            5,
            &tools,
            json!([
                msg("system", "another agent"),
                msg("user", "x"),
                msg("user", "y")
            ]),
            &mut pool,
        );
        assert!(predecessor(&other, [t1].iter(), &pool)
            .expect("readable")
            .is_none());
    }

    #[test]
    fn a_sub_agent_on_the_same_system_prompt_does_not_capture_its_parent() {
        let mut pool = TextPool::new(1 << 20);
        let system = msg("system", "You are a careful agent.");
        let p1 = context(
            0,
            json!([
                system.clone(),
                msg("user", "task"),
                msg("assistant", "a"),
                msg("user", "b")
            ]),
            &mut pool,
        );
        let s1 = context(
            1,
            json!([system.clone(), msg("user", "sub task")]),
            &mut pool,
        );
        let p2 = context(
            2,
            json!([
                system,
                msg("user", "task"),
                msg("assistant", "a"),
                msg("user", "b"),
                msg("user", "sub result")
            ]),
            &mut pool,
        );
        let recent = [p1.clone(), s1.clone()];
        let (found, _) = predecessor(&p2, recent.iter(), &pool)
            .expect("readable")
            .expect("paired");
        assert_eq!(found.step_index, 0, "the longer lead beats the nearer call");

        // Calls that name different agents never pair; unnamed ones may.
        assert!(predecessor(&s1, [p1.clone()].iter(), &pool)
            .expect("readable")
            .is_some());
        let planner = p1.with_agent(Some("planner".into()));
        let worker = s1.with_agent(Some("worker".into()));
        assert!(predecessor(&worker, [planner].iter(), &pool)
            .expect("readable")
            .is_none());
    }

    #[test]
    fn the_pool_holds_texts_while_a_context_needs_them() {
        let pieces = context_pieces(
            Some(&json!([msg("user", "abcd"), msg("user", "efgh")])),
            &[],
            10,
        )
        .list;
        let mut pool = TextPool::new(8);
        let first = pool.hold(&pieces);
        let second = pool.hold(&pieces);
        assert_eq!(first.len(), 2);
        assert_eq!(pool.get(pieces[0].exact), Some("abcd"));
        pool.release(&first);
        assert_eq!(pool.get(pieces[0].exact), Some("abcd"), "still held");
        pool.release(&second);
        assert_eq!(
            pool.get(pieces[0].exact),
            None,
            "evicted with its last holder"
        );
        assert!(!pool.overflowed);
        // Room is given back, and a refusal holds nothing.
        let big = context_pieces(Some(&json!([msg("user", "123456789")])), &[], 10).list;
        assert!(pool.hold(&big).is_empty());
        assert!(pool.overflowed);
        assert_eq!(pool.hold(&pieces).len(), 2);
        assert!(could_be_edit(90, 100) && !could_be_edit(89, 100));
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
