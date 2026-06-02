import { Link, useLocation, useParams } from 'react-router-dom';
import { EngramMark } from './EngramMark';
import { UserChip } from './UserChip';

// The masthead NAV SPINE for the four-surface IA. One persistent
// header carrying brand and navigation:
//
//   [mark] engrams   Sessions · Fleet · Storage · Settings      [§]
//                    ↳ a3f9c1b2…   (sub-crumb when drilled into a session)
//
// The mark here is the *static* canonical logo — the brand, not a
// status light. The growing-trace animation is reserved for loaders
// (the inline boot loaders on booting session rows), so the masthead
// stays a stable identity. The wordmark links home (Sessions); tabs
// are mono small-caps labels with a 1px amber underline on the active
// route — never pills. The `§` UserChip routes to Settings.

const SURFACES = [
  { to: '/', label: 'Sessions', match: (p: string) => p === '/' || p.startsWith('/sessions') },
  { to: '/fleet', label: 'Fleet', match: (p: string) => p.startsWith('/fleet') },
  { to: '/storage', label: 'Storage', match: (p: string) => p.startsWith('/storage') },
  { to: '/settings', label: 'Settings', match: (p: string) => p.startsWith('/settings') },
];

function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}

export function NavSpine() {
  const { pathname } = useLocation();
  const params = useParams();
  // Sub-crumb only on the session-detail route (a child of Sessions).
  const sessionId = pathname.startsWith('/sessions/') ? params.id : undefined;

  return (
    <div className="navspine">
      <div className="navspine-inner book-wide">
        <Link to="/" className="nav-brand" aria-label="engrams — sessions">
          <span className="nav-mark">
            {/* The static canonical mark — the brand, not a status
                light. Animation is reserved for loaders. */}
            <EngramMark size={26} mode="static" />
          </span>
          <span className="nav-wordmark">engrams</span>
        </Link>

        <nav className="nav-tabs" aria-label="Primary">
          {SURFACES.map((s) => (
            <Link
              key={s.to}
              to={s.to}
              className="nav-tab"
              aria-current={s.match(pathname) ? 'page' : undefined}
            >
              {s.label}
            </Link>
          ))}
        </nav>

        <div className="nav-right">
          <UserChip inline />
        </div>
      </div>

      {sessionId && (
        <div className="navspine-crumb book-wide">
          <span className="crumb-sep" aria-hidden>
            ↳
          </span>
          <span className="crumb-current font-mono">{shortId(sessionId)}</span>
        </div>
      )}

      <hr className="masthead-rule" />
    </div>
  );
}
