import React from 'react';
import { api } from '../lib/api.js';
import { navigate, useRead } from '../lib/route.js';
import { fmtNum, fmtTimeNs } from '../lib/format.js';
import { routeTenant, tenantParams } from '../lib/runs.js';
import {
  SEVERITY, TQL_EXAMPLES, TQL_SYNTAX, WINDOWS, caretLine, fmtPct, matchRoute, ofTotal, windowFrom, windowSince,
} from '../lib/insight.js';
import { Card, Chip, EmptyState, ErrorState, Kbd, LoadingBar, Mono, PanelHead, RangeBar, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Caveat, CiBar, OutcomeTag } from '../components/insight/Bits.jsx';

// Behaviors: query what agents did the way a regex queries text — a
// failing test, then an edit to a test file, then a pass; the same file
// edited five times in ten steps; a tool the model was never offered — and
// the detector library run over the window, each pattern's runs set beside
// how every run ended. The query is parsed as you type, by the server, so
// a refusal names its column before anything is searched.

const OUTCOMES = ['any', 'success', 'failure', 'abandoned', 'unknown'];

export function BehaviorsScreen({ go, params }) {
  const windowId = windowFrom(params);
  const since = windowSince(windowId);
  const q = params.get('q') || '';
  const until = params.get('until') || undefined;
  const agent = params.get('agent') || '';
  const outcome = OUTCOMES.includes(params.get('outcome')) ? params.get('outcome') : 'any';
  const tenant = routeTenant(params);
  const [draft, setDraft] = React.useState(q);
  const [agentDraft, setAgentDraft] = React.useState(agent);
  React.useEffect(() => { setDraft(q); }, [q]);

  const set = (changes, replace = true) => {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(changes)) {
      if (value == null || value === '') next.delete(key); else next.set(key, String(value));
    }
    navigate(['behaviors'], Object.fromEntries(next), { replace });
  };
  const search = (query) => {
    const text = (query ?? draft).trim();
    if (!text) return;
    setDraft(text);
    set({ q: text, until: null }, false);
  };

  const explain = useExplain(draft);
  const results = useRead((signal) => api.trajectories({
    q, since, until, agent: agent || undefined, outcome: outcome === 'any' ? undefined : outcome, limit: 50, ...tenantParams(tenant),
  }, signal), [q, since, until, agent, outcome, tenant], { skip: !q });
  const findings = useRead((signal) => api.findings({ since, agent: agent || undefined, ...tenantParams(tenant) }, signal),
    [since, agent, tenant]);
  const library = useRead((signal) => api.detectors(signal), []);

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1600 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: '8px 14px', flexWrap: 'wrap' }}>
      <RangeBar ranges={WINDOWS} active={windowId} onPick={(id) => set({ window: id === '30d' ? null : id, until: null })} />
      <span style={{ display: 'inline-flex', flexWrap: 'wrap', gap: 6, alignItems: 'center' }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>ended</span>
        {OUTCOMES.map((o) => <Chip key={o} active={outcome === o} onClick={() => set({ outcome: o === 'any' ? null : o })}>{o}</Chip>)}
      </span>
      <Input size="sm" mono value={agentDraft} onChange={setAgentDraft} placeholder="agent (any)" ariaLabel="Agent"
        style={{ width: 170 }} onKeyDown={(e) => { if (e.key === 'Enter') set({ agent: agentDraft.trim() || null }); }} />
    </div>

    <div className="tz-split" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(300px, 400px)', gap: 14, alignItems: 'start' }}>
      <div style={{ display: 'grid', gap: 14, minWidth: 0 }}>
        <QueryBox draft={draft} setDraft={setDraft} explain={explain} onSearch={() => search()} />
        {q ? <Results results={results} q={q} go={go} onContinue={(ns) => set({ until: ns })} until={until} /> : <EmptyState message={<span>
          Write a pattern above, or pick one: an atom matches one step (<code>tool</code>, <code>llm</code>,
          <code> edit</code>, <code>test</code>), brackets test it (<code>[error]</code>), and <code>-&gt;</code> and
          <code> ..&gt;</code> chain steps — next, and eventually.
        </span>} />}
        <Findings findings={findings} library={library} go={go} windowId={windowId}
          onSearch={(text) => { setDraft(text); search(text); if (window.scrollTo) window.scrollTo({ top: 0, behavior: 'smooth' }); }} />
      </div>
      <div style={{ position: 'sticky', top: 64, maxHeight: 'calc(100vh - 80px)', overflowY: 'auto' }}>
        <SyntaxHelp onPick={(text) => { setDraft(text); search(text); }} />
      </div>
    </div>
  </div>;
}

/** Parses the draft on the server as it changes — a quarter second after
    the last keystroke — and keeps the answer with the text it is about. */
function useExplain(text) {
  const [state, setState] = React.useState({ text: '', data: null, error: null });
  React.useEffect(() => {
    const trimmed = text.trim();
    if (!trimmed) { setState({ text: '', data: null, error: null }); return undefined; }
    const controller = new AbortController();
    const timer = setTimeout(() => {
      api.explainQuery(trimmed, controller.signal).then(
        (data) => setState({ text: trimmed, data, error: null }),
        (error) => { if (error && error.name !== 'AbortError') setState({ text: trimmed, data: null, error }); },
      );
    }, 250);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [text]);
  return state.text === text.trim() ? state : { text: '', data: null, error: null, pending: Boolean(text.trim()) };
}

function QueryBox({ draft, setDraft, explain, onSearch }) {
  const refusal = explain.error && explain.error.body;
  const column = refusal && typeof refusal.column === 'number' ? refusal.column : null;
  return <Card>
    <PanelHead title="TQL" note="a regular expression whose letters are steps" action={<span style={{ fontSize: 12, color: 'var(--ink-faint)' }}><Kbd>⌘↵</Kbd> search</span>} />
    <div className="tz-field" style={{ border: `1px solid ${column != null ? 'var(--error)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)' }}>
      <textarea value={draft} onChange={(e) => setDraft(e.target.value)} spellCheck={false} aria-label="TQL query" rows={3}
        placeholder={'test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]'}
        onKeyDown={(e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSearch(); } }}
        style={{
          display: 'block', width: '100%', resize: 'vertical', border: 'none', outline: 'none', background: 'transparent',
          color: 'var(--ink)', fontFamily: 'var(--font-mono)', fontSize: 13, lineHeight: '20px', padding: '8px 10px', minHeight: 64,
        }} />
    </div>
    <div style={{ display: 'flex', alignItems: 'flex-start', gap: 10, marginTop: 8, flexWrap: 'wrap' }}>
      <div style={{ flex: 1, minWidth: 0 }} aria-live="polite">
        {column != null ? <div data-testid="tql-refusal" style={{ display: 'grid', gap: 2 }}>
          <pre style={{ margin: 0, fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: '16px', color: 'var(--ink)', whiteSpace: 'pre', overflowX: 'auto' }}>{explain.text}{'\n'}<span style={{ color: 'var(--error)' }}>{caretLine(explain.text, column)}</span></pre>
          <div style={{ fontSize: 12, color: 'var(--error)', lineHeight: '18px', overflowWrap: 'anywhere' }}>{refusal.error}</div>
        </div> : explain.error ? <div style={{ fontSize: 12, color: 'var(--error)' }}>{explain.error.what}</div>
          : explain.data ? <div data-testid="tql-explain" style={{ display: 'flex', gap: '4px 12px', flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
            <span>reads as <Mono>{explain.data.query}</Mono></span>
            <span><Mono>{explain.data.atoms}</Mono> atoms</span>
            {explain.data.captures.length ? <span>captures <Mono>{explain.data.captures.map((c) => '@' + c).join(' ')}</Mono></span> : null}
            {explain.data.fields.length ? <span>fields <Mono>{explain.data.fields.join(', ')}</Mono></span> : null}
            <span>covers <Mono>{explain.data.min_steps}</Mono>–<Mono>{explain.data.max_steps == null ? '∞' : explain.data.max_steps}</Mono> steps</span>
            {explain.data.within ? <span>within <Mono>{explain.data.within.steps != null ? explain.data.within.steps + ' steps' : JSON.stringify(explain.data.within)}</Mono></span> : null}
          </div> : <div style={{ fontSize: 12, color: 'var(--ink-faint)' }}>{explain.pending ? 'Parsing…' : 'Parsed by the server as you type.'}</div>}
      </div>
      <Chip tone="primary" onClick={column != null ? undefined : onSearch}>Search</Chip>
    </div>
  </Card>;
}

function Results({ results, q, go, onContinue, until }) {
  if (results.error) {
    const body = results.error.body;
    return <ErrorState what={body && body.error ? body.error : results.error.what}
      next={results.error.status === 400 && !(body && body.column) ? 'Enumerating a window that does not fit the query deadline is refused; narrow it.' : results.error.next} />;
  }
  if (!results.data) return <Skeleton height={160} />;
  const d = results.data;
  return <Card pad="0" data-testid="tql-results">
    <div style={{ padding: '12px 14px 8px' }}>
      <PanelHead title="Matches" style={{ marginBottom: 4 }}
        note={`${fmtNum(d.matches.length)} run${d.matches.length === 1 ? '' : 's'} · ${ofTotal(d.runs_scanned, d.runs_in_window)} runs scanned, newest first${until ? ' · before ' + fmtTimeNs(Number(until)) : ''}`} />
      <LoadingBar active={results.loading} />
      {d.window_capped ? <Caveat>The window holds more than 5,000 runs; only the most recent were considered.</Caveat> : null}
      {d.stopped === 'deadline' ? <Caveat>The scan stopped at the query deadline; older runs were not checked.</Caveat> : null}
      {d.stopped === 'limit' ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>Stopped at 50 matching runs.</div> : null}
      {d.spans_skipped ? <Caveat>{fmtNum(d.spans_skipped)} spans were left out: their run id does not read back to them.</Caveat> : null}
    </div>
    {!d.matches.length ? <div style={{ padding: '4px 14px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>
      No run in this window has a step sequence matching <Mono>{d.query}</Mono>.
    </div> : null}
    {d.matches.map((m) => <MatchRow key={m.tenant + '\u0000' + m.run} match={m} go={go} />)}
    {d.next_until_ns != null ? <div style={{ padding: '10px 14px', borderTop: '1px solid var(--hairline)' }}>
      <Chip onClick={() => onContinue(String(d.next_until_ns))}>Continue with older runs</Chip>
    </div> : null}
  </Card>;
}

function MatchRow({ match, go }) {
  const route = matchRoute(match);
  const stepParams = (index) => ({ ...route.params, step: String(index) });
  const captures = Object.entries(match.captures || {});
  return <div data-testid="tql-match" style={{ display: 'grid', gap: 6, padding: '10px 14px', borderTop: '1px solid var(--hairline)' }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <OutcomeTag outcome={match.outcome} />
      <a href={'#/run/' + encodeURIComponent(match.run)} onClick={(e) => { e.preventDefault(); go(route.parts, route.params); }}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{match.run}</a>
      {match.agent ? <Mono size={11} color="var(--ink-muted)">{match.agent}</Mono> : null}
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        steps <Mono>{match.first_step}</Mono>–<Mono>{match.last_step}</Mono> of <Mono>{fmtNum(match.steps)}</Mono>
        {match.count > 1 ? <> · <Mono color="var(--accent)">{fmtNum(match.count)}{match.count_capped ? '+' : ''}</Mono> times</> : null}
      </span>
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        {match.inexact ? <Tag status="warn" mono title="A matcher cap dropped lower-priority alternatives in this run">inexact</Tag> : null}
        {match.run_truncated ? <Tag status="warn" mono title="Over 20,000 spans: matched on its beginning">run cut</Tag> : null}
        <Mono size={11} color="var(--ink-faint)">{fmtTimeNs(match.last_activity_ns)}</Mono>
      </span>
    </div>
    <div style={{ display: 'flex', gap: 4, flexWrap: 'wrap', alignItems: 'center' }}>
      {match.spans.map((s, i) => <React.Fragment key={s.span_id + i}>
        {i ? <span aria-hidden="true" style={{ color: 'var(--ink-faint)', fontSize: 11 }}>{match.spans[i - 1].step_index + 1 === s.step_index ? '→' : '…→'}</span> : null}
        <a href={'#/run/' + encodeURIComponent(match.run)} onClick={(e) => { e.preventDefault(); go(['run', match.run], stepParams(s.step_index)); }}
          title={`step ${s.step_index}: ${s.kind} ${s.name}`}
          style={{ textDecoration: 'none', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: '0 5px', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', background: 'var(--bg-raised)' }}>
          <span style={{ color: 'var(--ink-faint)' }}>#{s.step_index} {s.kind}:</span>{s.name}
        </a>
      </React.Fragment>)}
      {match.spans_truncated ? <Mono size={11} color="var(--ink-faint)">… more</Mono> : null}
    </div>
    {captures.length ? <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', fontSize: 12 }}>
      {captures.map(([k, v]) => <span key={k}><Mono color="var(--ink-muted)">@{k}</Mono> = <Mono style={{ overflowWrap: 'anywhere' }}>{String(v)}</Mono></span>)}
      {match.captures_truncated ? <Mono size={11} color="var(--ink-faint)">(cut to 1 KiB)</Mono> : null}
    </div> : null}
  </div>;
}

function SyntaxHelp({ onPick }) {
  const [open, setOpen] = React.useState(true);
  return <Card data-testid="tql-help">
    <PanelHead title="Syntax" action={<Chip onClick={() => setOpen((v) => !v)}>{open ? 'hide' : 'show'}</Chip>} />
    {open ? <div style={{ display: 'grid', gap: 10 }}>
      {TQL_SYNTAX.map((group) => <div key={group.group} style={{ display: 'grid', gap: 4 }}>
        <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>{group.group}</div>
        {group.rows.map(([syntax, means]) => <div key={syntax} style={{ display: 'grid', gap: 1 }}>
          <Mono size={12}>{syntax}</Mono>
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{means}</span>
        </div>)}
      </div>)}
      <div style={{ display: 'grid', gap: 4 }}>
        <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>examples</div>
        {TQL_EXAMPLES.map((ex) => <button key={ex.q} type="button" className="tz-plain-button" onClick={() => onPick(ex.q)}
          style={{ display: 'grid', gap: 1, cursor: 'pointer', padding: '4px 6px', margin: '0 -6px', borderRadius: 'var(--radius-control)' }}
          onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--bg-sunken)'; }} onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}>
          <Mono size={12} style={{ overflowWrap: 'anywhere' }}>{ex.q}</Mono>
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{ex.means}</span>
        </button>)}
      </div>
      <div style={{ fontSize: 11, color: 'var(--ink-faint)', lineHeight: '16px' }}>
        Names ignore ASCII case; <code>=</code> does not. A comparison with a field the step lacks is false. Heuristic fields
        (<code>edit</code>, <code>test</code>, <code>file</code>) read tool names, argument keys and command text.
      </div>
    </div> : null}
  </Card>;
}

/** The library over the window: per detector, the runs it matched and how
    they ended, against how every checked run ended. */
function Findings({ findings, library, go, onSearch, windowId }) {
  const byName = new Map(((library.data && library.data.detectors) || []).map((d) => [d.name, d]));
  const rows = findings.data ? findings.data.detectors.map((d) => ({ ...byName.get(d.name), ...d }))
    : [...byName.values()].map((d) => ({ ...d, runs_matched: null }));
  const fired = rows.filter((r) => r.runs_matched);
  const quiet = rows.filter((r) => !r.runs_matched);
  const f = findings.data;
  return <Card pad="0" data-testid="findings">
    <div style={{ padding: '12px 14px 8px' }}>
      <PanelHead title="Detectors" note={f ? `the library over the ${fmtNum(f.runs_checked)} most recent runs of the window (${windowId})` : 'nine named patterns, each ordinary TQL'}
        action={f ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>every checked run: <CiBar rate={f.baseline_success_rate} width={70} label={false} /> <Mono color="var(--accent)">{fmtPct(f.baseline_success_rate)}</Mono> succeeded</span> : null} />
      <LoadingBar active={findings.loading} />
      {findings.error ? <ErrorState what={findings.error.what} next={findings.error.next} onRetry={findings.reload} /> : null}
      {f && f.truncated ? <Caveat>{f.stopped === 'deadline' ? 'The scan stopped at the query deadline' : 'The window held more runs than the 2,000 checked'}; counts cover the runs checked.</Caveat> : null}
    </div>
    {!rows.length && (findings.loading || library.loading) ? <div style={{ padding: 14 }}><Skeleton height={120} /></div> : null}
    {fired.map((d) => <DetectorRow key={d.name} d={d} baseline={f && f.baseline_success_rate} checked={f && f.runs_checked} go={go} onSearch={onSearch} />)}
    {quiet.length ? <div style={{ padding: '10px 14px', borderTop: '1px solid var(--hairline)', display: 'grid', gap: 6 }}>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{f ? 'No findings in this window:' : 'The library:'}</div>
      {quiet.map((d) => <div key={d.name} style={{ display: 'flex', gap: 8, alignItems: 'baseline', flexWrap: 'wrap', fontSize: 12 }}>
        <Tag status={(SEVERITY[d.severity] || SEVERITY.low).tone} mono>{d.severity}</Tag>
        <span style={{ color: 'var(--ink)' }}>{d.title}</span>
        <Mono size={11} color="var(--ink-faint)">{d.name}</Mono>
        <span style={{ color: 'var(--ink-faint)', flex: '1 1 280px', minWidth: 0 }}>{d.summary}</span>
        {d.query ? <Chip onClick={() => onSearch(d.query)}>Search</Chip> : null}
      </div>)}
    </div> : null}
  </Card>;
}

function DetectorRow({ d, baseline, checked, go, onSearch }) {
  const severity = SEVERITY[d.severity] || SEVERITY.low;
  return <div data-testid="detector" style={{ display: 'grid', gap: 6, padding: '10px 14px', borderTop: '1px solid var(--hairline)' }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Tag status={severity.tone} mono>{d.severity}</Tag>
      <span style={{ fontSize: 13, fontWeight: 500 }}>{d.title}</span>
      <Mono size={11} color="var(--ink-faint)">{d.name}</Mono>
      <span style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
        <span><Mono color="var(--accent)">{fmtNum(d.runs_matched)}</Mono> of <Mono>{fmtNum(checked)}</Mono> runs · <Mono>{fmtNum(d.occurrences)}</Mono> times</span>
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }} title="Successes over runs whose outcome is known, against every checked run (the ink hairline)">
          <CiBar rate={d.success_rate} base={baseline} width={90} label={false} />
          <Mono color={d.success_rate != null && baseline != null && d.success_rate < baseline ? 'var(--error)' : 'var(--accent)'}>{fmtPct(d.success_rate, 0)}</Mono> succeeded
        </span>
        {d.last_seen_ns ? <Mono size={11} color="var(--ink-faint)">last {fmtTimeNs(d.last_seen_ns)}</Mono> : null}
      </span>
    </div>
    <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>{d.summary}</div>
    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
      {(d.examples || []).map((m) => {
        const route = matchRoute(m);
        return <span key={m.tenant + '\u0000' + m.run} style={{ display: 'inline-flex', flexWrap: 'wrap', gap: 5, alignItems: 'center', minWidth: 0 }}>
          <OutcomeTag outcome={m.outcome} />
          <a href={'#/run/' + encodeURIComponent(m.run)} onClick={(e) => { e.preventDefault(); go(route.parts, route.params); }}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>{m.run}</a>
          {Object.entries(m.captures || {}).map(([k, v]) => <span key={k} title={`@${k}=${String(v)}`}>
            <Mono size={11} color="var(--ink-faint)">@{k}={String(v).length > 24 ? String(v).slice(0, 23) + '…' : String(v)}</Mono>
          </span>)}
        </span>;
      })}
      {d.query ? <span style={{ marginLeft: 'auto' }}><Chip onClick={() => onSearch(d.query)}>Search this pattern</Chip></span> : null}
    </div>
  </div>;
}
