import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { useDrainHost } from '../hooks/useDrainHost';
import type { HostStatus, HostView, Session } from '../types';

// Fleet — the operator's home (`/fleet`). Firecracker hosts as
// first-class citizens, drawn as horizontal STRATA: each host a ruled
// lane with its running sandboxes as cells, a capacity bar, and a
// drain control. A reconciler note at the foot reads the desired-vs-
// observed state.

function hostGlyph(status: HostStatus) {
  return status === 'ready' ? '●' : status === 'draining' ? '◐' : '✕';
}
function hostTone(status: HostStatus) {
  return status === 'ready'
    ? 'var(--color-ink)'
    : status === 'draining'
      ? 'var(--color-amber)'
      : 'var(--color-ink-quiet)';
}

// One labelled utilization meter (disk / mem / cpu). For disk + mem
// pass `usedMib`/`totalMib` and the readout shows GiB; for cpu pass
// `pct` directly. The fill turns amber past 80% — the "this host is
// about to fall over" threshold an operator scans for.
function Meter({
  label,
  usedMib,
  totalMib,
  pct,
}: {
  label: string;
  usedMib?: number;
  totalMib?: number;
  pct?: number;
}) {
  const ratio =
    pct !== undefined
      ? pct
      : totalMib && totalMib > 0
        ? ((usedMib ?? 0) / totalMib) * 100
        : 0;
  const shown = Math.min(100, Math.max(0, Math.round(ratio)));
  const hot = shown >= 80;
  const known = pct !== undefined || (totalMib ?? 0) > 0;
  const readout =
    pct !== undefined
      ? `${shown}%`
      : known
        ? `${((usedMib ?? 0) / 1024).toFixed(1)}/${((totalMib ?? 0) / 1024).toFixed(0)} GiB`
        : '—';
  return (
    <div className="meter" title={`${label}: ${shown}%`}>
      <span className="meter-label">{label}</span>
      <div className="cap-bar">
        <div
          className="cap-fill"
          style={{
            width: `${shown}%`,
            background: hot ? 'var(--color-amber)' : 'var(--color-ink)',
          }}
        />
      </div>
      <span className="meter-readout" data-tabular>
        {readout}
      </span>
    </div>
  );
}

// Running sandboxes as a row of small cells; the ones backing an Active
// session render amber ("live"), the rest ink.
function SandboxCells({ count, live }: { count: number; live: number }) {
  if (count === 0) return <span className="cells-empty">no sandboxes</span>;
  return (
    <div className="sandbox-cells" aria-label={`${count} sandboxes`}>
      {Array.from({ length: count }, (_, i) => (
        <span key={i} className={`sb-cell ${i < live ? 'sb-live' : ''}`} />
      ))}
    </div>
  );
}

function HostStratum({
  host,
  liveSessions,
  onDrain,
  draining,
}: {
  host: HostView;
  liveSessions: number;
  onDrain: (id: string) => void;
  draining: boolean;
}) {
  const diskUsedGiB = (host.util_disk_used_mib / 1024).toFixed(1);
  const diskTotalGiB = (host.util_disk_total_mib / 1024).toFixed(0);
  return (
    <div className="stratum">
      <div className="stratum-id">
        <span className="glyph" style={{ color: hostTone(host.status) }}>
          {hostGlyph(host.status)}
        </span>
        <span className="stratum-name">{host.id}</span>
        <span className="section-label" style={{ letterSpacing: '0.05em' }}>
          {host.status}
        </span>
      </div>
      <div className="stratum-body">
        <SandboxCells count={host.running_sandboxes} live={liveSessions} />
        <div className="meters">
          <Meter label="disk" usedMib={host.util_disk_used_mib} totalMib={host.util_disk_total_mib} />
          <Meter label="mem" usedMib={host.util_mem_used_mib} totalMib={host.util_mem_total_mib} />
          <Meter label="cpu" pct={host.util_cpu_pct} />
        </div>
      </div>
      <div className="stratum-meta">
        <span>{host.running_sandboxes} sandboxes</span>
        {host.util_disk_total_mib > 0 && (
          <>
            <span className="dot">·</span>
            <span>
              {diskUsedGiB}/{diskTotalGiB} GiB disk
            </span>
          </>
        )}
        <span className="dot">·</span>
        <span>{host.local_snapshots} snapshots</span>
        {host.status === 'ready' && (
          <button
            type="button"
            className="press-btn section-label stratum-action"
            onClick={() => onDrain(host.id)}
            disabled={draining}
          >
            drain
          </button>
        )}
        {host.status === 'draining' && (
          <span
            className="section-label"
            style={{ color: 'var(--color-amber)', marginLeft: 'auto' }}
          >
            draining…
          </span>
        )}
      </div>
    </div>
  );
}

export function Fleet() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();
  const drain = useDrainHost();

  const h = hosts ?? [];
  const s = sessions ?? [];
  const totalSb = h.reduce((a, x) => a + x.running_sandboxes, 0);
  // Fleet-wide disk is the headline operational number — a full host
  // disk is what bricks sessions. (Memory + CPU live per-host.)
  const usedGiB = (h.reduce((a, x) => a + x.util_disk_used_mib, 0) / 1024).toFixed(0);
  const totGiB = (h.reduce((a, x) => a + x.util_disk_total_mib, 0) / 1024).toFixed(0);
  const anyDraining = h.some((x) => x.status === 'draining');

  // Count Active sessions bound to each host → how many cells light amber.
  const liveByHost = (hostId: string) =>
    s.filter((x: Session) => x.host_id === hostId && x.status === 'active').length;

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">fleet</h1>
          <p className="surface-sub">Firecracker hosts and capacity.</p>
        </div>
      </div>

      <div className="fleet-rollup">
        <span>
          <b data-tabular>{h.length}</b> hosts
        </span>
        <span className="dot">·</span>
        <span>
          <b data-tabular>{totalSb}</b> sandboxes
        </span>
        <span className="dot">·</span>
        <span>
          <b data-tabular>
            {usedGiB}/{totGiB}
          </b>{' '}
          GiB disk
        </span>
      </div>

      {h.length === 0 ? (
        <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
          No hosts have registered yet. Hosts appear here once they boot and
          complete their first heartbeat.
        </p>
      ) : (
        <div className="strata">
          {h.map((host) => (
            <HostStratum
              key={host.id}
              host={host}
              liveSessions={liveByHost(host.id)}
              onDrain={(id) => drain.mutate(id)}
              draining={drain.isPending}
            />
          ))}
        </div>
      )}

      <div className="reconciler-note">
        <span
          className="glyph"
          style={{
            color: anyDraining ? 'var(--color-amber)' : 'var(--color-verdigris)',
          }}
        >
          {anyDraining ? '◐' : '◌'}
        </span>
        <span className="font-mono">
          reconciler{' '}
          {anyDraining
            ? 'rebalancing — draining host, migrating sandboxes'
            : 'steady — desired state matches observed'}
        </span>
      </div>
    </main>
  );
}
