import { AnimatePresence, motion } from 'framer-motion';
import type { HostView } from '../types';

// "Manifest" because the visual model is a typeset ledger — entries
// listed in order with capacity / snapshot counts in a tabular row.

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
            No hosts have registered yet. Hosts appear here once they
            boot and complete their first heartbeat.
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
