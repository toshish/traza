//! Admission caps, and the one measurement pass ingest makes over a batch.
//!
//! A span's shape is the client's to choose, and some shapes are attacks or
//! bugs rather than telemetry: ten thousand attributes, a megabyte attribute
//! KEY (keys are indexed), a span that serializes to more than the segment
//! layer should ever have to decode for one record. These caps refuse such a
//! batch with a `400` naming the cap and the span, batch-atomically and before
//! anything is written — the same stance as an empty span id.
//!
//! The same pass measures each span's serialized size, which usage accounting
//! needs ([`crate::usage`]): the bytes a tenant is charged are the span's JSON
//! after tenant stamping and BEFORE payload offload — what the client sent,
//! in Traza's native encoding — so the measurement has to happen here, while
//! the batch is still as sent. Doing both in one pass is what keeps the extra
//! cost to one serialization per span.
//!
//! **Post-offload size without offloading.** `max_span_bytes` bounds the span
//! as STORED, after oversized values become payload references — a span whose
//! size is one large prompt is fine, because the prompt leaves the record. The
//! check runs before the offload writes anything, so it computes the stored
//! size instead: the as-sent size, minus each offloadable value's encoding,
//! plus the encoding of the reference object it would become
//! ([`crate::payload::offloaded_reference`] — the same constructor the
//! offload uses, so the two cannot drift). Refusing after the offload would
//! leave the refused batch's payload files on disk, unaccounted and
//! unreferenced.

use std::io;

use serde::Serialize;
use serde_json::Value;

use crate::{Config, Error, Result, Span};

/// Default for [`Config::max_span_attributes`].
pub const DEFAULT_MAX_SPAN_ATTRIBUTES: usize = 1024;
/// Default for [`Config::max_attribute_key_bytes`].
pub const DEFAULT_MAX_ATTRIBUTE_KEY_BYTES: usize = 256;
/// Default for [`Config::max_span_events`].
pub const DEFAULT_MAX_SPAN_EVENTS: usize = 1024;
/// Default for [`Config::max_span_bytes`]: 4 MiB.
pub const DEFAULT_MAX_SPAN_BYTES: usize = 4 * 1024 * 1024;
/// Default for [`Config::max_batch_spans`].
pub const DEFAULT_MAX_BATCH_SPANS: usize = 100_000;

/// One span's measurement, in batch order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Measured {
    /// Serialized JSON size as sent: after tenant stamping, before offload.
    pub(crate) bytes: u64,
}

/// What admission learned about a batch it let through.
#[derive(Clone, Debug, Default)]
pub(crate) struct Admitting {
    /// One entry per span, in batch order: the i-th measures the i-th span
    /// of the batch as it stands, which [`Admitting::retain`] keeps true
    /// when spans are dropped.
    pub(crate) spans: Vec<Measured>,
}

impl Admitting {
    /// Drops from `spans` every span `keep` refuses, and its measurement
    /// with it, in one pass — so each measurement stays paired with the
    /// span it measured however the batch is filtered.
    ///
    /// The pairing is positional on purpose. Re-aligning afterwards by
    /// primary key cannot work: a batch may carry two spans with one key
    /// that differ in exactly what decides the drop (a session attribute a
    /// pending erasure covers), and a key cannot say which of the two
    /// survived — the survivor would be charged the dropped copy's size.
    pub(crate) fn retain(&mut self, spans: &mut Vec<Span>, mut keep: impl FnMut(&Span) -> bool) {
        debug_assert_eq!(spans.len(), self.spans.len(), "one measurement per span");
        let mut measured = std::mem::take(&mut self.spans).into_iter();
        let mut kept = Vec::with_capacity(spans.len());
        // `Vec::retain` visits every element exactly once, in order, which
        // is what lets the measurements be walked alongside it.
        spans.retain(|span| {
            let measurement = measured.next();
            let keeping = keep(span);
            if keeping {
                kept.extend(measurement);
            }
            keeping
        });
        self.spans = kept;
    }
}

/// Measures `spans` and enforces `config`'s caps, refusing the whole batch
/// with [`Error::Invalid`] on the first violation.
pub(crate) fn measure(config: &Config, spans: &[Span]) -> Result<Admitting> {
    if let Some(cap) = config.max_batch_spans {
        if spans.len() > cap {
            return Err(Error::Invalid(format!(
                "batch of {} spans exceeds the max-batch-spans cap of {cap}",
                spans.len()
            )));
        }
    }
    let mut measured = Vec::with_capacity(spans.len());
    for (index, span) in spans.iter().enumerate() {
        let refuse = |what: String| {
            Error::Invalid(format!(
                "span {index} (trace {}, span {}): {what}",
                shown(&span.trace_id),
                shown(&span.span_id)
            ))
        };
        if let Some(cap) = config.max_span_attributes {
            if span.attributes.len() > cap {
                return Err(refuse(format!(
                    "{} attributes exceeds the max-span-attributes cap of {cap}",
                    span.attributes.len()
                )));
            }
        }
        if let Some(cap) = config.max_span_events {
            if span.events.len() > cap {
                return Err(refuse(format!(
                    "{} events exceeds the max-span-events cap of {cap}",
                    span.events.len()
                )));
            }
        }
        if let Some(cap) = config.max_attribute_key_bytes {
            // Every attribute map the span carries — events' and links' too:
            // each is indexed and walked the same way. The key itself is not
            // echoed; it is exactly the oversized thing being refused.
            if let Some(length) = span
                .payload_maps()
                .flat_map(|map| map.keys())
                .map(String::len)
                .find(|length| *length > cap)
            {
                return Err(refuse(format!(
                    "an attribute key of {length} bytes exceeds the max-attribute-key-bytes \
                     cap of {cap}"
                )));
            }
        }
        let bytes = json_len(span);
        if let Some(cap) = config.max_span_bytes {
            let stored = stored_len(span, bytes, config.payload_threshold);
            if stored > cap as u64 {
                return Err(refuse(format!(
                    "{stored} bytes after payload offload exceeds the max-span-bytes cap of {cap}"
                )));
            }
        }
        measured.push(Measured { bytes });
    }
    Ok(Admitting { spans: measured })
}

/// The span's size once stored: `sent` minus every value the offload would
/// replace, plus the reference object replacing it. Mirrors
/// `payload::offload_span`, which offloads top-level string values of every
/// attribute map longer than the threshold.
fn stored_len(span: &Span, sent: u64, threshold: Option<usize>) -> u64 {
    let Some(threshold) = threshold else {
        return sent;
    };
    let mut stored = sent;
    for value in span.payload_maps().flat_map(|map| map.values()) {
        if let Value::String(text) = value {
            if text.len() > threshold {
                stored = stored
                    .saturating_sub(json_len(text))
                    .saturating_add(json_len(&crate::payload::offloaded_reference(text)));
            }
        }
    }
    stored
}

/// Bytes `value` serializes to as compact JSON, counted without allocating
/// the encoding.
pub(crate) fn json_len<T: Serialize + ?Sized>(value: &T) -> u64 {
    struct Count(u64);
    impl io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Serializing a Span or a Value cannot fail: their keys are strings and
    // the sink never errors.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// An id as an error message may show it: whole when short, cut at a char
/// boundary near 64 bytes otherwise. The ids are client-chosen, and an error
/// must not become a way to make the server echo a megabyte.
fn shown(id: &str) -> String {
    const SHOWN: usize = 64;
    if id.len() <= SHOWN {
        return id.to_owned();
    }
    let mut end = SHOWN;
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &id[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn span(attributes: Value) -> Span {
        Span {
            trace_id: "t1".into(),
            span_id: "s1".into(),
            tenant: String::new(),
            parent_span_id: None,
            name: "op".into(),
            start_time_ns: 1,
            end_time_ns: 2,
            status: "ok".into(),
            service: "svc".into(),
            attributes: attributes.as_object().expect("object").clone(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn refusal(config: &Config, spans: &[Span]) -> String {
        match measure(config, spans) {
            Err(Error::Invalid(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_measured_size_is_the_serialized_size_as_sent() {
        let mut stamped = span(json!({"gen_ai.request.model": "m", "n": 3}));
        stamped.tenant = "acme".into();
        let admitted = measure(&Config::default(), std::slice::from_ref(&stamped)).expect("ok");
        assert_eq!(
            admitted.spans[0].bytes,
            serde_json::to_vec(&stamped).expect("encodes").len() as u64,
            "the tenant stamp is part of what is charged"
        );
    }

    /// Two spans with one primary key, told apart only by an attribute:
    /// whichever the filter keeps, its own measurement stays with it.
    #[test]
    fn retain_keeps_each_measurement_with_its_span() {
        let first = span(json!({"session.id": "doomed", "pad": "x"}));
        let second = span(json!({"session.id": "kept", "pad": "x".repeat(500)}));
        for (batch, survivor) in [
            (vec![first.clone(), second.clone()], &second),
            (vec![second.clone(), first.clone()], &second),
        ] {
            let mut spans = batch;
            let mut admitting = measure(&Config::default(), &spans).expect("ok");
            admitting.retain(&mut spans, |span| span.attributes["session.id"] == "kept");
            assert_eq!(spans.len(), 1);
            assert_eq!(
                admitting.spans,
                vec![Measured {
                    bytes: json_len(survivor)
                }]
            );
        }
        let mut spans = vec![first, second];
        let mut admitting = measure(&Config::default(), &spans).expect("ok");
        admitting.retain(&mut spans, |_| false);
        assert!(spans.is_empty() && admitting.spans.is_empty());
    }

    /// The estimate must equal what the offload actually produces, byte for
    /// byte — including a preview that cuts multi-byte characters and text
    /// that needs escaping.
    #[test]
    fn the_stored_size_estimate_matches_a_real_offload() {
        let dir = std::env::temp_dir().join(format!("traza-caps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let prompt = format!("{}\"quoted\"\n{}", "é".repeat(300), "x".repeat(2_000));
        let mut original = span(json!({"llm.prompt": prompt, "small": "stays inline"}));
        original.events.push(crate::Event {
            name: "e".into(),
            timestamp_ns: 1,
            attributes: json!({"big": "y".repeat(5_000)})
                .as_object()
                .expect("object")
                .clone(),
        });
        let sent = json_len(&original);
        let estimate = stored_len(&original, sent, Some(1_024));
        let registry = crate::payload::TouchRegistry::default();
        let mut offloaded = original.clone();
        crate::payload::offload_span(&dir, &mut offloaded, 1_024, &registry, None)
            .expect("offloads");
        assert_eq!(estimate, json_len(&offloaded));
        assert!(estimate < sent / 4, "{estimate} vs {sent}");
        assert_eq!(stored_len(&original, sent, None), sent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_cap_refuses_naming_itself_and_the_span() {
        let config = Config {
            max_span_attributes: Some(2),
            max_span_events: Some(0),
            max_attribute_key_bytes: Some(8),
            max_span_bytes: Some(400),
            max_batch_spans: Some(3),
            ..Config::default()
        };
        let fine = span(json!({"a": 1}));
        assert!(measure(&config, &[fine.clone(), fine.clone()]).is_ok());

        let message = refusal(&config, &vec![fine.clone(); 4]);
        assert!(message.contains("max-batch-spans cap of 3"), "{message}");

        let message = refusal(
            &config,
            &[fine.clone(), span(json!({"a": 1, "b": 2, "c": 3}))],
        );
        assert!(
            message.contains("span 1 (trace t1, span s1)")
                && message.contains("3 attributes exceeds the max-span-attributes cap of 2"),
            "{message}"
        );

        let mut with_event = fine.clone();
        with_event.events.push(crate::Event {
            name: "e".into(),
            timestamp_ns: 1,
            attributes: Map::new(),
        });
        let message = refusal(&config, &[with_event]);
        assert!(message.contains("max-span-events cap of 0"), "{message}");

        let message = refusal(&config, &[span(json!({"much-too-long-key": 1}))]);
        assert!(
            message.contains("17 bytes exceeds the max-attribute-key-bytes cap of 8")
                && !message.contains("much-too-long-key"),
            "the oversized key is described, not echoed: {message}"
        );

        let message = refusal(&config, &[span(json!({"a": "z".repeat(500)}))]);
        assert!(message.contains("max-span-bytes cap of 400"), "{message}");
    }

    #[test]
    fn a_value_the_offload_removes_does_not_count_against_the_stored_size() {
        let config = Config {
            max_span_bytes: Some(2_000),
            payload_threshold: Some(1_000),
            ..Config::default()
        };
        let big = span(json!({"prompt": "p".repeat(50_000)}));
        let admitted = measure(&config, std::slice::from_ref(&big)).expect("stored size is small");
        assert!(
            admitted.spans[0].bytes > 50_000,
            "but the charge is what was sent"
        );
        let without_offload = Config {
            payload_threshold: None,
            ..config
        };
        assert!(measure(&without_offload, &[big]).is_err());
    }

    #[test]
    fn none_disables_every_cap_and_long_ids_are_shortened() {
        let config = Config {
            max_span_attributes: None,
            max_span_events: None,
            max_attribute_key_bytes: None,
            max_span_bytes: None,
            max_batch_spans: None,
            ..Config::default()
        };
        let mut wide = Map::new();
        for index in 0..(DEFAULT_MAX_SPAN_ATTRIBUTES + 1) {
            wide.insert(format!("k{index}"), json!(index));
        }
        assert!(measure(&config, &[span(Value::Object(wide.clone()))]).is_ok());
        assert!(measure(&Config::default(), &[span(Value::Object(wide))]).is_err());

        let mut long = span(json!({"a": 1, "b": 2}));
        long.trace_id = "é".repeat(100);
        let strict = Config {
            max_span_attributes: Some(1),
            ..Config::default()
        };
        let message = refusal(&strict, &[long]);
        assert!(message.len() < 200, "{message}");
    }
}
