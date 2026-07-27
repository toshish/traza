import React from 'react';
import { useStored } from '../lib/route.js';
import { fromHash, toHash, toCurl, describe } from '../lib/query.js';
import { fmtAgo } from '../lib/format.js';
import { Card, Chip, Eyebrow, EmptyState, Mono } from '../components/primitives/Chrome.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';

// A saved search promoted to an eval set. Datasets live in this browser, not
// on the server: a dataset is a *query*, and the query is already reproducible
// from the export command — so persisting it server-side would add a stateful
// surface that stores nothing the store does not already hold.

export function DatasetsScreen({ go, params, pushToast }) {
  const [datasets, setDatasets] = useStored('traza_datasets', []);
  const [name, setName] = React.useState('');
  const incoming = React.useMemo(() => fromHash(params), [params]);
  const origin = typeof window !== 'undefined' ? window.location.origin : '';
  const hasIncoming = incoming.preds.length > 0;

  const save = () => {
    const label = name.trim() || `dataset ${datasets.length + 1}`;
    setDatasets([{ name: label, query: incoming, made: Date.now() }, ...datasets]);
    setName('');
    pushToast({ status: 'ok', title: 'Dataset saved', detail: `${label} — ${incoming.preds.length} predicates` });
  };

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1200 }}>
    {hasIncoming ? <Card>
      <Eyebrow style={{ marginBottom: 10 }}>Promote this search</Eyebrow>
      <div style={{ fontSize: 13, color: 'var(--ink-muted)', marginBottom: 10, lineHeight: '20px' }}>
        {incoming.preds.map(describe).join(' · ')}
      </div>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <input value={name} onChange={(event) => setName(event.target.value)}
          placeholder="name this dataset" aria-label="Dataset name"
          onKeyDown={(event) => { if (event.key === 'Enter') save(); }}
          style={{
            padding: '4px 9px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
            background: 'var(--bg)', fontSize: 13, color: 'var(--ink)', outline: 'none', width: 260,
          }} />
        <Chip tone="primary" onClick={save}>Save dataset</Chip>
      </div>
    </Card> : null}

    {!datasets.length && !hasIncoming ? <EmptyState
      message={<>No datasets yet. Build a search on the Traces screen, then choose{' '}
        <Mono color="var(--ink)">Make a dataset</Mono> — a dataset is a query plus the export command
        that reproduces it.</>} /> : null}

    {datasets.map((dataset, index) => <Card key={index}>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 8 }}>
        <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)' }}>{dataset.name}</span>
        <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>
          saved {fmtAgo(dataset.made * 1e6)} ago · {dataset.query.preds.length} predicates
        </span>
        <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
          <Chip onClick={() => go(['traces'], toHash(dataset.query))}>Open in Traces</Chip>
          <Chip onClick={() => go(['store'], toHash(dataset.query))}>Export</Chip>
          <Chip onClick={() => setDatasets(datasets.filter((_, i) => i !== index))}>Remove</Chip>
        </span>
      </div>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 10 }}>
        {dataset.query.preds.length ? dataset.query.preds.map(describe).join(' · ') : 'no predicates'}
      </div>
      <CodeBlock code={toCurl(dataset.query, origin, '/v1/export') + ' > ' +
        dataset.name.replace(/\s+/g, '-').toLowerCase() + '.ndjson'} />
    </Card>)}
  </div>;
}
