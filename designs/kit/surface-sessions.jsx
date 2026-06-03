// surface-sessions.jsx — the driver's home. A compact vital strip, a primary
// "new session" affordance, and the session manifest GROUPED by lifecycle
// (active pinned top → idle → archived) with ledger-section headers.
const { useState: useSess } = React;

// one-line ledger summary (replaces the big stat figures on this surface)
function VitalStrip({ hosts, sessions }) {
  const s = sessions, h = hosts;
  const n = (st) => s.filter((x) => x.status === st).length;
  const snapshots = h.reduce((a, x) => a + x.local_snapshots, 0);
  const items = [
    ['active', n('active')], ['idle', n('idle')],
    ['hosts', h.length], ['snapshots', snapshots],
  ];
  return (
    <div className="vital-strip">
      {items.map(([label, val], i) => (
        <span key={label} className="vital-cell">
          <span className="vital-num" data-tabular>{val}</span>
          <span className="vital-lbl">{label}</span>
        </span>
      ))}
    </div>
  );
}

const ARCHIVED = new Set(['completed', 'dead', 'failed']);
const ACTIVEISH = new Set(['active', 'created', 'guest_ready', 'pending', 'host_lost']);

function ManifestGroup({ label, sessions, onOpen, bootLoader, empty, showOwner }) {
  if (sessions.length === 0 && !empty) return null;
  return (
    <section className="manifest-group">
      <div className="manifest-group-head">
        <span className="section-label">{label}</span>
        <span className="section-label manifest-count" data-tabular>{sessions.length}</span>
      </div>
      <div className="space-y-1">
        {sessions.length === 0
          ? <p className="manifest-empty">none</p>
          : sessions.map((s) => <SessionRow key={s.id} session={s} onOpen={onOpen} bootLoader={bootLoader} showOwner={showOwner} />)}
      </div>
    </section>
  );
}

// admin-only scope: every session across the fleet (owner-attributed) vs. mine.
function ScopeToggle({ scope, setScope }) {
  const Tab = ({ id, label }) => (
    <button type="button" onClick={() => setScope(id)} className="scope-tab"
      aria-pressed={scope === id}
      style={{ background: 'none', border: 0, cursor: 'pointer', paddingBottom: '0.25rem', whiteSpace: 'nowrap',
        fontFamily: 'var(--font-display)', fontSize: '1rem',
        color: scope === id ? 'var(--fg)' : 'var(--fg-quiet)',
        borderBottom: scope === id ? '1px solid var(--fg)' : '1px solid transparent' }}>{label}</button>
  );
  return (
    <div style={{ display: 'flex', alignItems: 'baseline', gap: '1.25rem', marginBottom: '1.5rem' }} role="tablist" aria-label="Session scope">
      <Tab id="mine" label="My sessions" />
      <span aria-hidden className="font-mono" style={{ color: 'var(--border)', fontSize: '0.7rem' }}>·</span>
      <Tab id="all" label="All sessions" />
    </div>
  );
}

function SessionsSurface({ store, onOpen, onSettings, t = {} }) {
  const [creating, setCreating] = useSess(false);
  const isAdmin = (typeof PRINCIPAL !== 'undefined') && PRINCIPAL.is_admin;
  // members only ever see their own; admins default to the fleet-wide view.
  const [scope, setScope] = useSess(isAdmin ? 'all' : 'mine');
  const showOwner = isAdmin && scope === 'all';
  const byAge = (a, b) => new Date(b.last_active_at) - new Date(a.last_active_at);
  const mine = (typeof PRINCIPAL !== 'undefined') ? PRINCIPAL.email : null;
  const scoped = (showOwner || !mine)
    ? store.sessions
    : store.sessions.filter((s) => s.owner_email === mine);
  const active = scoped.filter((s) => ACTIVEISH.has(s.status)).sort(byAge);
  const idle = scoped.filter((s) => s.status === 'idle').sort(byAge);
  const archived = scoped.filter((s) => ARCHIVED.has(s.status)).sort(byAge);

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">sessions</h1>
          <p className="surface-sub">{showOwner
            ? 'every session across the fleet — owner-attributed.'
            : 'bounded units of agent work — launch, watch, resume.'}</p>
        </div>
        {!creating && (
          <button type="button" className="primary-action section-label" onClick={() => setCreating(true)}>
            + new session
          </button>
        )}
      </div>

      {isAdmin && <ScopeToggle scope={scope} setScope={setScope} />}

      {!isAdmin && !PRINCIPAL.has_claude_token && (
        <div className="token-nudge">
          <span className="nudge-text">no Claude Code token saved yet — built-in Claude sessions need one.</span>
          <button type="button" className="nudge-act" onClick={() => onSettings && onSettings()}>add token →</button>
        </div>
      )}

      <VitalStrip hosts={store.hosts} sessions={scoped} />

      {creating && (
        <NewSessionForm images={store.images} onCancel={() => setCreating(false)}
          onCreated={(uri, mode, prompt) => { setCreating(false); onOpen(store.createSession(uri, mode, prompt)); }} />
      )}

      <ManifestGroup label="ACTIVE" sessions={active} onOpen={onOpen} bootLoader={t.bootLoader} showOwner={showOwner} empty />
      <ManifestGroup label="IDLE — RESUMABLE" sessions={idle} onOpen={onOpen} bootLoader={t.bootLoader} showOwner={showOwner} />
      <ManifestGroup label="ARCHIVED" sessions={archived} onOpen={onOpen} bootLoader={t.bootLoader} showOwner={showOwner} />
    </main>
  );
}

Object.assign(window, { SessionsSurface, VitalStrip });
