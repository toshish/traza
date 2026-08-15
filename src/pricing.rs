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
//! about cached-prompt discounts, batch tiers, reasoning tokens billed at a
//! third rate, negotiated pricing, or the difference between what a vendor
//! quotes and what it invoices. **A metered cost always wins** — if a span
//! carries `llm.cost_usd`, that is the number, and the table is not consulted.
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
}

impl Rate {
    /// Cost of a call at this rate, in USD.
    fn cost(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        const PER_MTOK: f64 = 1_000_000.0;
        (prompt_tokens as f64 / PER_MTOK) * self.input_per_mtok
            + (completion_tokens as f64 / PER_MTOK) * self.output_per_mtok
    }
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
    ///   "gpt-5.6-sol":  {"input_per_mtok": 1.25, "output_per_mtok": 10.0},
    ///   "claude-opus-*": {"input_per_mtok": 15.0, "output_per_mtok": 75.0}
    /// }}
    /// ```
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
            let rate = Rate {
                input_per_mtok: rate_field(object.get("input_per_mtok"), name, "input_per_mtok")?,
                output_per_mtok: rate_field(
                    object.get("output_per_mtok"),
                    name,
                    "output_per_mtok",
                )?,
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
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        // Exact entries are a BTreeMap and prefixes are sorted by the parser,
        // so both iterate in a fixed order for a given table.
        for (name, rate) in &self.exact {
            eat(name.as_bytes());
            eat(&rate.input_per_mtok.to_bits().to_le_bytes());
            eat(&rate.output_per_mtok.to_bits().to_le_bytes());
        }
        // A separator no model name can contain, so {"ab": r} and {"a": r}
        // with a "b" prefix cannot collide into the same byte stream.
        eat(&[0]);
        for (prefix, rate) in &self.prefixes {
            eat(prefix.as_bytes());
            eat(&rate.input_per_mtok.to_bits().to_le_bytes());
            eat(&rate.output_per_mtok.to_bits().to_le_bytes());
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
}
