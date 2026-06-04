import { useSessionCheckpoints } from '../hooks/useCheckpoints';
import { fmtAgo, fmtBytes } from '../format';
import type { CheckpointSummary } from '../types';

// ADR 0028 A.log / Fix A: the consolidated durability view for a
// session — the recovery ladder made legible.
//
//   - A horizontal timeline strip: each coherent checkpoint is a
//     diamond (◆); the newest is filled + amber (the always-pinned
//     rung-1 recovery anchor — "if the host died now, you'd warm-resume
//     here"). The span to the right of it is the worst-case rewind
//     window Δ.
//   - A chain list below: every retained checkpoint (the forkable
//     history window), newest first, with age + size + whether it's
//     recoverable.
//
// Deliberately NOT in the transcript: periodic checkpoints land ~1/min;
// flooding the conversation with them would drown it. The transcript
// keeps the semantic moments (eviction snapshot, resume, the ↩ recovery
// boundary); the steady cadence lives here.

export function DurabilityTimeline({ sessionId }: { sessionId: string }) {
  const { data, isLoading } = useSessionCheckpoints(sessionId);
  const checkpoints = data?.checkpoints ?? [];

  if (isLoading && checkpoints.length === 0) {
    return null;
  }
  if (checkpoints.length === 0) {
    return (
      <p
        className="font-mono text-[0.72rem] italic"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        no checkpoints yet — recovery falls back to the latest disk flush.
      </p>
    );
  }

  // Render oldest→newest left-to-right (the API hands newest-first).
  const ordered = [...checkpoints].reverse();

  return (
    <div className="space-y-2">
      <TimelineStrip ordered={ordered} />
      <ChainList checkpoints={checkpoints} />
    </div>
  );
}

function TimelineStrip({ ordered }: { ordered: CheckpointSummary[] }) {
  return (
    <div
      className="flex items-center gap-1 overflow-x-auto"
      style={{ padding: '0.3rem 0' }}
      title="checkpoint chain (oldest → newest); the filled ◆ is the rung-1 recovery anchor"
    >
      {ordered.map((c, i) => (
        <span key={c.snapshot_id} className="flex items-center gap-1">
          {i > 0 && (
            <span
              aria-hidden
              style={{ color: 'var(--color-rule)', fontSize: '0.7rem' }}
            >
              ─
            </span>
          )}
          <span
            aria-hidden
            title={`${fmtAgo(c.created_at)} · ${fmtBytes(c.size_bytes)}${
              c.recoverable ? '' : ' · unverified'
            }${c.is_latest ? ' · rung-1 anchor' : ''}`}
            style={{
              fontSize: '0.85rem',
              color: c.is_latest
                ? 'var(--color-amber)'
                : c.recoverable
                  ? 'var(--color-ink-faded)'
                  : 'var(--color-ink-quiet)',
            }}
          >
            {c.is_latest ? '◆' : '◇'}
          </span>
        </span>
      ))}
    </div>
  );
}

function ChainList({ checkpoints }: { checkpoints: CheckpointSummary[] }) {
  return (
    <div className="font-mono text-[0.72rem]">
      {checkpoints.map((c) => (
        <div
          key={c.snapshot_id}
          style={{
            display: 'grid',
            gridTemplateColumns: '1.2em 7em 6em 1fr',
            gap: '0.6rem',
            padding: '0.2rem 0',
            borderBottom: '1px dotted var(--color-rule)',
            color: 'var(--color-ink-faded)',
          }}
          title={`snapshot ${c.snapshot_id} · events cursor ${
            c.events_cursor ?? 'unresolved'
          }`}
        >
          <span
            aria-hidden
            style={{
              color: c.is_latest ? 'var(--color-amber)' : 'var(--color-ink-quiet)',
            }}
          >
            {c.is_latest ? '◆' : '◇'}
          </span>
          <span>{fmtAgo(c.created_at)}</span>
          <span>{fmtBytes(c.size_bytes)}</span>
          <span style={{ color: 'var(--color-ink-quiet)' }}>
            {c.is_latest ? 'rung-1 anchor' : c.recoverable ? 'recoverable' : 'unverified'}
          </span>
        </div>
      ))}
    </div>
  );
}
