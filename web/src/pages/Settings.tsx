import { motion } from 'framer-motion';
import { NavLink, Outlet, useLocation } from 'react-router-dom';
import { useIsAdmin } from '../auth/AuthProvider';

// Settings surface — ADR 0031 redesign. Grouped into two sections:
//   You         → Profile · Tokens        (everyone)
//   Deployment  → Members · Images · Registries  (admin only)
//
// Settings is now visible to ALL users (it holds profile + tokens).
// Admin-only config is filtered inside, not by hiding the whole tab.
// A vertical hairline separates the two groups; on narrow viewports
// they stack with per-group hairline-tops.

interface Tab {
  to: string;
  label: string;
  hint: string;
}

const YOU_TABS: Tab[] = [
  {
    to: 'profile',
    label: 'Profile',
    hint: 'Your signed-in identity & what your role can do',
  },
  {
    to: 'tokens',
    label: 'Tokens',
    hint: 'Your service tokens — sealed & used automatically for every session',
  },
];

const DEPLOYMENT_TABS: Tab[] = [
  {
    to: 'members',
    label: 'Members',
    hint: 'Everyone in this deployment — roles & access',
  },
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
];

const ALL_HINTS: Record<string, string> = {
  ...Object.fromEntries(YOU_TABS.map((t) => [t.to, t.hint])),
  ...Object.fromEntries(DEPLOYMENT_TABS.map((t) => [t.to, t.hint])),
};

export function Settings() {
  const location = useLocation();
  const isAdmin = useIsAdmin();

  const activeSegment = location.pathname.split('/').pop() ?? '';
  const sub = ALL_HINTS[activeSegment] ?? 'Your settings';

  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">settings</h1>
          <motion.p
            key={sub}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: 0.25 }}
            className="surface-sub"
          >
            {sub}
          </motion.p>
        </div>
      </div>

      <nav className="settings-groups" aria-label="Settings sections">
        {/* You group — everyone */}
        <div className="settings-group">
          <span className="settings-group-label">you</span>
          <div className="settings-group-tabs">
            {YOU_TABS.map((tab) => (
              <SettingsTab key={tab.to} to={tab.to} label={tab.label} />
            ))}
          </div>
        </div>

        {/* Vertical hairline + Deployment group — admin only */}
        {isAdmin && (
          <>
            <span className="settings-divider" aria-hidden />
            <div className="settings-group">
              <span className="settings-group-label">deployment</span>
              <div className="settings-group-tabs">
                {DEPLOYMENT_TABS.map((tab) => (
                  <SettingsTab key={tab.to} to={tab.to} label={tab.label} />
                ))}
              </div>
            </div>
          </>
        )}
      </nav>

      <div className="mt-10">
        <Outlet />
      </div>
    </main>
  );
}

function SettingsTab({ to, label }: { to: string; label: string }) {
  return (
    <NavLink
      to={to}
      className={({ isActive }) =>
        `settings-tab${isActive ? ' tab-active' : ' tab-inactive'}`
      }
      style={({ isActive }) => ({
        color: isActive ? 'var(--color-ink)' : 'var(--color-ink-quiet)',
        borderBottom: isActive
          ? '1px solid var(--color-ink)'
          : '1px solid transparent',
      })}
    >
      {label}
    </NavLink>
  );
}
