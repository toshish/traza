import React from 'react';
import { api, getToken } from '../lib/api.js';
import { fmtNum, fmtBytes } from '../lib/format.js';
import { Section } from '../components/Section.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { StatTile } from '../components/data/StatTile.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';
import { ErrorState } from '../components/feedback/ErrorState.jsx';
import { LoadingBar } from '../components/feedback/LoadingBar.jsx';

/** Store stats, flush, and dataset export. */
export function StoreView({ pushToast }) {
  const [stats, setStats] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(true);
  const [flushing, setFlushing] = React.useState(false);
  const [exporting, setExporting] = React.useState(false);
  const [form, setForm] = React.useState({ service: '', name: '', attrKey: '', attrValue: '', minMs: '', limit: '' });
  const set = (key) => (value) => setForm((f) => ({ ...f, [key]: value }));

  const fetchStats = React.useCallback(async () => {
    setLoading(true); setError(null);
    try {
      setStats(await api.stats());
    } catch (e) {
      setError(e); setStats(null);
    } finally {
      setLoading(false);
    }
  }, []);
  React.useEffect(() => { fetchStats(); }, [fetchStats]);

  const flush = async () => {
    setFlushing(true);
    try {
      await api.flush();
      pushToast({ status: 'ok', title: 'Flushed', detail: 'Buffered spans are now in a durable segment.' });
      fetchStats();
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Flush failed', detail: e.next });
    } finally {
      setFlushing(false);
    }
  };

  const exportFilters = () => {
    const filters = { service: form.service, name: form.name, min_duration_ms: form.minMs, limit: form.limit };
    if (form.attrKey) filters['attr.' + form.attrKey] = form.attrValue;
    return filters;
  };

  const download = async () => {
    setExporting(true);
    try {
      const blob = await api.exportBlob(exportFilters());
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url; a.download = 'dataset.ndjson';
      a.click();
      URL.revokeObjectURL(url);
      pushToast({ status: 'ok', title: 'Export downloaded', detail: 'dataset.ndjson — ' + fmtBytes(blob.size) });
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Export failed', detail: e.next });
    } finally {
      setExporting(false);
    }
  };

  const curl = (() => {
    const path = api.exportPath(exportFilters());
    const auth = getToken() ? " -H 'Authorization: Bearer $TRAZA_TOKEN'" : '';
    return "curl" + auth + " '" + window.location.origin + path + "' > dataset.ndjson\n" +
      "# verify the X-Traza-Export-Complete: true trailer before trusting the file";
  })();

  return <>
    <Section title="Store" action={<Button variant="ghost" size="sm" onClick={fetchStats}>Refresh</Button>}>
      <LoadingBar active={loading} style={{ marginBottom: 8 }} />
      {error ? <ErrorState what={error.what} next={error.next} /> : null}
      {stats ? <>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 12, marginBottom: 12 }}>
          <StatTile label="records" value={fmtNum(stats.record_count)} />
          <StatTile label="segments" value={fmtNum(stats.segment_count)} />
          <StatTile label="on disk" value={fmtBytes(stats.bytes_on_disk)} />
          <StatTile label="buffered" value={fmtNum(stats.buffered_records)} />
          <StatTile label="persisted" value={fmtNum(stats.persisted_records)} />
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Button onClick={flush} disabled={flushing}>{flushing ? 'Flushing…' : 'Flush now'}</Button>
          <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)' }}>
            forces buffered spans into a durable segment; durability otherwise begins at the flush threshold
          </span>
        </div>
      </> : null}
    </Section>
    <Section title="Dataset export" style={{ marginTop: 12 }}>
      <div style={{ fontSize: 'var(--text-13)', color: 'var(--ink-muted)', marginBottom: 8 }}>
        Streams matching spans as NDJSON with bounded memory — an export larger than RAM is fine.
        Filters are optional; empty exports everything.
      </div>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 6, marginBottom: 8 }}>
        <Input size="sm" placeholder="service" value={form.service} onChange={set('service')} />
        <Input size="sm" placeholder="name" value={form.name} onChange={set('name')} />
        <Input size="sm" mono placeholder="min duration ms" value={form.minMs} onChange={set('minMs')} />
        <Input size="sm" mono placeholder="attr key" value={form.attrKey} onChange={set('attrKey')} />
        <Input size="sm" mono placeholder="attr value" value={form.attrValue} onChange={set('attrValue')} />
        <Input size="sm" mono placeholder="limit (default: all)" value={form.limit} onChange={set('limit')} />
      </div>
      <div style={{ display: 'flex', gap: 8, marginBottom: 8 }}>
        <Button variant="primary" onClick={download} disabled={exporting}>{exporting ? 'Exporting…' : 'Run export'}</Button>
      </div>
      <CodeBlock code={curl} />
    </Section>
  </>;
}
