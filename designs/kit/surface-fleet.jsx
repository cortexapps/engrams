// surface-fleet.jsx — the operator's home. Hosts as first-class citizens,
// drawn as horizontal STRATA: each host a ruled lane with a capacity bar, its
// running sandboxes as small cells along a line, and draining controls. A
// 'table' tweak swaps the strata for a dense ledger. Reconciler status footer.
const { useState: useFleet } = React;

function hostGlyph(status) { return status === 'ready' ? '●' : status === 'draining' ? '◐' : '✕'; }
function hostTone(status) { return status === 'ready' ? 'var(--fg)' : status === 'draining' ? 'var(--accent-now)' : 'var(--fg-quiet)'; }

function CapacityBar({ used, total }) {
  const pct = total > 0 ? Math.min(100, Math.round((used / total) * 100)) : 0;
  const hot = pct >= 80;
  return (
    <div className="cap-wrap" title={`${pct}% used`}>
      <div className="cap-bar">
        <div className="cap-fill" style={{ width: `${pct}%`, background: hot ? 'var(--accent-now)' : 'var(--fg)' }} />
      </div>
      <span className="cap-pct font-mono" data-tabular>{pct}%</span>
    </div>
  );
}

// running sandboxes as a row of small cells; a couple rendered amber ("active")
function SandboxCells({ count }) {
  if (count === 0) return <span className="cells-empty font-mono">no sandboxes</span>;
  const cells = Array.from({ length: count }, (_, i) => i);
  return (
    <div className="sandbox-cells" aria-label={`${count} sandboxes`}>
      {cells.map((i) => <span key={i} className={`sb-cell ${i < 2 ? 'sb-live' : ''}`} />)}
    </div>
  );
}

function HostStratum({ host, onDrain }) {
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(0);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);
  return (
    <div className="stratum">
      <div className="stratum-id">
        <span className="glyph" style={{ color: hostTone(host.status) }}>{hostGlyph(host.status)}</span>
        <span className="font-mono stratum-name">{host.id}</span>
        <span className="section-label" style={{ letterSpacing: '0.05em' }}>{host.status}</span>
      </div>
      <div className="stratum-body">
        <SandboxCells count={host.running_sandboxes} />
        <CapacityBar used={host.capacity_used_mib} total={host.capacity_total_mib} />
      </div>
      <div className="stratum-meta font-mono">
        <span>{host.running_sandboxes} sandboxes</span>
        <span className="dot">·</span>
        <span>{usedGiB}/{totalGiB} GiB</span>
        <span className="dot">·</span>
        <span>{host.local_snapshots} snapshots</span>
        {host.status === 'ready' && (
          <button type="button" className="press-btn section-label stratum-action" onClick={() => onDrain(host.id)}>drain</button>
        )}
        {host.status === 'draining' && <span className="section-label" style={{ color: 'var(--accent-now)', marginLeft: 'auto' }}>draining…</span>}
      </div>
    </div>
  );
}

function HostTableRow({ host }) {
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(0);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);
  const pct = host.capacity_total_mib ? Math.round((host.capacity_used_mib / host.capacity_total_mib) * 100) : 0;
  return (
    <div className="fleet-trow font-mono">
      <span style={{ color: hostTone(host.status) }}>{hostGlyph(host.status)} {host.id}</span>
      <span data-label="status">{host.status}</span>
      <span data-tabular data-label="sandboxes">{host.running_sandboxes}</span>
      <span data-tabular data-label="capacity">{usedGiB}/{totalGiB} GiB · {pct}%</span>
      <span data-tabular data-label="snapshots">{host.local_snapshots}</span>
    </div>
  );
}

function FleetSurface({ store, t = {} }) {
  const [, force] = useFleet(0);
  const hosts = store.hosts;
  const totalSb = hosts.reduce((a, h) => a + h.running_sandboxes, 0);
  const usedGiB = (hosts.reduce((a, h) => a + h.capacity_used_mib, 0) / 1024).toFixed(0);
  const totGiB = (hosts.reduce((a, h) => a + h.capacity_total_mib, 0) / 1024).toFixed(0);
  const draining = hosts.some((h) => h.status === 'draining');
  const drain = (id) => { const h = hosts.find((x) => x.id === id); if (h) { h.status = 'draining'; h.running_sandboxes = 0; h.capacity_used_mib = 0; force((x) => x + 1); } };
  const table = t.fleetView === 'table';

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">fleet</h1>
          <p className="surface-sub">Firecracker hosts and capacity.</p>
        </div>
      </div>

      <div className="fleet-rollup font-mono">
        <span><b data-tabular>{hosts.length}</b> hosts</span><span className="dot">·</span>
        <span><b data-tabular>{totalSb}</b> sandboxes</span><span className="dot">·</span>
        <span><b data-tabular>{usedGiB}/{totGiB}</b> GiB</span>
      </div>

      {table ? (
        <div className="fleet-table">
          <div className="fleet-trow fleet-thead section-label">
            <span>host</span><span>status</span><span>sandboxes</span><span>capacity</span><span>snapshots</span>
          </div>
          {hosts.map((h) => <HostTableRow key={h.id} host={h} />)}
        </div>
      ) : (
        <div className="strata">
          {hosts.map((h) => <HostStratum key={h.id} host={h} onDrain={drain} />)}
        </div>
      )}

      <div className="reconciler-note">
        <span className="glyph" style={{ color: draining ? 'var(--accent-now)' : 'var(--accent-archived)' }}>{draining ? '◐' : '◌'}</span>
        <span className="font-mono">reconciler {draining ? 'rebalancing — draining host, migrating sandboxes' : 'steady — desired state matches observed'}</span>
      </div>
    </main>
  );
}

Object.assign(window, { FleetSurface, HostStratum, CapacityBar });
