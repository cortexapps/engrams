import { AnimatePresence, motion } from 'framer-motion';
import { PoolSlot } from './Glyph';
import type { HostView } from '../types';

// "Manifest" because the visual model is a typeset ledger — entries
// listed in order, each with its own short heading and a row of slot
// glyphs marking warm-pool fill state.

export function HostManifest({ hosts }: { hosts: HostView[] | undefined }) {
  return (
    <section className="mb-12">
      <SectionHead label="HOSTS" />
      <div className="space-y-6">
        <AnimatePresence>
          {(hosts ?? []).map((h) => (
            <HostBlock key={h.id} host={h} />
          ))}
        </AnimatePresence>
        {hosts && hosts.length === 0 && (
          <p
            className="font-display italic"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            No hosts have registered yet. The coordinator runs in
            single-process mode under <code className="font-mono">just dev</code>;
            it will appear here on first heartbeat.
          </p>
        )}
      </div>
    </section>
  );
}

function HostBlock({ host }: { host: HostView }) {
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(1);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.35, ease: 'easeOut' }}
    >
      <div className="flex items-baseline gap-3">
        <span
          className="glyph"
          style={{
            color:
              host.status === 'ready'
                ? 'var(--color-ink)'
                : 'var(--color-ink-quiet)',
          }}
        >
          {host.status === 'ready' ? '●' : host.status === 'draining' ? '◐' : '✕'}
        </span>
        <span className="font-mono text-[0.95rem]">
          {short(host.id)}
        </span>
        <span
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {host.status}
        </span>
        <span
          className="font-mono text-[0.78rem] ml-auto"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          {host.running_sandboxes} sandboxes
          {host.capacity_total_mib > 0 && ` · ${usedGiB}/${totalGiB} GiB`}
          {host.local_snapshots > 0 && ` · ${host.local_snapshots} snapshots`}
        </span>
      </div>

      <div className="mt-2 ml-6 space-y-1.5">
        <AnimatePresence>
          {host.warm_pools.map((p) => (
            <PoolRow
              key={`${p.repo}:${p.image_version}`}
              repo={p.repo}
              imageVersion={p.image_version}
              ready={p.ready}
              target={p.target}
            />
          ))}
        </AnimatePresence>
        {host.warm_pools.length === 0 && (
          <span
            className="font-display italic text-[0.85rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            (no warm pools)
          </span>
        )}
      </div>
    </motion.div>
  );
}

function PoolRow({
  repo,
  imageVersion,
  ready,
  target,
}: {
  repo: string;
  imageVersion: string;
  ready: number;
  target: number;
}) {
  const slots = Math.max(target, ready, 1);
  return (
    <motion.div
      layout
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.4 }}
      className="flex items-baseline gap-3 font-mono text-[0.82rem]"
    >
      <span
        className="inline-block"
        style={{ color: 'var(--color-ink-faded)', minWidth: '24ch' }}
      >
        {imageVersion} · {repo}
      </span>
      <span className="inline-flex gap-1.5">
        {Array.from({ length: slots }).map((_, i) => (
          <PoolSlot key={i} filled={i < ready} />
        ))}
      </span>
      <span
        className="ml-auto"
        style={{ color: 'var(--color-ink-quiet)' }}
        data-tabular
      >
        {ready} / {target}
      </span>
    </motion.div>
  );
}

function short(id: string) {
  // Render long UUIDs as `prefix…` for inline density. The full id is
  // available on hover via the title attribute on parent rows.
  if (id.length <= 12) return id;
  return `${id.slice(0, 8)}…`;
}

export function SectionHead({ label }: { label: string }) {
  return (
    <h2
      className="font-mono smallcaps text-[0.7rem] mb-4 pb-2"
      style={{
        color: 'var(--color-ink-quiet)',
        borderBottom: '1px solid var(--color-rule)',
        letterSpacing: '0.18em',
      }}
    >
      {label}
    </h2>
  );
}
