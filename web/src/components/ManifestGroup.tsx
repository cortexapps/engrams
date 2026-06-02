import { AnimatePresence } from 'framer-motion';
import { SessionRow } from './SessionManifest';
import type { Session } from '../types';

// A ledger section of the grouped session manifest: a small-caps
// header with a trailing count, then the rows. An empty group renders
// nothing unless `keepEmpty` is set (used to pin ACTIVE at the top even
// when idle), in which case it shows a quiet "none".

export function ManifestGroup({
  label,
  sessions,
  keepEmpty = false,
}: {
  label: string;
  sessions: Session[];
  keepEmpty?: boolean;
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
              <SessionRow key={s.id} session={s} />
            ))}
          </AnimatePresence>
        )}
      </div>
    </section>
  );
}
