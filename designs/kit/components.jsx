// components.jsx — presentational components for the Engrams dashboard kit.
// Cosmetic recreations of cortexapps/engrams › web/src/components/*.
const { useState, useEffect, useRef } = React;

// ---- StatusGlyph -----------------------------------------------------------
// Shape carries meaning; color reinforces. Active pulses (heartbeat).
function glyphFor(status) {
  return ({
    pending: '○', created: '◐', guest_ready: '◐', active: '●', idle: '◌',
    host_lost: '⚠', completed: '✓', failed: '!', dead: '✕',
  })[status] || '·';
}
function toneFor(status) {
  switch (status) {
    case 'active': case 'host_lost': case 'failed': return 'var(--accent-now)';
    case 'idle': return 'var(--accent-archived)';
    case 'dead': return 'var(--fg-quiet)';
    default: return 'var(--fg-muted)';
  }
}
function StatusGlyph({ status, beat = true }) {
  const isLive = beat && status === 'active';
  return (
    <span className={`glyph ${isLive ? 'glyph-heartbeat' : ''}`}
      style={{ color: toneFor(status) }} aria-label={status}>
      {glyphFor(status)}
    </span>
  );
}

// ---- SectionHead -----------------------------------------------------------
function SectionHead({ label, right }) {
  return (
    <div className="flex items-baseline justify-between mb-4 pb-2"
      style={{ borderBottom: '1px solid var(--border)' }}>
      <h2 className="section-label">{label}</h2>
      {right}
    </div>
  );
}

// ---- VitalSigns ------------------------------------------------------------
function RollingNumber({ value }) {
  const [k, setK] = useState(0);
  const prev = useRef(value);
  useEffect(() => {
    if (prev.current !== value) { prev.current = value; setK((x) => x + 1); }
  }, [value]);
  return (
    <span key={k} className="stat-figure roll" data-tabular>{value}</span>
  );
}
function VitalSigns({ hosts, sessions }) {
  const h = hosts || [], s = sessions || [];
  const snapshots = h.reduce((a, x) => a + x.local_snapshots, 0);
  const stats = [
    { label: 'HOSTS', value: h.length },
    { label: 'SNAPSHOTS', value: snapshots },
    { label: 'ACTIVE', value: s.filter((x) => x.status === 'active').length },
    { label: 'IDLE', value: s.filter((x) => x.status === 'idle').length },
  ];
  return (
    <div className="grid grid-cols-2 sm:grid-cols-4 vitals mb-12">
      {stats.map((st) => (
        <div key={st.label} className="flex flex-col">
          <span className="section-label" style={{ letterSpacing: '0.05em', fontSize: '0.62rem' }}>{st.label}</span>
          <RollingNumber value={st.value} />
        </div>
      ))}
    </div>
  );
}

// ---- HostManifest ----------------------------------------------------------
function HostBlock({ host }) {
  const totalGiB = (host.capacity_total_mib / 1024).toFixed(1);
  const usedGiB = (host.capacity_used_mib / 1024).toFixed(1);
  const dot = host.status === 'ready' ? '●' : host.status === 'draining' ? '◐' : '✕';
  return (
    <div className="host-block">
      <div className="flex items-baseline gap-3">
        <span className="glyph" style={{ color: host.status === 'ready' ? 'var(--fg)' : 'var(--fg-quiet)' }}>{dot}</span>
        <span className="font-mono" style={{ fontSize: '0.95rem', whiteSpace: 'nowrap' }}>{shortId(host.id)}</span>
        <span className="section-label" style={{ letterSpacing: '0.05em' }}>{host.status}</span>
        <span className="font-mono ml-auto" style={{ fontSize: '0.78rem', color: 'var(--fg-muted)' }}>
          {host.running_sandboxes} sandboxes
          {host.capacity_total_mib > 0 && ` · ${usedGiB}/${totalGiB} GiB`}
          {host.local_snapshots > 0 && ` · ${host.local_snapshots} snapshots`}
        </span>
      </div>
      {host.status !== 'dead' && <HostCowToggle host={host} />}
    </div>
  );
}
function HostCowToggle({ host }) {
  const [open, setOpen] = useState(false);
  const rows = open ? cowRowsFor(host) : [];
  return (
    <div style={{ marginTop: '0.5rem' }}>
      <button type="button" onClick={() => setOpen((v) => !v)} className="cow-toggle">
        {open ? '▾' : '▸'} cow state
      </button>
      {open && (rows.length ? (
        <div style={{ marginTop: '0.5rem' }}>
          <CowHeader />
          {rows.map((r) => <CowRow key={r.sandbox_id} v={r} />)}
        </div>
      ) : (
        <p className="font-mono italic" style={{ fontSize: '0.78rem', color: 'var(--fg-muted)', marginTop: '0.5rem' }}>
          no chunk-tracked sandboxes on this host
        </p>
      ))}
    </div>
  );
}
function cowRowsFor(host) {
  return Array.from({ length: Math.min(2, host.running_sandboxes) }, (_, i) => ({
    sandbox_id: `sbx-${host.id.slice(-4)}-${i}`,
    session_id: uid(''),
    dirty_chunks: 3 + i * 5, dirty_bytes: (i + 1) * 8 * 1024 * 1024,
    base_chunks: 320, base_chunks_local: 320 - i * 12,
    last_flush_at: ago(20 + i * 35),
  }));
}
function CowHeader() {
  const cols = ['session', 'dirty', 'bytes', 'locality', 'rpo'];
  return (
    <div className="cow-grid section-label" style={{ fontSize: '0.65rem', letterSpacing: '0.12em', borderBottom: '1px solid var(--border)', padding: '0.25rem 0' }}>
      {cols.map((c) => <span key={c}>{c}</span>)}
    </div>
  );
}
function CowRow({ v }) {
  const pct = v.base_chunks > 0 ? Math.round((v.base_chunks_local / v.base_chunks) * 100) : null;
  return (
    <div className="cow-grid font-mono" style={{ fontSize: '0.78rem', padding: '0.35rem 0', borderBottom: '1px dotted var(--border)', color: 'var(--fg-muted)' }}>
      <span style={{ color: 'var(--fg)' }}>{shortId(v.session_id)}</span>
      <span>{v.dirty_chunks} dirty</span>
      <span>{fmtBytes(v.dirty_bytes)}</span>
      <span>{pct !== null ? `${pct}% local` : '—'}</span>
      <span>flush {relativeTime(v.last_flush_at)} ago</span>
    </div>
  );
}
function HostManifest({ hosts }) {
  return (
    <section className="mb-12">
      <SectionHead label="HOSTS" />
      <div className="space-y-6">
        {(hosts || []).map((h) => <HostBlock key={h.id} host={h} />)}
      </div>
    </section>
  );
}

// ---- SessionManifest -------------------------------------------------------
const STATUS_ORDER = ['active', 'created', 'guest_ready', 'pending', 'idle', 'host_lost', 'failed', 'completed', 'dead'];
function SessionRow({ session, onOpen, bootLoader, showOwner }) {
  const booting = session.status === 'created' || session.status === 'guest_ready';
  return (
    <a href="#" onClick={(e) => { e.preventDefault(); onOpen(session.id); }} className={`session-row${showOwner ? ' has-owner' : ''}`}>
      {bootLoader && booting
        ? <span className="row-loader" title="booting"><EngramMark size={18} mode="loop" period={2200} /></span>
        : <StatusGlyph status={session.status} />}
      <span className="font-mono" style={{ fontSize: '0.85rem', color: 'var(--fg)' }}>{shortId(session.id)}</span>
      <span style={{ fontFamily: 'var(--font-display)', color: 'var(--fg-muted)' }}>{stripImageHost(session.image)}</span>
      <span className="section-label" style={{ letterSpacing: '0.05em' }}>{session.status}</span>
      {showOwner && <OwnerCell session={session} />}
      <span className="font-mono" data-tabular style={{ fontSize: '0.78rem', color: 'var(--fg-quiet)', minWidth: '6ch', textAlign: 'right' }}>
        {relativeTime(session.last_active_at)}
      </span>
    </a>
  );
}
function SessionManifest({ sessions, onNewClick, onOpen, bootLoader }) {
  const sorted = [...(sessions || [])].sort((a, b) => {
    const r = STATUS_ORDER.indexOf(a.status) - STATUS_ORDER.indexOf(b.status);
    return r !== 0 ? r : new Date(b.last_active_at) - new Date(a.last_active_at);
  });
  return (
    <section className="mb-12">
      <SectionHead label="SESSIONS" right={onNewClick && (
        <button type="button" onClick={onNewClick} className="link-amber section-label"
          style={{ letterSpacing: '0.12em', color: 'var(--link)' }}>+ new session</button>
      )} />
      <div className="space-y-1">
        {sorted.map((s) => <SessionRow key={s.id} session={s} onOpen={onOpen} bootLoader={bootLoader} />)}
        {sessions && sessions.length === 0 && (
          <p className="italic" style={{ fontFamily: 'var(--font-display)', color: 'var(--fg-quiet)' }}>
            no sessions yet — start one with the link above.
          </p>
        )}
      </div>
    </section>
  );
}

// ---- TabRow ----------------------------------------------------------------
function TabRow({ tabs, active, onChange, right }) {
  return (
    <div className="flex items-baseline justify-between mb-4 pb-2" style={{ borderBottom: '1px solid var(--border)' }}>
      <nav className="flex items-baseline gap-5">
        {tabs.map((t) => (
          <button key={t.id} type="button" onClick={() => onChange(t.id)}
            className="tab-btn section-label" style={{
              color: t.id === active ? 'var(--fg)' : 'var(--fg-quiet)',
              borderBottom: t.id === active ? '1px solid var(--accent-now)' : '1px solid transparent',
            }}>{t.label}</button>
        ))}
      </nav>
      {right && <div className="font-mono" style={{ fontSize: '0.7rem', color: 'var(--fg-quiet)' }}>{right}</div>}
    </div>
  );
}

// ---- identity vocabulary (ADR 0031) ---------------------------------------
// The signed-in principal (mirrors GET /me). Seeded as a local admin.
const PRINCIPAL = {
  name: 'Nikhil Unni',
  email: 'nikhil.unni@cortex.io',
  role: 'admin',
  is_admin: true,
  has_claude_token: true,
  can_sign_out: true,
  source: 'claim',
};
// The deployment's people (mirrors GET /admin/users → AdminUser[]).
const PEOPLE = [
  { id: 'u_01', name: 'Nikhil Unni', email: 'nikhil.unni@cortex.io', role: 'admin', source: 'claim', active: true, you: true },
  { id: 'u_02', name: 'Dana Schuman', email: 'dana.schuman@cortex.io', role: 'admin', source: 'manual', active: true },
  { id: 'u_03', name: 'Priya Raman', email: 'priya.raman@cortex.io', role: 'member', source: 'scim', active: true },
  { id: 'u_04', name: 'Theo Park', email: 'theo.park@cortex.io', role: 'member', source: 'claim', active: true },
  { id: 'u_05', name: 'Marco Bianchi', email: 'marco.bianchi@cortex.io', role: 'member', source: 'scim', active: true },
  { id: 'u_06', name: 'Jen Okafor', email: 'jen.okafor@cortex.io', role: 'member', source: 'claim', active: false },
];
function initialOf(name, email) {
  return ((name || email || '?').charAt(0) || '?').toUpperCase();
}
// The embossed typesetter's initial — square at every size. size: 'xs'|'sm'|'md'.
function PersonMark({ name, email, size = 'sm', off = false }) {
  return (
    <span className={`person-mark pm-${size}${off ? ' pm-off' : ''}`} aria-hidden="true">
      <span>{initialOf(name, email)}</span>
    </span>
  );
}
function RoleTag({ role }) {
  return <span className={`role-tag role-${role}`}>{role}</span>;
}
const PROVENANCE = { claim: 'via okta claim', scim: 'via scim sync', manual: 'set by an admin' };
function Provenance({ source }) {
  return <span className="provenance">{PROVENANCE[source] || source}</span>;
}
function MemberStatus({ active }) {
  return active
    ? <span className="member-status">active</span>
    : <span className="member-status"><span className="x">✕</span>disabled</span>;
}
// owner badge: a person's initial, or the engram trace mark for sessions the
// platform itself launched (warm-pool boots, scheduled / automated runs).
function OwnerBadge({ owner, size = 'xs' }) {
  if (owner && owner.kind === 'system') {
    return (
      <span className={`person-mark pm-${size} pm-system`} title="engrams · automated">
        <EngramMark size={size === 'xs' ? 15 : 19} mode="static" />
      </span>
    );
  }
  return <PersonMark name={owner.name} email={owner.email} size={size} off={owner && owner.off} />;
}
// the full owner token used in a session row: badge + label (email, or the
// serif "engrams · <reason>" for platform-owned sessions).
function OwnerCell({ session }) {
  const sys = session.owner_kind === 'system';
  return (
    <span className="owner-cell" title={sys ? `engrams · ${session.owner_label || 'automated'}` : `owner · ${session.owner_email}`}>
      <OwnerBadge owner={{ kind: session.owner_kind, name: session.owner_name, email: session.owner_email }} />
      {sys
        ? <span className="owner-sys" title={`engrams · ${session.owner_label || 'automated'}`}>engrams</span>
        : <span className="owner-email">{session.owner_email}</span>}
    </span>
  );
}

// ---- UserChip --------------------------------------------------------------
function UserChip({ onSettings, inline = false, principal = PRINCIPAL }) {
  const [open, setOpen] = useState(false);
  const ref = useRef(null);
  useEffect(() => {
    if (!open) return;
    const onClick = (e) => { if (ref.current && !ref.current.contains(e.target)) setOpen(false); };
    const onKey = (e) => { if (e.key === 'Escape') setOpen(false); };
    document.addEventListener('mousedown', onClick);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onClick); document.removeEventListener('keydown', onKey); };
  }, [open]);
  return (
    <div ref={ref} className={`user-chip ${inline ? 'user-chip-inline' : ''}`} style={{ fontFamily: 'var(--font-display)' }}>
      <button type="button" aria-label="Open user menu" onClick={() => setOpen((v) => !v)}
        className="chip-btn" style={{ borderColor: open ? 'var(--fg)' : 'var(--border)' }}>
        <span style={{ fontStyle: 'italic', transform: 'translateY(-1px)' }}>
          {initialOf(principal.name, principal.email)}
        </span>
      </button>
      {open && (
        <div role="menu" className="chip-pop">
          <div style={{ padding: '0.75rem 1rem' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.6rem' }}>
              <span style={{ fontFamily: 'var(--font-display)', fontSize: '0.95rem', color: 'var(--fg)' }}>{principal.name}</span>
              <RoleTag role={principal.role} />
            </div>
            <div className="font-mono" style={{ fontSize: '0.76rem', color: 'var(--fg-quiet)', marginTop: '0.25rem' }}>{principal.email}</div>
          </div>
          <hr />
          <ul style={{ padding: '0.25rem 0' }}>
            <li><a href="#" onClick={(e) => { e.preventDefault(); setOpen(false); onSettings(); }} className="chip-item">Settings</a></li>
            {principal.can_sign_out
              ? <li><a href="#" onClick={(e) => { e.preventDefault(); setOpen(false); }} className="chip-item italic">Sign out</a></li>
              : <li className="chip-item disabled" title="re-authenticated upstream — nothing to revoke">Sign out</li>}
          </ul>
        </div>
      )}
    </div>
  );
}

Object.assign(window, {
  StatusGlyph, SectionHead, VitalSigns, HostManifest, SessionManifest,
  SessionRow, TabRow, UserChip, glyphFor, toneFor,
  PersonMark, PRINCIPAL, PEOPLE, initialOf, RoleTag, Provenance, MemberStatus, PROVENANCE,
  OwnerBadge, OwnerCell,
});
