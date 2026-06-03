import type { CowStateView } from '../types';
import { useSessionCowState } from '../hooks/useCowState';
import { useSession } from '../hooks/useSessions';
import { fmtAgo, fmtBytes, shortId as short } from '../format';

// ADR 0016 Phase A: per-session COW diagnostic.
//
// `<SessionCowState sessionId=… />` — a single session's COW view,
// embedded in the session detail page. Always visible (it's the point
// of the page when a session is mid-action). Pulls from the 2s-polling
// hook in `hooks/useCowState.ts`. A deliberately minimal table — no
// charts, no sparklines — to keep it scannable.
//
// ADR 0029 moved the host-wide / fleet-wide COW view (the old
// `HostCowState` toggle) onto the dedicated Storage surface as a
// first-class durability ledger; only the per-session view lives here.

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

export function SessionCowState({ sessionId }: { sessionId: string }) {
  const query = useSessionCowState(sessionId);
  // ADR 0016 Phase B commit 8: the previous copy "no live sandbox
  // — disk-tier diagnostic unavailable while the session is idle /
  // lost / pending" lied about Active sessions whose host hasn't
  // wired the chunked-disk pipeline. Pre-Phase-B, resumed sessions
  // ALSO hit this path even when Active (commit 5 fixes the
  // resume case, but hosts without NBD wiring still produce null
  // for Active sessions). Pulling session status here so the copy
  // can be honest about which case the user is in.
  const sessionQuery = useSession(sessionId);
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
    // Conditional copy: terminal/idle states say so honestly;
    // Active-with-null means the host isn't running the chunked-
    // disk pipeline (no nbd_pool / chunk_store wired, or a pre-
    // commit-7 host that lost tracking on its last restart).
    const status = sessionQuery.data?.status;
    const message =
      status === undefined
        ? 'disk-tier diagnostic unavailable.'
        : status === 'active'
          ? 'no chunked-disk tracking for this Active session — the host hasn’t wired the NBD pipeline, or its tracking didn’t survive the last restart.'
          : status === 'idle' ||
              status === 'host_lost' ||
              status === 'evacuating' ||
              status === 'evicting'
            ? `session is ${status.replace('_', ' ')}; durability lives on the latest snapshot row.`
            : `session is ${status.replace('_', ' ')} (terminal) — no live disk tier.`;
    return (
      <p
        className="font-mono text-[0.78rem] italic"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        {message}
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
