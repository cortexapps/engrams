// Code-based TanStack Router tree for engrams-web. The shell is RootLayout (the
// primary destinations rail + inset). `/sessions`, `/reviews`, `/kaizen`, `/operator`, and `/settings`
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
import { DeviceAuth } from "./pages/DeviceAuth";
import { SessionsLayout } from "./pages/sessions/SessionsLayout";
import { StartScreen } from "./pages/sessions/StartScreen";
import { MySessions } from "./pages/sessions/MySessions";
import { AllSessions } from "./pages/sessions/AllSessions";
import { SessionDetail } from "./pages/SessionDetail";
import { Reviews } from "./pages/reviews/Reviews";
import { ReviewsLayout } from "./pages/reviews/ReviewsLayout";
import { ReviewDossier } from "./pages/reviews/ReviewDossier";
import { OperatorLayout } from "./pages/operator/OperatorLayout";
import { Overview } from "./pages/operator/Overview";
import { ArtifactsLayout } from "./pages/artifacts/ArtifactsLayout";
import { ArtifactsLibrary } from "./pages/artifacts/ArtifactsLibrary";
import { ArtifactDetail } from "./pages/artifacts/ArtifactDetail";
import { ArtifactViewPage } from "./pages/artifacts/ArtifactViewPage";
import { SpecsLayout } from "./pages/specs/SpecsLayout";
import { SpecsList } from "./pages/specs/SpecsList";
import { SpecRouteStub } from "./pages/specs/SpecRouteStub";
import { KaizenLayout } from "./pages/kaizen/KaizenLayout";
import { Papercuts } from "./pages/kaizen/Papercuts";
import { Fleet } from "./pages/Fleet";
import { Storage } from "./pages/Storage";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { Members } from "./pages/Members";
import { ImagesPanel } from "./components/settings/ImagesPanel";
import { ProfilePanel } from "./components/settings/ProfilePanel";
import { RegistriesPanel } from "./components/settings/RegistriesPanel";
import { SecretsPanel } from "./components/settings/SecretsPanel";
import { ApiKeysPanel } from "./components/settings/ApiKeysPanel";
import { HarnessesPanel } from "./components/settings/HarnessesPanel";
import { IntegrationsPanel } from "./components/settings/IntegrationsPanel";
import { ReviewedReposPanel } from "./components/settings/ReviewedReposPanel";
import { IntegrationDetail } from "./components/integrations/IntegrationDetail";
import { GoogleCloudSetupPage } from "./components/integrations/GoogleCloudSetupPage";
import { TokensPanel } from "./components/settings/TokensPanel";
import { SessionProfiles } from "./pages/settings/SessionProfiles";
import { SessionProfileEditor } from "./pages/settings/SessionProfileEditor";
import { Automations } from "./pages/settings/Automations";
import { AutomationEditor } from "./pages/settings/AutomationEditor";
import { SpecReadPage } from "./pages/SpecReadPage";

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

// /device — the browser leg of `engrams auth login` (device-flow approval).
// Child of the app layout so an anonymous visitor logs in first; the CLI
// opens /device?user_code=XXXX (the code is also typeable by hand).
const deviceAuthRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/device",
  validateSearch: (search: Record<string, unknown>): { user_code?: string } =>
    typeof search["user_code"] === "string" ? { user_code: search["user_code"] } : {},
  component: DeviceAuthPage,
});

function DeviceAuthPage() {
  const { user_code } = deviceAuthRoute.useSearch();
  return <DeviceAuth initialCode={user_code} />;
}

// /sessions layout route (second sidebar) ----------------------------------
const sessionsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/sessions",
  component: SessionsLayout,
});
// The landing IS the composer-first start screen (the one canonical create
// surface). The full "My tasks" table moves to /sessions/list, reachable from
// the rail footer and the start screen's "See all".
const startScreenRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: "/",
  component: StartScreen,
});
const mySessionsRoute = createRoute({
  getParentRoute: () => sessionsLayoutRoute,
  path: "list",
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

// /reviews — org-visible PR review ledger, with a persistent rail over reviewed
// PRs (ADR 0100). The dossier is a CHILD of this layout at /reviews/$id, so the
// rail wraps it too and opening a PR moves the highlight rather than swapping
// the layout — the same shape as the /sessions section.
const reviewsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/reviews",
  component: ReviewsLayout,
});
const reviewsIndexRoute = createRoute({
  getParentRoute: () => reviewsLayoutRoute,
  path: "/",
  component: Reviews,
});
// `$id` is a review PASS id, not a PR: the marker in the summary comment engrams
// posts on the PR names the exact pass, so it can deep-link straight to it.
const reviewDossierRoute = createRoute({
  getParentRoute: () => reviewsLayoutRoute,
  path: "$id",
  component: ReviewDossier,
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

// /artifacts — the cross-session document library, with a persistent rail
// (the Reviews/Sessions content-rail shape). The detail page is a CHILD of
// this layout at /artifacts/$artifactId, so the rail wraps it too. A
// GitHub-style pretty suffix (/artifacts/<id>/<slug>) resolves to the same
// page; the slug is ignored. `?v=N` selects an older version, `?scope=`
// makes a filtered library view linkable.
const artifactsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/artifacts",
  component: ArtifactsLayout,
});
const artifactsIndexRoute = createRoute({
  getParentRoute: () => artifactsLayoutRoute,
  path: "/",
  validateSearch: (search: Record<string, unknown>): { scope?: "shared" | "all" } => {
    const scope = search["scope"];
    return scope === "shared" || scope === "all" ? { scope } : {};
  },
  component: ArtifactsLibrary,
});
const artifactVersionSearch = (search: Record<string, unknown>): { v?: number } => {
  const v = Number(search["v"]);
  return Number.isInteger(v) && v > 0 ? { v } : {};
};
const artifactDetailRoute = createRoute({
  getParentRoute: () => artifactsLayoutRoute,
  path: "$artifactId",
  validateSearch: artifactVersionSearch,
  component: ArtifactDetail,
});
const artifactDetailPrettyRoute = createRoute({
  getParentRoute: () => artifactsLayoutRoute,
  path: "$artifactId/$slug",
  validateSearch: artifactVersionSearch,
  component: ArtifactDetail,
});
// The chromeless standalone view (the detail page's popout target):
// a sibling of the app shell so the rendered document owns the window —
// no rail, no section chrome. Static "view" outranks the pretty $slug.
const artifactViewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/artifacts/$artifactId/view",
  beforeLoad: requireAuth,
  validateSearch: artifactVersionSearch,
  component: ArtifactViewPage,
});

// /specs is the organization-shared Tech Specs catalog. The layout owns the
// top-level Specs and Templates tabs.
const specsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/specs",
  component: SpecsLayout,
});
const specsIndexRoute = createRoute({
  getParentRoute: () => specsLayoutRoute,
  path: "/",
  validateSearch: (search: Record<string, unknown>): StatusSearch => {
    const status = search["status"];
    return status === "draft" || status === "published" ? { status } : {};
  },
  component: SpecsList,
});
type StatusSearch = { status?: "draft" | "published" };
const specTemplatesRoute = createRoute({
  getParentRoute: () => specsLayoutRoute,
  path: "templates",
  component: () => <SpecRouteStub surface="templates" />,
});
const specDetailRoute = createRoute({
  getParentRoute: () => specsLayoutRoute,
  path: "$specId",
  component: SpecReadPage,
});

// /kaizen layout route (second sidebar) ----------------------------------
const kaizenLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/kaizen",
  beforeLoad: requireAuth,
  component: KaizenLayout,
});
const kaizenIndexRoute = createRoute({
  getParentRoute: () => kaizenLayoutRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/kaizen/papercuts" });
  },
});
const papercutsRoute = createRoute({
  getParentRoute: () => kaizenLayoutRoute,
  path: "papercuts",
  component: Papercuts,
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
  path: "credentials",
  component: TokensPanel,
});
const legacyTokensRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "tokens",
  beforeLoad: () => {
    throw redirect({ to: "/settings/credentials" });
  },
});
const membersRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "members",
  beforeLoad: requireAdmin,
  component: Members,
});
const secretsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "secrets",
  beforeLoad: requireAdmin,
  component: SecretsPanel,
});
const apiKeysRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "api-keys",
  beforeLoad: requireAdmin,
  component: ApiKeysPanel,
});
const harnessesRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "harnesses",
  beforeLoad: requireAdmin,
  component: HarnessesPanel,
});
const integrationsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "integrations",
  beforeLoad: requireAdmin,
  component: IntegrationsPanel,
});
const reviewedReposRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "reviewed-repos",
  beforeLoad: requireAdmin,
  component: ReviewedReposPanel,
});
const integrationDetailRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "integrations/$provider",
  beforeLoad: requireAdmin,
  component: IntegrationDetail,
});
// ADR 0109 seam: the setup page is per PROVIDER connection, not per Google
// connection. The provider key rides the path so a second named-connection
// provider needs no new route.
const connectionSetupRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "integrations/$provider/$connectionId/setup",
  beforeLoad: requireAdmin,
  component: GoogleCloudSetupPage,
});
const profilesRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles",
  beforeLoad: requireAdmin,
  component: SessionProfiles,
});
const profilesNewRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles/new",
  beforeLoad: requireAdmin,
  component: () => <SessionProfileEditor mode="create" />,
});
const profileEditRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles/$id",
  beforeLoad: requireAdmin,
  component: () => <SessionProfileEditor mode="edit" />,
});
const automationsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations",
  beforeLoad: requireAdmin,
  component: Automations,
});
const automationsNewRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/new",
  beforeLoad: requireAdmin,
  component: () => <AutomationEditor mode="create" />,
});
const automationEditRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/$id",
  beforeLoad: requireAdmin,
  component: () => <AutomationEditor mode="edit" />,
});

export const routeTree = rootRoute.addChildren([
  // /login — bare page, no app chrome
  loginRoute,
  // Standalone artifact view — authenticated but chromeless (popout).
  artifactViewRoute,
  // Authenticated app shell — all authenticated routes nested here
  appLayoutRoute.addChildren([
    indexRoute,
    deviceAuthRoute,
    sessionsLayoutRoute.addChildren([
      startScreenRoute,
      mySessionsRoute,
      allSessionsRoute,
      sessionDetailRoute,
    ]),
    reviewsLayoutRoute.addChildren([reviewsIndexRoute, reviewDossierRoute]),
    artifactsLayoutRoute.addChildren([
      artifactsIndexRoute,
      artifactDetailRoute,
      artifactDetailPrettyRoute,
    ]),
    specsLayoutRoute.addChildren([specsIndexRoute, specTemplatesRoute, specDetailRoute]),
    operatorLayoutRoute.addChildren([
      operatorIndexRoute,
      operatorFleetRoute,
      operatorStorageRoute,
      operatorImagesRoute,
      operatorRegistriesRoute,
    ]),
    kaizenLayoutRoute.addChildren([kaizenIndexRoute, papercutsRoute]),
    settingsLayoutRoute.addChildren([
      settingsIndexRoute,
      profileRoute,
      tokensRoute,
      legacyTokensRoute,
      membersRoute,
      secretsRoute,
      apiKeysRoute,
      harnessesRoute,
      integrationsRoute,
      integrationDetailRoute,
      connectionSetupRoute,
      reviewedReposRoute,
      profilesRoute,
      profilesNewRoute,
      profileEditRoute,
      automationsRoute,
      automationsNewRoute,
      automationEditRoute,
    ]),
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
