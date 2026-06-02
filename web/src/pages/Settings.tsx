import { motion } from 'framer-motion';
import { NavLink, Outlet, useLocation } from 'react-router-dom';

// Settings surface — aligned to the nav spine like Fleet / Storage /
// Sessions (ADR 0030). It uses the same wide measure (`book-wide`) and
// the same `surface-head` header (big italic `surface-title` +
// `surface-sub`) as the other admin surfaces, so tabbing in from the
// spine no longer shifts the content's left edge or jumps to the old
// standalone layout. The old narrow `book` column and the
// `engrams › settings` breadcrumb-h1 are gone — the spine already
// provides home + location.
//
// The `surface-sub` carries the per-tab hint (Images / Registries /
// Profile) and fades on tab change. The tab row below stays as typeset
// section labels separated by middle dots; the active tab gets a 1px
// ink underline (a notebook "page-marker" flag, not a button pill).
//
// Sub-routes nest under /settings: /settings/images (default),
// /settings/registries, /settings/profile — each renders via <Outlet/>.
//
// ADR 0021 P1.5a retired the `/settings/harnesses` tab — the
// `/api/harnesses` registry doesn't exist anymore (harnesses are an
// image property baked at image-bake time).

interface Tab {
  to: string;
  label: string;
  /** Short hint rendered as the surface sub-line for the active tab.
   * Helps when the page label alone doesn't carry the section's
   * purpose. */
  hint: string;
}

const TABS: Tab[] = [
  {
    to: 'images',
    label: 'Images',
    hint: 'Curated OCI image URIs sessions may reference',
  },
  {
    to: 'registries',
    label: 'Registries',
    hint: 'Docker registry credentials, sealed under the deployment KEK',
  },
  {
    to: 'profile',
    label: 'Profile',
    hint: 'Identity for this deployment',
  },
];

export function Settings() {
  const location = useLocation();
  const active = TABS.find((t) => location.pathname.endsWith(`/${t.to}`));
  const sub = active?.hint ?? 'Configuration for this deployment';

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">settings</h1>
          <motion.p
            key={sub} // remount on tab change → fade
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: 0.25 }}
            className="surface-sub"
          >
            {sub}
          </motion.p>
        </div>
      </div>

      <TabRow />

      <div className="mt-10">
        <Outlet />
      </div>
    </main>
  );
}

function TabRow() {
  return (
    <nav
      aria-label="Settings sections"
      className="flex gap-6 items-baseline"
      style={{ fontFamily: 'var(--font-display)' }}
    >
      {TABS.map((tab, i) => (
        <div key={tab.to} className="flex items-baseline gap-6">
          {i > 0 && (
            <span
              aria-hidden
              className="font-mono"
              style={{ color: 'var(--color-rule)', fontSize: '0.7rem' }}
            >
              ·
            </span>
          )}
          <NavLink
            to={tab.to}
            className={({ isActive }) =>
              `transition-colors pb-1 ${isActive ? 'tab-active' : 'tab-inactive'}`
            }
            style={({ isActive }) => ({
              color: isActive ? 'var(--color-ink)' : 'var(--color-ink-quiet)',
              fontSize: '1rem',
              borderBottom: isActive
                ? '1px solid var(--color-ink)'
                : '1px solid transparent',
            })}
          >
            {tab.label}
          </NavLink>
        </div>
      ))}
    </nav>
  );
}
