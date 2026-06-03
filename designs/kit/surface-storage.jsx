// surface-storage.jsx — COW state's real home. The chunk/durability layer as
// a first-class diagnostics surface: fleet-wide rollups + a DURABILITY LEDGER
// (per-sandbox copy-on-write rows: dirty chunks, dirty bytes, base-chunk
// locality, RPO) + the snapshot register. The SRE's lab-notebook page.

// deterministic synth so the ledger is stable across renders
function hashStr(s) { let h = 0; for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0; return h; }
function durabilityRows(sessions, hosts) {
  const live = sessions.filter((s) => ['active', 'idle', 'created', 'guest_ready'].includes(s.status));
  return live.map((s, i) => {
    const h = hashStr(s.id);
    const base = 280 + (h % 120);
    const localPct = 72 + (h % 27);                       // 72–98% local
    const dirty = s.status === 'active' ? 4 + (h % 22) : 1 + (h % 5);
    const dirtyBytes = dirty * (256 * 1024) + (h % 512) * 1024;
    const flushSecs = s.status === 'active' ? 3 + (h % 25) : 40 + (h % 600);
    return {
      session: s.id, host: hosts[i % hosts.length]?.id || '—', status: s.status,
      dirty, dirtyBytes, base, baseLocal: Math.round(base * localPct / 100), localPct, flushSecs,
    };
  });
}

function LedgerRow({ r }) {
  const rpoHot = r.flushSecs <= 10;
  return (
    <div className="ledger-row font-mono">
      <span className="ld-session">{shortId(r.session)}</span>
      <span className="ld-host" data-label="host">{shortId(r.host)}</span>
      <span data-tabular data-label="dirty">{r.dirty}</span>
      <span data-tabular data-label="unflushed">{fmtBytes(r.dirtyBytes)}</span>
      <span data-tabular data-label="base locality">
        <span className="locality-bar"><span className="locality-fill" style={{ width: `${r.localPct}%` }} /></span>
        {r.localPct}%
      </span>
      <span data-tabular data-label="rpo" style={{ color: rpoHot ? 'var(--accent-now)' : 'var(--fg-muted)' }}>{r.flushSecs < 90 ? `${r.flushSecs}s` : relativeTime(ago(r.flushSecs))} ago</span>
    </div>
  );
}

function StorageSurface({ store }) {
  const rows = durabilityRows(store.sessions, store.hosts);
  const totalChunks = store.hosts.reduce((a, h) => a + h.local_snapshots * 64, 0);
  const snapshots = store.hosts.reduce((a, h) => a + h.local_snapshots, 0);
  const dirtyTotal = rows.reduce((a, r) => a + r.dirtyBytes, 0);
  const avgLocal = rows.length ? Math.round(rows.reduce((a, r) => a + r.localPct, 0) / rows.length) : 0;

  const rollups = [
    ['chunks stored', totalChunks.toLocaleString()],
    ['dedup ratio', '3.8×'],
    ['snapshots', snapshots],
    ['unflushed', fmtBytes(dirtyTotal)],
    ['avg locality', `${avgLocal}%`],
    ['gc pending', '12'],
  ];

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">storage</h1>
          <p className="surface-sub">content-addressed chunk store, snapshots, and copy-on-write durability.</p>
        </div>
      </div>

      <div className="storage-rollup">
        {rollups.map(([label, val]) => (
          <div key={label} className="rollup-cell">
            <span className="rollup-num stat-figure" data-tabular>{val}</span>
            <span className="rollup-lbl section-label">{label}</span>
          </div>
        ))}
      </div>

      <section className="manifest-group">
        <div className="manifest-group-head">
          <span className="section-label">DURABILITY LEDGER · per-sandbox copy-on-write</span>
          <span className="section-label manifest-count" data-tabular>{rows.length}</span>
        </div>
        <div className="ledger">
          <div className="ledger-row ledger-head section-label">
            <span>session</span><span>host</span><span>dirty</span><span>unflushed</span><span>base locality</span><span>rpo</span>
          </div>
          {rows.length === 0
            ? <p className="manifest-empty">no chunk-tracked sandboxes</p>
            : rows.map((r) => <LedgerRow key={r.session} r={r} />)}
        </div>
        <p className="ledger-note">dirty chunks flush to the content-addressed store on the snapshot cadence; locality is the share of base chunks resident on the host (the rest stream on restore).</p>
      </section>
    </main>
  );
}

Object.assign(window, { StorageSurface, durabilityRows });
