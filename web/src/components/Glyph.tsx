import { motion } from 'framer-motion';
import type { SessionStatus } from '../types';

// Status glyphs in the margin — these stand in for colored dots. The
// shape carries the meaning, not the color.
//
//   ●  active             ◐  warming / starting
//   ◌  idle / snapshotted  ○  pending
//   ✕  dead                ✓  completed
//   !  failed
//
// Active sessions get a slow opacity heartbeat (see .glyph-heartbeat
// in theme.css). Idle sessions render in verdigris to mark them as
// archival, never amber. Dead sessions fade into ink-quiet.

export interface GlyphProps {
  status: SessionStatus;
  /** Override beat (e.g. event ticker doesn't pulse). */
  beat?: boolean;
}

export function StatusGlyph({ status, beat = true }: GlyphProps) {
  const glyph = glyphFor(status);
  const tone = toneFor(status);
  const isLive = beat && status === 'active';

  return (
    <motion.span
      key={glyph} // remount on change → triggers cross-fade
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.35, ease: 'easeOut' }}
      className={`glyph ${isLive ? 'glyph-heartbeat' : ''}`}
      style={{ color: tone }}
      aria-label={status}
    >
      {glyph}
    </motion.span>
  );
}

function glyphFor(status: SessionStatus): string {
  switch (status) {
    case 'pending':
      return '○';
    case 'active':
      return '●';
    case 'idle':
      return '◌';
    case 'cold_evicted':
      return '◇';
    case 'completed':
      return '✓';
    case 'failed':
      return '!';
    case 'dead':
      return '✕';
  }
}

function toneFor(status: SessionStatus): string {
  switch (status) {
    case 'active':
      return 'var(--color-amber)';
    case 'idle':
      return 'var(--color-verdigris)';
    case 'cold_evicted':
      return 'var(--color-ink-quiet)';
    case 'pending':
      return 'var(--color-ink-faded)';
    case 'completed':
      return 'var(--color-ink-faded)';
    case 'failed':
      return 'var(--color-amber)';
    case 'dead':
      return 'var(--color-ink-quiet)';
  }
}
