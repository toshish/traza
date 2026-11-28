//! The statistics the change radar's verdicts rest on, from first principles.
//!
//! No dependency is allowed (doctrine: serde, serde_json and lz4_flex only),
//! so every distribution function a p-value needs is implemented here and
//! pinned by tests against published reference values (computed with SciPy
//! 1.14; each test names its value):
//!
//! - the standard normal CDF, through `erfc` by the Chebyshev fit of
//!   *Numerical Recipes* (§6.2) — fractional error below 1.2e-7 everywhere,
//!   tails included, which is what a p-value of 1e-9 needs and what the
//!   common Abramowitz–Stegun 7.1.26 fit (absolute error 1.5e-7) does not
//!   give;
//! - Student's t two-sided tail, as the regularized incomplete beta function
//!   `I_x(ν/2, 1/2)` with `x = ν/(ν+t²)`, evaluated by Lentz's continued
//!   fraction, with `ln Γ` by the Lanczos approximation (g = 7, n = 9);
//! - the two-proportion z-test (pooled), and Fisher's exact test for the
//!   sparse tables where the z-test's normal approximation overstates
//!   significance (an expected cell count below 5), Welch's unequal-variance
//!   t-test with Welch–Satterthwaite degrees of freedom, Holm's step-down
//!   correction, and the Jensen–Shannon distance (base 2, so it lies in
//!   `[0, 1]`).
//!
//! Every function returns `None` rather than a number when its inputs cannot
//! support one — an empty side, no variation at all — because a p-value
//! computed from nothing is the most convincing kind of wrong answer.

use std::collections::HashMap;
use std::hash::Hash;

/// The largest |Cohen's d| reported (see [`cohens_d`]).
pub(crate) const MAX_COHENS_D: f64 = 4.0;

/// Standard normal cumulative distribution function `Φ(x)`.
pub(crate) fn normal_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Two-sided tail probability of a standard normal statistic `z`:
/// `2·Φ(−|z|)`, computed from the lower tail directly so it keeps its
/// relative accuracy far out (no `1 − 0.999999…` cancellation).
pub(crate) fn normal_two_sided(z: f64) -> f64 {
    if z.is_nan() {
        return 1.0;
    }
    (2.0 * normal_cdf(-z.abs())).min(1.0)
}

/// The complementary error function, *Numerical Recipes* `erfcc`: fractional
/// error below 1.2e-7 for every real argument.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let value = t * poly.exp();
    if x >= 0.0 {
        value
    } else {
        2.0 - value
    }
}

/// `ln Γ(x)` for `x > 0`, Lanczos (g = 7, n = 9): about 15 significant
/// digits over the range the t distribution uses.
pub(crate) fn ln_gamma(x: f64) -> f64 {
    const COEFFICIENTS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection: Γ(x)Γ(1−x) = π / sin(πx).
        let pi = std::f64::consts::PI;
        return (pi / (pi * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut sum = COEFFICIENTS[0];
    for (index, coefficient) in COEFFICIENTS.iter().enumerate().skip(1) {
        sum += coefficient / (x + index as f64);
    }
    let t = x + 7.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

/// The regularized incomplete beta function `I_x(a, b)` for `a, b > 0` and
/// `0 ≤ x ≤ 1` (*Numerical Recipes* `betai`, continued fraction by Lentz).
pub(crate) fn incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front =
        (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    // The continued fraction converges fast for x < (a+1)/(a+b+2); past it
    // the symmetry I_x(a,b) = 1 − I_{1−x}(b,a) is used instead.
    if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_fraction(a, b, x) / a
    } else {
        1.0 - front * beta_fraction(b, a, 1.0 - x) / b
    }
}

/// The continued fraction of [`incomplete_beta`], by the modified Lentz
/// method. It converges in O(√max(a,b)) terms — about 700 for a million
/// degrees of freedom — so the cap only bounds the loop.
fn beta_fraction(a: f64, b: f64, x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    const EPSILON: f64 = 1e-15;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=20_000 {
        let m = f64::from(m);
        let m2 = 2.0 * m;
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let step = d * c;
        h *= step;
        if (step - 1.0).abs() < EPSILON {
            break;
        }
    }
    h
}

/// Two-sided tail probability of Student's t with `df` degrees of freedom:
/// `P(|T| ≥ |t|) = I_{ν/(ν+t²)}(ν/2, 1/2)`.
pub(crate) fn student_t_two_sided(t: f64, df: f64) -> f64 {
    if t.is_nan() || df.is_nan() || df <= 0.0 {
        return 1.0;
    }
    if t.is_infinite() {
        return 0.0;
    }
    let x = df / (df + t * t);
    incomplete_beta(df / 2.0, 0.5, x).clamp(0.0, 1.0)
}

/// A two-sample test's result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TestResult {
    /// The statistic (z or t), signed `after − before`.
    pub(crate) statistic: f64,
    /// Two-sided p-value.
    pub(crate) p_value: f64,
}

/// The pooled two-proportion z-test of `x1/n1` (before) against `x2/n2`
/// (after). `None` when a side is empty or there is no variation at all
/// (every run on both sides the same), where the test has no denominator.
pub(crate) fn two_proportion(x1: usize, n1: usize, x2: usize, n2: usize) -> Option<TestResult> {
    if n1 == 0 || n2 == 0 {
        return None;
    }
    let (n1f, n2f) = (n1 as f64, n2 as f64);
    let pooled = (x1 + x2) as f64 / (n1f + n2f);
    let variance = pooled * (1.0 - pooled) * (1.0 / n1f + 1.0 / n2f);
    if variance <= 0.0 {
        return None;
    }
    let z = (x2 as f64 / n2f - x1 as f64 / n1f) / variance.sqrt();
    Some(TestResult {
        statistic: z,
        p_value: normal_two_sided(z),
    })
}

/// The smallest expected cell count at which [`proportions`] trusts the
/// z-test's normal approximation: Cochran's rule for a 2×2 table.
pub(crate) const MIN_EXPECTED_CELL: f64 = 5.0;

/// Fisher's exact test of `x1/n1` (before) against `x2/n2` (after),
/// two-sided: conditioned on the table's margins, the probability of a
/// table no more likely than the one observed (relative tolerance `1e-7`,
/// so a table that ties the observed one in exact arithmetic counts, as R's
/// `fisher.test` and SciPy's `fisher_exact` count it). `None` when a side is
/// empty.
///
/// The hypergeometric probabilities are built by their ratio recurrence
/// outward from the mode, each relative to the mode's (so none overflows),
/// and normalized by their own sum — no factorial is evaluated, and a table
/// whose every outcome counts is exactly `1`. The support, and the one
/// vector this holds, has at most `min(n1, n2) + 1` entries.
pub(crate) fn fisher_exact(x1: usize, n1: usize, x2: usize, n2: usize) -> Option<f64> {
    if n1 == 0 || n2 == 0 || x1 > n1 || x2 > n2 {
        return None;
    }
    let total = n1 + n2;
    let hits = x1 + x2;
    let misses = total - hits;
    // X = k: k of the `hits` fall in the first row, for k in [low, high].
    let (low, high) = (n1.saturating_sub(misses), hits.min(n1));
    // P(X = k + 1) / P(X = k); `misses + k + 1 > n1` because `k >= low`.
    let up = |k: usize| {
        ((hits - k) as f64 * (n1 - k) as f64) / ((k + 1) as f64 * (misses + k + 1 - n1) as f64)
    };
    let mode = ((n1 + 1) * (hits + 1) / (total + 2)).clamp(low, high);
    let mut weights = vec![0.0f64; high - low + 1];
    weights[mode - low] = 1.0;
    for k in mode..high {
        weights[k + 1 - low] = weights[k - low] * up(k);
    }
    for k in (low..mode).rev() {
        weights[k - low] = weights[k + 1 - low] / up(k);
    }
    let threshold = weights[x1 - low] * (1.0 + 1e-7);
    let (mut extreme, mut all) = (0.0f64, 0.0f64);
    for &weight in &weights {
        all += weight;
        if weight <= threshold {
            extreme += weight;
        }
    }
    Some((extreme / all).clamp(0.0, 1.0))
}

/// The two-proportion comparison the radar's rates use: the pooled z-test
/// when every expected cell count of the 2×2 table is at least
/// [`MIN_EXPECTED_CELL`], and [`fisher_exact`] when one is not — where the
/// normal approximation overstates significance (1 of 1 against 0 of 10 is
/// z's p = 0.0009 and the exact p = 1/11). `None` exactly when
/// [`two_proportion`] is: an empty side, or no variation at all. The
/// statistic is z's either way; its sign is the direction of the move.
pub(crate) fn proportions(x1: usize, n1: usize, x2: usize, n2: usize) -> Option<TestResult> {
    let z = two_proportion(x1, n1, x2, n2)?;
    let total = n1 + n2;
    let hits = x1 + x2;
    let smallest = n1.min(n2) as f64 * hits.min(total - hits) as f64 / total as f64;
    if smallest >= MIN_EXPECTED_CELL {
        return Some(z);
    }
    Some(TestResult {
        statistic: z.statistic,
        p_value: fisher_exact(x1, n1, x2, n2)?,
    })
}

/// Differences smaller than this, relative to the magnitude of the values
/// compared, are floating-point rounding and not data.
const RELATIVE_TOLERANCE: f64 = 1e-12;

/// The absolute tolerance for comparing two means (and a standard deviation
/// against zero): [`RELATIVE_TOLERANCE`] of the larger magnitude, at least
/// of 1.
fn tolerance(m1: f64, m2: f64) -> f64 {
    RELATIVE_TOLERANCE * m1.abs().max(m2.abs()).max(1.0)
}

/// Mean and sample variance (n − 1 denominator), computed relative to the
/// first value so they are shift-invariant: a constant sample has exactly
/// its value as mean and exactly zero variance, whatever its length. (A
/// plain `Σx / n` rounds differently for 356 copies of `ln 3` than for 122,
/// and the residue would read as spread.)
pub(crate) fn mean_variance(values: &[f64]) -> Option<(f64, f64)> {
    let first = *values.first()?;
    let n = values.len() as f64;
    let offset = values.iter().map(|value| value - first).sum::<f64>() / n;
    let mean = first + offset;
    if values.len() < 2 {
        return Some((mean, 0.0));
    }
    let variance = values
        .iter()
        .map(|value| {
            let deviation = (value - first) - offset;
            deviation * deviation
        })
        .sum::<f64>()
        / (n - 1.0);
    Some((mean, variance))
}

/// A variance that is rounding residue — a standard deviation below the
/// tolerance of the means it belongs to — as exactly zero.
fn settled(variance: f64, tolerance: f64) -> f64 {
    if variance < tolerance * tolerance {
        0.0
    } else {
        variance
    }
}

/// Welch's unequal-variance t-test of `after` against `before`, with
/// Welch–Satterthwaite degrees of freedom. `None` with fewer than two values
/// on a side. Two sides with no variance at all are `p = 1` when their means
/// agree and `p = 0` when they do not: a deterministic difference. "No
/// variance" and "agree" are judged with a relative tolerance of `1e-12`,
/// so rounding is never mistaken for spread or for a difference.
pub(crate) fn welch(before: &[f64], after: &[f64]) -> Option<TestResult> {
    if before.len() < 2 || after.len() < 2 {
        return None;
    }
    let (m1, v1) = mean_variance(before)?;
    let (m2, v2) = mean_variance(after)?;
    let tolerance = tolerance(m1, m2);
    let (v1, v2) = (settled(v1, tolerance), settled(v2, tolerance));
    let (n1, n2) = (before.len() as f64, after.len() as f64);
    let se2 = v1 / n1 + v2 / n2;
    if se2 <= 0.0 {
        let p_value = if (m2 - m1).abs() <= tolerance {
            1.0
        } else {
            0.0
        };
        let statistic = if p_value == 1.0 {
            0.0
        } else {
            (m2 - m1).signum() * f64::INFINITY
        };
        return Some(TestResult { statistic, p_value });
    }
    let t = (m2 - m1) / se2.sqrt();
    let df = se2 * se2 / ((v1 / n1).powi(2) / (n1 - 1.0) + (v2 / n2).powi(2) / (n2 - 1.0));
    Some(TestResult {
        statistic: t,
        p_value: student_t_two_sided(t, df),
    })
}

/// Cohen's h between two proportions, signed `after − before`.
pub(crate) fn cohens_h(before: f64, after: f64) -> f64 {
    2.0 * after.clamp(0.0, 1.0).sqrt().asin() - 2.0 * before.clamp(0.0, 1.0).sqrt().asin()
}

/// Cohen's d between two samples (pooled standard deviation), signed
/// `after − before`, clamped to `±`[`MAX_COHENS_D`]: past about 4 the two
/// sides barely overlap, and an unbounded d would let one deterministic
/// difference drown every other effect size. With no spread at all, equal
/// means are `0` and different ones the cap — a difference every run shows
/// is as large an effect as can be stated. Spread and equality are judged
/// with the tolerance [`welch`] uses.
pub(crate) fn cohens_d(before: &[f64], after: &[f64]) -> f64 {
    let (Some((m1, v1)), Some((m2, v2))) = (mean_variance(before), mean_variance(after)) else {
        return 0.0;
    };
    let tolerance = tolerance(m1, m2);
    let (n1, n2) = (before.len() as f64, after.len() as f64);
    let pooled = if n1 + n2 > 2.0 {
        (((n1 - 1.0) * v1 + (n2 - 1.0) * v2) / (n1 + n2 - 2.0)).sqrt()
    } else {
        0.0
    };
    let difference = m2 - m1;
    if difference.abs() <= tolerance {
        0.0
    } else if pooled < tolerance {
        difference.signum() * MAX_COHENS_D
    } else {
        (difference / pooled).clamp(-MAX_COHENS_D, MAX_COHENS_D)
    }
}

/// Holm's step-down procedure: which of `p_values` are significant with the
/// family-wise error rate held at `alpha`. Order-preserving.
pub(crate) fn holm(p_values: &[f64], alpha: f64) -> Vec<bool> {
    let mut order: Vec<usize> = (0..p_values.len()).collect();
    order.sort_by(|&a, &b| {
        p_values[a]
            .partial_cmp(&p_values[b])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let tests = p_values.len();
    let mut significant = vec![false; tests];
    for (rank, &index) in order.iter().enumerate() {
        if p_values[index] <= alpha / (tests - rank) as f64 {
            significant[index] = true;
        } else {
            break;
        }
    }
    significant
}

/// Jensen–Shannon distance (the square root of the divergence, base 2)
/// between two empirical distributions given as counts with their totals.
/// `0` for identical distributions, `1` for disjoint ones; `None` when either
/// side is empty.
pub(crate) fn js_distance<K: Eq + Hash>(
    left: &HashMap<K, usize>,
    left_total: usize,
    right: &HashMap<K, usize>,
    right_total: usize,
) -> Option<f64> {
    if left_total == 0 || right_total == 0 {
        return None;
    }
    let (lt, rt) = (left_total as f64, right_total as f64);
    let mut divergence = 0.0;
    let term = |p: f64, m: f64| if p > 0.0 { p * (p / m).log2() } else { 0.0 };
    for (key, &count) in left {
        let p = count as f64 / lt;
        let q = right.get(key).copied().unwrap_or(0) as f64 / rt;
        let m = 0.5 * (p + q);
        divergence += 0.5 * term(p, m) + 0.5 * term(q, m);
    }
    for (key, &count) in right {
        if left.contains_key(key) {
            continue;
        }
        let q = count as f64 / rt;
        divergence += 0.5 * term(q, 0.5 * q);
    }
    Some(divergence.clamp(0.0, 1.0).sqrt())
}

/// [`js_distance`] over two slices of labels.
pub(crate) fn js_of<K: Eq + Hash + Copy>(left: &[K], right: &[K]) -> Option<f64> {
    js_distance(&counts(left), left.len(), &counts(right), right.len())
}

/// Occurrences of each label.
pub(crate) fn counts<K: Eq + Hash + Copy>(labels: &[K]) -> HashMap<K, usize> {
    let mut counts = HashMap::new();
    for label in labels {
        *counts.entry(*label).or_insert(0) += 1;
    }
    counts
}

/// The Jensen–Shannon distance of two count vectors over the same dense
/// labels, with their totals; `0` when either side is empty.
fn js_of_counts(left: &[usize], left_total: usize, right: &[usize], right_total: usize) -> f64 {
    if left_total == 0 || right_total == 0 {
        return 0.0;
    }
    let (lt, rt) = (left_total as f64, right_total as f64);
    let term = |p: f64, m: f64| if p > 0.0 { p * (p / m).log2() } else { 0.0 };
    let mut divergence = 0.0;
    for (&l, &r) in left.iter().zip(right) {
        if l == 0 && r == 0 {
            continue;
        }
        let (p, q) = (l as f64 / lt, r as f64 / rt);
        let m = 0.5 * (p + q);
        divergence += 0.5 * term(p, m) + 0.5 * term(q, m);
    }
    divergence.clamp(0.0, 1.0).sqrt()
}

/// A deterministic permutation test of a Jensen–Shannon distance: the share
/// of `permutations` random relabellings of `left ++ right` whose distance is
/// at least `observed`, as `(1 + hits) / (1 + permutations)` — never zero,
/// and with resolution `1 / (1 + permutations)`.
///
/// The generator (xorshift64*) is seeded by the caller, so the same data and
/// seed give the same p-value on every run and every machine: a verdict that
/// changed between two identical requests would be worse than none.
pub(crate) fn js_permutation_p<K: Eq + Hash + Copy>(
    left: &[K],
    right: &[K],
    observed: f64,
    permutations: usize,
    seed: u64,
) -> f64 {
    js_permutation_test(left, right, observed, permutations, seed, None)
}

/// [`js_permutation_p`], stopping early once the p-value can no longer fall
/// below `give_up`: the value returned then is the p-value so far, a lower
/// bound on the full one and itself at or above `give_up`. A caller that only
/// needs to know whether `p < give_up` pays for the full run only when it
/// is. Labels are mapped to dense indices once, so each relabelling costs
/// one shuffle and one count.
pub(crate) fn js_permutation_test<K: Eq + Hash + Copy>(
    left: &[K],
    right: &[K],
    observed: f64,
    permutations: usize,
    seed: u64,
    give_up: Option<f64>,
) -> f64 {
    let mut ids: HashMap<K, usize> = HashMap::new();
    let mut pooled: Vec<usize> = left
        .iter()
        .chain(right)
        .map(|label| {
            let next = ids.len();
            *ids.entry(*label).or_insert(next)
        })
        .collect();
    let labels = ids.len();
    let mut totals = vec![0usize; labels];
    for &label in &pooled {
        totals[label] += 1;
    }
    let split = left.len();
    let right_total = pooled.len() - split;
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut left_counts = vec![0usize; labels];
    let mut right_counts = vec![0usize; labels];
    let mut hits = 0usize;
    for _ in 0..permutations {
        for index in (1..pooled.len()).rev() {
            let other = (next() % (index as u64 + 1)) as usize;
            pooled.swap(index, other);
        }
        left_counts.iter_mut().for_each(|count| *count = 0);
        for &label in &pooled[..split] {
            left_counts[label] += 1;
        }
        for ((right, total), left) in right_counts.iter_mut().zip(&totals).zip(&left_counts) {
            *right = total - left;
        }
        let distance = js_of_counts(&left_counts, split, &right_counts, right_total);
        if distance >= observed - 1e-12 {
            hits += 1;
            let floor = (1 + hits) as f64 / (1 + permutations) as f64;
            if give_up.is_some_and(|give_up| floor >= give_up) {
                return floor;
            }
        }
    }
    (1 + hits) as f64 / (1 + permutations) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64, relative: f64) {
        let error = ((actual - expected) / expected).abs();
        assert!(
            error <= relative,
            "{actual} vs {expected}: relative error {error:e} > {relative:e}"
        );
    }

    #[test]
    fn the_normal_cdf_matches_published_values() {
        // The fit's stated bound is a fractional error below 1.2e-7.
        close(normal_cdf(0.0), 0.5, 1.2e-7);
        close(normal_cdf(1.959964), 0.975_000_000_9, 1e-7);
        close(normal_cdf(-1.0), 0.158_655_253_931_457_07, 2e-7);
        close(normal_cdf(3.0), 0.998_650_101_968_369_9, 1e-7);
        // Far tails keep their RELATIVE accuracy, which is what a small
        // p-value needs.
        close(normal_cdf(-6.0), 9.865_876_450_376_944e-10, 2e-7);
        close(normal_two_sided(5.0), 5.733_031_437_583_869e-7, 2e-7);
        close(normal_two_sided(8.0), 1.244_192_114_854_348e-15, 2e-7);
        close(normal_two_sided(-1.959964), 0.05, 2e-7);
        assert_eq!(normal_two_sided(0.0), 1.0);
    }

    #[test]
    fn ln_gamma_and_the_incomplete_beta_match_published_values() {
        close(ln_gamma(0.5), 0.572_364_942_924_700_4, 1e-12);
        close(ln_gamma(10.0), 12.801_827_480_081_467, 1e-12);
        close(ln_gamma(100.5), 361.435_540_467_767_7, 1e-12);
        close(ln_gamma(1e-3), 6.907_178_885_383_854, 1e-10);
        close(incomplete_beta(2.0, 3.0, 0.4), 0.5248, 1e-10);
        close(
            incomplete_beta(0.5, 0.5, 0.1),
            0.204_832_764_699_133_45,
            1e-10,
        );
        close(
            incomplete_beta(5.0, 0.5, 0.9),
            0.316_642_915_020_012_2,
            1e-10,
        );
        assert_eq!(incomplete_beta(2.0, 2.0, 0.0), 0.0);
        assert_eq!(incomplete_beta(2.0, 2.0, 1.0), 1.0);
    }

    #[test]
    fn student_t_tails_match_published_values() {
        // (df, t, two-sided p) from scipy.stats.t.sf.
        for (df, t, p) in [
            (1.0, 1.0, 0.5),
            (10.0, 2.228139, 0.049_999_987_452_882_53),
            (30.0, 2.042272, 0.050_000_048_004_615_03),
            (5.0, 4.032143, 0.009_999_999_837_548_53),
            (2.0, 4.302653, 0.049_999_994_182_204_47),
            (10.0, 2.0, 0.073_388_034_770_740_39),
            (3.7, 1.5, 0.213_598_169_202_013_3),
            (1e6, 1.96, 0.049_996_067_585_269_78),
            (24.9, 2.46, 0.021_185_467_288_960_63),
            (1.0, 50.0, 0.012_730_698_201_945_594),
            (200.0, 0.1, 0.920_444_492_504_320_9),
        ] {
            close(student_t_two_sided(t, df), p, 1e-8);
            close(student_t_two_sided(-t, df), p, 1e-8);
        }
        assert_eq!(student_t_two_sided(f64::INFINITY, 3.0), 0.0);
        assert_eq!(student_t_two_sided(1.0, 0.0), 1.0);
    }

    #[test]
    fn welch_matches_scipy_on_the_textbook_samples() {
        // The two worked examples of Welch's test (Wikipedia), checked with
        // scipy.stats.ttest_ind(equal_var=False).
        let a1 = [
            27.5, 21.0, 19.0, 23.6, 17.0, 17.9, 16.9, 20.1, 21.9, 22.6, 23.1, 19.6, 19.0, 21.7,
            21.4,
        ];
        let a2 = [
            27.1, 22.0, 20.8, 23.4, 23.4, 23.5, 25.8, 22.0, 24.8, 20.2, 21.9, 22.1, 22.9, 20.5,
            24.4,
        ];
        let result = welch(&a1, &a2).expect("computes");
        close(result.statistic, 2.455_356_398_286_006, 1e-12);
        close(result.p_value, 0.021_378_001_462_866_985, 1e-8);
        let b1 = [17.2, 20.9, 22.6, 18.1, 21.7, 21.4, 23.5, 24.2, 14.7, 21.8];
        let b2 = [
            21.5, 22.8, 21.0, 23.0, 21.6, 23.6, 22.5, 20.7, 23.4, 21.8, 20.7, 21.7, 21.5, 22.5,
            23.6, 21.5, 22.5, 23.5, 21.5, 21.8,
        ];
        let result = welch(&b1, &b2).expect("computes");
        close(result.statistic, 1.565_433_523_598_503_7, 1e-12);
        close(result.p_value, 0.148_841_696_605_328_34, 1e-8);
        // Degenerate sides.
        assert_eq!(welch(&[1.0], &[1.0, 2.0]), None);
        assert_eq!(
            welch(&[2.0, 2.0], &[2.0, 2.0]).map(|r| r.p_value),
            Some(1.0)
        );
        assert_eq!(
            welch(&[2.0, 2.0], &[3.0, 3.0]).map(|r| r.p_value),
            Some(0.0)
        );
    }

    #[test]
    fn constant_sides_are_no_difference_whatever_their_lengths() {
        // A single-call agent: every run's ln(1 + steps) is ln 2, every
        // run's duration the same. Summing 356 copies and 122 copies of one
        // value rounds differently, and that rounding must not read as a
        // variance, a difference, or an effect.
        for value in [3f64.ln(), 2f64.ln_1p(), (15.0f64 / 1e6).ln_1p(), 0.1 + 0.2] {
            let before = vec![value; 356];
            let after = vec![value; 122];
            let (mean, variance) = mean_variance(&before).expect("non-empty");
            assert_eq!((mean, variance), (value, 0.0), "{value}");
            let result = welch(&before, &after).expect("two sides");
            assert_eq!(result.p_value, 1.0, "{value}: {result:?}");
            assert_eq!(result.statistic, 0.0, "{value}: {result:?}");
            assert_eq!(cohens_d(&before, &after), 0.0, "{value}");
        }
        // Two values one ulp apart are one value: 0.1 + 0.2 and 0.3, mixed in
        // different proportions on each side.
        let (a, b) = (0.1 + 0.2, 0.3);
        assert_ne!(a, b);
        let before: Vec<f64> = (0..200).map(|i| if i % 2 == 0 { a } else { b }).collect();
        let after: Vec<f64> = (0..200).map(|i| if i % 4 == 0 { b } else { a }).collect();
        let result = welch(&before, &after).expect("two sides");
        assert_eq!(result.p_value, 1.0, "{result:?}");
        assert_eq!(cohens_d(&before, &after), 0.0);
        // Where the residue survives the arithmetic — a side of 1 and of
        // 1 + 4ε has a "variance" of about 1e-31, and the means differ by 2ε —
        // the tolerance still reads it as no spread and no difference.
        let near = 1.0 + 4.0 * f64::EPSILON;
        let before: Vec<f64> = (0..200)
            .map(|i| if i % 2 == 0 { 1.0 } else { near })
            .collect();
        let after = vec![near; 200];
        let (_, variance) = mean_variance(&before).expect("non-empty");
        assert!(variance > 0.0, "the residue is there: {variance:e}");
        let result = welch(&before, &after).expect("two sides");
        assert_eq!(result.p_value, 1.0, "{result:?}");
        assert_eq!(cohens_d(&before, &after), 0.0);
    }

    #[test]
    fn a_difference_with_no_spread_is_certain_and_its_effect_is_bounded() {
        // Every run took 3 steps, then every run took 4: a deterministic
        // difference, as large an effect as can be stated — and no larger
        // than the cap, so it cannot drown every other effect size.
        let before = vec![3f64.ln_1p(); 50];
        let after = vec![4f64.ln_1p(); 70];
        let result = welch(&before, &after).expect("two sides");
        assert_eq!(result.p_value, 0.0);
        assert!(result.statistic > 0.0);
        assert_eq!(cohens_d(&before, &after), MAX_COHENS_D);
        assert_eq!(cohens_d(&after, &before), -MAX_COHENS_D);
        // A near-constant side keeps a real (bounded) effect.
        let mut spread = vec![1.0; 99];
        spread.push(1.5);
        let d = cohens_d(&spread, &[2.0; 100]);
        assert!(d > 0.0 && d <= MAX_COHENS_D, "{d}");
    }

    #[test]
    fn the_two_proportion_test_matches_scipy() {
        let result = two_proportion(30, 100, 45, 100).expect("computes");
        close(result.statistic, 2.190_890_230_020_664_7, 1e-12);
        close(result.p_value, 0.028_459_736_916_310_555, 1e-7);
        let result = two_proportion(4, 120, 16, 85).expect("computes");
        close(result.statistic, 3.682_417_871_161_343, 1e-12);
        close(result.p_value, 0.000_231_032_308_566_074_1, 2e-7);
        assert_eq!(two_proportion(0, 10, 0, 10), None, "no variation");
        assert_eq!(two_proportion(10, 10, 10, 10), None, "no variation");
        assert_eq!(two_proportion(1, 0, 1, 10), None, "an empty side");
    }

    #[test]
    fn fishers_exact_test_matches_scipy() {
        // (x1, n1, x2, n2, scipy.stats.fisher_exact([[x1, n1-x1], [x2, n2-x2]]).pvalue)
        for (x1, n1, x2, n2, p) in [
            (1, 1, 0, 10, 0.090_909_090_909_090_91),
            (0, 12, 5, 12, 0.037_267_080_745_341_61),
            (12, 12, 7, 12, 0.037_267_080_745_341_62),
            (0, 10, 4, 10, 0.086_687_306_501_547_99),
            (10, 10, 0, 10, 1.082_508_822_446_903e-5),
            (1, 40, 8, 40, 0.028_888_503_554_707_81),
            (0, 20, 6, 20, 0.020_196_020_196_020_2),
            (0, 100, 3, 10, 0.000_556_018_904_642_757_8),
            (3, 60, 24, 60, 5.008_189_159_570_698e-6),
            (30, 100, 45, 100, 0.040_534_006_514_982_64),
            (4, 120, 16, 85, 0.000_468_764_607_206_940_75),
        ] {
            let actual = fisher_exact(x1, n1, x2, n2).expect("computes");
            close(actual, p, 1e-9);
        }
        assert_eq!(fisher_exact(5, 2000, 0, 10), Some(1.0));
        assert_eq!(fisher_exact(1, 0, 1, 10), None, "an empty side");
    }

    #[test]
    fn sparse_tables_are_tested_exactly_and_ample_ones_by_z() {
        // Every expected count at least 5: the z-test, as before.
        let ample = proportions(3, 60, 24, 60).expect("computes");
        assert_eq!(Some(ample), two_proportion(3, 60, 24, 60));
        // An expected count below 5: the exact test, keeping z's direction.
        for (x1, n1, x2, n2) in [(1, 1, 0, 10), (0, 20, 6, 20), (0, 100, 3, 10)] {
            let sparse = proportions(x1, n1, x2, n2).expect("computes");
            let z = two_proportion(x1, n1, x2, n2).expect("computes");
            assert_eq!(sparse.p_value, fisher_exact(x1, n1, x2, n2).expect("exact"));
            assert!(
                sparse.p_value > z.p_value,
                "z overstates: {sparse:?} vs {z:?}"
            );
            assert_eq!(sparse.statistic, z.statistic);
        }
        // No variation is no test, whichever test would have run.
        assert_eq!(proportions(0, 1, 0, 10), None);
        assert_eq!(proportions(10, 10, 10, 10), None);
    }

    #[test]
    fn holm_controls_the_family_and_preserves_order() {
        assert_eq!(holm(&[0.01, 0.04, 0.03], 0.05), [true, false, false]);
        assert_eq!(holm(&[0.001, 0.012, 0.02], 0.05), [true, true, true]);
        assert_eq!(holm(&[0.2, 0.001], 0.05), [false, true]);
        assert!(holm(&[], 0.05).is_empty());
    }

    #[test]
    fn effect_sizes_are_signed_after_minus_before() {
        assert!(cohens_h(0.04, 0.19) > 0.0);
        assert!((cohens_h(0.5, 0.5)).abs() < 1e-12);
        assert!(cohens_d(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]) > 0.0);
        assert!(cohens_d(&[4.0, 5.0, 6.0], &[1.0, 2.0, 3.0]) < 0.0);
        assert_eq!(cohens_d(&[2.0, 2.0], &[2.0, 2.0]), 0.0);
    }

    #[test]
    fn jensen_shannon_matches_scipy() {
        let a = counts(&["p", "p", "p", "p", "p"]);
        let b = counts(&["p", "p", "p", "q", "q"]);
        // scipy.spatial.distance.jensenshannon([1,0],[0.6,0.4], base=2)
        close(
            js_distance(&a, 5, &b, 5).expect("both sides"),
            0.486_264_123_352_759_76,
            1e-12,
        );
        assert_eq!(js_of(&[1, 1], &[2, 2]), Some(1.0), "disjoint");
        assert_eq!(js_of(&[1, 2], &[2, 1]), Some(0.0), "identical");
        // jensenshannon([0.5,0.5,0],[0.1,0.2,0.7], base=2)
        let left = [0, 1, 0, 1, 0, 1, 0, 1, 0, 1];
        let right = [0, 1, 1, 2, 2, 2, 2, 2, 2, 2];
        close(
            js_of(&left, &right).expect("both"),
            0.709_155_183_656_703_5,
            1e-12,
        );
        assert_eq!(js_of::<u8>(&[], &[1]), None);
    }

    #[test]
    fn the_permutation_test_is_deterministic_and_discriminating() {
        let same: Vec<u8> = (0..40).map(|i| (i % 3) as u8).collect();
        let observed = js_of(&same[..20], &same[20..]).unwrap_or(0.0);
        let p = js_permutation_p(&same[..20], &same[20..], observed, 199, 7);
        assert!(p > 0.2, "no shift: p={p}");
        assert_eq!(
            p,
            js_permutation_p(&same[..20], &same[20..], observed, 199, 7),
            "same seed, same answer"
        );
        let left = vec![0u8; 20];
        let right = vec![1u8; 20];
        let observed = js_of(&left, &right).unwrap_or(0.0);
        let p = js_permutation_p(&left, &right, observed, 199, 7);
        assert_eq!(
            p,
            1.0 / 200.0,
            "a total shift is as significant as 199 draws can say"
        );
        // Stopping early changes no verdict: a shift stays as significant,
        // and no shift stops at a value at or above the give-up line.
        assert_eq!(
            js_permutation_test(&left, &right, observed, 199, 7, Some(0.01)),
            p
        );
        let observed = js_of(&same[..20], &same[20..]).unwrap_or(0.0);
        let early = js_permutation_test(&same[..20], &same[20..], observed, 1_999, 7, Some(0.01));
        assert!(early >= 0.01, "{early}");
        assert!(
            early <= js_permutation_p(&same[..20], &same[20..], observed, 1_999, 7),
            "a lower bound on the full p-value"
        );
    }
}
