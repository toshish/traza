import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { fromHash, toCurl, toParams, describe } from '../lib/query.js';
import { fmtBytes, fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, LoadingBar, Mono } from '../components/primitives/Chrome.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';
import { Input } from '../components/primitives/Input.jsx';

// Segments, the durability statement, and an export that reuses the query you
// arrived with. The old screen made you retype the filter you had just built
// on the search screen into a second, differently-shaped form.

export function StoreScreen({ params, pushToast }) {
  const stats = useRead((signal) => api.stats(signal), []);
  const [flushing, setFlushing] = React.useState(false);
  const [exporting, setExporting] = React.useState(false);
  const [limit, setLimit] = React.useState('');
  // The query arrives in the hash — "Export NDJSON" on the Traces screen
  // navigates here carrying it, so the export is the search, not a retype.
  //
  // Arriving with nothing means "export everything", so the window defaults to
  // unbounded rather than to a search screen's last hour: a card that says it
  // exports everything must not quietly ship a one-hour filter.
  const query = React.useMemo(() => {
    const parsed = fromHash(params);
    return params.get('q') || params.get('t') ? parsed : { ...parsed, range: 'all' };
  }, [params]);
  const origin = typeof window !== 'undefined' ? window.location.origin : '';

  const data = stats.data;
  const bounded = /^[1-9]\d*$/.test(limit.trim());

  const flush = async () => {
    setFlushing(true);
    try {
      await api.flush();
      pushToast({ status: 'ok', title: 'Flushed', detail: 'Buffered spans are now in a durable segment.' });
      stats.reload();
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Flush failed', detail: e.next });
    } finally { setFlushing(false); }
  };

  const download = async () => {
    setExporting(true);
    try {
      const { blob, rows, bytes } = await api.exportStream({ ...toParams(query), limit });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = 'dataset.ndjson';
      anchor.click();
      URL.revokeObjectURL(url);
      pushToast({
        status: 'ok', title: 'Export downloaded',
        detail: `${fmtNum(rows)} rows · ${fmtBytes(bytes)}. For canonical datasets, verify the completion trailer via curl.`,
      });
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Export failed', detail: e.next });
    } finally { setExporting(false); }
  };

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <LoadingBar active={stats.loading} />
    {stats.error ? <ErrorState what={stats.error.what} next={stats.error.next} onRetry={stats.reload} /> : null}

    {data ? <>
      <Card>
        <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink)', textWrap: 'pretty' }}>
          Durability is <Mono color="var(--accent)">{data.durability}</Mono>.{' '}
          {data.durability === 'wal'
            ? <>An acknowledged write is fsynced to the write-ahead log and recovered on restart — it survives
              a kill&#8209;9, a panic, or an OS crash.</>
            : data.durability === 'flushed'
              ? <>An acknowledged write is already in a sealed segment.</>
              : <>An acknowledged write is in memory only and is lost if this process dies. Set{' '}
                <Mono>--durability wal</Mono> to change that.</>}
          {' '}The log currently holds <Mono color="var(--accent)">{fmtBytes(data.wal_bytes)}</Mono> —
          the work a restart would replay.
        </div>
      </Card>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(6,1fr)', gap: 12 }}>
        {[
          ['records', fmtNum(data.record_count)],
          ['segments', fmtNum(data.segment_count)],
          ['on disk', fmtBytes(data.bytes_on_disk)],
          ['buffered', fmtNum(data.buffered_records)],
          ['persisted', fmtNum(data.persisted_records)],
          ['WAL bytes', fmtBytes(data.wal_bytes)],
        ].map(([label, value]) => <Card key={label} pad="12px 14px">
          <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 5 }}>{label}</div>
          <div style={{
            fontFamily: 'var(--font-mono)', fontSize: 18, lineHeight: '26px', fontWeight: 500,
            fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
          }}>{value}</div>
        </Card>)}
      </div>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>Segments</Eyebrow>
          <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
            one mark per segment · {fmtNum(data.segment_count)} files holding {fmtNum(data.persisted_records)} physical records
          </span>
        </div>
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 2 }}>
          {Array.from({ length: Math.min(data.segment_count, 400) }, (_, i) => (
            <div key={i} title={`segment ${i}`} style={{
              width: 9, height: 16, borderRadius: 1,
              background: `var(--measure-${1 + (i % 4)})`,
            }} />
          ))}
          {data.segment_count === 0
            ? <span style={{ fontSize: 13, color: 'var(--ink-muted)' }}>Nothing sealed yet — every record is still in the write buffer.</span>
            : null}
        </div>
        <div style={{ display: 'flex', gap: 10, alignItems: 'center', marginTop: 14 }}>
          <Chip onClick={flush}>{flushing ? 'Flushing…' : 'Flush now'}</Chip>
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
            forces buffered spans into a durable segment; durability otherwise begins at the flush threshold
          </span>
        </div>
      </Card>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>Dataset export</Eyebrow>
          <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
            {query.preds.length
              ? <>exporting the query you arrived with: {query.preds.map(describe).join(' · ')}</>
              : 'no predicates — this exports everything'}
          </span>
        </div>
        <div style={{ fontSize: 13, color: 'var(--ink-muted)', marginBottom: 10, lineHeight: '20px' }}>
          Streams matching spans as NDJSON. In-browser downloads are buffered in memory, so they
          require a limit and cap at 256 MiB; for unbounded exports — and whenever completeness
          matters — use the curl command, which streams to disk and can verify the{' '}
          <code>X-Traza-Export-Complete</code> trailer (browsers cannot read trailers).
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 10 }}>
          <Input size="sm" mono placeholder="limit (required here; curl exports all)"
            value={limit} onChange={setLimit} />
          <Chip tone="primary" onClick={bounded ? download : undefined}
            style={{ opacity: bounded && !exporting ? 1 : 0.5 }}>
            {exporting ? 'Exporting…' : 'Run export'}
          </Chip>
        </div>
        {/* `limit` is dropped from the curl on purpose: exports are unbounded
            by default, and carrying the search screen's page size here would
            silently truncate the dataset to a hundred rows. */}
        <CodeBlock code={
          toCurl({ ...query, limit: null }, origin, '/v1/export')
          + ' > dataset.ndjson\n# verify the X-Traza-Export-Complete: true trailer before trusting the file'
        } />
      </Card>
    </> : null}
  </div>;
}
