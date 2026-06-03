import { motion } from 'framer-motion';
import { Link } from '@tanstack/react-router';
import { StatusGlyph } from './Glyph';
import { EngramMark } from './EngramMark';
import { OwnerCell } from './Identity';
import type { SessionListItem, SessionState } from '../types';

// A single session manifest row: status glyph · short id · image ·
// status · (owner) · age, laid out on the `.session-row` grid.
//
// ADR 0031: the grid bug fix — adding a 6th child (owner) to a 5-column grid
// caused the age cell to wrap onto a second line. The `has-owner` class switches
// in an explicit 17rem owner track between status and age.
//
// While a session is transitioning toward Active (created / pending /
// guest_ready) the status glyph is replaced by the inline trace loader.

const BOOTING: ReadonlySet<SessionState> = new Set([
  'created',
  'pending',
  'guest_ready',
]);

export function SessionRow({
  session,
  showOwner = false,
}: {
  session: SessionListItem;
  /** ADR 0031: show the owner chip per row (admin "all sessions" view). */
  showOwner?: boolean;
}) {
  const since = relativeTime(session.last_active_at);
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
      <Link
        to="/sessions/$id"
        params={{ id: session.id }}
        className={`session-row${showOwner ? ' has-owner' : ''}`}
      >
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
        {showOwner && (
          <OwnerCell
            ownerKind={session.owner_kind}
            ownerName={session.owner_name}
            ownerEmail={session.owner_email}
          />
        )}
        <span
          className="font-mono text-[0.78rem]"
          style={{ color: 'var(--color-ink-quiet)', minWidth: '6ch', textAlign: 'right' }}
          data-tabular
        >
          {since}
        </span>
      </Link>
    </motion.div>
  );
}

/** Drop the leading `<host>/` and trailing `:<tag>` from an OCI URI. */
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
