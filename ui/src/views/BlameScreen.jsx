import React from 'react';
import { api } from '../lib/api.js';
import { publicBase } from '../lib/base.js';
import { navigate, useRead } from '../lib/route.js';
import { fmtNum, fmtTimeNs } from '../lib/format.js';
import { routeTenant, tenantParams } from '../lib/runs.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Caveat, Kv } from '../components/insight/Bits.jsx';

// Blame: `git blame` names a commit; this names the agent step. Give it a
// file and the exact text of one line, and it answers with the tool calls
// whose edit introduced that line — the session, the step on its tape, the
// prompt that turn was given and the model that asked for the edit —
// earliest first.

export function BlameScreen({ go, params }) {
  const path = params.get('path') || '';
  const line = params.get('line') || '';
  const project = params.getAll ? params.getAll('project') : [];
  const tenant = routeTenant(params);
  const [pathDraft, setPathDraft] = React.useState(path);
  const [lineDraft, setLineDraft] = React.useState(line);
  // One root per line: a checkout path may hold spaces.
  const [projectDraft, setProjectDraft] = React.useState(project.join('\n'));

  const ask = () => {
    const p = pathDraft.trim();
    const l = lineDraft;
    if (!p || !l.trim()) return;
    const next = new URLSearchParams();
    next.set('path', p);
    next.set('line', l);
    for (const root of projectRoots(projectDraft)) next.append('project', root);
    if (tenant !== undefined) next.set('tenant', tenant);
    // A URLSearchParams keeps repeated `project` keys through the route.
    navigate(['blame'], next);
  };

  const answer = useRead((signal) => api.blame({ path, line, project: project.length ? project : undefined, limit: 20, ...tenantParams(tenant) }, signal),
    [path, line, project.join('\u0000'), tenant], { skip: !path || !line.trim() });

  const curl = `curl -sG '${publicBase()}/v1/blame' --data-urlencode 'path=${path || 'src/cart.rs'}' --data-urlencode 'line=${(line || 'pub price: u64,').replace(/'/g, "'\\''")}'`;

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1200 }}>
    <Card>
      <PanelHead title="Which agent step wrote this line?" note="exact line text, whitespace normalised" />
      <div style={{ display: 'grid', gap: 8 }}>
        <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 2fr)', gap: 8 }}>
          <Input mono value={pathDraft} onChange={setPathDraft} placeholder="path or suffix: src/cart.rs" ariaLabel="File path"
            onKeyDown={(e) => { if (e.key === 'Enter') ask(); }} />
          <Input mono value={lineDraft} onChange={setLineDraft} placeholder="the line, exactly: self.items.iter().map(|item| item.price).sum()" ariaLabel="Line text"
            onKeyDown={(e) => { if (e.key === 'Enter') ask(); }} />
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'flex-end', flexWrap: 'wrap' }}>
          <span className="tz-field" style={{ flex: 1, minWidth: 240, display: 'flex', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)' }}>
            <textarea value={projectDraft} onChange={(e) => setProjectDraft(e.target.value)} spellCheck={false} rows={2}
              aria-label="Repository roots" placeholder={'repository roots (optional), one per line:\n/srv/app\n/srv/app/.claude/worktrees/*'}
              onKeyDown={(e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); ask(); } }}
              style={{
                display: 'block', width: '100%', resize: 'vertical', border: 'none', outline: 'none', background: 'transparent',
                color: 'var(--ink)', fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: '18px', padding: '4px 8px', minHeight: 30,
              }} />
          </span>
          <Chip tone="primary" onClick={ask}>Blame</Chip>
        </div>
        <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
          Without a repository root every project is searched and the path is a suffix match, so a common line in a
          common path can be answered from an unrelated repository. A step introduces a line when its edit adds it
          more times than it removes it; re-indenting is not authorship.
        </div>
      </div>
    </Card>
    <LoadingBar active={answer.loading} />

    {answer.error ? <ErrorState what={answer.error.what} next={answer.error.next} onRetry={answer.reload} /> : null}
    {answer.data ? <BlameAnswer data={answer.data} go={go} /> : null}
    {!path ? <EmptyState message={<span>The same answer from a shell — <code>traza-server blame</code> scopes itself to the repository it runs in:</span>}
      command={'traza-server blame src/cart.rs:42\n' + curl} /> : null}
  </div>;
}

/** The repository roots a draft names: one per line, each trimmed, blank
    lines dropped. Never split on spaces — `/Users/me/My Repo` is one root. */
export function projectRoots(draft) {
  return String(draft || '').split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
}

function BlameAnswer({ data, go }) {
  if (!data.matches.length) {
    return <Card data-testid="blame-none">
      <PanelHead title="No recorded edit introduced this line" note={`${fmtNum(data.searched_spans)} edit calls checked`} />
      <div style={{ fontSize: 13, color: 'var(--ink-muted)', lineHeight: '20px' }}>
        <div>Searched for <Mono>{data.line}</Mono> in <Mono>{data.path}</Mono>{data.projects ? <> under <Mono>{data.projects.join(', ')}</Mono></> : ''}.</div>
        <div style={{ marginTop: 6 }}>
          Blame cannot see lines written by shell commands (<code>sed -i</code>, heredocs, generators), by a tool outside
          the editing tools (<code>Edit</code>, <code>MultiEdit</code>, <code>Write</code>, <code>NotebookEdit</code>,
          <code> apply_patch</code>, the text-editor tools), edits whose arguments were not recorded, or the part of an
          argument offloaded past its preview.
        </div>
      </div>
      {data.truncated ? <Caveat>The search reached its 200,000-candidate bound; an earlier introduction may be missing.</Caveat> : null}
    </Card>;
  }
  return <div style={{ display: 'grid', gap: 12 }} data-testid="blame-matches">
    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ display: 'flex', alignItems: 'stretch', fontFamily: 'var(--font-mono)', fontSize: 14, lineHeight: '22px' }}>
        <div style={{ padding: '10px 14px', background: 'var(--bg-sunken)', color: 'var(--ink-muted)', fontSize: 12, borderRight: '1px solid var(--hairline)', whiteSpace: 'nowrap' }}>{data.path}</div>
        <div style={{ padding: '10px 14px', color: 'var(--ink)', overflowX: 'auto', whiteSpace: 'pre' }}>{data.line}</div>
      </div>
      <div style={{ padding: '7px 14px', borderTop: '1px solid var(--hairline)', fontSize: 12, color: 'var(--ink-muted)' }}>
        <Mono color="var(--accent)">{data.matches.length}</Mono> introduction{data.matches.length === 1 ? '' : 's'}, earliest first, from <Mono>{fmtNum(data.searched_spans)}</Mono> edit calls checked{data.projects ? <> under <Mono>{data.projects.join(', ')}</Mono></> : ' in every project'}
      </div>
    </Card>
    {data.truncated ? <Caveat>The search reached its 200,000-candidate bound; an earlier introduction may be missing.</Caveat> : null}
    <ol style={{ listStyle: 'none', margin: 0, padding: 0, display: 'grid', gap: 12, position: 'relative' }}>
      {data.matches.map((m, i) => <li key={m.trace_id + m.span_id} style={{ display: 'grid', gridTemplateColumns: '18px minmax(0, 1fr)', gap: 10 }}>
        <span aria-hidden="true" style={{ position: 'relative' }}>
          <span style={{ position: 'absolute', left: 8, top: 0, bottom: i === data.matches.length - 1 ? 'calc(100% - 18px)' : -12, width: 1, background: 'var(--hairline)' }} />
          <span style={{ position: 'absolute', left: 3, top: 13, width: 11, height: 11, borderRadius: '50%', background: i === 0 ? 'var(--accent)' : 'var(--bg-raised)', border: `2px solid ${i === 0 ? 'var(--accent)' : 'var(--ink-faint)'}` }} />
        </span>
        <BlameMatch m={m} first={i === 0} go={go} />
      </li>)}
    </ol>
  </div>;
}

function BlameMatch({ m, first, go }) {
  const runParams = { tenant: m.tenant || '' };
  if (typeof m.step_index === 'number') runParams.step = String(m.step_index);
  return <Card data-testid="blame-match">
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 8 }}>
      <Tag status={first ? 'accent' : 'neutral'} mono>{first ? 'introduced' : 'introduced again'}</Tag>
      <Mono size={12}>{fmtTimeNs(m.at_ns)}</Mono>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>by</span>
      <Tag mono>{m.tool}</Tag>
      {m.agent ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>in sub-agent <Mono>{m.agent}</Mono></span> : null}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        <Chip onClick={() => go(['run', m.run], runParams)}>Open the step on its tape</Chip>
        <Chip onClick={() => go(['trace', m.trace_id], { span: m.span_id })}>Span</Chip>
      </span>
    </div>
    {m.prompt ? <blockquote style={{
      margin: '0 0 10px', padding: '8px 12px', borderLeft: '2px solid var(--accent)', background: 'var(--bg-sunken)',
      borderRadius: '0 var(--radius-control) var(--radius-control) 0', fontSize: 13, lineHeight: '20px', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere',
    }}>{m.prompt}</blockquote> : <div style={{ fontSize: 12, color: 'var(--ink-faint)', marginBottom: 10 }}>No prompt recorded on an enclosing turn.</div>}
    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: '3px 24px', fontSize: 12 }}>
      <Kv k="run">{m.run}</Kv>
      <Kv k="step on the tape">{m.step_index == null ? 'past the tape read' : m.step_index}</Kv>
      <Kv k="model that asked">{m.model || '—'}</Kv>
      <Kv k="service">{m.service || '—'}</Kv>
      <Kv k="file as recorded">{m.file}</Kv>
      <Kv k="project">{m.project || '—'}</Kv>
    </div>
  </Card>;
}

