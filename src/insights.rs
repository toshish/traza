//! Aggregations that answer "where should I look": distributions over time,
//! duration percentiles, and errors grouped by signature.
//!
//! Every function here folds a filtered span set into a bounded result in one
//! pass and constant memory (see [`crate::Store::fold_spans`]). That property
//! is the point: a dashboard asks these questions over a whole window, and an
//! implementation that first materialized the window would make the cheapest
//! screen the most expensive request the server serves.

use std::collections::HashMap;

use serde::Serialize;

use crate::{Result, Span, SpanFilter, Store};

// One bucketing scheme for the whole codebase: the ingest-stage histogram and
// this one must agree, or "p95" would mean two different things depending on
// which surface reported it.
use crate::metrics::{bucket_index, bucket_upper_bound, BUCKET_COUNT};

/// Most distinct failure signatures tracked in one grouping.
///
/// Each tracked signature costs a duration histogram — a few KiB — so an
/// unbounded map turns a corpus with high-cardinality error text into
/// gigabytes of server memory while the answer anybody wants is the top
/// twenty. Past this bound new signatures are counted, not tracked, and the
/// response says so rather than quietly reporting a subset as the whole.
const MAX_FAILURE_GROUPS: usize = 4096;

/// Most spans a tail request will rank. See [`Store::slowest_spans`].
const SLOWEST_LIMIT: usize = 1000;

/// A duration distribution held in fixed-width log-linear buckets.
///
/// Traza already had a power-of-two latency histogram, and its own monitoring
/// guide forbids publishing those percentiles as latencies: a bucket spans a
/// factor of two, so a reported p95 can be twice the truth. Splitting each
/// octave into sixteen makes a reported percentile **at most 1/16 (6.25%)
/// high and never low**, which is the accuracy a number on a screen needs.
///
/// Recording stays branch-light and allocation-free: one `leading_zeros`, one
/// shift, one increment. The whole histogram is under 4 KiB regardless of how
/// many spans it summarizes.
#[derive(Clone, Debug)]
pub struct DurationHistogram {
    buckets: Vec<u32>,
    count: u64,
    sum_ns: u64,
    min_ns: u64,
    max_ns: u64,
}

impl Default for DurationHistogram {
    fn default() -> Self {
        Self {
            buckets: vec![0; BUCKET_COUNT],
            count: 0,
            sum_ns: 0,
            min_ns: u64::MAX,
            max_ns: 0,
        }
    }
}

impl DurationHistogram {
    /// Adds one observation.
    pub fn record(&mut self, value_ns: u64) {
        let index = bucket_index(value_ns).min(BUCKET_COUNT - 1);
        self.buckets[index] += 1;
        self.count += 1;
        self.sum_ns = self.sum_ns.saturating_add(value_ns);
        self.min_ns = self.min_ns.min(value_ns);
        self.max_ns = self.max_ns.max(value_ns);
    }

    /// How many observations were recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The requested percentile in nanoseconds, `0` when nothing was recorded.
    ///
    /// The result is the upper bound of the bucket the true value falls in:
    /// at most 6.25% high, never low.
    pub fn percentile_ns(&self, percent: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        // The rank of the observation we want, 1-based. `ceil` keeps p100 at
        // the last observation rather than one short of it.
        let rank = ((percent / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (index, hits) in self.buckets.iter().enumerate() {
            seen += u64::from(*hits);
            if seen >= rank {
                // Never claim more than the largest value actually seen: the
                // top bucket is wide, and its upper bound would overstate a
                // max that landed near the bucket's floor.
                return bucket_upper_bound(index).min(self.max_ns);
            }
        }
        self.max_ns
    }

    /// Mean duration in nanoseconds, `0` when nothing was recorded.
    pub fn mean_ns(&self) -> u64 {
        self.sum_ns.checked_div(self.count).unwrap_or(0)
    }

    /// Smallest observation, `0` when nothing was recorded.
    pub fn min_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.min_ns
        }
    }

    /// Largest observation, `0` when nothing was recorded.
    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    /// The occupied buckets as `(upper_bound_ns, count)`, ascending.
    ///
    /// Only non-empty buckets are returned: a distribution over nanoseconds to
    /// minutes occupies a few dozen of the thousand, and sending the zeros
    /// would be almost all of the payload.
    pub fn occupied(&self) -> Vec<(u64, u32)> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, hits)| **hits > 0)
            .map(|(index, hits)| (bucket_upper_bound(index), *hits))
            .collect()
    }
}

/// One time bucket of a series.
#[derive(Clone, Debug, Serialize)]
pub struct SeriesBucket {
    /// Bucket start, Unix nanoseconds.
    pub start_ns: u64,
    /// Spans whose start time fell in the bucket.
    pub spans: u64,
    /// Spans with status `error`.
    pub errors: u64,
    /// Spans recognized as LLM calls.
    pub llm_calls: u64,
    /// Summed prompt + completion tokens.
    pub total_tokens: u64,
    /// Summed cost, when ingest supplied one.
    pub cost_usd: f64,
    /// Median span duration in the bucket.
    pub p50_ns: u64,
    /// 95th-percentile span duration in the bucket.
    pub p95_ns: u64,
}

/// A time series plus the window it was computed over.
#[derive(Clone, Debug, Serialize)]
pub struct Series {
    /// Window start, Unix nanoseconds.
    pub since_ns: u64,
    /// Window end, Unix nanoseconds.
    pub until_ns: u64,
    /// Width of one bucket, nanoseconds.
    pub bucket_ns: u64,
    /// The buckets, oldest first. Always exactly the requested count, so a
    /// quiet period is a visible gap rather than a missing element the client
    /// has to reconstruct from timestamps.
    pub buckets: Vec<SeriesBucket>,
}

/// Errors sharing a signature, with enough context to open one.
#[derive(Clone, Debug, Serialize)]
pub struct FailureGroup {
    /// Emitting service.
    pub service: String,
    /// Operation name.
    pub name: String,
    /// The status string the spans carried.
    pub status: String,
    /// How many spans share this signature.
    pub count: u64,
    /// Earliest occurrence, Unix nanoseconds.
    pub first_seen_ns: u64,
    /// Latest occurrence, Unix nanoseconds.
    pub last_seen_ns: u64,
    /// A trace containing the most recent occurrence, so the group opens.
    pub example_trace_id: String,
    /// A span within that trace.
    pub example_span_id: String,
    /// Median duration of spans in the group.
    pub p50_ns: u64,
    /// 95th-percentile duration of spans in the group.
    pub p95_ns: u64,
}

/// Failure groups plus the totals a caller needs to read them honestly.
#[derive(Clone, Debug, Serialize)]
pub struct FailureReport {
    /// The groups, most frequent first, truncated to the requested limit.
    pub groups: Vec<FailureGroup>,
    /// Every matching span, counted before any truncation. A share computed
    /// against the returned groups' subtotal overstates itself, sometimes by a
    /// lot, so the denominator is supplied rather than inferred.
    pub total: u64,
    /// Distinct signatures seen, up to the cardinality bound.
    pub distinct: usize,
    /// Signatures measured but not returned, because `limit` cut them.
    pub groups_omitted: usize,
    /// Spans whose signature was never tracked because the cardinality bound
    /// was already reached. Non-zero means `distinct` is a floor, not a count.
    pub spans_untracked: u64,
}

/// Accumulator for one failure signature while folding.
#[derive(Debug)]
struct FailureAccumulator {
    count: u64,
    first_seen_ns: u64,
    last_seen_ns: u64,
    example_trace_id: String,
    example_span_id: String,
    durations: DurationHistogram,
}

impl Store {
    /// Buckets matching spans into `bucket_count` even time buckets.
    ///
    /// One pass produces volume, errors, tokens, cost and duration percentiles
    /// together, because every screen that wants one of them wants several and
    /// separate endpoints would mean separate scans of the same window.
    pub fn series(
        &self,
        filter: &SpanFilter,
        since_ns: u64,
        until_ns: u64,
        bucket_count: usize,
    ) -> Result<Series> {
        let bucket_count = bucket_count.clamp(1, 512);
        let span_ns = until_ns.saturating_sub(since_ns).max(1);
        // Round up so the last bucket ends at or after `until_ns`; rounding
        // down would drop the final partial bucket, which on a live window is
        // the only one anybody is watching.
        // Ceiling division, written as quotient plus a non-zero remainder.
        // `div_ceil` is stable only since 1.73 and this crate supports 1.70,
        // and the obvious longhand — `(span + count - 1) / count` — overflows
        // on `until = u64::MAX`. Saturating that addition does not rescue it:
        // the sum clamps to `u64::MAX`, which is one short of the true ceiling
        // and leaves the last bucket ending before the window does.
        let divisor = bucket_count as u64;
        let bucket_ns = (span_ns / divisor + u64::from(span_ns % divisor != 0)).max(1);

        let mut spans = vec![0u64; bucket_count];
        let mut errors = vec![0u64; bucket_count];
        let mut llm_calls = vec![0u64; bucket_count];
        let mut tokens = vec![0u64; bucket_count];
        let mut cost = vec![0f64; bucket_count];
        let mut durations: Vec<DurationHistogram> = (0..bucket_count)
            .map(|_| DurationHistogram::default())
            .collect();

        let windowed = SpanFilter {
            since_ns: Some(filter.since_ns.unwrap_or(since_ns).max(since_ns)),
            until_ns: Some(filter.until_ns.unwrap_or(until_ns).min(until_ns)),
            limit: None,
            sort: None,
            ..filter.clone()
        };
        self.fold_spans(&windowed, |span| {
            let offset = span.start_time_ns.saturating_sub(since_ns);
            let index = ((offset / bucket_ns) as usize).min(bucket_count - 1);
            spans[index] += 1;
            if span.status == "error" {
                errors[index] += 1;
            }
            durations[index].record(span.end_time_ns.saturating_sub(span.start_time_ns));
            let usage = self.facts(span);
            if usage.is_llm {
                llm_calls[index] += 1;
            }
            tokens[index] = tokens[index].saturating_add(usage.total());
            if let Some(spent) = usage.cost_usd {
                // Non-finite cost is ignored rather than propagated: one NaN
                // from bad instrumentation would turn the whole series into
                // NaN and take the chart with it.
                if spent.is_finite() {
                    cost[index] += spent;
                }
            }
        })?;

        let buckets = (0..bucket_count)
            .map(|index| SeriesBucket {
                // Saturating and clamped to the window. `since + width * i`
                // overflows for any window sitting near the top of the range —
                // reachable with a plain `since`/`until` pair — and a bucket
                // start past `until` describes no time the window covers.
                start_ns: since_ns
                    .saturating_add(bucket_ns.saturating_mul(index as u64))
                    .min(until_ns),
                spans: spans[index],
                errors: errors[index],
                llm_calls: llm_calls[index],
                total_tokens: tokens[index],
                cost_usd: cost[index],
                p50_ns: durations[index].percentile_ns(50.0),
                p95_ns: durations[index].percentile_ns(95.0),
            })
            .collect();
        Ok(Series {
            since_ns,
            until_ns,
            bucket_ns,
            buckets,
        })
    }

    /// The duration distribution of matching spans.
    pub fn duration_histogram(&self, filter: &SpanFilter) -> Result<DurationHistogram> {
        let mut histogram = DurationHistogram::default();
        let unbounded = SpanFilter {
            limit: None,
            sort: None,
            ..filter.clone()
        };
        self.fold_spans(&unbounded, |span| {
            histogram.record(span.end_time_ns.saturating_sub(span.start_time_ns));
        })?;
        Ok(histogram)
    }

    /// Error spans grouped by `(service, name, status)`, most frequent first.
    ///
    /// Grouping happens here rather than in the browser because the useful
    /// answer is a dozen rows and the input can be every error in the window:
    /// shipping the spans so the client could group them would move megabytes
    /// to produce a table that fits on a screen.
    pub fn failures(&self, filter: &SpanFilter, limit: usize) -> Result<FailureReport> {
        let mut groups: HashMap<(String, String, String), FailureAccumulator> = HashMap::new();
        let mut total = 0u64;
        let mut untracked = 0u64;
        let errors_only = SpanFilter {
            // An explicit status filter wins: "failures, but only the 503s" is
            // a narrowing of this screen, not a contradiction of it.
            status: filter.status.clone().or_else(|| Some("error".to_owned())),
            limit: None,
            sort: None,
            ..filter.clone()
        };
        self.fold_spans(&errors_only, |span| {
            // Counted before anything is decided about tracking, so the total
            // is the truth even when the map is full. A share computed against
            // a truncated subtotal reads as a much larger fraction than it is.
            total += 1;
            let key = (span.service.clone(), span.name.clone(), span.status.clone());
            let entry = match groups.get_mut(&key) {
                Some(entry) => entry,
                None => {
                    // High-cardinality error text — an id or a timestamp in a
                    // span name — would otherwise allocate a histogram per
                    // distinct string until the process died.
                    if groups.len() >= MAX_FAILURE_GROUPS {
                        untracked += 1;
                        return;
                    }
                    groups.entry(key).or_insert_with(|| FailureAccumulator {
                        count: 0,
                        first_seen_ns: u64::MAX,
                        last_seen_ns: 0,
                        example_trace_id: String::new(),
                        example_span_id: String::new(),
                        durations: DurationHistogram::default(),
                    })
                }
            };
            entry.count += 1;
            entry.first_seen_ns = entry.first_seen_ns.min(span.start_time_ns);
            // Keep the most recent example: when a signature is still firing,
            // the newest occurrence is the one worth opening.
            if span.start_time_ns >= entry.last_seen_ns {
                entry.last_seen_ns = span.start_time_ns;
                entry.example_trace_id.clear();
                entry.example_trace_id.push_str(&span.trace_id);
                entry.example_span_id.clear();
                entry.example_span_id.push_str(&span.span_id);
            }
            entry
                .durations
                .record(span.end_time_ns.saturating_sub(span.start_time_ns));
        })?;

        let distinct = groups.len();
        let mut rows: Vec<FailureGroup> = groups
            .into_iter()
            .map(|((service, name, status), entry)| FailureGroup {
                service,
                name,
                status,
                count: entry.count,
                first_seen_ns: if entry.first_seen_ns == u64::MAX {
                    0
                } else {
                    entry.first_seen_ns
                },
                last_seen_ns: entry.last_seen_ns,
                example_trace_id: entry.example_trace_id,
                example_span_id: entry.example_span_id,
                p50_ns: entry.durations.percentile_ns(50.0),
                p95_ns: entry.durations.percentile_ns(95.0),
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| right.last_seen_ns.cmp(&left.last_seen_ns))
                .then_with(|| left.service.cmp(&right.service))
                .then_with(|| left.name.cmp(&right.name))
        });
        let returned = rows.len().min(limit);
        rows.truncate(limit);
        Ok(FailureReport {
            groups: rows,
            total,
            distinct,
            // Two different truncations, and a client needs to tell them
            // apart: `limit` cut the response, the cardinality bound cut what
            // was measured at all.
            groups_omitted: distinct.saturating_sub(returned),
            spans_untracked: untracked,
        })
    }

    /// The slowest matching spans, for the tail behind a latency distribution.
    ///
    /// Kept separate from [`Self::duration_histogram`] so the common case —
    /// drawing the distribution — never pays for ranking, and so the ranking
    /// path can bound its own memory to `limit` rather than to the match set.
    pub fn slowest_spans(&self, filter: &SpanFilter, limit: usize) -> Result<Vec<Span>> {
        // A tail is read, not paged: the point is the handful of outliers
        // behind a distribution. Capping here keeps `limit` from being a way
        // to ask the server to hold the whole match set in memory — and stops
        // `limit + 1` overflowing on `usize::MAX`.
        let limit = limit.min(SLOWEST_LIMIT);
        let mut worst: Vec<Span> = Vec::with_capacity(limit.saturating_add(1));
        let unbounded = SpanFilter {
            limit: None,
            sort: None,
            ..filter.clone()
        };
        let duration = |span: &Span| span.end_time_ns.saturating_sub(span.start_time_ns);
        self.fold_spans(&unbounded, |span| {
            if limit == 0 {
                return;
            }
            // A bounded insertion sort beats collecting and sorting: `limit`
            // is a handful, so this stays O(matches) with O(limit) memory
            // instead of materializing every match to keep ten of them.
            if worst.len() == limit && duration(span) <= worst.last().map_or(0, duration) {
                return;
            }
            let at = worst
                .binary_search_by(|probe| duration(span).cmp(&duration(probe)))
                .unwrap_or_else(|position| position);
            worst.insert(at, span.clone());
            worst.truncate(limit);
        })?;
        Ok(worst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_is_never_below_the_truth() {
        let mut histogram = DurationHistogram::default();
        for value in 1..=1000u64 {
            histogram.record(value);
        }
        // p50 of 1..=1000 is 500; the bucket upper bound may exceed it but
        // must never fall short, or a dashboard would understate latency.
        let p50 = histogram.percentile_ns(50.0);
        assert!(p50 >= 500, "p50 {p50} understated the true 500");
    }

    #[test]
    fn a_percentile_is_within_one_sixteenth_of_the_truth() {
        let mut histogram = DurationHistogram::default();
        for value in 1..=100_000u64 {
            histogram.record(value);
        }
        let p95 = histogram.percentile_ns(95.0);
        let truth = 95_000f64;
        let error = (p95 as f64 - truth) / truth;
        assert!(
            (0.0..=1.0 / crate::metrics::SUB_COUNT as f64).contains(&error),
            "p95 {p95} is {error} off a true {truth}, past the 1/16 bound"
        );
    }

    #[test]
    fn an_empty_histogram_reports_zero_rather_than_dividing_by_zero() {
        let histogram = DurationHistogram::default();
        assert_eq!(histogram.percentile_ns(95.0), 0);
        assert_eq!(histogram.mean_ns(), 0);
        assert_eq!(histogram.min_ns(), 0);
        assert_eq!(histogram.count(), 0);
    }

    #[test]
    fn bucket_bounds_ascend_without_gaps() {
        // Every value must land in a bucket whose upper bound covers it, and
        // consecutive buckets must not overlap — otherwise a percentile could
        // read from the wrong side of a boundary.
        let mut previous = 0u64;
        for value in [0u64, 1, 31, 32, 33, 63, 64, 1000, 1 << 20, 1 << 40] {
            let index = bucket_index(value);
            let bound = bucket_upper_bound(index);
            assert!(bound >= value, "bucket for {value} tops out at {bound}");
            assert!(bound >= previous, "bounds went backwards at {value}");
            previous = bound;
        }
    }

    #[test]
    fn the_top_bucket_never_overstates_the_largest_value_seen() {
        let mut histogram = DurationHistogram::default();
        histogram.record(1 << 40);
        // The bucket is wide; the reported max must be what was recorded.
        assert_eq!(histogram.percentile_ns(100.0), 1 << 40);
    }
}
