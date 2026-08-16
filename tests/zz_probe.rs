//! probe
use std::collections::HashMap;
use traza::attribution::diagnose;
use traza::{seed, semconv, Span};

const IDLE_NS: u64 = 900_000_000_000;
fn corpus() -> Vec<Span> {
    seed::corpus(&seed::SeedOptions::default()).spans
}
fn after(spans: &[Span]) -> u64 {
    spans
        .iter()
        .map(|s| s.end_time_ns)
        .max()
        .unwrap_or_default()
        + IDLE_NS * 2
}

#[test]
fn probe_traces_with_repetition() {
    let spans = corpus();
    let now = after(&spans);
    let sessioned = spans
        .iter()
        .filter(|s| semconv::facts(&s.attributes).session.is_some())
        .count();
    println!(
        "total spans={} with session={} without={}",
        spans.len(),
        sessioned,
        spans.len() - sessioned
    );

    let mut by_trace: HashMap<String, Vec<Span>> = HashMap::new();
    for s in &spans {
        by_trace
            .entry(s.trace_id.clone())
            .or_default()
            .push(s.clone());
    }
    let mut reported = 0;
    for (trace, members) in &by_trace {
        let mut per: HashMap<(String, String), usize> = HashMap::new();
        for s in members {
            *per.entry((s.service.clone(), s.name.clone())).or_default() += 1;
        }
        let (sig, n) = per
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(k, v)| (k.clone(), *v))
            .unwrap();
        if n >= 5 {
            let has_session = members
                .iter()
                .any(|s| semconv::facts(&s.attributes).session.is_some());
            let d = diagnose(members, now, IDLE_NS, false);
            let shapes: Vec<String> = d
                .findings
                .iter()
                .map(|f| {
                    format!(
                        "{:?}/{} x{} err={} serial={:.2} trend={:?}",
                        f.shape, f.name, f.count, f.error_count, f.serial_fraction, f.token_trend
                    )
                })
                .collect();
            println!(
                "trace {trace} sig={:?} n={n} session={has_session} -> {:?}",
                sig.1, shapes
            );
            reported += 1;
            if reported > 25 {
                break;
            }
        }
    }
    panic!("show");
}
