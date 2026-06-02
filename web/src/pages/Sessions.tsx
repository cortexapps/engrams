import { AnimatePresence } from 'framer-motion';
import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { VitalStrip } from '../components/VitalStrip';
import { ManifestGroup } from '../components/ManifestGroup';
import { NewSessionForm } from '../components/NewSessionForm';
import type { Session } from '../types';

// Sessions — the driver's home (`/`). A compact vital strip, a primary
// "+ new session" affordance, and the session manifest grouped by
// lifecycle: ACTIVE (pinned top) → IDLE — RESUMABLE → ARCHIVED.

const ACTIVEISH = new Set<Session['status']>([
  'active',
  'created',
  'guest_ready',
  'pending',
  'host_lost',
]);
const ARCHIVED = new Set<Session['status']>(['completed', 'dead', 'failed']);

const byRecent = (a: Session, b: Session) =>
  new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime();

export function Sessions() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();
  const [creating, setCreating] = useState(false);
  const navigate = useNavigate();

  const all = sessions ?? [];
  const active = all.filter((s) => ACTIVEISH.has(s.status)).sort(byRecent);
  const idle = all.filter((s) => s.status === 'idle').sort(byRecent);
  const archived = all.filter((s) => ARCHIVED.has(s.status)).sort(byRecent);

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">sessions</h1>
          <p className="surface-sub">
            bounded units of agent work — launch, watch, resume.
          </p>
        </div>
        {!creating && (
          <button
            type="button"
            className="primary-action"
            onClick={() => setCreating(true)}
          >
            + new session
          </button>
        )}
      </div>

      <VitalStrip hosts={hosts} sessions={sessions} />

      <AnimatePresence>
        {creating && (
          <NewSessionForm
            key="new-session-form"
            onCancel={() => setCreating(false)}
            onCreated={(id) => {
              setCreating(false);
              navigate(`/sessions/${id}`);
            }}
          />
        )}
      </AnimatePresence>

      <ManifestGroup label="ACTIVE" sessions={active} keepEmpty />
      <ManifestGroup label="IDLE — RESUMABLE" sessions={idle} />
      <ManifestGroup label="ARCHIVED" sessions={archived} />

      {all.length === 0 && (
        <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
          no sessions yet — start one with “+ new session”.
        </p>
      )}
    </main>
  );
}
