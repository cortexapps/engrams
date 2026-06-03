import { motion } from 'framer-motion';
import type { SessionState } from '../types';

// Status glyphs in the margin — these stand in for colored dots. The
// shape carries the meaning, not the color.
//
//   ●  active            ◐  created / guest_ready (starting)
//   ◌  idle              ○  pending
//   ⚠  host_lost         ✓  completed
//   ✕  dead              !  failed
//   ◑  evicting / evacuating (transitional: suspend/relocate in
//      flight — the half-moon mirrors the starting states' ◐)
//
// Active sessions get a slow opacity heartbeat (see .glyph-heartbeat
// in theme.css). Idle sessions render in verdigris to mark them as
// archival, never amber. Dead sessions fade into ink-quiet. Host-
// lost sessions render in amber to flag that they need attention
// (snapshot exists → /resume; no snapshot → going Dead shortly).

export interface GlyphProps {
  status: SessionState;
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

function glyphFor(status: SessionState): string {
  switch (status) {
    case 'pending':
      return '○';
    case 'created':
    case 'guest_ready':
      return '◐';
    case 'active':
      return '●';
    case 'idle':
      return '◌';
    case 'evicting':
    case 'evacuating':
      return '◑';
    case 'host_lost':
      return '⚠';
    case 'completed':
      return '✓';
    case 'failed':
      return '!';
    case 'dead':
      return '✕';
  }
}

function toneFor(status: SessionState): string {
  switch (status) {
    case 'active':
      return 'var(--color-amber)';
    case 'idle':
      return 'var(--color-verdigris)';
    case 'pending':
    case 'created':
    case 'guest_ready':
      return 'var(--color-ink-faded)';
    // Transitional suspend/relocate: verdigris like idle — they're
    // on their way there (or back to active), not in trouble.
    case 'evicting':
    case 'evacuating':
      return 'var(--color-verdigris)';
    case 'host_lost':
      return 'var(--color-amber)';
    case 'completed':
      return 'var(--color-ink-faded)';
    case 'failed':
      return 'var(--color-amber)';
    case 'dead':
      return 'var(--color-ink-quiet)';
  }
}
