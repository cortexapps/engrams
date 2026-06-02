import { Link, useLocation, useParams } from 'react-router-dom';
import { EngramMark } from './EngramMark';
import { UserChip } from './UserChip';

// The masthead NAV SPINE for the four-surface IA. One persistent
// header carrying brand, navigation, and live status:
//
//   [mark] engrams   Sessions · Fleet · Storage · Settings      [§]
//                    ↳ a3f9c1b2…   (sub-crumb when drilled into a session)
//
// The wordmark links home (Sessions). Tabs are mono small-caps labels
// with a 1px amber underline on the active route — never pills. The
// `§` UserChip stays top-right and routes to Settings. The mark is the
// living-status indicator: it loops while a session is booting and
// strikes once per poll tick otherwise (wired by the Layout).

const SURFACES = [
  { to: '/', label: 'Sessions', match: (p: string) => p === '/' || p.startsWith('/sessions') },
  { to: '/fleet', label: 'Fleet', match: (p: string) => p.startsWith('/fleet') },
  { to: '/storage', label: 'Storage', match: (p: string) => p.startsWith('/storage') },
  { to: '/settings', label: 'Settings', match: (p: string) => p.startsWith('/settings') },
];

function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}

export interface NavSpineProps {
  /** True while any session is created/booting — loops the mark. */
  booting?: boolean;
  /** Poll tick: each change fires one mark strike (when not booting). */
  tick?: number;
}

export function NavSpine({ booting = false, tick = 0 }: NavSpineProps) {
  const { pathname } = useLocation();
  const params = useParams();
  // Sub-crumb only on the session-detail route (a child of Sessions).
  const sessionId = pathname.startsWith('/sessions/') ? params.id : undefined;

  return (
    <div className="navspine">
      <div className="navspine-inner book-wide">
        <Link to="/" className="nav-brand" aria-label="engrams — sessions">
          <span className="nav-mark">
            <EngramMark
              size={26}
              mode={booting ? 'loop' : 'static'}
              pulseKey={booting ? 0 : tick}
            />
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
