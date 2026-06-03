import { AnimatePresence } from 'framer-motion';
import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useHosts } from '../hooks/useHosts';
import { useSessions } from '../hooks/useSessions';
import { useAuth, useIsAdmin } from '../auth/AuthProvider';
import { VitalStrip } from '../components/VitalStrip';
import { ManifestGroup } from '../components/ManifestGroup';
import { NewSessionForm } from '../components/NewSessionForm';
import type { Session, SessionListItem } from '../types';

// Sessions — the driver's home (`/`). A compact vital strip, a primary
// "+ new session" affordance, and the session manifest grouped by
// lifecycle: ACTIVE (pinned top) → IDLE — RESUMABLE → ARCHIVED.
//
// ADR 0031: owner-scoped. Members see only their own sessions (no scope UI)
// and see a token nudge if they have no Claude token saved.
// Admins default to "My sessions" and can switch to "All sessions" (the
// fleet-wide oversight view, with an owner chip per row).

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
  const isAdmin = useIsAdmin();
  const { principal } = useAuth();
  const [scope, setScope] = useState<'mine' | 'all'>('mine');
  const effectiveScope = isAdmin ? scope : 'mine';

  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions(isAdmin ? effectiveScope : undefined);
  const [creating, setCreating] = useState(false);
  const navigate = useNavigate();

  const all: SessionListItem[] = sessions ?? [];
  const showOwner = isAdmin && effectiveScope === 'all';
  const active = all.filter((s) => ACTIVEISH.has(s.status)).sort(byRecent);
  const idle = all.filter((s) => s.status === 'idle').sort(byRecent);
  const archived = all.filter((s) => ARCHIVED.has(s.status)).sort(byRecent);

  const showTokenNudge = !isAdmin && !principal.has_claude_token;

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">sessions</h1>
          <p className="surface-sub">
            {showOwner
              ? 'every session across the fleet — owner-attributed.'
              : 'bounded units of agent work — launch, watch, resume.'}
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

      {/* ADR 0031: admins get a Mine/All switch; members see no scope UI. */}
      {isAdmin && (
        <div
          className="flex items-baseline gap-5 mb-2"
          style={{ fontFamily: 'var(--font-display)' }}
          role="tablist"
          aria-label="Session scope"
        >
          <ScopeTab
            label="My sessions"
            active={scope === 'mine'}
            onClick={() => setScope('mine')}
          />
          <span
            aria-hidden
            className="font-mono"
            style={{ color: 'var(--color-rule)', fontSize: '0.7rem' }}
          >
            ·
          </span>
          <ScopeTab
            label="All sessions"
            active={scope === 'all'}
            onClick={() => setScope('all')}
          />
        </div>
      )}

      {/* Member token nudge — shown when no Claude token is saved */}
      {showTokenNudge && (
        <div className="token-nudge">
          <span className="nudge-text">
            no Claude Code token saved yet — built-in Claude sessions need one.
          </span>
          <button
            type="button"
            className="nudge-act"
            onClick={() => navigate('/settings/tokens')}
          >
            add token →
          </button>
        </div>
      )}

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

      <ManifestGroup label="ACTIVE" sessions={active} keepEmpty showOwner={showOwner} />
      <ManifestGroup label="IDLE — RESUMABLE" sessions={idle} showOwner={showOwner} />
      <ManifestGroup label="ARCHIVED" sessions={archived} showOwner={showOwner} />

      {all.length === 0 && (
        <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
          {showOwner
            ? 'no active sessions across the fleet.'
            : 'no sessions yet — start one with "+ new session".'}
        </p>
      )}
    </main>
  );
}

function ScopeTab({
  label,
  active,
  onClick,
}: {
  label: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className="transition-colors pb-1"
      style={{
        color: active ? 'var(--color-ink)' : 'var(--color-ink-quiet)',
        fontSize: '1rem',
        background: 'none',
        border: 0,
        cursor: 'pointer',
        borderBottom: active
          ? '1px solid var(--color-ink)'
          : '1px solid transparent',
      }}
    >
      {label}
    </button>
  );
}
