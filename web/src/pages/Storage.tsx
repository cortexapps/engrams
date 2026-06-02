import { useStorageSummary } from '../hooks/useStorageSummary';
import { fmtAgo, fmtBytes, secondsSince, shortId } from '../format';
import type { DurabilityRow } from '../types';

// Storage — COW state's real home (`/storage`). The chunk/durability
// layer as a first-class diagnostics surface: fleet-wide rollups + a
// per-sandbox durability ledger (dirty chunks, unflushed bytes, base-
// chunk locality, RPO). All values come from the coordinator's
// /storage/summary endpoint — no synthesized numbers.

function LocalityCell({ row }: { row: DurabilityRow }) {
  const pct =
    row.base_chunks > 0
      ? Math.round((row.base_chunks_local / row.base_chunks) * 100)
      : null;
  if (pct === null) return <span data-label="base locality">—</span>;
  return (
    <span
      data-label="base locality"
      title={`${row.base_chunks_local}/${row.base_chunks} base chunks resident`}
    >
      <span className="locality-bar">
        <span className="locality-fill" style={{ width: `${pct}%` }} />
      </span>
      {pct}%
    </span>
  );
}

function LedgerRow({ row }: { row: DurabilityRow }) {
  // RPO goes amber when the last flush is recent (≤10s) — the window
  // in which unflushed work is most exposed.
  const rpoHot = secondsSince(row.last_flush_at) <= 10;
  return (
    <div className="ledger-row">
      <span className="ld-session" data-label="session">
        {row.session_id ? shortId(row.session_id) : shortId(row.sandbox_id)}
      </span>
      <span data-label="host">{shortId(row.host_id)}</span>
      <span data-tabular data-label="dirty">
        {row.dirty_chunks}
      </span>
      <span data-tabular data-label="unflushed">
        {fmtBytes(row.dirty_bytes)}
      </span>
      <LocalityCell row={row} />
      <span
        data-tabular
        data-label="rpo"
        style={{ color: rpoHot ? 'var(--color-amber)' : 'var(--color-ink-faded)' }}
      >
        {fmtAgo(row.last_flush_at)}
      </span>
    </div>
  );
}

export function Storage() {
  const { data, isPending, error } = useStorageSummary();

  const rollups: [string, string | number][] = [
    ['snapshots', data?.snapshots ?? 0],
    ['snapshot bytes', fmtBytes(data?.snapshot_bytes ?? 0)],
    ['tracked sandboxes', data?.tracked_sandboxes ?? 0],
    ['unflushed', fmtBytes(data?.unflushed_bytes ?? 0)],
    ['avg locality', `${data?.avg_locality_pct ?? 0}%`],
    ['gc pending', data?.gc_pending ?? 0],
  ];

  const rows = data?.rows ?? [];

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">storage</h1>
          <p className="surface-sub">
            content-addressed chunk store, snapshots, and copy-on-write
            durability.
          </p>
        </div>
      </div>

      <div className="storage-rollup">
        {rollups.map(([label, value]) => (
          <div key={label} className="rollup-cell">
            <span className="rollup-num stat-figure" data-tabular>
              {value}
            </span>
            <span className="rollup-lbl section-label">{label}</span>
          </div>
        ))}
      </div>

      <section className="manifest-group">
        <div className="manifest-group-head">
          <span className="section-label">
            DURABILITY LEDGER · per-sandbox copy-on-write
          </span>
          <span className="section-label manifest-count" data-tabular>
            {rows.length}
          </span>
        </div>
        <div className="ledger">
          <div className="ledger-row ledger-head section-label">
            <span>session</span>
            <span>host</span>
            <span>dirty</span>
            <span>unflushed</span>
            <span>base locality</span>
            <span>rpo</span>
          </div>
          {error ? (
            <p className="manifest-empty">{(error as Error).message}</p>
          ) : rows.length === 0 ? (
            <p className="manifest-empty">
              {isPending ? 'loading…' : 'no chunk-tracked sandboxes'}
            </p>
          ) : (
            rows.map((r) => <LedgerRow key={r.sandbox_id} row={r} />)
          )}
        </div>
        <p className="ledger-note">
          dirty chunks flush to the content-addressed store on the snapshot
          cadence; base locality is the share of a sandbox’s base chunks
          resident on its host (the rest stream on restore). RPO is time since
          the last flush.
        </p>
      </section>
    </main>
  );
}
