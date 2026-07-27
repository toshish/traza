//! Does the tail ring's byte budget describe the memory it actually holds?
//!
//! Every hole found in `approximate_bytes` so far was a shape nobody had
//! thought to probe: first it counted only text, then only logical length. Unit
//! tests cannot close that class, because a unit test can only assert about a
//! shape its author imagined. So this measures the real thing — bytes the
//! allocator hands out — through a counting global allocator, and asserts the
//! estimate tracks it.
//!
//! A global allocator affects the whole test binary, which is why this is its
//! own file rather than a case in another one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Map, Value};
use traza::{Config, Durability, Span, SpanFilter, Store};

/// Live allocated bytes, as counted by the allocator itself.
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let fresh = unsafe { System.realloc(pointer, layout, new_size) };
        if !fresh.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        fresh
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn base_span(id: &str) -> Span {
    Span {
        trace_id: format!("trace-{id}"),
        span_id: id.to_owned(),
        parent_span_id: None,
        name: "op".into(),
        start_time_ns: 1_000,
        end_time_ns: 2_000,
        status: "ok".into(),
        service: "svc".into(),
        attributes: Map::new(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

/// A span whose weight is a long string — the shape the first estimator got
/// right and the only one it got right.
fn wide_text(id: &str, bytes: usize) -> Span {
    let mut span = base_span(id);
    span.attributes
        .insert("prompt".into(), Value::String("x".repeat(bytes)));
    span
}

/// A span whose weight is nested structure, with no long strings anywhere.
fn deep_structure(id: &str, width: usize, depth: usize) -> Span {
    fn nest(depth: usize, width: usize) -> Value {
        if depth == 0 {
            return Value::Array((0..width).map(|n| Value::from(n as u64)).collect());
        }
        let mut object = Map::new();
        for n in 0..width {
            object.insert(format!("k{n}"), nest(depth - 1, width));
        }
        Value::Object(object)
    }
    let mut span = base_span(id);
    span.attributes.insert("tree".into(), nest(depth, width));
    span
}

/// A span whose weight is spare capacity: allocations retained by collections
/// that were grown and then truncated.
fn retained_capacity(id: &str, slots: usize) -> Span {
    let mut wide: Vec<Value> = Vec::with_capacity(slots);
    wide.extend((0..slots as u64).map(Value::from));
    wide.truncate(1);

    let mut fat = String::with_capacity(slots * 8);
    fat.push('x');

    let mut span = base_span(id);
    span.attributes.insert("array".into(), Value::Array(wide));
    span.attributes.insert("text".into(), Value::String(fat));
    span
}

/// Bytes the allocator reports for holding `count` copies of `make`, and what
/// the engine's estimate said they would cost.
fn measure(make: impl Fn(usize) -> Span, count: usize) -> (usize, usize) {
    // Warm up: the first span pulls in whatever lazily-initialized machinery
    // the construction path uses, which would otherwise be charged to the
    // measurement.
    drop(make(0));

    let before = live_bytes();
    let spans: Vec<Span> = (1..=count).map(&make).collect();
    let held = live_bytes().saturating_sub(before);

    let estimated: usize = spans.iter().map(traza::tail::approximate_bytes).sum();
    drop(spans);
    (held, estimated)
}

#[test]
fn the_estimate_tracks_what_the_allocator_actually_hands_out() {
    // The estimate must never be far BELOW real allocation — that is the
    // direction that makes the ceiling bypassable — across every shape,
    // including ones whose cost is invisible to a serializer.
    for (label, held, estimated) in [
        (
            "wide text",
            measure(|n| wide_text(&format!("s{n}"), 64 * 1024), 32),
        ),
        (
            "deep structure",
            measure(|n| deep_structure(&format!("s{n}"), 8, 3), 8),
        ),
        (
            "retained capacity",
            measure(|n| retained_capacity(&format!("s{n}"), 50_000), 16),
        ),
    ]
    .map(|(label, (held, estimated))| (label, held, estimated))
    {
        assert!(
            estimated * 2 >= held,
            "{label}: estimate {estimated} is less than half of the {held} bytes \
             actually allocated — the byte ceiling is bypassable by this shape"
        );
        // And not absurdly above it either: an estimate that over-charged by
        // orders of magnitude would evict a nearly empty ring and make the
        // tail useless without anyone noticing it had.
        assert!(
            estimated <= held * 8 + 64 * 1024,
            "{label}: estimate {estimated} wildly exceeds the {held} bytes held"
        );
    }
}

#[test]
fn the_ring_holds_no_more_than_its_byte_budget_says() {
    // End to end, through the real ingest path: the allocator's own count of
    // what the process holds must stay within reach of the configured ceiling,
    // whatever shape the spans are.
    let directory = std::env::temp_dir().join(format!(
        "traza-tail-memory-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create directory");

    const BUDGET: usize = 4 * 1024 * 1024;
    let store = Store::open(
        &directory,
        Config {
            durability: Durability::Buffered,
            // No offloading, so the spans stay whole and the ring holds them.
            payload_threshold: None,
            tail_ring_spans: 1_000_000,
            tail_ring_bytes: BUDGET,
            ..Config::default()
        },
    )
    .expect("store opens");

    for index in 0..200 {
        store
            .ingest_batch(vec![retained_capacity(&format!("s{index}"), 50_000)])
            .expect("ingest");
    }

    // Flush BEFORE measuring, and this is the whole subtlety of the test.
    //
    // Until a flush, the write buffer holds every admitted span, so process
    // growth measures the buffer rather than the ring — the first version of
    // this test read 370 MB and blamed a ring that was correctly holding two
    // spans. (That buffer is bounded by `flush_spans`, a COUNT, so the same
    // shape inflates it too. Pre-existing, and a separate question from this
    // one.) After the flush the segments own the data on disk and the ring is
    // the only thing left holding spans in memory, which is exactly what the
    // budget is supposed to bound.
    store.flush().expect("flush");
    let settled = live_bytes();

    let (spans, bytes, _, budget) = store.tail_usage().expect("usage");
    assert!(
        bytes <= budget,
        "the ring's own accounting exceeded its budget: {bytes} > {budget}"
    );

    // With the buffer drained, what the process still holds is the ring plus
    // the segment set's indexes. Generous, but the assertion that matters:
    // without capacity counting these 200 spans retained hundreds of megabytes
    // while the ring believed it held a few kilobytes.
    let after_flush = live_bytes();
    assert!(
        after_flush <= settled,
        "flush should not grow the heap: {settled} -> {after_flush}"
    );
    assert!(
        after_flush < BUDGET * 12,
        "with the buffer drained the process holds {after_flush} bytes against \
         a {BUDGET}-byte tail budget holding {spans} spans"
    );

    // The tail still works, which is the other half: a budget that bounds
    // memory by holding nothing would pass the assertion above and be useless.
    let read = store
        .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
        .expect("tail reads");
    match read {
        traza::tail::TailRead::Batch { spans, .. } => {
            assert!(!spans.is_empty(), "the ring must still serve a tail")
        }
        traza::tail::TailRead::Gap { .. } => panic!("a fresh subscriber cannot gap"),
    }

    drop(store);
    let _ = std::fs::remove_dir_all(&directory);
}
