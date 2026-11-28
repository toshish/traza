import React from 'react';
import { Chip } from '../primitives/Chrome.jsx';
import { Input } from '../primitives/Input.jsx';

// Said where an operator reads a run with no tenant named. On the tape, diff
// and replay routes that is the operator view across every tenant, so a run
// id two tenants share reads as one run with both tenants' steps interleaved.
// Links from Guard, a session or a trace name the run's tenant; a pasted run
// id or the palette cannot know it, and the page says so rather than letting
// the mix pass for one run. `onScope` gets the tenant typed, '' meaning the
// default tenant.

export function UnscopedNote({ children, onScope }) {
  const [tenant, setTenant] = React.useState('');
  const scope = () => onScope(tenant.trim());
  return <div data-testid="unscoped" style={{
    display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap',
    fontSize: 12, lineHeight: '18px', color: 'var(--warn)',
  }}>
    <span style={{ flex: '1 1 320px' }}>{children}</span>
    <Input size="sm" mono value={tenant} onChange={setTenant} placeholder="tenant (empty: default)"
      ariaLabel="Tenant to read in" style={{ width: 180 }}
      onKeyDown={(e) => { if (e.key === 'Enter') scope(); }} />
    <Chip onClick={scope}>Scope</Chip>
  </div>;
}
