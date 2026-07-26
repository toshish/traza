//! Token extraction and Bloom filters for content search.
//!
//! Traza's exact indexes answer "which spans have `model = gpt-4o`". They
//! cannot answer "which spans mention a refund", because the text they would
//! have to search is the largest part of the record and the index deliberately
//! does not keep it (see [`crate::hash`]). Content search closes that gap with
//! a structure that never stores the text either: a Bloom filter over the
//! words a span's text contains, which can say *this block definitely does not
//! mention it* and nothing else.
//!
//! Everything in this module is part of the on-disk format. A filter built by
//! one process is probed by another, so the tokenizer's rules and the bit
//! derivation are format constants, pinned by test, and may only change behind
//! a segment version bump.
//!
//! # Content search is WORD search, and that is a soundness requirement
//!
//! A Bloom filter can only skip work, never add it: it has false positives and
//! no false negatives. So the filter is safe *only* for a question it can
//! actually over-approximate. That constraint decides the feature's semantics,
//! and it is worth spelling out because the obvious alternative is subtly
//! broken.
//!
//! A **substring** search cannot be driven by a word index. Searching `refund`
//! against a span reading `refunds were issued`: the span's words are
//! `{refunds, were, issued}`, which does not contain `refund`, so the filter
//! says "definitely absent" and the span is skipped — while a substring match
//! would have returned it. That is a false negative, which is a wrong answer,
//! not a slow one. Dropping the query's first and last words from the probe
//! restores soundness but makes every single-word search unindexable, which is
//! the common case.
//!
//! So [`Query::matches`] tests **word containment**, exactly what the filter
//! over-approximates. Index and answer agree by construction:
//!
//! - `refund` matches `Refund the order` and `"refund"` and `refund,`.
//! - `refund` does **not** match `refunds` or `prerefund`. There is no
//!   stemming and no substring matching.
//! - `refund order` matches a span containing both words, anywhere, in any
//!   order. It is a conjunction, not a phrase.
//!
//! For finding a value rather than a word in it, an exact attribute filter is
//! the right tool and is already indexed.
//!
//! # What the tokenizer sees
//!
//! Tokens are maximal runs of ASCII alphanumerics, lowercased. Everything
//! else — punctuation, whitespace, JSON structure — is a separator. Tokens
//! longer than [`MAX_TOKEN_LEN`] are *hashed* under their first
//! `MAX_TOKEN_LEN` bytes, which bounds the filter; they are still *compared*
//! in full, so two long words sharing a prefix cost a decode rather than
//! producing a match. Non-ASCII bytes are separators too: this is an
//! ASCII tokenizer and it does not pretend otherwise. Text in a script it
//! cannot segment produces no tokens, and a query for it declares itself
//! unindexable (see [`Query::is_indexable`]) so the planner scans instead of
//! returning nothing.

use std::collections::HashSet;

use crate::hash::hash128;

/// Longest token PROBED, in bytes. A longer token is hashed under its first
/// `MAX_TOKEN_LEN` bytes.
///
/// Without a cap, one base64 blob or minified payload contributes a token as
/// large as itself and as distinct as itself, which is exactly the unbounded
/// cardinality this index exists to avoid.
///
/// **The cap applies to the filter and to nothing else.** Two different words
/// sharing their first 40 bytes therefore land on the same bits and are
/// candidates for each other — which is precisely what a Bloom filter already
/// permits, and it costs a decode. [`Query::matches`] compares tokens IN FULL,
/// so a candidate admitted by a shared prefix is rejected there. An earlier
/// version truncated on both sides "so the two agree", which made them agree
/// on a wrong answer: two distinct words matched each other in results.
pub const MAX_TOKEN_LEN: usize = 40;

/// Number of bit positions each token sets. Four is the practical optimum for
/// the ~8 bits per token this index budgets, and it is a format constant: a
/// filter written with one `k` cannot be probed with another.
pub const HASH_COUNT: u32 = 4;

/// Bits budgeted per distinct token when sizing a filter. At `HASH_COUNT = 4`
/// this puts a single token's false-positive rate near 2.4%, and a query is a
/// conjunction over its tokens, so a two-word query is already under a
/// thousandth.
const BITS_PER_TOKEN: usize = 8;

/// Calls `f` with every token in `text`, lowercased.
///
/// Allocates only for tokens that actually need case folding, which most
/// machine-generated text does not.
pub fn for_each_token(text: &str, f: &mut impl FnMut(&str)) {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut buffer = String::with_capacity(MAX_TOKEN_LEN);
    while index <= bytes.len() {
        if index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        if index > start {
            // The whole run, untruncated. Truncation belongs to the filter
            // (see `bit_positions`), not to what a match is compared against.
            let run = &text[start..index];
            if run.bytes().any(|byte| byte.is_ascii_uppercase()) {
                buffer.clear();
                buffer.extend(run.chars().map(|c| c.to_ascii_lowercase()));
                f(&buffer);
            } else {
                f(run);
            }
        }
        index += 1;
        start = index;
    }
}

/// The tokens in `text`, lowercased, in order of appearance.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_token(text, &mut |token| out.push(token.to_owned()));
    out
}

/// The key a token is hashed under: the token itself, or its first
/// [`MAX_TOKEN_LEN`] bytes if it is longer.
///
/// Tokens are runs of ASCII alphanumerics by construction, so a byte cut is
/// always a character boundary.
pub fn probe_key(token: &str) -> &str {
    match token.len() > MAX_TOKEN_LEN {
        true => &token[..MAX_TOKEN_LEN],
        false => token,
    }
}

/// The distinct PROBE KEYS across many texts — what a filter is built from.
///
/// Deliberately not the distinct tokens: this set exists to be inserted into a
/// Bloom filter, and holding full-length tokens here would put an unbounded
/// amount of text in a transient set while the filter it feeds is fixed-size.
/// Matching never consults this; it walks the text with [`for_each_token`].
pub fn distinct_probe_keys<'a>(texts: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut distinct = HashSet::new();
    for text in texts {
        for_each_token(text, &mut |token| {
            let key = probe_key(token);
            // `contains` on a borrowed &str avoids allocating for the repeats,
            // which in prose are the overwhelming majority.
            if !distinct.contains(key) {
                distinct.insert(key.to_owned());
            }
        });
    }
    distinct
}

/// A parsed content query: the distinct words a matching span must contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    needle: String,
    tokens: Vec<String>,
}

impl Query {
    /// Parses a content query.
    pub fn new(needle: &str) -> Self {
        let mut tokens = tokens(needle);
        tokens.sort();
        tokens.dedup();
        Self {
            needle: needle.to_owned(),
            tokens,
        }
    }

    /// The text as the caller wrote it, for error messages and echoing back.
    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// The words a matching span must contain.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// Whether the index can narrow this query at all.
    ///
    /// A query with no tokens — punctuation alone, or text in a script the
    /// ASCII tokenizer does not segment — matches nothing and cannot be
    /// probed. The planner treats it as unindexable rather than letting an
    /// empty conjunction pass every block.
    pub fn is_indexable(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Whether `texts` collectively contain every word of the query.
    ///
    /// This decides the answer. It tests exactly what the Bloom filter
    /// over-approximates, so a span the filter admits is either a real match
    /// or a false positive rejected here — and a span the filter rejects could
    /// never have matched.
    pub fn matches<'a>(&self, texts: impl Iterator<Item = &'a str>) -> bool {
        if self.tokens.is_empty() {
            return false;
        }
        let mut outstanding: HashSet<&str> = self.tokens.iter().map(String::as_str).collect();
        for text in texts {
            for_each_token(text, &mut |token| {
                outstanding.remove(token);
            });
            if outstanding.is_empty() {
                return true;
            }
        }
        outstanding.is_empty()
    }
}

/// A Bloom filter over tokens, sized in whole bytes and a power-of-two number
/// of bits.
///
/// The power-of-two constraint lets a bit position be derived with a mask
/// rather than a modulo, and makes a filter's size self-evidently valid on
/// read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bloom {
    bits: Vec<u8>,
}

impl Bloom {
    /// An empty filter holding `bits` bits, which must be a power of two and
    /// at least eight.
    pub fn new(bits: usize) -> Self {
        debug_assert!(bits.is_power_of_two(), "bloom size must be a power of two");
        debug_assert!(bits >= 8, "bloom must be at least one byte");
        Self {
            bits: vec![0u8; bits / 8],
        }
    }

    /// Wraps bytes read from a segment.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bits: bytes }
    }

    /// The filter's bytes, exactly as stored.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// The filter's size in bits.
    pub fn bit_len(&self) -> usize {
        self.bits.len() * 8
    }

    /// Records that `token` is present.
    pub fn insert(&mut self, token: &str) {
        for position in bit_positions(token, self.bit_len()) {
            self.bits[position / 8] |= 1 << (position % 8);
        }
    }

    /// Whether `token` MAY be present. `false` means definitely absent, which
    /// is the only direction this structure can be trusted in.
    pub fn may_contain(&self, token: &str) -> bool {
        bit_positions(token, self.bit_len())
            .all(|position| self.bits[position / 8] & (1 << (position % 8)) != 0)
    }

    /// Whether every one of `tokens` may be present.
    pub fn may_contain_all(&self, tokens: &[String]) -> bool {
        tokens.iter().all(|token| self.may_contain(token))
    }

    /// Fraction of bits set. A filter approaching 1.0 has saturated and no
    /// longer prunes; it is still correct, just useless, and this is how an
    /// operator sees that happening.
    pub fn fill_ratio(&self) -> f64 {
        if self.bits.is_empty() {
            return 0.0;
        }
        let set: u32 = self.bits.iter().map(|byte| byte.count_ones()).sum();
        f64::from(set) / (self.bits.len() * 8) as f64
    }
}

/// The `HASH_COUNT` bit positions a token sets in a filter of `bit_len` bits.
///
/// Derived from one 128-bit digest by the standard double-hashing construction
/// (Kirsch–Mitzenmacher): the two halves seed an arithmetic progression whose
/// members are as good as independent hashes for this purpose. One digest per
/// token rather than four is most of what makes building the index affordable.
///
/// Public because a bit-sliced filter is probed position by position rather
/// than through [`Bloom`] — see the segment's content index, which stores one
/// row per position so that testing a token across every block is one read.
pub fn bit_positions(token: &str, bit_len: usize) -> impl Iterator<Item = usize> {
    let digest = hash128(probe_key(token).as_bytes());
    let low = u64::from_le_bytes(digest.0[..8].try_into().expect("8 bytes"));
    let high = u64::from_le_bytes(digest.0[8..].try_into().expect("8 bytes"));
    let mask = (bit_len - 1) as u64;
    // A zero step would make all k positions identical, collapsing the filter
    // to k=1 for that token. Forcing it odd also keeps the progression from
    // cycling early against a power-of-two mask.
    let step = high | 1;
    (0..u64::from(HASH_COUNT))
        .map(move |i| (low.wrapping_add(i.wrapping_mul(step)) & mask) as usize)
}

/// The filter size for `distinct_tokens`, in bits: eight bits each, rounded up
/// to a power of two and clamped to `[min_bytes, max_bytes]`.
///
/// The clamp is the whole memory story. Without an upper bound a segment full
/// of distinct text sizes its own filter without limit, which is the failure
/// the attribute index just escaped. With one, a filter too small for its
/// content saturates and stops pruning — it never returns a wrong answer, it
/// just stops helping — so the cost of the bound is paid in latency and shows
/// up in [`Bloom::fill_ratio`] rather than in RAM.
pub fn size_bits(distinct_tokens: usize, min_bytes: usize, max_bytes: usize) -> usize {
    debug_assert!(min_bytes <= max_bytes);
    let wanted = distinct_tokens
        .saturating_mul(BITS_PER_TOKEN)
        .max(min_bytes * 8)
        .next_power_of_two();
    wanted.clamp(min_bytes * 8, (max_bytes * 8).next_power_of_two())
}

/// Builds a filter over the distinct tokens of `texts`.
pub fn build<'a>(
    texts: impl Iterator<Item = &'a str> + Clone,
    min_bytes: usize,
    max_bytes: usize,
) -> Bloom {
    // Deduplicate before hashing. Prose repeats itself heavily, and hashing
    // one token per occurrence rather than per distinct value is the
    // difference between an index costing a fraction of a seal and one that
    // dominates it.
    let distinct = distinct_probe_keys(texts);
    let mut bloom = Bloom::new(size_bits(distinct.len(), min_bytes, max_bytes));
    for token in &distinct {
        bloom.insert(token);
    }
    bloom
}

#[cfg(test)]
mod tests {
    use super::{
        build, for_each_token, size_bits, tokens, Bloom, Query, HASH_COUNT, MAX_TOKEN_LEN,
    };

    fn one(text: &str) -> std::iter::Once<&str> {
        std::iter::once(text)
    }

    #[test]
    fn tokenization_is_pinned_to_the_format() {
        // These rules are written into every segment. A change here silently
        // stops old filters from matching new queries.
        assert_eq!(
            tokens("Refund the ORDER, please."),
            ["refund", "the", "order", "please"]
        );
        assert_eq!(
            tokens("gen_ai.request.model"),
            ["gen", "ai", "request", "model"]
        );
        assert_eq!(
            tokens(r#"{"role":"user","content":"hi"}"#),
            ["role", "user", "content", "hi"]
        );
        assert_eq!(tokens("gpt-4o-mini"), ["gpt", "4o", "mini"]);
        assert_eq!(tokens("   "), Vec::<String>::new());
        assert_eq!(tokens(""), Vec::<String>::new());
    }

    #[test]
    fn long_runs_are_truncated_for_the_filter_and_nowhere_else() {
        // The cap is a property of the FILTER. A token is tokenized whole,
        // and hashed under its first MAX_TOKEN_LEN bytes.
        let long = "a".repeat(MAX_TOKEN_LEN + 25);
        assert_eq!(
            tokens(&long),
            std::slice::from_ref(&long),
            "the token itself is whole"
        );
        assert_eq!(super::probe_key(&long).len(), MAX_TOKEN_LEN);

        let first = format!("{}aaa", "z".repeat(MAX_TOKEN_LEN));
        let second = format!("{}bbb", "z".repeat(MAX_TOKEN_LEN));

        // They share a probe key, so each is a CANDIDATE for the other. That
        // is a Bloom filter behaving normally.
        let mut bloom = Bloom::new(size_bits(8, 64, 4096));
        bloom.insert(&second);
        assert!(
            bloom.may_contain(&first),
            "a shared 40-byte prefix must still admit the candidate"
        );

        // And matching must reject it. An earlier version truncated on both
        // sides "so the two agree" -- they agreed on a wrong answer, and two
        // distinct words matched each other in results.
        assert!(
            !Query::new(&first).matches(one(&second)),
            "distinct words sharing a prefix must not match"
        );
        assert!(Query::new(&first).matches(one(&first)));
    }

    #[test]
    fn non_ascii_is_a_separator_and_says_so() {
        // Not a claim that this is good segmentation — a claim that it is
        // honest. Tokens around the non-ASCII text are still indexed.
        assert_eq!(tokens("hello 世界 world"), ["hello", "world"]);
        // A query that tokenizes to nothing declares itself unindexable rather
        // than quietly matching everything.
        assert!(!Query::new("世界").is_indexable());
        assert!(!Query::new("!!!").is_indexable());
        assert!(Query::new("hello").is_indexable());
    }

    #[test]
    fn matching_tests_exactly_what_the_filter_approximates() {
        // The soundness argument for the whole feature. Every span the filter
        // admits must be decidable here, and every span it rejects must be a
        // span this would have rejected too.
        let query = Query::new("refund");
        assert!(query.matches(one("please REFUND the order")));
        assert!(query.matches(one("refund,")));
        assert!(query.matches(one(r#"{"note":"refund"}"#)));

        // Not substring matching. If these matched, a filter probe for
        // "refund" would skip them and the answer would be WRONG rather than
        // slow -- their token sets do not contain "refund" at all.
        assert!(!query.matches(one("refunds were issued")));
        assert!(!query.matches(one("prerefund")));
        let mut bloom = Bloom::new(size_bits(8, 64, 4096));
        for token in tokens("refunds were issued") {
            bloom.insert(&token);
        }
        assert!(
            !bloom.may_contain("refund"),
            "the filter really does exclude this span, so matches() must too"
        );
    }

    #[test]
    fn a_multi_word_query_is_a_conjunction_not_a_phrase() {
        let query = Query::new("refund the order");
        assert_eq!(query.tokens(), ["order", "refund", "the"]);
        assert!(query.matches(one("Please Refund The Order now")));
        assert!(query.matches(one("the order was flagged; we issued a refund")));
        assert!(!query.matches(one("refund an order")), "'the' is missing");
        // Words may come from different texts on the same span: a prompt and a
        // completion are searched together.
        assert!(query.matches(["refund the", "ORDER"].into_iter()));
    }

    #[test]
    fn a_filter_never_reports_a_token_it_holds_as_absent() {
        // The only direction a Bloom filter may be trusted in.
        let inserted: Vec<String> = (0..500).map(|i| format!("token{i}")).collect();
        let joined = inserted.join(" ");
        let bloom = build(one(&joined), 64, 4096);
        for token in &inserted {
            assert!(bloom.may_contain(token), "{token} was inserted");
        }
    }

    #[test]
    fn the_false_positive_rate_is_near_what_the_sizing_promises() {
        // If this drifts the index still answers correctly but stops paying
        // for itself, and nothing else would notice.
        let present: Vec<String> = (0..1_000).map(|i| format!("present{i}")).collect();
        let bloom = build(one(&present.join(" ")), 64, 65_536);
        let probes = 20_000;
        let hits = (0..probes)
            .filter(|i| bloom.may_contain(&format!("absent{i}")))
            .count();
        let rate = hits as f64 / probes as f64;
        assert!(
            rate < 0.05,
            "false-positive rate {rate:.4} is far above the ~2.4% the sizing \
             budgets at k={HASH_COUNT}"
        );
        assert!(bloom.fill_ratio() > 0.1 && bloom.fill_ratio() < 0.9);
    }

    #[test]
    fn sizing_is_clamped_at_both_ends() {
        assert_eq!(size_bits(0, 64, 4096), 64 * 8);
        assert_eq!(size_bits(1, 64, 4096), 64 * 8);
        // Unbounded content must not produce an unbounded filter: that is the
        // failure mode the attribute index just escaped.
        assert_eq!(size_bits(100_000_000, 64, 4096), 4096 * 8);
        assert!(size_bits(1_000, 64, 65_536).is_power_of_two());
    }

    #[test]
    fn a_saturated_filter_stops_pruning_without_lying() {
        // Deliberately undersize: 20,000 tokens into 64 bytes. The filter is
        // useless, but it must never claim a token it holds is absent.
        let corpus: Vec<String> = (0..20_000).map(|i| format!("word{i}")).collect();
        let joined = corpus.join(" ");
        let bloom = build(one(&joined), 64, 64);
        assert!(bloom.fill_ratio() > 0.99, "this filter should be saturated");
        for token in corpus.iter().take(200) {
            assert!(bloom.may_contain(token), "no false negatives, ever");
        }
    }

    #[test]
    fn folding_a_token_does_not_change_which_bits_it_sets() {
        let mut bloom = Bloom::new(size_bits(10, 64, 4096));
        bloom.insert("refund");
        let mut seen = Vec::new();
        for_each_token("REFUND Refund refund", &mut |token| {
            seen.push(token.to_owned())
        });
        assert_eq!(seen, ["refund", "refund", "refund"]);
        for token in seen {
            assert!(bloom.may_contain(&token));
        }
    }
}
