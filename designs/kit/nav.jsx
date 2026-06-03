// nav.jsx — the masthead NAV SPINE for the four-surface IA.
// Wordmark + living-status mark, a row of typeset section labels
// (Sessions · Fleet · Storage · Settings — underline on active, never pills),
// and the § chip. One persistent header that carries brand, nav, and status.
const { useState: useNav } = React;

const SURFACES = [
  { id: 'sessions', label: 'Sessions' },
  { id: 'fleet', label: 'Fleet', adminOnly: true },
  { id: 'storage', label: 'Storage', adminOnly: true },
  { id: 'settings', label: 'Settings' },
];

function NavSpine({ active, onNavigate, onHome, crumb, markMode = 'pulse', pulseKey = 0, animate = true }) {
  return (
    <div className="navspine">
      <div className="navspine-inner book-wide">
        <a href="#" className="nav-brand" onClick={(e) => { e.preventDefault(); onHome(); }} aria-label="engrams — sessions">
          <span className="masthead-mark">
            <EngramMark size={26} mode={animate ? (markMode === 'loop' ? 'loop' : 'static') : 'static'}
              pulseKey={animate && markMode === 'pulse' ? pulseKey : 0} />
          </span>
          <span className="masthead-wordmark">engrams</span>
        </a>

        <nav className="nav-tabs" aria-label="Primary">
          {SURFACES.filter((s) => !s.adminOnly || (typeof PRINCIPAL === 'undefined') || PRINCIPAL.is_admin).map((s) => (
            <button key={s.id} type="button" onClick={() => onNavigate(s.id)}
              className="nav-tab section-label"
              aria-current={active === s.id ? 'page' : undefined}
              style={{
                color: active === s.id ? 'var(--fg)' : 'var(--fg-quiet)',
                borderBottomColor: active === s.id ? 'var(--accent-now)' : 'transparent',
              }}>
              {s.label}
            </button>
          ))}
        </nav>

        <div className="nav-right">
          <UserChip onSettings={() => onNavigate('settings')} inline />
        </div>
      </div>
      {crumb && (
        <div className="navspine-crumb book-wide">
          <span className="crumb-sep" aria-hidden>↳</span>
          <span className="crumb-current">{crumb}</span>
        </div>
      )}
      <hr className="masthead-rule" />
    </div>
  );
}

Object.assign(window, { NavSpine, SURFACES });
