import { useMemo, useState } from 'react';
import type { CowStateView } from '../types';
import { useHostCowState, useSessionCowState } from '../hooks/useCowState';

// ADR 0016 Phase A: COW diagnostic surface. Two modes:
//
// - `<HostCowState hostId=… />` — one row per chunk-tracked sandbox
//   on the host. Lives inside the host card; toggle-expand because
//   most operators don't want it open by default.
// - `<SessionCowState sessionId=… />` — a single session's view,
//   embedded in the session detail page. Always visible (it's the
//   point of the page when a session is mid-action).
//
// Both pull from the 2s-polling hooks in `hooks/useCowState.ts`. The
// component is a deliberately minimal table — no charts, no
// sparklines — to keep it scannable. Anything fancier can come once
// we know which numbers operators actually look at.

function fmtBytes(n: number): string {
  if (n === 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} ${units[unit]}`;
}

function fmtAgo(iso: string | null): string {
  if (!iso) return 'never';
  const then = new Date(iso).getTime();
  const now = Date.now();
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

function short(id: string): string {
  if (id.length <= 12) return id;
  return `${id.slice(0, 8)}…`;
}

function CowStateRow({ view }: { view: CowStateView }) {
  const localPct =
    view.base_chunks > 0
      ? Math.round((view.base_chunks_local / view.base_chunks) * 100)
      : null;
  return (
    <div
      className="font-mono text-[0.78rem]"
      style={{
        display: 'grid',
        gridTemplateColumns: 'minmax(8em, 1fr) 6em 7em 6em 7em',
        gap: '0.75rem',
        padding: '0.35rem 0',
        borderBottom: '1px dotted var(--color-rule)',
        color: 'var(--color-ink-faded)',
      }}
    >
      <span
        style={{ color: 'var(--color-ink)' }}
        title={`session ${view.session_id ?? '?'} · sandbox ${view.sandbox_id} · disk manifest ${view.disk_manifest_id}@v${view.disk_manifest_version}`}
      >
        {view.session_id ? short(view.session_id) : short(view.sandbox_id)}
      </span>
      <span>
        {view.dirty_chunks} dirty
      </span>
      <span>{fmtBytes(view.dirty_bytes)}</span>
      <span title={`base chunks: ${view.base_chunks_local}/${view.base_chunks}`}>
        {localPct !== null ? `${localPct}% local` : '—'}
      </span>
      <span
        title={`last flush ${view.last_flush_at ?? 'never'} · last snapshot ${view.last_snapshot_at ?? 'never'}`}
      >
        flush {fmtAgo(view.last_flush_at)}
      </span>
    </div>
  );
}

function CowStateHeader() {
  return (
    <div
      className="font-mono smallcaps text-[0.65rem]"
      style={{
        display: 'grid',
        gridTemplateColumns: 'minmax(8em, 1fr) 6em 7em 6em 7em',
        gap: '0.75rem',
        padding: '0.25rem 0',
        color: 'var(--color-ink-quiet)',
        letterSpacing: '0.12em',
        borderBottom: '1px solid var(--color-rule)',
      }}
    >
      <span>session</span>
      <span>dirty</span>
      <span>bytes</span>
      <span>locality</span>
      <span>rpo</span>
    </div>
  );
}

export function HostCowState({ hostId }: { hostId: string }) {
  const [open, setOpen] = useState(false);
  const query = useHostCowState(open ? hostId : undefined);

  const rows = useMemo<CowStateView[]>(
    () => query.data?.sessions ?? [],
    [query.data],
  );

  return (
    <div style={{ marginTop: '0.5rem' }}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="font-mono smallcaps text-[0.65rem]"
        style={{
          background: 'none',
          border: 'none',
          padding: 0,
          color: 'var(--color-ink-quiet)',
          cursor: 'pointer',
          letterSpacing: '0.12em',
        }}
        title="ADR 0016 Phase A — per-sandbox COW state"
      >
        {open ? '▾' : '▸'} cow state
      </button>
      {open && (
        <div style={{ marginTop: '0.5rem' }}>
          {query.isPending && (
            <p
              className="font-mono text-[0.78rem]"
              style={{ color: 'var(--color-ink-faded)' }}
            >
              loading…
            </p>
          )}
          {query.error && (
            <p
              className="font-mono text-[0.78rem]"
              style={{ color: 'var(--color-ink-faded)' }}
            >
              {(query.error as Error).message}
            </p>
          )}
          {!query.isPending && !query.error && rows.length === 0 && (
            <p
              className="font-mono text-[0.78rem] italic"
              style={{ color: 'var(--color-ink-faded)' }}
            >
              no chunk-tracked sandboxes on this host
            </p>
          )}
          {rows.length > 0 && (
            <>
              <CowStateHeader />
              {rows.map((row) => (
                <CowStateRow key={row.sandbox_id} view={row} />
              ))}
            </>
          )}
        </div>
      )}
    </div>
  );
}

export function SessionCowState({ sessionId }: { sessionId: string }) {
  const query = useSessionCowState(sessionId);
  if (query.isPending) {
    return (
      <p
        className="font-mono text-[0.78rem]"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        loading COW state…
      </p>
    );
  }
  if (query.error) {
    return (
      <p
        className="font-mono text-[0.78rem]"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        {(query.error as Error).message}
      </p>
    );
  }
  const state = query.data?.state;
  if (!state) {
    return (
      <p
        className="font-mono text-[0.78rem] italic"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        no live sandbox — disk-tier diagnostic unavailable while the
        session is idle / lost / pending.
      </p>
    );
  }
  return (
    <div>
      <CowStateHeader />
      <CowStateRow view={state} />
    </div>
  );
}
