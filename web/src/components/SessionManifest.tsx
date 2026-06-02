import { motion } from 'framer-motion';
import { Link } from 'react-router-dom';
import { StatusGlyph } from './Glyph';
import { EngramMark } from './EngramMark';
import type { Session, SessionState } from '../types';

// A single session manifest row: status glyph · short id · image ·
// status · age, laid out on the `.session-row` grid (which the
// responsive rules collapse to two lines on phones). Shared by the
// grouped manifest on the Sessions surface.
//
// While a session is transitioning toward Active (created / pending /
// guest_ready, and the brief states a resume passes through) the
// status glyph is replaced by the inline trace loader — the same
// growing-bolt motion as the masthead mark, scaled down to glyph size.

// States that read as "booting / resuming" — show the inline loader.
const BOOTING: ReadonlySet<SessionState> = new Set([
  'created',
  'pending',
  'guest_ready',
]);

export function SessionRow({
  session,
  owner,
}: {
  session: Session;
  /** ADR 0031: owner email, shown only in the admin "all sessions" view. */
  owner?: string | null;
}) {
  const since = relativeTime(session.last_active_at);
  // ADR 0005: there's no workspace-level repo/branch on a session
  // anymore — the bake image is the whole story. Strip the registry
  // host + tag for visual density.
  const imageLabel = stripImageHost(session.image);
  const booting = BOOTING.has(session.status);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -2 }}
      transition={{ duration: 0.35 }}
    >
      <Link to={`/sessions/${session.id}`} className="session-row">
        {booting ? (
          <span className="row-loader" aria-label={session.status}>
            <EngramMark
              size={15}
              mode="loop"
              period={1600}
              title={session.status}
            />
          </span>
        ) : (
          <StatusGlyph status={session.status} />
        )}
        <span
          className="font-mono text-[0.85rem]"
          style={{ color: 'var(--color-ink)' }}
        >
          {short(session.id)}
        </span>
        <span
          className="font-display"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          {imageLabel}
        </span>
        <span
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {session.status}
        </span>
        {owner && (
          <span
            className="font-mono text-[0.72rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
            title={`owner: ${owner}`}
          >
            {owner}
          </span>
        )}
        <span
          className="font-mono text-[0.78rem]"
          style={{ color: 'var(--color-ink-quiet)', minWidth: '6ch' }}
          data-tabular
        >
          {since}
        </span>
      </Link>
    </motion.div>
  );
}

/** Drop the leading `<host>/` and trailing `:<tag>` from an OCI URI,
 * leaving the repo segment ("ghcr.io/cortex/api:warm-1" → "cortex/api").
 * Falls back to the input verbatim if either delimiter is missing. */
export function stripImageHost(uri: string): string {
  const slash = uri.indexOf('/');
  const colon = uri.lastIndexOf(':');
  const start = slash >= 0 ? slash + 1 : 0;
  const end = colon > start ? colon : uri.length;
  return uri.slice(start, end);
}

function short(id: string) {
  if (id.length <= 12) return id;
  return `${id.slice(0, 8)}…`;
}

export function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  const dt = Math.max(0, (Date.now() - t) / 1000);
  if (dt < 60) return `${Math.floor(dt)}s`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m`;
  if (dt < 86400) return `${Math.floor(dt / 3600)}h`;
  return `${Math.floor(dt / 86400)}d`;
}
