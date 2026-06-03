// Code-based TanStack Router tree for engrams-web. Mirrors the four-surface
// IA 1:1 (was src/App.tsx's <Routes>). Admin surfaces guard via a shared
// `requireAdmin` beforeLoad reading `isAdmin` from typed router context; the
// context's `auth` is populated at <RouterProvider/> time (see App.tsx), and
// AuthProvider gates rendering until the principal resolves, so `context.auth`
// is always present when beforeLoad runs. The coordinator's require_admin
// layer remains the real gate — these guards are UX-only.
import {
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
} from '@tanstack/react-router';
import type { AuthState } from './auth/AuthProvider';
import { Layout } from './pages/Layout';
import { Sessions } from './pages/Sessions';
import { SessionDetail } from './pages/SessionDetail';
import { Fleet } from './pages/Fleet';
import { Storage } from './pages/Storage';
import { Settings } from './pages/Settings';
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
const rootRoute = createRootRoute({ component: Layout });

const sessionsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: Sessions,
});

const sessionDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/sessions/$id',
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

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/settings',
  component: Settings,
});

const settingsIndexRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: '/',
  beforeLoad: () => {
    throw redirect({ to: '/settings/profile' });
  },
});

const profileRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: 'profile',
  component: ProfilePanel,
});

const tokensRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: 'tokens',
  component: TokensPanel,
});

const membersRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: 'members',
  beforeLoad: requireAdmin,
  component: Members,
});

const imagesRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: 'images',
  beforeLoad: requireAdmin,
  component: ImagesPanel,
});

const registriesRoute = createRoute({
  getParentRoute: () => settingsRoute,
  path: 'registries',
  beforeLoad: requireAdmin,
  component: RegistriesPanel,
});

const routeTree = rootRoute.addChildren([
  sessionsRoute,
  sessionDetailRoute,
  fleetRoute,
  storageRoute,
  settingsRoute.addChildren([
    settingsIndexRoute,
    profileRoute,
    tokensRoute,
    membersRoute,
    imagesRoute,
    registriesRoute,
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
