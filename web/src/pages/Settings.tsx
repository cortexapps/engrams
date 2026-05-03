import { motion } from 'framer-motion';
import { NavLink, Outlet, useLocation } from 'react-router-dom';

// Settings layout — the second top-level page next to the Overview.
// Same `book` column, same hairline rule under the header. The tab
// row is rendered as typeset section labels separated by middle
// dots; the active tab gets a 1-pixel ink underline (a "page-marker"
// flag in notebook terms, not a button-shaped pill).
//
// Sub-routes are nested under /settings: /settings/registries (the
// default), /settings/harnesses, /settings/profile. Each renders as
// the <Outlet/> below.

interface Tab {
  to: string;
  label: string;
  /** Short marginal note rendered under the active tab. Helps when
   * the page label alone doesn't carry the section's purpose. */
  hint: string;
}

const TABS: Tab[] = [
  {
    to: 'registries',
    label: 'Registries',
    hint: 'Docker registry credentials, sealed under the deployment KEK',
  },
  {
    to: 'harnesses',
    label: 'Harnesses',
    hint: 'Agent packs published to a registry, indexed by name',
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

  return (
    <main className="book py-12 relative">
      <Header subtitle={active?.hint ?? 'Configuration for this deployment'} />
      <TabRow />
      <div className="mt-10">
        <Outlet />
      </div>
    </main>
  );
}

function Header({ subtitle }: { subtitle: string }) {
  return (
    <header className="mb-8">
      <h1
        className="font-display"
        style={{
          fontSize: '2.6rem',
          fontWeight: 400,
          letterSpacing: '-0.015em',
          lineHeight: 1.05,
          fontStyle: 'italic',
        }}
      >
        settings
      </h1>
      <motion.p
        key={subtitle} // remount on tab change → fade
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.25 }}
        className="font-mono smallcaps text-[0.7rem] mt-2"
        style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
      >
        {subtitle}
      </motion.p>
      <hr className="mt-6" />
    </header>
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
