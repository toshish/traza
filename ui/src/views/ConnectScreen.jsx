import React from 'react';
import { api } from '../lib/api.js';
import { publicBase } from '../lib/base.js';
import { usePoll } from '../lib/route.js';
import { fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, LiveDot, Mono } from '../components/primitives/Chrome.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';

// First run. Two environment variables, then watch the first span arrive —
// the screen keeps polling and tells you the moment it does, because "did it
// work?" is the only question anybody has at this point.

/** How many spans the current credential can see arrive.

    An operator reads the store's record count (GET /v1/stats). A
    tenant-bound credential may not — that count includes every co-tenant —
    so it reads its own tenant's sent-span counter (GET /v1/usage), and if
    that answers nothing it asks for one span of its own (GET /v1/spans
    ?limit=1). Either way "waiting for the first span" ends when the first
    span this credential can see lands, which is the only first span that
    matters to whoever is holding it. */
export async function arrivedCount(identity) {
  if (identity && identity.operator) {
    const stats = await api.stats();
    return { count: stats.record_count, label: 'records' };
  }
  try {
    const usage = await api.usage();
    const rows = usage?.tenants || [];
    const row = rows.find((r) => r.tenant === (identity?.tenant || '')) || rows[0];
    if (row) return { count: row.lifetime.spans, label: 'spans sent' };
  } catch (e) { /* fall through to a search */ }
  const page = await api.spans({ limit: 1 });
  return { count: page?.spans?.length ? null : 0, label: 'spans', any: Boolean(page?.spans?.length) };
}

export function ConnectScreen({ go, identity }) {
  const [seen, setSeen] = React.useState(null);

  const check = React.useCallback(async () => {
    if (!identity || identity.unauthorized) return;
    try { setSeen(await arrivedCount(identity)); } catch (e) { /* keep waiting */ }
  }, [identity]);
  React.useEffect(() => { check(); }, [check]);
  usePoll(check, 2000, Boolean(identity && !identity.unauthorized));

  const arrived = Boolean(seen && (seen.any || (seen.count || 0) > 0));
  // The base is read from the page (origin plus any path prefix) rather than
  // written into it, so the instructions are correct behind any host, port or
  // reverse proxy.
  const base = publicBase();
  const token = identity && identity.auth === 'token';
  const scoped = identity && identity.tenant;

  return <div style={{ display: 'grid', gap: 14, maxWidth: 900 }}>
    <Card>
      <div style={{ display: 'flex', alignItems: 'center', gap: 9, marginBottom: 10, flexWrap: 'wrap' }}>
        <LiveDot color={arrived ? 'var(--ok)' : 'var(--warn)'} />
        <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)' }}>
          {arrived ? 'Spans are arriving.' : 'Waiting for the first span.'}
        </span>
        {seen && seen.count != null ? <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--accent)' }}>
          {fmtNum(seen.count)} {seen.label}
        </span> : null}
      </div>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink-muted)', textWrap: 'pretty' }}>
        {arrived
          ? <>{scoped ? <>Tenant <Mono color="var(--ink)">{identity.tenant}</Mono> has spans. </> : <>This store has spans. </>}
            Nothing else is needed — open Traces to search them, or Live tail to watch them land.</>
          : <>Point any OpenTelemetry SDK at this server. Traza accepts OTLP/HTTP on{' '}
            <Mono color="var(--ink)">/v1/traces</Mono> (protobuf or JSON) and its own JSON on{' '}
            <Mono color="var(--ink)">/v1/spans</Mono>. This page updates the moment something
            {scoped ? <> arrives for tenant <Mono color="var(--ink)">{identity.tenant}</Mono>.</> : ' arrives.'}</>}
      </div>
      {arrived ? <div style={{ display: 'flex', gap: 6, marginTop: 12, flexWrap: 'wrap' }}>
        <Chip tone="primary" onClick={() => go(['traces'])}>Open Traces</Chip>
        <Chip onClick={() => go(['tail'])}>Watch the live tail</Chip>
        <Chip onClick={() => go(['guard'])}>See runs in Guard</Chip>
        <Chip onClick={() => go(['mcp'])}>Let an agent read it</Chip>
      </div> : null}
    </Card>

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>1 · point an SDK at it</Eyebrow>
      <CodeBlock language="sh" code={`export OTEL_EXPORTER_OTLP_ENDPOINT=${base}
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_COMPRESSION=gzip${token ? `
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer%20$TRAZA_TOKEN"` : ''}`} />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8, lineHeight: '19px' }}>
        Traza decompresses gzip itself, so the exporter may compress; no other coding is accepted.
        {token ? <> The server requires a token: set <code>TRAZA_TOKEN</code> to an <code>rw</code>{' '}
          credential{scoped ? <> bound to <code>{identity.tenant}</code> — its spans are stamped with
          that tenant</> : null}. The header value is URL-encoded, as the OpenTelemetry specification
          requires (<code>%20</code> is the space).</> : null}
      </div>
    </Card>

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>2 · or send one span by hand</Eyebrow>
      <CodeBlock language="sh" code={`curl -X POST ${base}/v1/spans \\
  -H 'Content-Type: application/json' \\${token ? `
  -H "Authorization: Bearer $TRAZA_TOKEN" \\` : ''}
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
        (<Mono color="var(--ink)">llm.*</Mono>, <Mono color="var(--ink)">traceloop.*</Mono>), OpenInference
        (<Mono color="var(--ink)">openinference.span.kind</Mono>, <Mono color="var(--ink)">llm.token_count.*</Mono>),
        or Traza's own shorthand. Sessions group by <Mono color="var(--ink)">session.id</Mono>,{' '}
        <Mono color="var(--ink)">gen_ai.conversation.id</Mono>, or a{' '}
        <Mono color="var(--ink)">traceloop.association.properties.*</Mono> key — a session whose spans
        mix conventions still returns whole, and is one run for the guard, the tape and the diff.
      </div>
    </Card>
  </div>;
}
