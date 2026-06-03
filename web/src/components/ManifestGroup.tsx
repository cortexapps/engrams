import { AnimatePresence } from 'framer-motion';
import { SessionRow } from './SessionManifest';
import type { SessionListItem } from '../types';

// A ledger section of the grouped session manifest: a small-caps
// header with a trailing count, then the rows. An empty group renders
// nothing unless `keepEmpty` is set (used to pin ACTIVE at the top even
// when idle), in which case it shows a quiet "none".

export function ManifestGroup({
  label,
  sessions,
  keepEmpty = false,
  showOwner = false,
}: {
  label: string;
  sessions: SessionListItem[];
  keepEmpty?: boolean;
  /** ADR 0031: show the owner chip per row (admin "all sessions" view). */
  showOwner?: boolean;
}) {
  if (sessions.length === 0 && !keepEmpty) return null;

  return (
    <section className="manifest-group">
      <div className="manifest-group-head">
        <span className="section-label">{label}</span>
        <span className="section-label manifest-count" data-tabular>
          {sessions.length}
        </span>
      </div>
      <div className="space-y-1">
        {sessions.length === 0 ? (
          <p className="manifest-empty">none</p>
        ) : (
          <AnimatePresence>
            {sessions.map((s) => (
              <SessionRow
                key={s.id}
                session={s}
                showOwner={showOwner}
              />
            ))}
          </AnimatePresence>
        )}
      </div>
    </section>
  );
}
