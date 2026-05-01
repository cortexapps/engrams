import { AnimatePresence, motion } from 'framer-motion';
import { useEffect, useRef, useState } from 'react';
import type { HostView, Session } from '../types';

// Six tabular numerals across the top of the page, like the meter
// row on an instrument panel. Each digit roll-ups when it changes —
// the change carries the signal, the absolute value is secondary.

export interface VitalSignsProps {
  hosts: HostView[] | undefined;
  sessions: Session[] | undefined;
}

export function VitalSigns({ hosts, sessions }: VitalSignsProps) {
  const stats = computeStats(hosts, sessions);

  return (
    <div className="grid grid-cols-3 gap-y-6 gap-x-8 sm:grid-cols-5 mb-12">
      {stats.map((s) => (
        <Stat key={s.label} label={s.label} value={s.value} />
      ))}
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="flex flex-col items-baseline">
      <span
        className="font-mono smallcaps text-[0.62rem]"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        {label}
      </span>
      <RollingNumber value={value} />
    </div>
  );
}

function RollingNumber({ value }: { value: number }) {
  // Animate the *whole* number, not per-digit — keeps the motion legible.
  const prev = useRef(value);
  const [animKey, setAnimKey] = useState(0);
  useEffect(() => {
    if (prev.current !== value) {
      prev.current = value;
      setAnimKey((k) => k + 1);
    }
  }, [value]);

  return (
    <span
      data-tabular
      className="font-display"
      style={{
        fontSize: '2.4rem',
        lineHeight: 1,
        marginTop: '0.15rem',
        fontVariantNumeric: 'tabular-nums lining-nums',
      }}
    >
      <AnimatePresence mode="popLayout" initial={false}>
        <motion.span
          key={animKey}
          initial={{ y: -8, opacity: 0 }}
          animate={{ y: 0, opacity: 1 }}
          exit={{ y: 8, opacity: 0 }}
          transition={{ duration: 0.22, ease: 'easeOut' }}
          style={{ display: 'inline-block' }}
        >
          {value}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

function computeStats(
  hosts: HostView[] | undefined,
  sessions: Session[] | undefined,
) {
  const h = hosts ?? [];
  const s = sessions ?? [];

  const warm = h.reduce(
    (acc, host) => acc + host.warm_pools.reduce((a, p) => a + p.ready, 0),
    0,
  );
  const active = s.filter((x) => x.status === 'active').length;
  const idle = s.filter((x) => x.status === 'idle').length;
  const dead = s.filter((x) => x.status === 'dead').length;

  return [
    { label: 'HOSTS', value: h.length },
    { label: 'WARM', value: warm },
    { label: 'ACTIVE', value: active },
    { label: 'IDLE', value: idle },
    { label: 'DEAD', value: dead },
  ];
}
