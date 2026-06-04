// Code-based TanStack Router tree for engrams-web. The shell is RootLayout (the
// primary destinations rail + inset). `/sessions` and `/settings` are nested
// LAYOUT routes that each render their own second sidebar + <Outlet/>;
// Fleet/Storage render full-bleed in the inset. SessionDetail is a CHILD of the
// sessions layout, so the persistent sessions rail stays mounted across the
// list views and the transcript (the highlight moves; the rail doesn't remount).
//
// Admin surfaces guard via a shared `requireAdmin` beforeLoad reading `isAdmin`
// from typed router context; the context's `auth` is populated at
// <RouterProvider/> time (see App.tsx), and AuthProvider gates rendering until
// the principal resolves, so `context.auth` is always present when beforeLoad
// runs. The coordinator's require_admin layer remains the real gate — these
// guards are UX-only.
import {
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
} from '@tanstack/react-router';
import type { AuthState } from './auth/AuthProvider';
import { RootLayout } from './pages/RootLayout';
import { SessionsLayout } from './pages/sessions/SessionsLayout';
import { MySessions } from './pages/sessions/MySessions';
import { AllSessions } from './pages/sessions/AllSessions';
import { SessionDetail } from './pages/SessionDetail';
import { Fleet } from './pages/Fleet';
import { Storage } from './pages/Storage';
import { SettingsLayout } from './pages/settings/SettingsLayout';
import { Members } from './pages/Members';
import { ImagesPanel } from './components/settings/ImagesPanel';
import { ProfilePanel } from './components/settings/ProfilePanel';
import { RegistriesPanel } from './components/settings/RegistriesPanel';
import { TokensPanel } from './components/settings/TokensPanel';

export interface RouterContext {
  auth: AuthState;
}

/** Shared UX-only admin guard. Members who deep-link to an admin route get
 * bounced to their profile instead of rendering an empty/erroring panel. */
function requireAdmin({ context }: { context: RouterContext }) {
  if (!context.auth.isAdmin) {
    throw redirect({ to: '/settings/profile' });
  }
}

const createRootRoute = createRootRouteWithContext<RouterContext>();
const rootRoute = createRootRoute({ component: RootLayout });

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  beforeLoad: () => {
    throw redirect({ to: '/sessions' });
  },
});

// /sessions layout route (second sidebar) ----------------------------------
const sessionsLayoutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/sessions',
  component: SessionsLayout,
});
const mySessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: '/',
  component: MySessions,
});
const allSessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: 'all',
  beforeLoad: requireAdmin,
  component: AllSessions,
});
// Session detail is a CHILD of the sessions layout at /sessions/$id, so the
// persistent sessions rail wraps it too. Static `all` outranks the dynamic
// `$id`, so /sessions/all still resolves to the fleet list.
const sessionDetailRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: '$id',
  component: SessionDetail,
});

const fleetRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/fleet',
  beforeLoad: requireAdmin,
  component: Fleet,
});
const storageRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/storage',
  beforeLoad: requireAdmin,
  component: Storage,
});

// /settings layout route (second sidebar) ----------------------------------
const settingsLayoutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/settings',
  component: SettingsLayout,
});
const settingsIndexRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: '/',
  beforeLoad: () => {
    throw redirect({ to: '/settings/profile' });
  },
});
const profileRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'profile', component: ProfilePanel });
const tokensRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'tokens', component: TokensPanel });
const membersRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'members', beforeLoad: requireAdmin, component: Members });
const imagesRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'images', beforeLoad: requireAdmin, component: ImagesPanel });
const registriesRoute = createRoute({ getParentRoute: () => settingsLayoutRoute, path: 'registries', beforeLoad: requireAdmin, component: RegistriesPanel });

const routeTree = rootRoute.addChildren([
  indexRoute,
  sessionsLayoutRoute.addChildren([mySessionsRoute, allSessionsRoute, sessionDetailRoute]),
  fleetRoute,
  storageRoute,
  settingsLayoutRoute.addChildren([
    settingsIndexRoute, profileRoute, tokensRoute, membersRoute, imagesRoute, registriesRoute,
  ]),
]);

export const router = createRouter({
  routeTree,
  // Populated per-render at <RouterProvider context={{ auth }} /> in App.tsx.
  context: { auth: undefined! as AuthState },
});

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router;
  }
}
