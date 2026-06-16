// Code-based TanStack Router tree for engrams-web. The shell is RootLayout (the
// primary destinations rail + inset). `/sessions`, `/operator`, and `/settings`
// are nested LAYOUT routes that each render their own second sidebar + <Outlet/>.
// `/operator` is the admin hat: one section gathers fleet, storage, images, and
// registries behind a single rail, landing on a read-only Overview cockpit.
// SessionDetail is a CHILD of the sessions layout, so the persistent sessions
// rail stays mounted across the list views and the transcript (the highlight
// moves; the rail doesn't remount).
//
// Auth gate: appLayoutRoute.beforeLoad redirects to /login when context.auth is
// null (unauthenticated). /login is a sibling of appLayoutRoute (not a child),
// so it is never caught by the guard. This replaces the old window.location
// redirect in AuthProvider, which caused an infinite reload loop on /login.
//
// Admin surfaces guard via a shared `requireAdmin` beforeLoad reading `isAdmin`
// from typed router context. The coordinator's require_admin layer remains the
// real gate — these guards are UX-only.
import {
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
} from "@tanstack/react-router";
import type { AuthState } from "./auth/AuthProvider";
import { RootLayout } from "./pages/RootLayout";
import { Login } from "./pages/Login";
import { SessionsLayout } from "./pages/sessions/SessionsLayout";
import { MySessions } from "./pages/sessions/MySessions";
import { AllSessions } from "./pages/sessions/AllSessions";
import { SessionDetail } from "./pages/SessionDetail";
import { OperatorLayout } from "./pages/operator/OperatorLayout";
import { Overview } from "./pages/operator/Overview";
import { Fleet } from "./pages/Fleet";
import { Storage } from "./pages/Storage";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { Members } from "./pages/Members";
import { ImagesPanel } from "./components/settings/ImagesPanel";
import { ProfilePanel } from "./components/settings/ProfilePanel";
import { RegistriesPanel } from "./components/settings/RegistriesPanel";
import { TokensPanel } from "./components/settings/TokensPanel";

export interface RouterContext {
  /** Null when the session has resolved but no user is signed in.
   * The appLayoutRoute.beforeLoad gate redirects to /login in that case.
   * Authenticated routes can safely assert non-null after the gate runs. */
  auth: AuthState | null;
}

/** Auth gate for all authenticated routes (appLayoutRoute and its children).
 * Redirects to /login when no session is present. The /login route is a
 * sibling of appLayoutRoute — it is never covered by this guard. */
function requireAuth({ context }: { context: RouterContext }) {
  if (!context.auth) {
    throw redirect({ to: "/login" });
  }
}

/** Shared UX-only admin guard. Members who deep-link to an admin route get
 * bounced to their profile instead of rendering an empty/erroring panel. */
function requireAdmin({ context }: { context: RouterContext }) {
  if (!context.auth?.isAdmin) {
    throw redirect({ to: "/settings/profile" });
  }
}

const createRootRoute = createRootRouteWithContext<RouterContext>();

// The true root is a bare passthrough (no component) so the tree can host
// both the app shell (RootLayout) and the /login page (no chrome) as siblings.
const rootRoute = createRootRoute();

// /login — unauthenticated entry point; no app chrome.
// beforeLoad: redirect already-authenticated users (e.g. after IAP sets a
// session cookie and the browser returns to /login) straight to the app.
const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  beforeLoad: ({ context }) => {
    if (context.auth) {
      throw redirect({ to: "/" });
    }
  },
  component: Login,
});

// The app shell — pathless layout wrapping all authenticated routes.
// Using id (no path) makes TanStack Router treat this as a layout-only segment
// that contributes no URL prefix — children like /sessions still resolve
// as /sessions, not /app/sessions.
// beforeLoad: requireAuth redirects unauthenticated visitors to /login.
const appLayoutRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "_app",
  beforeLoad: requireAuth,
  component: RootLayout,
});

const indexRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/sessions" });
  },
});

// /sessions layout route (second sidebar) ----------------------------------
const sessionsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/sessions",
  component: SessionsLayout,
});
const mySessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: "/",
  component: MySessions,
});
const allSessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: "all",
  beforeLoad: requireAdmin,
  component: AllSessions,
});
// Session detail is a CHILD of the sessions layout at /sessions/$id, so the
// persistent sessions rail wraps it too. Static `all` outranks the dynamic
// `$id`, so /sessions/all still resolves to the fleet list.
const sessionDetailRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: "$id",
  component: SessionDetail,
});

// /operator layout route (second sidebar) — the admin hat. The whole section
// is admin-gated here, so the child telemetry/config routes don't each re-guard.
const operatorLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/operator",
  beforeLoad: requireAdmin,
  component: OperatorLayout,
});
const operatorIndexRoute = createRoute({
  getParentRoute: () => operatorLayoutRoute,
  path: "/",
  component: Overview,
});
const operatorFleetRoute = createRoute({
  getParentRoute: () => operatorLayoutRoute,
  path: "fleet",
  component: Fleet,
});
const operatorStorageRoute = createRoute({
  getParentRoute: () => operatorLayoutRoute,
  path: "storage",
  component: Storage,
});
const operatorImagesRoute = createRoute({
  getParentRoute: () => operatorLayoutRoute,
  path: "images",
  component: ImagesPanel,
});
const operatorRegistriesRoute = createRoute({
  getParentRoute: () => operatorLayoutRoute,
  path: "registries",
  component: RegistriesPanel,
});

// /settings layout route (second sidebar) ----------------------------------
const settingsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/settings",
  component: SettingsLayout,
});
const settingsIndexRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/settings/profile" });
  },
});
const profileRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profile",
  component: ProfilePanel,
});
const tokensRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "tokens",
  component: TokensPanel,
});
const membersRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "members",
  beforeLoad: requireAdmin,
  component: Members,
});

export const routeTree = rootRoute.addChildren([
  // /login — bare page, no app chrome
  loginRoute,
  // Authenticated app shell — all authenticated routes nested here
  appLayoutRoute.addChildren([
    indexRoute,
    sessionsLayoutRoute.addChildren([mySessionsRoute, allSessionsRoute, sessionDetailRoute]),
    operatorLayoutRoute.addChildren([
      operatorIndexRoute,
      operatorFleetRoute,
      operatorStorageRoute,
      operatorImagesRoute,
      operatorRegistriesRoute,
    ]),
    settingsLayoutRoute.addChildren([settingsIndexRoute, profileRoute, tokensRoute, membersRoute]),
  ]),
]);

export const router = createRouter({
  routeTree,
  // Populated per-render at <RouterProvider context={{ auth }} /> in App.tsx.
  // auth is null when signed out; appLayoutRoute.beforeLoad redirects to /login.
  context: { auth: null },
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
