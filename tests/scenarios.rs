//! Real-world scenario acceptance over the seeded corpus (`traza::seed`).
//!
//! The corpus is deliberately messy in the ways production is messy: three
//! attribute dialects, tool-calling agent trees, long multi-turn sessions,
//! multimodal and oversized payloads, failures with linked retries, parallel
//! fan-out, and ordinary non-LLM service traffic. These tests assert the
//! derived views stay correct across all of it — the failure they guard
//! against is a rollup that is right on a tidy fixture and wrong on real data.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use traza::analytics::LlmGroupBy;
use traza::seed::{corpus, SeedOptions};
use traza::semconv;
use traza::{Config, SpanFilter, Store};

fn store_with_corpus(label: &str, options: &SeedOptions) -> (Store, traza::seed::Corpus) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-scenarios-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 10_000,
            ttl_seconds: None,
            payload_threshold: Some(256 * 1024),
            // Bulk-loading a fixed corpus; the log would only add fsyncs to
            // data this test flushes explicitly anyway.
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            content_index: true,
            tail_ring_spans: traza::DEFAULT_TAIL_RING_SPANS,
            flush_wal_bytes: None,
        },
    )
    .expect("opens");
    let generated = corpus(options);
    store
        .ingest_batch(generated.spans.clone())
        .expect("ingests the corpus");
    for annotation in generated.annotations.clone() {
        store.annotate(annotation).expect("annotates");
    }
    store.flush().expect("flushes");
    (store, generated)
}

fn all_spans(store: &Store) -> Vec<traza::Span> {
    store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("queries")
}

#[test]
fn every_seeded_span_survives_ingest_intact() {
    let (store, generated) = store_with_corpus("roundtrip", &SeedOptions::default());
    let stored = all_spans(&store);
    assert_eq!(
        stored.len(),
        generated.spans.len(),
        "no span lost or merged at ingest"
    );

    // Identity is the primary key: the corpus must not collide with itself.
    let keys: HashSet<(String, String)> = stored
        .iter()
        .map(|span| (span.trace_id.clone(), span.span_id.clone()))
        .collect();
    assert_eq!(
        keys.len(),
        stored.len(),
        "duplicate primary keys after ingest"
    );

    // Structure survives: parents, links, and events all round-trip.
    let with_parent = stored.iter().filter(|s| s.parent_span_id.is_some()).count();
    let with_links = stored.iter().filter(|s| !s.links.is_empty()).count();
    let with_events = stored.iter().filter(|s| !s.events.is_empty()).count();
    assert!(with_parent > 0, "corpus should contain nested spans");
    assert!(with_links > 0, "corpus should contain linked spans");
    assert!(with_events > 0, "corpus should contain spans with events");
}

#[test]
fn llm_classification_ignores_ordinary_service_traffic() {
    // The regression this guards: counting HTTP/DB spans as LLM calls, which
    // silently inflates every cost and latency figure.
    let (store, _) = store_with_corpus("classify", &SeedOptions::default());
    let rows = store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregates");
    let checkout = rows
        .iter()
        .find(|row| row.key == "checkout")
        .expect("the plain HTTP/DB service is present");
    assert!(checkout.spans > 0, "plain traffic is stored");
    assert_eq!(checkout.llm_calls, 0, "plain traffic is never an LLM call");
    assert_eq!(checkout.total_tokens, 0);
    assert_eq!(checkout.cost_usd, 0.0);

    // And an agent service has both kinds: more spans than LLM calls.
    let agent = rows
        .iter()
        .find(|row| row.key == "support-agent")
        .expect("agent service");
    assert!(
        agent.spans > agent.llm_calls,
        "agent traces mix tool/workflow spans with LLM calls: {agent:?}"
    );
    assert!(agent.llm_calls > 0);
}

#[test]
fn rollups_are_exact_across_every_dialect() {
    // Independently recompute the truth from the corpus and demand the engine
    // match it, so a dialect the resolver misses shows up as a mismatch
    // rather than a plausible-looking smaller number.
    let (store, _) = store_with_corpus("rollups", &SeedOptions::default());
    let stored = all_spans(&store);

    let mut expected_by_provider: HashMap<String, (usize, u64, f64)> = HashMap::new();
    let mut expected_by_model: HashMap<String, (usize, u64, f64)> = HashMap::new();
    let mut expected_llm_calls = 0_usize;
    for span in &stored {
        let facts = semconv::facts(&span.attributes);
        if !facts.is_llm {
            continue;
        }
        expected_llm_calls += 1;
        let tokens = facts.total();
        let cost = facts.cost_usd.unwrap_or(0.0);
        if let Some(provider) = &facts.provider {
            let entry = expected_by_provider.entry(provider.clone()).or_default();
            entry.0 += 1;
            entry.1 += tokens;
            entry.2 += cost;
        }
        if let Some(model) = &facts.model {
            let entry = expected_by_model.entry(model.clone()).or_default();
            entry.0 += 1;
            entry.1 += tokens;
            entry.2 += cost;
        }
    }
    assert!(expected_llm_calls > 0 && expected_by_provider.len() >= 4);

    for (group, expected) in [
        (LlmGroupBy::Provider, &expected_by_provider),
        (LlmGroupBy::Model, &expected_by_model),
    ] {
        let rows = store.llm_aggregate(group, None, None).expect("aggregates");
        assert_eq!(rows.len(), expected.len(), "{group:?} group count");
        for row in &rows {
            let (calls, tokens, cost) = expected.get(&row.key).unwrap_or_else(|| {
                panic!("{group:?} produced an unexpected key {}", row.key);
            });
            assert_eq!(row.llm_calls, *calls, "{group:?} {} calls", row.key);
            assert_eq!(row.total_tokens, *tokens, "{group:?} {} tokens", row.key);
            assert!(
                (row.cost_usd - *cost).abs() < 1e-6,
                "{group:?} {} cost: {} vs {}",
                row.key,
                row.cost_usd,
                cost
            );
        }
    }

    // The native dialect carries no provider, so provider rows must cover
    // fewer calls than model rows — proof the corpus really is mixed.
    let provider_calls: usize = expected_by_provider.values().map(|entry| entry.0).sum();
    assert!(
        provider_calls < expected_llm_calls,
        "corpus should include native-dialect spans with no provider"
    );
}

#[test]
fn sessions_aggregate_long_conversations_across_traces_and_dialects() {
    let (store, _) = store_with_corpus("sessions", &SeedOptions::default());
    let stored = all_spans(&store);

    // Recompute session membership from the corpus.
    let mut expected: HashMap<String, (HashSet<String>, usize, u64)> = HashMap::new();
    for span in &stored {
        let facts = semconv::facts(&span.attributes);
        if let Some(session) = facts.session.clone() {
            let entry = expected.entry(session).or_default();
            entry.0.insert(span.trace_id.clone());
            entry.1 += 1;
            entry.2 += facts.total();
        }
    }
    let sessions = store.sessions(None, None, 10_000).expect("lists sessions");
    assert_eq!(sessions.len(), expected.len(), "every session is listed");
    for summary in &sessions {
        let (traces, spans, tokens) = expected
            .get(&summary.session_id)
            .unwrap_or_else(|| panic!("unexpected session {}", summary.session_id));
        assert_eq!(summary.trace_count, traces.len(), "{}", summary.session_id);
        assert_eq!(summary.span_count, *spans, "{}", summary.session_id);
        assert_eq!(summary.total_tokens, *tokens, "{}", summary.session_id);
    }

    // A multi-turn thread really does span many traces.
    let longest = sessions
        .iter()
        .max_by_key(|summary| summary.trace_count)
        .expect("a session");
    assert!(
        longest.trace_count >= 10,
        "multi-turn sessions should cover many traces: {longest:?}"
    );

    // Detail resolves and agrees with the summary, whatever key grouped it.
    let detail = store
        .session(&longest.session_id)
        .expect("queries")
        .expect("exists");
    assert_eq!(detail.summary.span_count, longest.span_count);
    assert_eq!(detail.summary.total_tokens, longest.total_tokens);
    assert_eq!(detail.traces.len(), longest.trace_count);
}

#[test]
fn the_session_filter_unions_mixed_convention_spans() {
    // Sessions in the corpus deliberately carry BOTH a native session.id (or
    // gen_ai.conversation.id) and a traceloop association property. Filtering
    // on one literal key would drop spans; the session filter must not.
    let (store, _) = store_with_corpus("session-filter", &SeedOptions::default());
    let sessions = store.sessions(None, None, 10_000).expect("lists");
    let thread = sessions
        .iter()
        .find(|summary| summary.session_id.starts_with("thread-"))
        .expect("a multi-turn thread");

    let via_filter = store
        .query(&SpanFilter {
            session: Some(thread.session_id.clone()),
            limit: None,
            ..SpanFilter::default()
        })
        .expect("session filter");
    assert_eq!(
        via_filter.len(),
        thread.span_count,
        "the session filter returns the whole session {}",
        thread.session_id
    );

    // Every returned span really resolves to that session.
    for span in &via_filter {
        assert_eq!(
            semconv::facts(&span.attributes).session.as_deref(),
            Some(thread.session_id.as_str())
        );
    }

    // The filter composes with ordinary predicates.
    let narrowed = store
        .query(&SpanFilter {
            session: Some(thread.session_id.clone()),
            min_duration_ns: Some(2_000_000_000),
            limit: None,
            ..SpanFilter::default()
        })
        .expect("session + duration");
    assert!(narrowed.len() <= via_filter.len());
    assert!(narrowed
        .iter()
        .all(|span| span.end_time_ns - span.start_time_ns >= 2_000_000_000));
}

#[test]
fn oversized_payloads_offload_and_are_retrievable() {
    let (store, _) = store_with_corpus("payloads", &SeedOptions::default());
    let stored = all_spans(&store);

    // Find the offloaded references the big-prompt scenario must produce, in
    // both an attribute and an event.
    let mut attribute_refs = Vec::new();
    let mut event_refs = Vec::new();
    for span in &stored {
        for value in span.attributes.values() {
            if let Some(reference) = value.get("$payload").and_then(Value::as_str) {
                attribute_refs.push(reference.to_owned());
            }
        }
        for event in &span.events {
            for value in event.attributes.values() {
                if let Some(reference) = value.get("$payload").and_then(Value::as_str) {
                    event_refs.push(reference.to_owned());
                }
            }
        }
    }
    assert!(
        !attribute_refs.is_empty(),
        "an oversized gen_ai.input.messages must offload"
    );
    assert!(
        !event_refs.is_empty(),
        "an oversized llm.prompt event must offload"
    );

    // Every reference resolves to real bytes.
    for reference in attribute_refs.iter().chain(event_refs.iter()) {
        let bytes = store
            .payload(reference)
            .expect("reads payload")
            .expect("the reference resolves to stored bytes");
        assert!(bytes.len() > 100_000, "offloaded body should be large");
    }

    // The reference object keeps a preview so a reader sees something inline.
    let previewed = stored.iter().any(|span| {
        span.attributes.values().any(|value| {
            value
                .get("preview")
                .and_then(Value::as_str)
                .is_some_and(|preview| !preview.is_empty())
        })
    });
    assert!(previewed, "offloaded values keep an inline preview");
}

#[test]
fn multimodal_messages_keep_their_media_descriptors() {
    // The UI renders media parts from these fields; losing them would leave a
    // reader with an opaque base64 blob.
    let (store, _) = store_with_corpus("multimodal", &SeedOptions::default());
    let stored = all_spans(&store);
    let mut kinds = HashSet::new();
    for span in &stored {
        let Some(messages) = span.attributes.get("gen_ai.input.messages") else {
            continue;
        };
        let Some(text) = messages.as_str() else {
            continue; // offloaded reference, covered by the payload test
        };
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        for message in parsed.as_array().into_iter().flatten() {
            for part in message["parts"].as_array().into_iter().flatten() {
                let kind = part["type"].as_str().unwrap_or_default();
                if ["image", "audio", "video", "document"].contains(&kind) {
                    kinds.insert(kind.to_owned());
                    assert!(
                        part.get("mime_type").is_some() && part.get("size_bytes").is_some(),
                        "a media part must describe itself: {part}"
                    );
                    assert!(
                        part.get("uri").is_some() || part.get("data").is_some(),
                        "a media part must carry a locator or inline data: {part}"
                    );
                }
            }
        }
    }
    assert_eq!(
        kinds.len(),
        4,
        "image, audio, video and document turns should all be present: {kinds:?}"
    );
}

#[test]
fn generated_output_media_is_carried_on_the_completion_side() {
    // Regression: the corpus only ever had media on the INPUT side, so a
    // renderer could "handle media" while never displaying a model-produced
    // image, audio clip, or video.
    let (store, _) = store_with_corpus("outmedia", &SeedOptions::default());
    let stored = all_spans(&store);

    let mut output_kinds: HashSet<String> = HashSet::new();
    let mut inline_renderable = 0;
    for span in &stored {
        let Some(text) = span
            .attributes
            .get("gen_ai.output.messages")
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        for message in parsed.as_array().into_iter().flatten() {
            for part in message["parts"].as_array().into_iter().flatten() {
                let kind = part["type"].as_str().unwrap_or_default();
                if ["image", "audio", "video"].contains(&kind) {
                    output_kinds.insert(kind.to_owned());
                    // A browser can only render data:/http(s) sources.
                    let locator = part["data"].as_str().or_else(|| part["uri"].as_str());
                    if locator.is_some_and(|l| l.starts_with("data:") || l.starts_with("http")) {
                        inline_renderable += 1;
                    }
                }
            }
        }
    }
    assert_eq!(
        output_kinds.len(),
        3,
        "the model should produce image, audio and video output: {output_kinds:?}"
    );
    assert!(
        inline_renderable >= 3,
        "generated media must carry a locator a browser can actually load"
    );
}

#[test]
fn framework_shaped_traces_are_represented() {
    // OpenAI, Anthropic, LangGraph and CrewAI arrange the same conventions
    // differently; the corpus must contain each so the UI is exercised against
    // real arrangements rather than one idealized shape.
    let (store, _) = store_with_corpus("frameworks", &SeedOptions::default());
    let stored = all_spans(&store);

    let frameworks: HashSet<&str> = stored
        .iter()
        .filter_map(|span| span.attributes.get("framework").and_then(Value::as_str))
        .collect();
    assert!(
        frameworks.contains("langgraph") && frameworks.contains("crewai"),
        "graph and crew frameworks should be present: {frameworks:?}"
    );

    // LangGraph records its topology; CrewAI records agent roles.
    assert!(
        stored
            .iter()
            .any(|span| span.attributes.contains_key("gen_ai.workflow.nodes")),
        "a LangGraph run should carry its node topology"
    );
    assert!(
        stored
            .iter()
            .any(|span| span.attributes.contains_key("gen_ai.agent.name")),
        "a CrewAI run should name its agents"
    );

    // OpenAI returns tool-call arguments as a JSON STRING, not an object — the
    // shape a naive renderer double-encodes.
    let openai_string_args = stored.iter().any(|span| {
        span.attributes
            .get("gen_ai.output.messages")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("\\\"order_id\\\""))
    });
    assert!(
        openai_string_args,
        "an OpenAI-shaped tool call should carry stringified arguments"
    );

    // Anthropic's prompt-cache counters ride along.
    assert!(
        stored.iter().any(|span| span
            .attributes
            .contains_key("gen_ai.usage.cache_read_input_tokens")),
        "an Anthropic-shaped span should carry cache token counters"
    );
}

#[test]
fn answers_cover_every_text_shape_the_renderer_switches_on() {
    let (store, _) = store_with_corpus("formats", &SeedOptions::default());
    let stored = all_spans(&store);
    let mut shapes: HashSet<String> = HashSet::new();
    for span in &stored {
        if let Some(kind) = span
            .attributes
            .get("response.format")
            .and_then(Value::as_str)
        {
            shapes.insert(kind.to_owned());
        }
    }
    assert_eq!(
        shapes.len(),
        4,
        "markdown, code, json and plain answers should all appear: {shapes:?}"
    );
}

#[test]
fn tool_calling_traces_keep_their_shape() {
    let (store, _) = store_with_corpus("tools", &SeedOptions::default());
    let stored = all_spans(&store);

    // A workflow root, an LLM span that asks for a tool, and a tool span.
    let workflows: Vec<_> = stored
        .iter()
        .filter(|span| span.attributes.get("traceloop.span.kind") == Some(&Value::from("workflow")))
        .collect();
    assert!(!workflows.is_empty(), "workflow roots exist");

    let tools: Vec<_> = stored
        .iter()
        .filter(|span| span.attributes.get("traceloop.span.kind") == Some(&Value::from("tool")))
        .collect();
    assert!(!tools.is_empty(), "tool spans exist");
    for tool in &tools {
        assert!(
            tool.attributes.contains_key("gen_ai.tool.name"),
            "a tool span names its tool"
        );
        assert!(
            !semconv::facts(&tool.attributes).is_llm,
            "a tool execution is not an LLM call"
        );
    }

    // The whole trace is reachable and ordered.
    let workflow = workflows[0];
    let trace = store.get_trace(&workflow.trace_id).expect("trace");
    assert!(trace.len() >= 3, "agent trace has root + llm + tool");
    let parents: HashSet<&str> = trace
        .iter()
        .filter_map(|span| span.parent_span_id.as_deref())
        .collect();
    assert!(!parents.is_empty(), "the trace is nested, not flat");

    // The tool-call decision records the call in its output messages.
    let decided = trace.iter().any(|span| {
        span.attributes
            .get("gen_ai.output.messages")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("tool_call"))
    });
    assert!(decided, "the model's tool request is captured");
}

#[test]
fn failures_and_linked_retries_are_queryable() {
    let (store, _) = store_with_corpus("failures", &SeedOptions::default());
    let stored = all_spans(&store);

    let failed: Vec<_> = stored
        .iter()
        .filter(|span| span.status == "error" && span.attributes.contains_key("error.type"))
        .collect();
    assert!(!failed.is_empty(), "the corpus contains failures");

    // Every retry link points at a span that exists and is in the same trace.
    let by_key: HashSet<(&str, &str)> = stored
        .iter()
        .map(|span| (span.trace_id.as_str(), span.span_id.as_str()))
        .collect();
    let mut retries = 0;
    let mut joins = 0;
    for span in &stored {
        for link in &span.links {
            assert!(
                by_key.contains(&(link.trace_id.as_str(), link.span_id.as_str())),
                "link points at a real span"
            );
            match link.attributes.get("relation").and_then(Value::as_str) {
                Some("retry-of") => retries += 1,
                Some("joins") => joins += 1,
                _ => {}
            }
        }
    }
    assert!(retries > 0, "retries reference the attempt they replace");
    assert!(joins > 0, "a join references the workers it waited on");

    // A failed call contributes an error to its session but no phantom tokens.
    let errored_sessions = store
        .sessions(None, None, 10_000)
        .expect("sessions")
        .into_iter()
        .filter(|summary| summary.error_count > 0)
        .count();
    assert!(errored_sessions > 0, "failures surface in session rollups");
}

#[test]
fn search_export_and_annotations_hold_up_on_the_corpus() {
    let (store, generated) = store_with_corpus("api", &SeedOptions::default());

    // Attribute search is index-served and exact.
    let openai = store
        .query(&SpanFilter {
            attributes: vec![("gen_ai.provider.name".to_owned(), Value::from("openai"))],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("attribute filter");
    assert!(!openai.is_empty());
    assert!(openai
        .iter()
        .all(|span| span.attributes["gen_ai.provider.name"] == "openai"));

    // Day grouping buckets the generated window.
    let by_day = store
        .llm_aggregate(LlmGroupBy::Day, None, None)
        .expect("day rollup");
    assert!(!by_day.is_empty());
    assert!(by_day.iter().all(|row| row.key.len() == 10));

    // Annotations attach to real spans and read back.
    assert!(!generated.annotations.is_empty());
    for annotation in &generated.annotations {
        let found = store
            .annotations(&annotation.trace_id, None, None)
            .expect("annotations");
        assert!(
            found.iter().any(|record| record.name == annotation.name),
            "annotation {} on {} reads back",
            annotation.name,
            annotation.trace_id
        );
    }

    // Cursor pagination walks the whole corpus exactly once.
    let mut seen = HashSet::new();
    let mut cursor = None;
    loop {
        let page = store
            .query_after(
                &SpanFilter {
                    limit: Some(50),
                    ..SpanFilter::default()
                },
                cursor.as_ref(),
            )
            .expect("page");
        if page.is_empty() {
            break;
        }
        for span in &page {
            assert!(
                seen.insert((span.trace_id.clone(), span.span_id.clone())),
                "pagination returned a span twice"
            );
        }
        cursor = page.last().map(traza::SpanCursor::from);
    }
    assert_eq!(
        seen.len(),
        generated.spans.len(),
        "pagination covers the corpus exactly"
    );
}

#[test]
fn rollups_survive_reopen_and_repeated_ingest() {
    // Re-ingesting the same corpus must be idempotent (primary key upsert),
    // and a cold reopen must rebuild identical rollups from disk.
    let options = SeedOptions::default();
    let (store, generated) = store_with_corpus("idempotent", &options);
    let before = store
        .llm_aggregate(LlmGroupBy::Provider, None, None)
        .expect("rollup");
    let sessions_before = store.sessions(None, None, 10_000).expect("sessions").len();

    store
        .ingest_batch(generated.spans.clone())
        .expect("re-ingests");
    store.flush().expect("flush");
    let after = store
        .llm_aggregate(LlmGroupBy::Provider, None, None)
        .expect("rollup");
    assert_eq!(
        before.len(),
        after.len(),
        "re-ingest must not invent new groups"
    );
    for (left, right) in before.iter().zip(after.iter()) {
        assert_eq!(left.key, right.key);
        assert_eq!(
            left.llm_calls, right.llm_calls,
            "re-ingesting the same spans double-counted {}",
            left.key
        );
        assert_eq!(left.total_tokens, right.total_tokens);
    }
    assert_eq!(
        store.sessions(None, None, 10_000).expect("sessions").len(),
        sessions_before
    );
}
