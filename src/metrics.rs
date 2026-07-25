//! Instrumentation for the ingest path: where does a span's time actually go?
//!
//! Ingest throughput is a sum of stages — decode, validate, log-append, fsync,
//! buffer upsert, segment seal — and tuning the wrong one is the usual way to
//! spend a week for nothing. Every stage here is timed separately so the
//! limiting one is a measurement rather than a guess.
//!
//! **Cost.** One [`Latency::record`] is three relaxed atomic adds and a
//! compare-exchange loop for the max. Stages are timed per BATCH, not per
//! span, so at the default batch of 1,000 the instrumentation is far below the
//! noise floor of the thing it measures. Nothing here allocates or locks.
//!
//! **Percentiles are approximate, on purpose.** Latencies land in power-of-two
//! nanosecond buckets, so a reported percentile is the upper bound of the
//! bucket the true value falls in — at most 2x high, never low. That is the
//! right trade for "which stage dominates", and it is why the benchmark still
//! measures end-to-end request latency exactly, from the client, with a plain
//! `Instant`. Do not publish these numbers as request latencies; they exist to
//! rank stages against each other.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Number of power-of-two buckets. Bucket `i` holds `[2^i, 2^(i+1))` ns, so 41
/// buckets reach ~18 minutes — far past any stage that could still be called a
/// bottleneck rather than a hang.
const BUCKETS: usize = 41;

/// A count/total/max/histogram summary of one stage's duration.
#[derive(Debug)]
pub struct Latency {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

// Hand-written because std only derives `Default` for arrays up to 32 long.
impl Default for Latency {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Latency {
    /// Records one observation.
    pub fn record(&self, nanos: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(nanos, Ordering::Relaxed);
        // 64 - leading_zeros is floor(log2)+1; bucket 0 collects sub-2ns.
        let bucket = (64 - nanos.leading_zeros()) as usize;
        self.buckets[bucket.min(BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
        let mut observed = self.max_ns.load(Ordering::Relaxed);
        while nanos > observed {
            match self.max_ns.compare_exchange_weak(
                observed,
                nanos,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => observed = current,
            }
        }
    }

    /// Times `body`, records how long it took, and returns its value.
    pub fn time<T>(&self, body: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let value = body();
        self.record(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        value
    }

    /// How many observations have been recorded.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Sum of every observation, in nanoseconds. Divided by [`Self::count`]
    /// this is the mean; kept separate so callers can aggregate across stages.
    pub fn total_ns(&self) -> u64 {
        self.total_ns.load(Ordering::Relaxed)
    }

    /// Largest single observation, in nanoseconds. Exact, unlike the
    /// percentiles.
    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    /// Mean observation in nanoseconds, or 0 with no observations.
    pub fn mean_ns(&self) -> u64 {
        self.total_ns().checked_div(self.count()).unwrap_or(0)
    }

    /// Approximate percentile in nanoseconds: the upper bound of the bucket
    /// the true value falls in, so the answer is at most 2x high and never
    /// low. See the module docs before reporting this anywhere.
    pub fn percentile_ns(&self, percent: f64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        let rank = ((percent / 100.0) * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen += bucket.load(Ordering::Relaxed);
            if seen >= rank {
                // Bucket `index` covers [2^(index-1), 2^index); report the
                // exclusive upper bound.
                return if index == 0 {
                    1
                } else {
                    1u64 << (index - 1) << 1
                };
            }
        }
        self.max_ns()
    }
}

/// A monotonically increasing count.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Adds `amount`.
    pub fn add(&self, amount: u64) {
        self.0.fetch_add(amount, Ordering::Relaxed);
    }

    /// Adds one.
    pub fn increment(&self) {
        self.add(1);
    }

    /// Current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Engine-side ingest instrumentation, one instance per [`crate::Store`].
///
/// Per-store rather than global: a process holding several stores (every test
/// binary does) must not blend their numbers into one meaningless total.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Spans accepted through the ingest surfaces.
    pub spans_admitted: Counter,
    /// Batches accepted, so `spans_admitted / batches_admitted` is the mean
    /// batch size actually reaching the engine.
    pub batches_admitted: Counter,
    /// Time spent waiting to acquire the writer lock. The contention signal:
    /// if this dominates, the fix is to do less work while holding it, not to
    /// make the work faster.
    pub writer_lock_wait: Latency,
    /// Encoding a batch into its log frame. Deliberately measured outside the
    /// writer lock — see [`crate::Store::admit`].
    pub wal_encode: Latency,
    /// Writing the frame to the log file (inside the writer lock).
    pub wal_write: Latency,
    /// `fsync`. The one stage that is not CPU.
    pub wal_fsync: Latency,
    /// Calls to `commit`, whether or not they performed their own fsync.
    /// Divided by `wal_fsync.count()` this is the **group-commit ratio**: how
    /// many acknowledgements each fsync actually covered.
    pub wal_commits: Counter,
    /// Upserting a batch into the write buffer (inside the writer lock).
    pub buffer_upsert: Latency,
    /// Sealing the write buffer into a segment.
    pub segment_seal: Latency,
    /// Spans written out by seals, for seal cost per span.
    pub segment_seal_spans: Counter,
}

impl Metrics {
    /// Renders every counter in Prometheus text exposition format.
    ///
    /// Latencies are emitted as `_ns_count`/`_ns_sum`/`_ns_max` plus
    /// approximate `_ns_p50`/`_ns_p99` gauges. They are NOT emitted as
    /// Prometheus histograms: the bucket bounds here are powers of two chosen
    /// for stage ranking, and dressing them up as `le` buckets would invite
    /// quantile math the resolution does not support.
    pub fn render_prometheus(&self, into: &mut String) {
        use std::fmt::Write as _;

        let counters: [(&str, &Counter); 4] = [
            ("traza_spans_admitted_total", &self.spans_admitted),
            ("traza_batches_admitted_total", &self.batches_admitted),
            ("traza_wal_commits_total", &self.wal_commits),
            ("traza_segment_seal_spans_total", &self.segment_seal_spans),
        ];
        for (name, counter) in counters {
            let _ = writeln!(into, "# TYPE {name} counter");
            let _ = writeln!(into, "{name} {}", counter.get());
        }

        let stages: [(&str, &Latency); 6] = [
            ("traza_writer_lock_wait", &self.writer_lock_wait),
            ("traza_wal_encode", &self.wal_encode),
            ("traza_wal_write", &self.wal_write),
            ("traza_wal_fsync", &self.wal_fsync),
            ("traza_buffer_upsert", &self.buffer_upsert),
            ("traza_segment_seal", &self.segment_seal),
        ];
        for (name, stage) in stages {
            let _ = writeln!(into, "# TYPE {name}_ns_count counter");
            let _ = writeln!(into, "{name}_ns_count {}", stage.count());
            let _ = writeln!(into, "# TYPE {name}_ns_sum counter");
            let _ = writeln!(into, "{name}_ns_sum {}", stage.total_ns());
            let _ = writeln!(into, "# TYPE {name}_ns_max gauge");
            let _ = writeln!(into, "{name}_ns_max {}", stage.max_ns());
            // Approximate: bucket upper bounds. See the type docs.
            let _ = writeln!(into, "# TYPE {name}_ns_p50 gauge");
            let _ = writeln!(into, "{name}_ns_p50 {}", stage.percentile_ns(50.0));
            let _ = writeln!(into, "# TYPE {name}_ns_p99 gauge");
            let _ = writeln!(into, "{name}_ns_p99 {}", stage.percentile_ns(99.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_latency_reports_what_it_recorded() {
        let latency = Latency::default();
        latency.record(100);
        latency.record(300);
        assert_eq!(latency.count(), 2);
        assert_eq!(latency.total_ns(), 400);
        assert_eq!(latency.max_ns(), 300);
        assert_eq!(latency.mean_ns(), 200);
    }

    #[test]
    fn percentiles_bound_the_true_value_from_above() {
        // The contract that makes approximate buckets safe: never report LOW,
        // because a stage that looks faster than it is would be dismissed as
        // a bottleneck for the wrong reason.
        let latency = Latency::default();
        for value in [1_000u64, 2_000, 4_000, 8_000, 1_000_000] {
            latency.record(value);
        }
        let p99 = latency.percentile_ns(99.0);
        assert!(
            p99 >= 1_000_000,
            "p99 {p99} must not understate the 1ms observation"
        );
        assert!(p99 <= 2_000_000, "and must stay within one bucket: {p99}");
    }

    #[test]
    fn an_empty_latency_reports_zero_rather_than_dividing_by_zero() {
        let latency = Latency::default();
        assert_eq!(latency.count(), 0);
        assert_eq!(latency.mean_ns(), 0);
        assert_eq!(latency.percentile_ns(50.0), 0);
    }

    #[test]
    fn extreme_values_stay_inside_the_bucket_array() {
        // A pathological stall must not index past the histogram.
        let latency = Latency::default();
        latency.record(u64::MAX);
        latency.record(0);
        assert_eq!(latency.count(), 2);
        assert_eq!(latency.max_ns(), u64::MAX);
    }

    #[test]
    fn prometheus_output_names_every_stage() {
        let metrics = Metrics::default();
        metrics.spans_admitted.add(7);
        metrics.wal_fsync.record(1_500);
        let mut rendered = String::new();
        metrics.render_prometheus(&mut rendered);
        assert!(rendered.contains("traza_spans_admitted_total 7"));
        assert!(rendered.contains("traza_wal_fsync_ns_count 1"));
        assert!(rendered.contains("traza_wal_fsync_ns_sum 1500"));
    }
}
