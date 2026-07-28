import React from 'react';
import { api, getToken, MCP_PROTOCOL_VERSION } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { Card, Chip, Eyebrow, ErrorState, LiveDot, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';

// Connecting an agent to READ this store, which is the other direction from
// the Connect screen's "send spans in".
//
// Everything here is asked of the running server rather than written into the
// page: the state, the tools, the resources, the prompts. A screen that listed
// what this build believes the surface to be would be wrong the moment the
// server was started without --mcp-annotations, and wrong in the direction
// that costs somebody an afternoon.

const TOOL_NOTE = {
  describe_store: 'the orientation call an agent should make first',
  search_spans: 'the general filter, one compact line per span',
  get_trace: 'a trace as a tree — usually the answer itself',
  list_sessions: 'conversations by recency, cost, errors or tokens',
  get_session: 'one conversation, trace by trace',
  top_failures: 'errors grouped by signature, with an example to open',
  slowest_spans: 'the tail, ranked across the whole match set',
  analyze_cost: 'tokens and money by model, service, session or day',
  get_payload: 'the full text behind a $payload reference',
  record_annotation: 'the only writer — append-only, forced source',
};

function Row({ name, note, extra }) {
  return <div style={{
    display: 'flex', alignItems: 'baseline', gap: 10, padding: '5px 0',
    borderTop: '1px solid var(--hairline)',
  }}>
    <Mono size={12} style={{ minWidth: 168 }}>{name}</Mono>
    <span style={{ fontSize: 12, color: 'var(--ink-muted)', flex: 1, textWrap: 'pretty' }}>{note}</span>
    {extra}
  </div>;
}

export function McpScreen({ go }) {
  // One probe drives the whole screen. `initialize` is the cheapest call that
  // proves the endpoint is really serving rather than merely routed.
  const probe = useRead((signal) => api.mcp('initialize', {
    protocolVersion: MCP_PROTOCOL_VERSION,
    capabilities: {},
    clientInfo: { name: 'traza-dashboard', version: 'ui' },
  }, signal), []);

  const enabled = probe.data?.enabled === true;
  const surface = useRead(async (signal) => {
    if (!enabled) return null;
    const [tools, resources, prompts, templates] = await Promise.all([
      api.mcp('tools/list', {}, signal),
      api.mcp('resources/list', {}, signal),
      api.mcp('prompts/list', {}, signal),
      api.mcp('resources/templates/list', {}, signal),
    ]);
    return {
      tools: tools.result?.tools || [],
      resources: resources.result?.resources || [],
      prompts: prompts.result?.prompts || [],
      templates: templates.result?.resourceTemplates || [],
    };
  }, [enabled], { skip: !enabled });

  const origin = typeof window !== 'undefined' ? window.location.origin : 'http://localhost:8080';
  // The stdio bridge speaks plain HTTP and refuses an https:// URL outright.
  // Interpolating this page's origin into it behind TLS produced a snippet
  // that looked copy-ready and could not work, so on https the bridge block
  // asks for the plaintext endpoint instead of inventing one.
  const secure = typeof window !== 'undefined' && window.location.protocol === 'https:';
  const token = getToken();
  const writable = (surface.data?.tools || []).some((tool) => tool.name === 'record_annotation');

  if (probe.error) {
    return <ErrorState what={probe.error.what} next={probe.error.next} onRetry={probe.reload} />;
  }

  return <div style={{ display: 'grid', gap: 14, maxWidth: 900 }}>
    <Card>
      <div style={{ display: 'flex', alignItems: 'center', gap: 9, marginBottom: 10 }}>
        <LiveDot color={enabled ? 'var(--ok)' : 'var(--warn)'} />
        <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)' }}>
          {probe.loading ? 'Checking…' : enabled ? 'The MCP endpoint is serving.' : 'The MCP endpoint is off.'}
        </span>
        {enabled ? <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--accent)' }}>
          {probe.data?.result?.serverInfo?.name} {probe.data?.result?.serverInfo?.version} · {probe.data?.result?.protocolVersion}
        </span> : null}
      </div>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink-muted)', textWrap: 'pretty' }}>
        {enabled
          ? <>An agent can read this store directly — searching spans, opening traces, grouping
            failures and attributing cost — over <Mono color="var(--ink)">{origin}/v1/mcp</Mono>.
            {/* Only once the tool list has actually arrived: claiming "read-only"
                while it is still loading is a sentence that flips under the reader. */}
            {!surface.data ? null : writable
              ? <> Annotations are writable by a token with the <Mono color="var(--ink)">rw</Mono> scope.</>
              : <> Every tool is read-only; annotations need <Mono color="var(--ink)">--mcp-annotations</Mono>.</>}</>
          : <>The endpoint is off by default. Serving it means exposing every stored prompt to
            whatever holds the token, so it is a decision an operator makes rather than
            something that happens.</>}
      </div>
    </Card>

    {!enabled && !probe.loading ? <Card>
      <Eyebrow style={{ marginBottom: 10 }}>turn it on</Eyebrow>
      <CodeBlock code={`traza-server --data-dir ./data --mcp

# and, to let an rw token record annotations as agent:mcp
traza-server --data-dir ./data --mcp --mcp-annotations`} />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8, lineHeight: '19px' }}>
        Two more flags bound what one call may return:{' '}
        <Mono color="var(--ink)">--mcp-max-result-bytes</Mono> (32 KiB) and{' '}
        <Mono color="var(--ink)">--mcp-max-payload-bytes</Mono> (256 KiB). Served behind a
        hostname rather than loopback, this page is a browser origin like any other and needs{' '}
        <Mono color="var(--ink)">--mcp-allowed-origin {origin}</Mono>. Restart to apply — this
        page rechecks on reload.
      </div>
    </Card> : null}

    {enabled ? <Card>
      <PanelHead title="Point a client at it" note="streamable HTTP, one endpoint" />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', margin: '8px 0 10px', lineHeight: '19px' }}>
        Clients that speak HTTP connect straight to the endpoint. Clients that launch their server
        as a subprocess use the bundled stdio bridge, which translates framing and nothing else.
      </div>
      <CodeBlock code={`# HTTP client (Claude Code)
claude mcp add --transport http traza ${origin}/v1/mcp${token ? ` \\
  --header "Authorization: Bearer ${token}"` : ''}

# stdio client (.mcp.json / claude_desktop_config.json)
{
  "mcpServers": {
    "traza": {
      "command": "traza-server",
      "args": ["mcp", "--url", "${secure ? 'http://TRAZA-HOST:PORT' : origin}"]${token ? `,
      "env": { "TRAZA_TOKEN": "${token}" }` : ''}
    }
  }
}`} />
      {secure ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8, lineHeight: '19px' }}>
        This page is served over TLS, and the stdio bridge speaks plain HTTP — so the URL above is
        a placeholder, not this origin. Point it at the server's plaintext address behind your
        proxy (<Mono color="var(--ink)">http://localhost:8080</Mono> on the host itself), or use
        the HTTP client form, which handles <Mono color="var(--ink)">https://</Mono> directly.
      </div> : null}
      {token ? <div style={{ fontSize: 12, color: 'var(--warn)', marginTop: 8, lineHeight: '19px' }}>
        The snippet above contains the bearer token you entered in this browser session. Treat it
        the way you would treat the token itself.
      </div> : null}
    </Card> : null}

    {enabled ? <Card>
      <PanelHead
        title="Tools"
        note={`${(surface.data?.tools || []).length} advertised to this token`}
        action={<Chip onClick={() => go(['traces'])}>Same data, by hand</Chip>}
      />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', margin: '8px 0 4px', lineHeight: '19px' }}>
        Read from the running server, so this is exactly what an agent holding your token would be
        offered. A tool it will be refused on is never advertised.
      </div>
      {surface.loading ? <Skeleton height={90} /> : (surface.data?.tools || []).map((tool) => (
        <Row key={tool.name} name={tool.name} note={TOOL_NOTE[tool.name] || tool.title} />
      ))}
    </Card> : null}

    {enabled ? <Card>
      <PanelHead title="Resources and prompts" note="attachable context, and saved investigations" />
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', margin: '8px 0 4px', lineHeight: '19px' }}>
        Resources are context a host can attach without a tool call; templates address one trace,
        session or payload by id. Prompts are user-controlled — most hosts surface them as slash
        commands.
      </div>
      {(surface.data?.resources || []).map((resource) => (
        <Row key={resource.uri} name={resource.uri.replace('traza://', '')} note={resource.description} />
      ))}
      {(surface.data?.templates || []).map((template) => (
        <Row key={template.uriTemplate} name={template.uriTemplate.replace('traza://', '')} note={template.description} />
      ))}
      {(surface.data?.prompts || []).map((prompt) => (
        <Row key={prompt.name} name={'/' + prompt.name} note={prompt.description} />
      ))}
    </Card> : null}

    <Card>
      <Eyebrow style={{ marginBottom: 10 }}>what an agent sees</Eyebrow>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink-muted)', textWrap: 'pretty' }}>
        Results are bounded in tokens rather than rows: twenty spans by default, stored prompts and
        completions omitted until asked for, and every result capped — with the truncation stated,
        because a silently shortened answer gets reported as a complete one. Stored span text is
        returned inside a delimited block marked untrusted: it can contain whatever a user or a
        third party wrote, and this server holds no fetcher, shell or outbound path for such text
        to actuate.
      </div>
    </Card>
  </div>;
}
