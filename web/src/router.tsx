// Code-based TanStack Router tree for engrams-web. The shell is RootLayout (the
// spine + inset). `/sessions`, `/reviews`, `/artifacts`, `/automations`, and
// `/settings` are nested LAYOUT routes that each render their own section rail +
// <Outlet/> through SectionLayout. Settings holds every configuration surface,
// the fleet and storage included; there is no separate admin hat.
// SessionDetail is a CHILD of the sessions layout, so the persistent sessions
// rail stays mounted across the list views and the transcript (the highlight
// moves; the rail doesn't remount).
//
// Retired paths (`/operator/*`, `/kaizen/*`, and automations under `/settings`)
// keep redirect routes so bookmarks and the review-engine runbook still land.
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
import { hasNextParam } from "./lib/next-url";
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
import { ArtifactsLayout } from "./pages/artifacts/ArtifactsLayout";
import { ArtifactsLibrary } from "./pages/artifacts/ArtifactsLibrary";
import { ArtifactDetail } from "./pages/artifacts/ArtifactDetail";
import { ArtifactViewPage } from "./pages/artifacts/ArtifactViewPage";
import { SpecsLayout } from "./pages/specs/SpecsLayout";
import { SpecsList } from "./pages/specs/SpecsList";
import { SpecTemplates } from "./pages/specs/SpecTemplates";
import { NewSpecPage } from "./pages/specmode/NewSpecPage";
import { SpecShellPage } from "./pages/specmode/SpecShellPage";
import { Papercuts } from "./pages/settings/Papercuts";
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
import { ModelRoutersPanel } from "./components/settings/ModelRoutersPanel";
import { IntegrationsPanel } from "./components/settings/IntegrationsPanel";
import { IntegrationDetail } from "./components/integrations/IntegrationDetail";
import { GoogleCloudSetupPage } from "./components/integrations/GoogleCloudSetupPage";
import { TokensPanel } from "./components/settings/TokensPanel";
import { SessionProfiles } from "./pages/settings/SessionProfiles";
import { SessionProfileEditor } from "./pages/settings/SessionProfileEditor";
import { AutomationsLayout } from "./pages/automations/AutomationsLayout";
import { AutomationsList } from "./pages/automations/AutomationsList";
import { ComposePage } from "./pages/automations/ComposePage";
import {
  AutomationEditor,
  isEditorTab,
  type EditorTab,
} from "./pages/automations/AutomationEditor";
import { ActivityTab } from "./pages/automations/activity/ActivityTab";
import { ActivityPage } from "./pages/automations/activity/ActivityPage";
import { WorkstreamsPage } from "./pages/automations/workstreams/WorkstreamsPage";
import { WorkstreamPage } from "./pages/automations/workstreams/WorkstreamPage";
import { SettingsTab } from "./pages/automations/settings/SettingsTab";
import { RedirectToBuiltin } from "./pages/automations/settings/RedirectToBuiltin";

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
  beforeLoad: ({ context, location }) => {
    if (!context.auth) return;
    // ADR 0118: a `?next=` arrival is a session-app round trip — the preview
    // edge bounced an unauthenticated navigation here and wants the human
    // returned to the app. Only the Login page can finish it: validating a
    // cross-origin destination needs the deployment's preview domain, which is
    // fetched, and the hop is a full-page navigation off this origin that a
    // router redirect cannot express. Falling through is the whole point —
    // redirecting to "/" here silently discarded the destination and stranded
    // the user on the dashboard, which is what it did in prod.
    if (hasNextParam(location.searchStr)) return;
    throw redirect({ to: "/" });
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

// Retired: the Operator section. Its pages live under /settings now (the
// Overview cockpit folded into Fleet), so the old paths only redirect.
const LEGACY_OPERATOR = {
  "/operator": "/settings/fleet",
  "/operator/fleet": "/settings/fleet",
  "/operator/storage": "/settings/storage",
  "/operator/images": "/settings/images",
  "/operator/registries": "/settings/registries",
} as const;
const legacyOperatorRoutes = Object.entries(LEGACY_OPERATOR).map(([path, to]) =>
  createRoute({
    getParentRoute: () => appLayoutRoute,
    path,
    beforeLoad: () => {
      throw redirect({ to, replace: true });
    },
  }),
);

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
  beforeLoad: requireAuth,
  component: SpecsLayout,
});
const specsIndexRoute = createRoute({
  getParentRoute: () => specsLayoutRoute,
  path: "/",
  validateSearch: (search: Record<string, unknown>): StatusSearch => {
    const status = search["status"];
    return status === "ideation" || status === "drafting" || status === "published"
      ? { status }
      : {};
  },
  component: SpecsList,
});
type StatusSearch = { status?: "ideation" | "drafting" | "published" };
const specTemplatesRoute = createRoute({
  getParentRoute: () => specsLayoutRoute,
  path: "templates",
  component: SpecTemplates,
});
// Both spec pages keep the app sidebar. The document surface used to render
// chromeless behind its own narrow spine, but that spine was a lossy copy of
// the sidebar — it carried three destinations where the sidebar carries six,
// and it would have drifted further with every entry the real one gained. A
// person who wants the document at full width collapses the sidebar, which is
// a control the app already has.
const specDetailRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/specs/$specId",
  component: SpecShellPage,
});
const newSpecRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/specs/new",
  component: NewSpecPage,
});

// Retired: the Kaizen section (one rail, one item). Papercuts is a Settings page.
const legacyKaizenRoutes = ["/kaizen", "/kaizen/papercuts"].map((path) =>
  createRoute({
    getParentRoute: () => appLayoutRoute,
    path,
    beforeLoad: () => {
      throw redirect({ to: "/settings/papercuts", replace: true });
    },
  }),
);

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
const modelRoutersRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "model-routers",
  beforeLoad: requireAdmin,
  component: ModelRoutersPanel,
});
const integrationsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "integrations",
  beforeLoad: requireAdmin,
  component: IntegrationsPanel,
});
// Kept for bookmarks: reviewed repos became the PR-review built-in's `repos`
// input (ADR 0119 phase 3.8). The panel itself retires with the enrollment
// RPCs in phase 4.7.
const reviewedReposRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "reviewed-repos",
  beforeLoad: requireAdmin,
  component: RedirectToBuiltin,
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
// Infrastructure and runtime config joined Settings when the Operator section
// retired. Papercuts stays reachable to members, as it was under /kaizen.
const fleetRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "fleet",
  beforeLoad: requireAdmin,
  component: Fleet,
});
const storageRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "storage",
  beforeLoad: requireAdmin,
  component: Storage,
});
const imagesRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "images",
  beforeLoad: requireAdmin,
  component: ImagesPanel,
});
const registriesRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "registries",
  beforeLoad: requireAdmin,
  component: RegistriesPanel,
});
const papercutsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "papercuts",
  component: Papercuts,
});

// /automations layout route (second sidebar) — a spine product with its own
// switcher rail. The whole section is admin-gated here, so the children don't
// each re-guard.
/** The editor shell owns the tab vocabulary (`isEditorTab`); these names are
 * kept as aliases so either spelling resolves to the one definition. */
export type AutomationEditorTab = EditorTab;
export interface AutomationEditorSearch {
  tab?: EditorTab;
  run?: string;
}
/** `?tab=` plus `?run=` (opens that entry's trace on the Activity tab). The
 * retired `runs` tab name still resolves, to Activity. */
const editorSearch = (search: Record<string, unknown>): AutomationEditorSearch => {
  const raw = search["tab"] === "runs" ? "activity" : search["tab"];
  return {
    ...(isEditorTab(raw) ? { tab: raw } : {}),
    ...(typeof search["run"] === "string" ? { run: search["run"] } : {}),
  };
};

const automationsLayoutRoute = createRoute({
  getParentRoute: () => appLayoutRoute,
  path: "/automations",
  beforeLoad: requireAdmin,
  component: AutomationsLayout,
});
const automationsIndexRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "/",
  component: AutomationsList,
});
/** Builder v2: "New automation" lands on the plain-English composer; the
 * blank editor lives at /new/manual (the "build by hand" escape hatch). */
const automationsNewRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "new",
  component: ComposePage,
});
const automationsNewManualRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "new/manual",
  component: () => <AutomationEditor mode="create" />,
});
/** The editor with the sibling tabs mounted. Tab components read `$id`
 * themselves. */
function AutomationEditorPage() {
  const { id } = automationEditRoute.useParams();
  return (
    <AutomationEditor
      mode="edit"
      activityTab={<ActivityTab automationId={id} />}
      settingsTab={<SettingsTab automationId={id} />}
    />
  );
}
// The section pages: every open workstream across automations, and every
// automation's activity in one ledger. Static segments, so they outrank `$id`.
const workstreamsRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "workstreams",
  component: WorkstreamsPage,
});
const workstreamRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "workstreams/$id",
  component: WorkstreamPage,
});
const activityRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "activity",
  component: ActivityPage,
});
const automationEditRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "$id",
  // ?tab=build|inputs|workstreams|activity|settings; anything else → build.
  validateSearch: editorSearch,
  component: AutomationEditorPage,
});
// Retired: the standalone run page. A run is an entry on the Activity tab,
// so the old link opens that entry's trace in place.
const automationRunRoute = createRoute({
  getParentRoute: () => automationsLayoutRoute,
  path: "$id/runs/$runId",
  beforeLoad: ({ params }) => {
    throw redirect({
      to: "/automations/$id",
      params: { id: params.id },
      search: { tab: "activity", run: params.runId },
      replace: true,
    });
  },
});

// Retired: automations under Settings. The `$id` redirects carry the tab.
const legacyAutomationsRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations",
  beforeLoad: () => {
    throw redirect({ to: "/automations", replace: true });
  },
});
const legacyAutomationsNewRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/new",
  beforeLoad: () => {
    throw redirect({ to: "/automations/new", replace: true });
  },
});
const legacyAutomationsNewManualRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/new/manual",
  beforeLoad: () => {
    throw redirect({ to: "/automations/new/manual", replace: true });
  },
});
const legacyAutomationEditRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/$id",
  validateSearch: editorSearch,
  beforeLoad: ({ params, search }) => {
    throw redirect({ to: "/automations/$id", params: { id: params.id }, search, replace: true });
  },
});
const legacyAutomationRunRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "automations/$id/runs/$runId",
  beforeLoad: ({ params }) => {
    throw redirect({
      to: "/automations/$id/runs/$runId",
      params: { id: params.id, runId: params.runId },
      replace: true,
    });
  },
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
    newSpecRoute,
    specDetailRoute,
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
    specsLayoutRoute.addChildren([specsIndexRoute, specTemplatesRoute]),
    automationsLayoutRoute.addChildren([
      automationsIndexRoute,
      automationsNewRoute,
      automationsNewManualRoute,
      workstreamsRoute,
      workstreamRoute,
      activityRoute,
      automationEditRoute,
      automationRunRoute,
    ]),
    ...legacyOperatorRoutes,
    ...legacyKaizenRoutes,
    settingsLayoutRoute.addChildren([
      settingsIndexRoute,
      profileRoute,
      tokensRoute,
      legacyTokensRoute,
      membersRoute,
      secretsRoute,
      apiKeysRoute,
      harnessesRoute,
      modelRoutersRoute,
      integrationsRoute,
      integrationDetailRoute,
      connectionSetupRoute,
      reviewedReposRoute,
      profilesRoute,
      profilesNewRoute,
      profileEditRoute,
      fleetRoute,
      storageRoute,
      imagesRoute,
      registriesRoute,
      papercutsRoute,
      legacyAutomationsRoute,
      legacyAutomationsNewRoute,
      legacyAutomationsNewManualRoute,
      legacyAutomationEditRoute,
      legacyAutomationRunRoute,
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
