//! Model pricing: turning token counts into a cost when nothing metered one.
//!
//! Cost is not an OpenTelemetry attribute, so a span only carries one if the
//! pipeline that produced it happened to meter it. Most do not. The result was
//! a store that knew the model and both token counts and still reported
//! `$0.00` across every cost surface it has — a number that reads as "this was
//! free" rather than "nobody told me the rates".
//!
//! So the rates are configuration. They cannot be built in: prices change on
//! the vendor's schedule rather than Traza's, private and self-hosted models
//! have no public rate at all, and a table baked into a release would be
//! wrong for somebody the day it shipped. A deployment supplies its own.
//!
//! # What derived cost is, and is not
//!
//! A derived cost is arithmetic over the token counts the span reported, at
//! the rates this deployment configured. It is not a bill. It does not know
//! about batch tiers, reasoning tokens billed at a third rate, negotiated
//! pricing, or the difference between what a vendor quotes and what it
//! invoices. **A metered cost always wins** — if a span carries
//! `llm.cost_usd`, that is the number, and the table is not consulted.
//!
//! # Prompt caching
//!
//! A rate may name what cached prompt tokens cost (`cache_read_per_mtok`,
//! `cache_write_per_mtok`). Without them a call that reported cache counters
//! is priced exactly as it always was — prompt and completion at the two base
//! rates — which under-counts an Anthropic-style call (its prompt count
//! EXCLUDES the cached tokens, so they were never priced) and over-counts an
//! OpenAI-style one (its prompt count INCLUDES them, so they were priced at
//! the full input rate). With them, cached tokens are priced at their own
//! rates under the provider's counting convention ([`CacheConvention`]); a
//! provider whose convention is unknown keeps the old arithmetic, because
//! which way to correct it cannot be told.
//!
//! Because the two are not the same kind of fact, they are summed separately:
//! [`crate::analytics`] reports `cost_usd` alongside the derived share of it,
//! so a reader can always ask how much of a total is measurement and how much
//! is this file's opinion.
//!
//! # Staleness
//!
//! Rollups persist the counters they fold, cost among them, which makes the
//! table an input to a cached derived value. Editing the table therefore has
//! to invalidate rollups computed under the old one, or a sealed segment would
//! report last month's prices forever — the exact failure `rollup_file`'s
//! schema version exists to prevent, arriving through configuration where a
//! compile-time constant cannot see it. [`Pricing::fingerprint`] closes that:
//! it joins the sidecar's binding, so a changed table fails the same gate a
//! changed format does, and the rollup is rebuilt.

use std::collections::BTreeMap;

use serde_json::Value;

/// Per-million-token rates for one model.
///
/// Per *million* because that is the unit vendors quote, and quoting the same
/// unit is what lets somebody check the file against a price page without
/// arithmetic. Per-token rates are 1e-6 of these and would round to noise in
/// a config file.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rate {
    /// USD per million input (prompt) tokens.
    pub input_per_mtok: f64,
    /// USD per million output (completion) tokens.
    pub output_per_mtok: f64,
    /// USD per million prompt tokens served from a provider's cache, when the
    /// table names it (`cache_read_per_mtok`).
    pub cache_read_per_mtok: Option<f64>,
    /// USD per million prompt tokens written into a provider's cache, when
    /// the table names it (`cache_write_per_mtok`).
    pub cache_write_per_mtok: Option<f64>,
}

/// Tokens per million: the unit every rate is quoted in.
const PER_MTOK: f64 = 1_000_000.0;

impl Rate {
    /// Cost of a call at this rate, in USD, prompt caching ignored.
    fn cost(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / PER_MTOK) * self.input_per_mtok
            + (completion_tokens as f64 / PER_MTOK) * self.output_per_mtok
    }

    /// What one prompt token served from the cache saves against the same
    /// token processed afresh, in USD per million tokens — the price of a
    /// token the cache could have served and did not. `None` when the table
    /// names no cache-read rate for this model.
    pub fn cache_read_saving_per_mtok(&self) -> Option<f64> {
        self.cache_read_per_mtok
            .map(|read| (self.input_per_mtok - read).max(0.0))
    }
}

/// How a provider's prompt-token count relates to its cache counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheConvention {
    /// The prompt count is the UNCACHED remainder; cached tokens are reported
    /// beside it and come on top (Anthropic, Bedrock, Vertex).
    Exclusive,
    /// The prompt count INCLUDES the cached tokens (OpenAI, Azure OpenAI).
    Inclusive,
    /// Not known, so no correction is attempted.
    #[default]
    Unknown,
}

/// What one model call reported, for [`Pricing::cost_with_cache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallTokens {
    /// Prompt/input tokens, as the provider counted them.
    pub prompt: Option<u64>,
    /// Completion/output tokens.
    pub completion: Option<u64>,
    /// Prompt tokens served from the provider's cache.
    pub cache_read: Option<u64>,
    /// Prompt tokens written into the provider's cache.
    pub cache_write: Option<u64>,
    /// How [`Self::prompt`] relates to the two cache counters.
    pub convention: CacheConvention,
}

/// A model-name-to-rate table.
///
/// Empty is the default and means "derive nothing" — the behaviour every
/// deployment had before this existed, and the one a deployment that meters
/// its own cost should keep.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pricing {
    /// Exact model names.
    exact: BTreeMap<String, Rate>,
    /// Patterns written `prefix*`, stored without the star. Longest match
    /// wins, so `gpt-4o-mini*` beats `gpt-4o*` for `gpt-4o-mini-2024-07-18`
    /// regardless of what order the file listed them in.
    prefixes: Vec<(String, Rate)>,
}

impl Pricing {
    /// Whether the table can price anything at all.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefixes.is_empty()
    }

    /// Parses the documented JSON table.
    ///
    /// ```json
    /// {"models": {
    ///   "gpt-5.6-sol":  {"input_per_mtok": 1.25, "output_per_mtok": 10.0,
    ///                    "cache_read_per_mtok": 0.125},
    ///   "claude-opus-*": {"input_per_mtok": 15.0, "output_per_mtok": 75.0,
    ///                     "cache_read_per_mtok": 1.5, "cache_write_per_mtok": 18.75}
    /// }}
    /// ```
    ///
    /// The two cache rates are optional; see [`Self::cost_with_cache`] for
    /// what they change.
    ///
    /// Every rejection names the model it was reading. A pricing file is
    /// edited by hand, by somebody copying numbers off a price page, and
    /// "invalid pricing file" would send them to bisect it by deletion.
    pub fn parse(text: &str) -> Result<Self, String> {
        let root: Value =
            serde_json::from_str(text).map_err(|error| format!("pricing is not JSON: {error}"))?;
        let models = match root.get("models") {
            Some(Value::Object(map)) => map,
            Some(_) => return Err("pricing: \"models\" must be an object".to_owned()),
            None => return Err("pricing: missing \"models\"".to_owned()),
        };

        let mut pricing = Pricing::default();
        for (name, entry) in models {
            let object = entry
                .as_object()
                .ok_or_else(|| format!("pricing: {name} must be an object"))?;
            let optional = |field: &str| match object.get(field) {
                None | Some(Value::Null) => Ok(None),
                value => rate_field(value, name, field).map(Some),
            };
            let rate = Rate {
                input_per_mtok: rate_field(object.get("input_per_mtok"), name, "input_per_mtok")?,
                output_per_mtok: rate_field(
                    object.get("output_per_mtok"),
                    name,
                    "output_per_mtok",
                )?,
                cache_read_per_mtok: optional("cache_read_per_mtok")?,
                cache_write_per_mtok: optional("cache_write_per_mtok")?,
            };
            match name.strip_suffix('*') {
                // A bare "*" prices everything the table did not name. Legal,
                // and the only way to say "assume this rate unless I said
                // otherwise" — but it is a prefix like any other, so an exact
                // entry and a longer prefix both still beat it.
                Some(prefix) => pricing.prefixes.push((prefix.to_owned(), rate)),
                None => {
                    pricing.exact.insert(name.clone(), rate);
                }
            }
        }
        // Longest first, so the first match found is the most specific one.
        // Ties break on the name to keep the order — and so the fingerprint —
        // independent of the file's key order.
        pricing.prefixes.sort_by(|left, right| {
            right
                .0
                .len()
                .cmp(&left.0.len())
                .then_with(|| left.0.cmp(&right.0))
        });
        Ok(pricing)
    }

    /// The rate for `model`: exact name first, then the longest matching
    /// prefix pattern.
    pub fn rate(&self, model: &str) -> Option<Rate> {
        if let Some(rate) = self.exact.get(model) {
            return Some(*rate);
        }
        self.prefixes
            .iter()
            .find(|(prefix, _)| model.starts_with(prefix.as_str()))
            .map(|(_, rate)| *rate)
    }

    /// Cost of one call, or `None` when this table cannot honestly price it.
    ///
    /// Both directions must be known. A span that reported only a total has
    /// no input/output split, and input and output are priced differently by
    /// every vendor — splitting a total by an assumed ratio would produce a
    /// number that looks like the others and is made up, which is worse than
    /// the blank it replaces.
    pub fn cost(
        &self,
        model: &str,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
    ) -> Option<f64> {
        let (prompt, completion) = (prompt_tokens?, completion_tokens?);
        let cost = self.rate(model)?.cost(prompt, completion);
        cost.is_finite().then_some(cost)
    }

    /// [`Self::cost`], with prompt caching priced where the table can.
    ///
    /// A call that reported no cache tokens, a provider whose convention is
    /// [`CacheConvention::Unknown`], and a rate missing the cache rate a
    /// reported counter needs all price exactly as [`Self::cost`] does — the
    /// documented arithmetic from before cache rates existed, kept rather
    /// than replaced by a guessed correction. Otherwise:
    ///
    /// - **Exclusive** (the prompt count is the uncached remainder): the
    ///   prompt at the input rate, plus cache reads and writes at theirs.
    /// - **Inclusive** (the prompt count includes them): the prompt count less
    ///   the cached tokens at the input rate, plus the cached tokens at
    ///   theirs. Cache counters larger than the prompt count they are part of
    ///   contradict each other, and price as [`Self::cost`].
    pub fn cost_with_cache(&self, model: &str, tokens: &CallTokens) -> Option<f64> {
        let (prompt, completion) = (tokens.prompt?, tokens.completion?);
        let rate = self.rate(model)?;
        let plain = rate.cost(prompt, completion);
        let read = tokens.cache_read.unwrap_or(0);
        let write = tokens.cache_write.unwrap_or(0);
        let cache_rate = |count: u64, rate: Option<f64>| match (count, rate) {
            (0, _) => Some(0.0),
            (_, rate) => rate,
        };
        let uncached = match tokens.convention {
            _ if read == 0 && write == 0 => None,
            CacheConvention::Exclusive => Some(prompt),
            CacheConvention::Inclusive => prompt.checked_sub(read.saturating_add(write)),
            CacheConvention::Unknown => None,
        };
        let cost = match (
            uncached,
            cache_rate(read, rate.cache_read_per_mtok),
            cache_rate(write, rate.cache_write_per_mtok),
        ) {
            (Some(uncached), Some(read_rate), Some(write_rate)) => {
                (uncached as f64 / PER_MTOK) * rate.input_per_mtok
                    + (read as f64 / PER_MTOK) * read_rate
                    + (write as f64 / PER_MTOK) * write_rate
                    + (completion as f64 / PER_MTOK) * rate.output_per_mtok
            }
            _ => plain,
        };
        cost.is_finite().then_some(cost)
    }

    /// A stable digest of the table, for the rollup validity gate.
    ///
    /// Zero for an empty table, and deliberately: a store that prices nothing
    /// must produce byte-identical sidecars to one built before pricing
    /// existed, so adding this feature does not invalidate anybody's rollups
    /// on upgrade. Float bits rather than a formatted number, so the digest
    /// changes whenever the arithmetic would.
    pub fn fingerprint(&self) -> u64 {
        if self.is_empty() {
            return 0;
        }
        // The cache rates are fed only when present, each behind a tag byte
        // UTF-8 text can never contain (0xFE, 0xFF): a table without them
        // digests exactly as it did before they existed — upgrading
        // invalidates nobody's rollups — while adding or changing one
        // changes the digest, because it changes the arithmetic.
        fn eat_rate(stream: &mut Vec<u8>, name: &str, rate: &Rate) {
            stream.extend_from_slice(name.as_bytes());
            stream.extend_from_slice(&rate.input_per_mtok.to_bits().to_le_bytes());
            stream.extend_from_slice(&rate.output_per_mtok.to_bits().to_le_bytes());
            if let Some(read) = rate.cache_read_per_mtok {
                stream.push(0xFE);
                stream.extend_from_slice(&read.to_bits().to_le_bytes());
            }
            if let Some(write) = rate.cache_write_per_mtok {
                stream.push(0xFF);
                stream.extend_from_slice(&write.to_bits().to_le_bytes());
            }
        }
        let mut stream = Vec::new();
        // Exact entries are a BTreeMap and prefixes are sorted by the parser,
        // so both iterate in a fixed order for a given table.
        for (name, rate) in &self.exact {
            eat_rate(&mut stream, name, rate);
        }
        // A separator no model name can contain, so {"ab": r} and {"a": r}
        // with a "b" prefix cannot collide into the same byte stream.
        stream.push(0);
        for (prefix, rate) in &self.prefixes {
            eat_rate(&mut stream, prefix, rate);
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in stream {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

/// Reads one rate field, refusing what cannot be a price.
fn rate_field(value: Option<&Value>, model: &str, field: &str) -> Result<f64, String> {
    let number = value
        .ok_or_else(|| format!("pricing: {model} is missing {field}"))?
        .as_f64()
        .ok_or_else(|| format!("pricing: {model}.{field} must be a number"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!(
            "pricing: {model}.{field} must be a finite, non-negative number"
        ));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"{"models": {
        "gpt-5.6-sol": {"input_per_mtok": 1.25, "output_per_mtok": 10.0},
        "gpt-4o-mini*": {"input_per_mtok": 0.15, "output_per_mtok": 0.6},
        "gpt-4o*": {"input_per_mtok": 2.5, "output_per_mtok": 10.0}
    }}"#;

    #[test]
    fn prices_an_exact_model() {
        let pricing = Pricing::parse(TABLE).expect("parses");
        // 5138 in at $1.25/Mtok + 7488 out at $10/Mtok.
        let cost = pricing
            .cost("gpt-5.6-sol", Some(5138), Some(7488))
            .expect("priced");
        assert!((cost - (0.005138 * 1.25 + 0.007488 * 10.0)).abs() < 1e-12);
    }

    #[test]
    fn the_longest_prefix_wins_regardless_of_file_order() {
        let pricing = Pricing::parse(TABLE).expect("parses");
        assert_eq!(
            pricing
                .rate("gpt-4o-mini-2024-07-18")
                .map(|r| r.input_per_mtok),
            Some(0.15),
            "gpt-4o-mini* is longer than gpt-4o* and must win"
        );
        assert_eq!(
            pricing.rate("gpt-4o-2024-08-06").map(|r| r.input_per_mtok),
            Some(2.5)
        );
    }

    #[test]
    fn an_exact_name_beats_any_prefix() {
        let pricing = Pricing::parse(
            r#"{"models": {
                "gpt-4o*": {"input_per_mtok": 2.5, "output_per_mtok": 10.0},
                "gpt-4o-special": {"input_per_mtok": 0.0, "output_per_mtok": 0.0}
            }}"#,
        )
        .expect("parses");
        assert_eq!(
            pricing.cost("gpt-4o-special", Some(1000), Some(1000)),
            Some(0.0)
        );
    }

    #[test]
    fn refuses_to_price_a_call_with_no_direction_split() {
        // Only a total is known: input and output are priced differently, so
        // there is no honest answer here.
        let pricing = Pricing::parse(TABLE).expect("parses");
        assert_eq!(pricing.cost("gpt-5.6-sol", None, Some(7488)), None);
        assert_eq!(pricing.cost("gpt-5.6-sol", Some(5138), None), None);
    }

    #[test]
    fn an_unpriced_model_stays_unpriced() {
        let pricing = Pricing::parse(TABLE).expect("parses");
        assert_eq!(pricing.cost("some-private-model", Some(10), Some(10)), None);
    }

    #[test]
    fn a_bare_star_is_a_default_rate() {
        let pricing = Pricing::parse(
            r#"{"models": {
                "*": {"input_per_mtok": 1.0, "output_per_mtok": 1.0},
                "cheap*": {"input_per_mtok": 0.0, "output_per_mtok": 0.0}
            }}"#,
        )
        .expect("parses");
        assert_eq!(
            pricing.cost("anything", Some(1_000_000), Some(0)),
            Some(1.0)
        );
        assert_eq!(
            pricing.cost("cheap-model", Some(1_000_000), Some(0)),
            Some(0.0)
        );
    }

    #[test]
    fn the_empty_table_fingerprints_to_zero() {
        // So a store that prices nothing writes the sidecars it always wrote.
        assert_eq!(Pricing::default().fingerprint(), 0);
        assert!(Pricing::default().is_empty());
    }

    #[test]
    fn the_fingerprint_follows_the_rates_not_the_file() {
        let one = Pricing::parse(TABLE).expect("parses");
        let reordered = Pricing::parse(
            r#"{"models": {
                "gpt-4o*": {"input_per_mtok": 2.5, "output_per_mtok": 10.0},
                "gpt-4o-mini*": {"input_per_mtok": 0.15, "output_per_mtok": 0.6},
                "gpt-5.6-sol": {"input_per_mtok": 1.25, "output_per_mtok": 10.0}
            }}"#,
        )
        .expect("parses");
        assert_eq!(
            one.fingerprint(),
            reordered.fingerprint(),
            "key order is not part of the table's meaning"
        );

        let repriced = Pricing::parse(
            r#"{"models": {
                "gpt-5.6-sol": {"input_per_mtok": 1.26, "output_per_mtok": 10.0},
                "gpt-4o-mini*": {"input_per_mtok": 0.15, "output_per_mtok": 0.6},
                "gpt-4o*": {"input_per_mtok": 2.5, "output_per_mtok": 10.0}
            }}"#,
        )
        .expect("parses");
        assert_ne!(
            one.fingerprint(),
            repriced.fingerprint(),
            "a changed rate must invalidate rollups computed under the old one"
        );
    }

    #[test]
    fn a_prefix_and_an_exact_name_cannot_collide_in_the_digest() {
        let split = Pricing::parse(
            r#"{"models": {"ab": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}}"#,
        )
        .expect("parses");
        let joined = Pricing::parse(
            r#"{"models": {"a": {"input_per_mtok": 1.0, "output_per_mtok": 1.0},
                           "b*": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}}"#,
        )
        .expect("parses");
        assert_ne!(split.fingerprint(), joined.fingerprint());
    }

    #[test]
    fn rejections_name_the_model() {
        let error = Pricing::parse(r#"{"models": {"gpt-5.6-sol": {"input_per_mtok": 1.25}}}"#)
            .expect_err("missing output rate");
        assert!(error.contains("gpt-5.6-sol"), "{error}");
        assert!(error.contains("output_per_mtok"), "{error}");

        let error = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": -1.0, "output_per_mtok": 1.0}}}"#,
        )
        .expect_err("negative rate");
        assert!(error.contains("non-negative"), "{error}");

        assert!(Pricing::parse("{}").is_err(), "missing models is an error");
        assert!(Pricing::parse("not json").is_err());
    }

    const CACHED: &str = r#"{"models": {
        "claude-x": {"input_per_mtok": 3.0, "output_per_mtok": 15.0,
                     "cache_read_per_mtok": 0.3, "cache_write_per_mtok": 3.75},
        "gpt-x": {"input_per_mtok": 2.0, "output_per_mtok": 8.0,
                  "cache_read_per_mtok": 0.5},
        "bare": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}
    }}"#;

    fn call(
        prompt: u64,
        read: u64,
        write: u64,
        completion: u64,
        convention: CacheConvention,
    ) -> CallTokens {
        CallTokens {
            prompt: Some(prompt),
            completion: Some(completion),
            cache_read: Some(read),
            cache_write: Some(write),
            convention,
        }
    }

    fn close(left: Option<f64>, right: f64) -> bool {
        left.is_some_and(|left| (left - right).abs() < 1e-12)
    }

    #[test]
    fn exclusive_cache_tokens_are_priced_on_top_of_the_prompt() {
        let pricing = Pricing::parse(CACHED).expect("parses");
        // 1M uncached at $3, 2M read at $0.30, 0.5M written at $3.75, 0.1M
        // out at $15: the old arithmetic priced the 1M prompt and nothing of
        // the 2.5M cached tokens.
        let cost = pricing.cost_with_cache(
            "claude-x",
            &call(
                1_000_000,
                2_000_000,
                500_000,
                100_000,
                CacheConvention::Exclusive,
            ),
        );
        assert!(close(cost, 3.0 + 0.6 + 1.875 + 1.5), "{cost:?}");
        assert!(close(
            pricing.cost("claude-x", Some(1_000_000), Some(100_000)),
            4.5
        ));
    }

    #[test]
    fn inclusive_cache_reads_are_discounted_out_of_the_prompt() {
        let pricing = Pricing::parse(CACHED).expect("parses");
        // 1M prompt of which 0.8M were cache reads: 0.2M at $2 + 0.8M at
        // $0.50, where the old arithmetic charged all 1M at $2.
        let cost = pricing.cost_with_cache(
            "gpt-x",
            &call(1_000_000, 800_000, 0, 0, CacheConvention::Inclusive),
        );
        assert!(close(cost, 0.4 + 0.4), "{cost:?}");
        // Cache counters exceeding the prompt they are part of contradict
        // it: the plain arithmetic stands.
        let contradictory =
            pricing.cost_with_cache("gpt-x", &call(100, 800, 0, 0, CacheConvention::Inclusive));
        assert!(close(contradictory, 100.0 * 2.0 / 1e6), "{contradictory:?}");
    }

    #[test]
    fn without_the_needed_rate_or_convention_nothing_changes() {
        let pricing = Pricing::parse(CACHED).expect("parses");
        let plain = |model| pricing.cost(model, Some(1_000_000), Some(0));
        // No cache rates at all.
        assert_eq!(
            pricing.cost_with_cache(
                "bare",
                &call(1_000_000, 500_000, 0, 0, CacheConvention::Exclusive)
            ),
            plain("bare")
        );
        // A write the table has no rate for.
        assert_eq!(
            pricing.cost_with_cache(
                "gpt-x",
                &call(1_000_000, 0, 10, 0, CacheConvention::Inclusive)
            ),
            plain("gpt-x")
        );
        // An unknown convention cannot be corrected either way.
        assert_eq!(
            pricing.cost_with_cache(
                "claude-x",
                &call(1_000_000, 500_000, 0, 0, CacheConvention::Unknown)
            ),
            plain("claude-x")
        );
        // No cache tokens is the old arithmetic under every convention.
        for convention in [CacheConvention::Exclusive, CacheConvention::Inclusive] {
            assert_eq!(
                pricing.cost_with_cache("claude-x", &call(1_000_000, 0, 0, 0, convention)),
                plain("claude-x")
            );
        }
        // And both directions are still required.
        let mut partial = call(1, 1, 0, 1, CacheConvention::Exclusive);
        partial.completion = None;
        assert_eq!(pricing.cost_with_cache("claude-x", &partial), None);
    }

    #[test]
    fn a_table_without_cache_rates_digests_as_it_always_did() {
        // The digest before cache rates existed, written out: FNV-1a over
        // each exact name and its two rates' bits, a zero byte, then each
        // prefix likewise.
        fn legacy(entries: &[(&str, f64, f64)], prefixes: &[(&str, f64, f64)]) -> u64 {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            let mut eat = |bytes: &[u8]| {
                for byte in bytes {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            };
            for (name, input, output) in entries {
                eat(name.as_bytes());
                eat(&input.to_bits().to_le_bytes());
                eat(&output.to_bits().to_le_bytes());
            }
            eat(&[0]);
            for (name, input, output) in prefixes {
                eat(name.as_bytes());
                eat(&input.to_bits().to_le_bytes());
                eat(&output.to_bits().to_le_bytes());
            }
            hash
        }
        let pricing = Pricing::parse(TABLE).expect("parses");
        assert_eq!(
            pricing.fingerprint(),
            legacy(
                &[("gpt-5.6-sol", 1.25, 10.0)],
                &[("gpt-4o-mini", 0.15, 0.6), ("gpt-4o", 2.5, 10.0)]
            )
        );

        let with_read = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": 1.0, "output_per_mtok": 1.0,
                                 "cache_read_per_mtok": 0.1}}}"#,
        )
        .expect("parses");
        let with_write = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": 1.0, "output_per_mtok": 1.0,
                                 "cache_write_per_mtok": 0.1}}}"#,
        )
        .expect("parses");
        let without = legacy(&[("m", 1.0, 1.0)], &[]);
        assert_ne!(with_read.fingerprint(), without);
        assert_ne!(with_write.fingerprint(), without);
        assert_ne!(
            with_read.fingerprint(),
            with_write.fingerprint(),
            "a read rate and a write rate of the same value are different tables"
        );
    }

    #[test]
    fn a_cache_rate_is_validated_like_any_other() {
        let error = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": 1.0, "output_per_mtok": 1.0,
                                 "cache_read_per_mtok": -0.1}}}"#,
        )
        .expect_err("negative cache rate");
        assert!(error.contains("m.cache_read_per_mtok"), "{error}");
        let error = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": 1.0, "output_per_mtok": 1.0,
                                 "cache_write_per_mtok": "cheap"}}}"#,
        )
        .expect_err("non-numeric cache rate");
        assert!(error.contains("must be a number"), "{error}");
        let null = Pricing::parse(
            r#"{"models": {"m": {"input_per_mtok": 1.0, "output_per_mtok": 1.0,
                                 "cache_read_per_mtok": null}}}"#,
        )
        .expect("null is absent");
        assert_eq!(null.rate("m").and_then(|r| r.cache_read_per_mtok), None);
        assert_eq!(
            Pricing::parse(CACHED)
                .expect("parses")
                .rate("claude-x")
                .and_then(|rate| rate.cache_read_saving_per_mtok()),
            Some(3.0 - 0.3)
        );
    }
}
