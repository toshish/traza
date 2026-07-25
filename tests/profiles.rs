//! Profiles as a configuration surface.
//!
//! A profile is a claim that a name sets a coherent group of knobs. Two things
//! can go wrong with that: the values can drift out of the ordering they are
//! named for, and they can fail to reach behaviour at all — a `Config` field
//! that no code path reads is documentation, not configuration. Both are
//! checked here. The CLI half (a profile is only a default, and an explicit
//! flag beats it) lives in `src/bin/traza-server.rs`.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use traza::{CompactionConfig, Config, Durability, Profile, Span, Store};

fn scratch(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("traza-profiles-{tag}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn span(index: usize) -> Span {
    Span {
        trace_id: format!("{:032x}", index / 10 + 1),
        span_id: format!("{:016x}", index + 1),
        start_time_ns: 1_700_000_000_000_000_000 + index as u64 * 1_000_000,
        end_time_ns: 1_700_000_000_000_500_000 + index as u64 * 1_000_000,
        name: "operation".to_owned(),
        service: "bench".to_owned(),
        parent_span_id: None,
        status: String::new(),
        attributes: Default::default(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Default::default(),
    }
}

#[test]
fn every_profile_round_trips_through_its_name() {
    for profile in [Profile::Throughput, Profile::Balanced, Profile::Latency] {
        assert_eq!(Profile::parse(profile.as_str()), Some(profile));
    }
    assert_eq!(Profile::parse("fast"), None);
    assert_eq!(Profile::parse(""), None);
    assert_eq!(Profile::default(), Profile::Balanced);
}

/// The named ordering, asserted rather than assumed. If someone retunes the
/// numbers, they stay on the axis the names promise or this fails.
#[test]
fn the_profiles_are_ordered_the_way_they_are_named() {
    assert!(
        Profile::Throughput.flush_spans() > Profile::Balanced.flush_spans(),
        "throughput must seal less often than balanced"
    );
    assert!(
        Profile::Balanced.flush_spans() > Profile::Latency.flush_spans(),
        "latency must seal more often than balanced"
    );
    // Only throughput buys amortization with delay; the other two must never
    // add latency they were not asked for.
    assert!(Profile::Throughput.wal_commit_window().is_some());
    assert_eq!(Profile::Balanced.wal_commit_window(), None);
    assert_eq!(Profile::Latency.wal_commit_window(), None);
}

/// `balanced` is the documented no-op: choosing it must be indistinguishable
/// from choosing nothing.
#[test]
fn balanced_is_exactly_the_built_in_defaults() {
    assert_eq!(Profile::Balanced.config(), Config::default());
}

/// The safety invariant. A profile is a performance choice, so it must not be
/// able to move the acknowledgement contract or the read-path settings.
#[test]
fn no_profile_touches_durability_or_compaction() {
    for profile in [Profile::Throughput, Profile::Balanced, Profile::Latency] {
        let config = profile.config();
        assert_eq!(
            config.durability,
            Durability::Wal,
            "profile {} changed durability",
            profile.as_str()
        );
        assert_eq!(
            config.compaction,
            Some(CompactionConfig::default()),
            "profile {} changed compaction",
            profile.as_str()
        );
    }
}

/// A profile's `flush_spans` has to reach the engine, not just the struct.
/// Ingesting `latency`'s threshold seals a segment; the same corpus under
/// `balanced` (a 5x higher threshold) must still be entirely in the buffer.
#[test]
fn a_profile_flush_threshold_drives_actual_sealing() {
    let spans: Vec<Span> = (0..Profile::Latency.flush_spans()).map(span).collect();
    assert!(spans.len() < Profile::Balanced.flush_spans());

    let latency_dir = scratch("latency");
    let store = Store::open(&latency_dir, Profile::Latency.config()).expect("opens");
    store.ingest_batch(spans.clone()).expect("ingests");
    let stats = store.stats().expect("stats");
    assert!(
        stats.segment_count > 0,
        "latency profile did not seal at its own threshold ({} spans)",
        spans.len()
    );
    assert_eq!(stats.total_records, spans.len());
    drop(store);
    let _ = std::fs::remove_dir_all(&latency_dir);

    let balanced_dir = scratch("balanced");
    let store = Store::open(&balanced_dir, Profile::Balanced.config()).expect("opens");
    store.ingest_batch(spans.clone()).expect("ingests");
    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.segment_count, 0,
        "balanced sealed below its own threshold"
    );
    assert_eq!(stats.buffered_records, spans.len());
    drop(store);
    let _ = std::fs::remove_dir_all(&balanced_dir);
}

/// The startup banner reports the RESOLVED knobs, so an operator reading a log
/// sees what is in force rather than a profile name that an explicit flag may
/// have partly overridden. Driving the real binary also proves the resolved
/// `Config` is the one actually handed to the engine, which parsing tests
/// alone cannot show.
fn announced_profile_line(extra: &[&str]) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("traza-profiles-banner-{stamp}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .arg("--data-dir")
        .arg(&dir)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .args(extra)
        .env_remove("TRAZA_TOKENS")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns traza-server");
    let stderr = child.stderr.take().expect("stderr");
    let mut found = String::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.expect("stderr line");
        if let Some(rest) = line.strip_prefix("traza-server: profile=") {
            found = rest.to_owned();
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!found.is_empty(), "server never announced its profile");
    found
}

#[test]
fn the_server_announces_the_resolved_knobs() {
    let banner = announced_profile_line(&["--profile", "throughput"]);
    assert!(
        banner.starts_with("throughput"),
        "unexpected banner: {banner}"
    );
    assert!(
        banner.contains(&format!(
            "flush-spans={}",
            Profile::Throughput.flush_spans()
        )),
        "unexpected banner: {banner}"
    );
    assert!(
        banner.contains("wal-commit-window=500us"),
        "unexpected banner: {banner}"
    );

    // An override has to be visible in the banner, not hidden behind the
    // profile's name.
    let overridden = announced_profile_line(&["--profile", "throughput", "--flush-spans", "1234"]);
    assert!(
        overridden.contains("flush-spans=1234"),
        "the banner reported the profile's value, not the resolved one: {overridden}"
    );
    assert!(
        overridden.contains("wal-commit-window=500us"),
        "overriding one knob dropped the rest of the profile: {overridden}"
    );

    let default = announced_profile_line(&[]);
    assert!(default.starts_with("balanced"), "unexpected: {default}");
    assert!(
        default.contains("wal-commit-window=off"),
        "unexpected: {default}"
    );
}

/// A profile changes when an acknowledgement arrives, never whether the data
/// behind it survives. Ingest under each profile, reopen, and require every
/// span back.
#[test]
fn every_profile_recovers_everything_it_acknowledged() {
    for profile in [Profile::Throughput, Profile::Balanced, Profile::Latency] {
        let dir = scratch(profile.as_str());
        let spans: Vec<Span> = (0..500).map(span).collect();
        let store = Store::open(&dir, profile.config()).expect("opens");
        store.ingest_batch(spans).expect("ingests");
        drop(store);

        let reopened = Store::open(&dir, profile.config()).expect("reopens");
        assert_eq!(
            reopened.stats().expect("stats").total_records,
            500,
            "profile {} lost acknowledged spans across a restart",
            profile.as_str()
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
