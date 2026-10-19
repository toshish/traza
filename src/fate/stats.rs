//! The statistics the Fate Map stands on, std-only and tested against
//! published values.
//!
//! Three tools, each chosen for what it does with SMALL samples, because
//! most branches of a behaviour tree are small:
//!
//! - **Wilson score interval** for a success rate. The textbook
//!   `p ± z·√(p(1−p)/n)` collapses to `[0, 0]` for 0 of 3 and claims
//!   certainty it does not have; Wilson's interval stays inside `[0, 1]` and
//!   is wide exactly when `n` is small, which is what keeps three unlucky
//!   runs from reading as a death sentence.
//! - **Fisher's exact test** (two-sided) for "is this branch really worse
//!   than its siblings?" — exact at any sample size, where a normal
//!   approximation is wrong precisely for the small branches a fork test
//!   cares about.
//! - **Shannon entropy** in bits of a node's next-step distribution, the
//!   "how undecided is the agent here" measure a hotspot is flagged on.

/// The two-sided 95% normal quantile, Φ⁻¹(0.975).
pub(crate) const Z95: f64 = 1.959_963_984_540_054;

/// The Wilson score interval for `successes` of `trials` at 95%, as
/// `(low, high)`. `(0, 1)` for no trials: nothing is known.
pub(crate) fn wilson(successes: u64, trials: u64) -> (f64, f64) {
    if trials == 0 {
        return (0.0, 1.0);
    }
    let n = trials as f64;
    let p = (successes.min(trials)) as f64 / n;
    let z2 = Z95 * Z95;
    let denominator = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denominator;
    let half = Z95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denominator;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

/// Shannon entropy, in bits, of the distribution `counts` describes. Zero
/// counts contribute nothing; an empty or all-zero distribution is 0.
pub(crate) fn entropy(counts: &[u64]) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / total;
            -p * p.log2()
        })
        .sum::<f64>()
        .max(0.0)
}

/// `ln(k!)` for every `k` in `0..=max`, by summation: exact to the last few
/// ulps, and a table because a fork test evaluates many of them.
pub(crate) struct LnFactorials(Vec<f64>);

impl LnFactorials {
    /// The table up to `max!`.
    pub(crate) fn up_to(max: u64) -> Self {
        let max = usize::try_from(max).unwrap_or(usize::MAX).min(1 << 24);
        let mut table = Vec::with_capacity(max + 1);
        table.push(0.0);
        let mut sum = 0.0_f64;
        for k in 1..=max {
            sum += (k as f64).ln();
            table.push(sum);
        }
        Self(table)
    }

    fn get(&self, k: u64) -> f64 {
        // Callers size the table to the largest margin; past it, extend by
        // Stirling rather than panic (never reached in practice).
        match usize::try_from(k).ok().and_then(|k| self.0.get(k)) {
            Some(value) => *value,
            None => {
                let k = k as f64;
                k * k.ln() - k + 0.5 * (2.0 * std::f64::consts::PI * k).ln() + 1.0 / (12.0 * k)
            }
        }
    }

    /// `ln C(n, k)`.
    fn ln_choose(&self, n: u64, k: u64) -> f64 {
        self.get(n) - self.get(k) - self.get(n - k)
    }
}

/// Two-sided Fisher exact test p-value for the 2×2 table
///
/// ```text
///            success  failure
/// branch        a        b
/// siblings      c        d
/// ```
///
/// The sum of the probabilities of every table with the same margins that
/// is no more likely than the observed one (the "minimum likelihood"
/// definition R's `fisher.test` uses), with a relative tolerance so that
/// tables tied with the observed one in exact arithmetic are not lost to
/// rounding. `facts` must cover `a + b + c + d`.
pub(crate) fn fisher_two_sided(a: u64, b: u64, c: u64, d: u64, facts: &LnFactorials) -> f64 {
    let row1 = a + b;
    let row2 = c + d;
    let col1 = a + c;
    let n = row1 + row2;
    if n == 0 || row1 == 0 || row2 == 0 || col1 == 0 || col1 == n {
        return 1.0;
    }
    let ln_total = facts.ln_choose(n, col1);
    let probability = |x: u64| -> f64 {
        (facts.ln_choose(row1, x) + facts.ln_choose(row2, col1 - x) - ln_total).exp()
    };
    let low = col1.saturating_sub(row2);
    let high = row1.min(col1);
    let observed = probability(a);
    let threshold = observed * (1.0 + 1e-7);
    let mut p = 0.0;
    for x in low..=high {
        let px = probability(x);
        if px <= threshold {
            p += px;
        }
    }
    p.clamp(0.0, 1.0)
}

/// `value` rounded to four decimal places, for the wire: a rate printed to
/// seventeen digits claims a precision a sample of thirty runs does not
/// have.
pub(crate) fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64, tolerance: f64) -> bool {
        (actual - expected).abs() <= tolerance
    }

    #[test]
    fn wilson_matches_published_values() {
        // Newcombe (1998), Table I: 81/263 → 0.2553–0.3662;
        // 15/148 → 0.0624–0.1605; 0/20 → 0–0.1611; 1/29 → 0.0061–0.1718.
        for (successes, trials, low, high) in [
            (81, 263, 0.2553, 0.3662),
            (15, 148, 0.0624, 0.1605),
            (0, 20, 0.0, 0.1611),
            (1, 29, 0.0061, 0.1718),
        ] {
            let (l, h) = wilson(successes, trials);
            assert!(
                close(l, low, 5e-4) && close(h, high, 5e-4),
                "{successes}/{trials}: got ({l:.4}, {h:.4}), want ({low}, {high})"
            );
        }
        // The contract's worked example: 3 of 112 → about 1.0%–7.6%.
        let (l, h) = wilson(3, 112);
        assert!(close(l, 0.0092, 5e-4) && close(h, 0.0758, 5e-4), "{l} {h}");
        assert_eq!(wilson(0, 0), (0.0, 1.0));
        let (l, h) = wilson(5, 5);
        assert!(h <= 1.0 && l > 0.5);
    }

    /// Φ, the standard normal CDF, by its Taylor series about zero:
    /// `Φ(x) = ½ + φ(x)·Σ x^(2n+1) / (1·3·5···(2n+1))`, every term positive,
    /// so summing to convergence is exact to a few ulps for moderate `x`.
    fn normal_cdf(x: f64) -> f64 {
        let density = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
        let (mut term, mut sum, mut odd) = (x, x, 1.0);
        while term.abs() > 1e-18 * sum.abs() {
            odd += 2.0;
            term *= x * x / odd;
            sum += term;
        }
        0.5 + density * sum
    }

    #[test]
    fn the_quantile_is_the_two_sided_ninety_five_percent_one() {
        // Every interval this module labels "95%" stands on this constant:
        // Φ(Z95) must be 0.975, not 0.95 (which 1.645 would give).
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-15);
        assert!((normal_cdf(1.0) - 0.841_344_746_068_543).abs() < 1e-13);
        assert!(
            (normal_cdf(Z95) - 0.975).abs() < 1e-12,
            "Φ({Z95}) = {}",
            normal_cdf(Z95)
        );
    }

    #[test]
    fn entropy_is_in_bits() {
        assert!(close(entropy(&[1, 1]), 1.0, 1e-12));
        assert!(close(entropy(&[5, 5, 5, 5]), 2.0, 1e-12));
        assert_eq!(entropy(&[7]), 0.0);
        assert_eq!(entropy(&[]), 0.0);
        assert_eq!(entropy(&[0, 0]), 0.0);
        // H(1/2, 1/4, 1/4) = 1.5 bits.
        assert!(close(entropy(&[2, 1, 1]), 1.5, 1e-12));
    }

    #[test]
    fn fisher_matches_r() {
        let facts = LnFactorials::up_to(400);
        // R: fisher.test(matrix(c(3,1,1,3),2))$p.value = 0.4857143
        assert!(close(
            fisher_two_sided(3, 1, 1, 3, &facts),
            0.485_714_3,
            1e-6
        ));
        // R: fisher.test(matrix(c(1,9,11,3),2))$p.value = 0.002759456
        //    (a=1,b=11 | c=9,d=3 in row-major branch/sibling order)
        assert!(close(
            fisher_two_sided(1, 11, 9, 3, &facts),
            0.002_759_456,
            1e-8
        ));
        // R: fisher.test(matrix(c(10,40,93,37),2))$p.value ≈ 1.1e-12-ish:
        // a strongly lopsided branch is overwhelmingly significant.
        assert!(fisher_two_sided(10, 40, 93, 37, &facts) < 1e-9);
        // Identical rows are no evidence at all.
        assert!(close(fisher_two_sided(5, 5, 5, 5, &facts), 1.0, 1e-9));
        // Degenerate margins.
        assert_eq!(fisher_two_sided(0, 0, 3, 4, &facts), 1.0);
        assert_eq!(fisher_two_sided(3, 0, 4, 0, &facts), 1.0);
    }

    #[test]
    fn rounding_keeps_four_places() {
        assert_eq!(round4(0.123_456), 0.1235);
        assert_eq!(round4(1.0), 1.0);
        assert_eq!(round4(0.0), 0.0);
    }
}
