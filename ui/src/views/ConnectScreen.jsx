import React from 'react';
import { api } from '../lib/api.js';
import { useRead, usePoll } from '../lib/route.js';
import { fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, LiveDot, Mono } from '../components/primitives/Chrome.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';

// First run. Two environment variables, then watch the first span arrive —
// the screen keeps polling and tells you the moment it does, because "did it
// work?" is the only question anybody has at this point.

export function ConnectScreen({ go }) {
  const stats = useRead((signal) => api.stats(signal), []);
  const [count, setCount] = React.useState(null);
  const baseline = React.useRef(null);

  usePoll(async () => {
    try {
      const now = await api.stats();
      if (baseline.current == null) baseline.current = now.record_count;
      setCount(now.record_count);
    } catch (e) { /* keep waiting */ }
  }, 2000);

  const records = count ?? stats.data?.record_count ?? 0;
  const arrived = records > 0;
  // The origin is read from the page rather than written into it, so the
  // instructions are correct behind any host, port or reverse proxy.
  const origin = typeof window !== 'undefined' ? window.location.origin : 'http://localhost:8080';

  return <div style={{ display: 'grid', gap: 14, maxWidth: 900 }}>
    <Card>
      <div style={{ display: 'flex', alignItems: 'center', gap: 9, marginBottom: 10 }}>
        <LiveDot color={arrived ? 'var(--ok)' : 'var(--warn)'} />
        <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)' }}>
          {arrived ? 'Spans are arriving.' : 'Waiting for the first span.'}
        </span>
        <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--accent)' }}>
          {fmtNum(records)} records
        </span>
      </div>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink-muted)', textWrap: 'pretty' }}>
        {arrived
          ? <>This store holds <Mono color="var(--ink)">{fmtNum(records)}</Mono> records. Nothing else
            is needed — open Traces to search them, or Live tail to watch them land.</>
          : <>Point any OpenTelemetry SDK at this server. Traza accepts OTLP/HTTP on{' '}
            <Mono color="var(--ink)">/v1/traces</Mono> (protobuf or JSON) and its own JSON on{' '}
            <Mono color="var(--ink)">/v1/spans</Mono>. This page updates the moment something arrives.</>}
      </div>
      {arrived ? <div style={{ display: 'flex', gap: 6, marginTop: 12 }}>
        <Chip tone="primary" onClick={() => go(['traces'])}>Open Traces</Chip>
        <Chip onClick={() => go(['tail'])}>Watch the live tail</Chip>
      </div> : null}
    </Card>

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>1 · point an SDK at it</Eyebrow>
      <CodeBlock code={`export OTEL_EXPORTER_OTLP_ENDPOINT=${origin}
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_COMPRESSION=none`} />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8, lineHeight: '19px' }}>
        Compression is off because Traza does not decompress OTLP bodies — an SDK that gzips will
        get a decode error rather than silently losing data.
      </div>
    </Card>

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>2 · or send one span by hand</Eyebrow>
      <CodeBlock code={`curl -X POST ${origin}/v1/spans \\
  -H 'Content-Type: application/json' \\
  -d '[{"trace_id":"trace-1","span_id":"span-1","name":"hello",
        "service":"my-agent","status":"ok",
        "start_time_ns":${Date.now()}000000,"end_time_ns":${Date.now() + 12}000000,
        "attributes":{"gen_ai.request.model":"gpt-4o",
                      "gen_ai.usage.prompt_tokens":412,
                      "gen_ai.usage.completion_tokens":88}}]'`} />
    </Card>

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>3 · what Traza recognizes</Eyebrow>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink-muted)', textWrap: 'pretty' }}>
        Token counts, cost and model resolve from the OTel GenAI conventions
        (<Mono color="var(--ink)">gen_ai.*</Mono>), OpenLLMetry
        (<Mono color="var(--ink)">llm.*</Mono>, <Mono color="var(--ink)">traceloop.*</Mono>), or Traza's
        own shorthand. Sessions group by <Mono color="var(--ink)">session.id</Mono>,{' '}
        <Mono color="var(--ink)">gen_ai.conversation.id</Mono>, or a{' '}
        <Mono color="var(--ink)">traceloop.association.properties.*</Mono> key — a session whose spans
        mix conventions still returns whole.
      </div>
    </Card>
  </div>;
}
