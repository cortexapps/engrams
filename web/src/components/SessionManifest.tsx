import { AnimatePresence, motion } from 'framer-motion';
import { Link } from 'react-router-dom';
import { StatusGlyph } from './Glyph';
import type { Session } from '../types';

const STATUS_ORDER: Session['status'][] = [
  'active',
  'pending',
  'idle',
  'failed',
  'completed',
  'dead',
];

export function SessionManifest({
  sessions,
  onNewClick,
}: {
  sessions: Session[] | undefined;
  /** When defined, render a "+ new session" link in the section head. */
  onNewClick?: () => void;
}) {
  const sorted = sortSessions(sessions ?? []);

  return (
    <section className="mb-12">
      <div
        className="flex items-baseline justify-between mb-4 pb-2"
        style={{ borderBottom: '1px solid var(--color-rule)' }}
      >
        <h2
          className="font-mono smallcaps text-[0.7rem]"
          style={{
            color: 'var(--color-ink-quiet)',
            letterSpacing: '0.18em',
          }}
        >
          SESSIONS
        </h2>
        {onNewClick && (
          <button
            type="button"
            onClick={onNewClick}
            className="font-mono smallcaps text-[0.7rem] transition-colors hover:[opacity:0.75]"
            style={{
              color: 'var(--color-amber)',
              letterSpacing: '0.12em',
            }}
          >
            + new session
          </button>
        )}
      </div>
      <div className="space-y-1">
        <AnimatePresence>
          {sorted.map((s) => (
            <SessionRow key={s.id} session={s} />
          ))}
        </AnimatePresence>
        {sessions && sessions.length === 0 && (
          <p
            className="font-display italic"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            no sessions yet
            {onNewClick && ' — start one with the link above'}.
          </p>
        )}
      </div>
    </section>
  );
}

function SessionRow({ session }: { session: Session }) {
  const since = relativeTime(session.last_active_at);
  const repoLabel = formatRepo(session);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -2 }}
      transition={{ duration: 0.35 }}
    >
      <Link
        to={`/sessions/${session.id}`}
        className="grid items-baseline gap-x-4 py-1 px-2 -mx-2 hover:[background-color:var(--color-paper-warm)] transition-colors"
        style={{
          gridTemplateColumns: 'min-content min-content 1fr min-content min-content',
        }}
      >
        <StatusGlyph status={session.status} />
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
          {repoLabel}
        </span>
        <span
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {session.status}
        </span>
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

function sortSessions(sessions: Session[]): Session[] {
  return [...sessions].sort((a, b) => {
    const ra = STATUS_ORDER.indexOf(a.status);
    const rb = STATUS_ORDER.indexOf(b.status);
    if (ra !== rb) return ra - rb;
    return (
      new Date(b.last_active_at).getTime() -
      new Date(a.last_active_at).getTime()
    );
  });
}

function formatRepo(s: Session): string {
  switch (s.workspace.kind) {
    case 'empty':
      // Strip registry host + tag for visual density. Stage B1 made
      // session.image a flat OCI URI like
      // `ghcr.io/cortex/api:warm-1`; the panel only has room for the
      // repo segment.
      return `${stripImageHost(s.image)} (empty)`;
    case 'git': {
      const branch =
        s.workspace.branch && s.workspace.branch !== 'main'
          ? ` · ${s.workspace.branch}`
          : '';
      // Strip the scheme + host prefix for visual density.
      return `${s.workspace.url.replace(/^https?:\/\/[^/]+\//, '')}${branch}`;
    }
  }
}

/** Drop the leading `<host>/` and trailing `:<tag>` from an OCI URI,
 * leaving the repo segment ("ghcr.io/cortex/api:warm-1" → "cortex/api").
 * Falls back to the input verbatim if either delimiter is missing. */
function stripImageHost(uri: string): string {
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
