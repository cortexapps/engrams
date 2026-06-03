# TanStack Router Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `react-router-dom` v7 with `@tanstack/react-router` in `engrams-web`, using code-based routing and `beforeLoad` admin guards, and update all component tests.

**Architecture:** A single `src/router.tsx` defines the route tree code-based (`createRootRoute`/`createRoute`) with a typed `RouterContext` carrying `auth: AuthState`. `AuthProvider` still resolves the principal and gates the app *above* the router, so `context.auth` is always populated when `beforeLoad` runs. Admin routes guard via a shared `requireAdmin` `beforeLoad` that throws `redirect()`. `RequireAdmin` wrapper is deleted. Tests render components inside a minimal memory-history TanStack router seeded with a test principal.

**Tech Stack:** React 19, Vite 6, TanStack Router (code-based), TanStack Query (unchanged), Vitest + testing-library/react, TypeScript (`verbatimModuleSyntax` + `isolatedModules` — use `import type` for type-only imports).

**Working directory:** all paths are relative to `web/` unless noted. Commands run from `web/`.

**Important sequencing note (read before starting):** A router swap is *atomic* — the moment `App.tsx` mounts `RouterProvider`, every component still importing from `react-router-dom` (any `Link`/`useLocation`/`useParams`/`Outlet`) throws for lack of a react-router context, and every test rendering such a component breaks. Likewise a component cannot import TanStack's `Link` while the app is still under `BrowserRouter`. Therefore:

- Task 1 (install) and Task 2 (write unused `router.tsx`) each leave the app fully green on the **old** router.
- Task 3 is the **atomic switch**: all component edits + `App.tsx` + `test-utils.tsx` land together, and the suite/build is only expected green at the *end* of Task 3, not between its edit steps.
- Task 4 removes the dead dependency. Task 5 is a manual smoke check.

Do not run `pnpm test` between the edit steps inside Task 3 expecting green — run it only at the Task 3 verify step.

---

## File map

| File | Action | Responsibility after migration |
|---|---|---|
| `package.json` / `pnpm-lock.yaml` | Modify | add `@tanstack/react-router`, remove `react-router-dom` |
| `src/router.tsx` | **Create** | route tree, `RouterContext`, `requireAdmin`, `createRouter`, module register |
| `src/App.tsx` | Rewrite | `QueryClientProvider > AuthProvider > InnerApp`; `InnerApp` mounts `RouterProvider` with `context={{ auth }}` |
| `src/auth/RequireAdmin.tsx` | **Delete** | (guard logic moves into `beforeLoad`) |
| `src/pages/Layout.tsx` | Modify | `Outlet` from TanStack |
| `src/pages/SessionDetail.tsx` | Modify | `useParams({ from: '/sessions/$id' })` |
| `src/pages/Sessions.tsx` | Modify | `useNavigate` → object form |
| `src/pages/Settings.tsx` | Modify | TanStack `Link` w/ `activeProps`/`inactiveProps`, absolute paths, `useLocation`/`Outlet` from TanStack |
| `src/components/NavSpine.tsx` | Modify | `Link` + `useRouterState` (pathname) + `useParams({ strict: false })` |
| `src/components/NewSessionForm.tsx` | Modify | `Link` import only |
| `src/components/SessionManifest.tsx` | Modify | `Link` dynamic `to`/`params` |
| `src/components/UserChip.tsx` | Modify | `Link` import only |
| `src/components/settings/ProfilePanel.tsx` | Modify | `Link` import only |
| `src/test-utils.tsx` | Rewrite | minimal memory-history TanStack router seeded with `auth` |

No test file call sites change (all render at `/`, none use `initialEntries`).

---

## Task 1: Add the TanStack Router dependency

**Files:**
- Modify: `package.json`, `pnpm-lock.yaml`

- [ ] **Step 1: Install the package**

Run (from `web/`):

```bash
pnpm add @tanstack/react-router
```

Expected: `package.json` `dependencies` gains `@tanstack/react-router` (a `^1.x` version). `react-router-dom` stays for now. If pnpm reports a store/version mismatch, resolve it (e.g. `pnpm install`) — no special handling assumed.

- [ ] **Step 2: Confirm the app is still green on the old router**

Run:

```bash
pnpm test && pnpm build
```

Expected: PASS / clean — nothing imports TanStack yet, so the existing react-router app is untouched.

- [ ] **Step 3: Commit**

```bash
git add package.json pnpm-lock.yaml
git commit -m "build(web): add @tanstack/react-router dependency"
```

---

## Task 2: Create the route tree (`src/router.tsx`)

This module compiles standalone and is **not yet imported** by anything, so the app stays green on the old router after this task.

**Files:**
- Create: `src/router.tsx`

- [ ] **Step 1: Write `src/router.tsx`**

```tsx
// Code-based TanStack Router tree for engrams-web. Mirrors the four-surface
// IA 1:1 (was src/App.tsx's <Routes>). Admin surfaces guard via a shared
// `requireAdmin` beforeLoad reading `isAdmin` from typed router context; the
// context's `auth` is populated at <RouterProvider/> time (see App.tsx), and
// AuthProvider gates rendering until the principal resolves, so `context.auth`
// is always present when beforeLoad runs. The coordinator's require_admin
// layer remains the real gate — these guards are UX-only.
import {
  createRootRoute,
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
```

- [ ] **Step 2: Type-check (the new module must compile)**

Run:

```bash
pnpm build
```

Expected: clean. If `tsc` complains that `Settings`/`Layout`/page components aren't exported with those names, fix the import to match the actual export (they are all named exports per current source). The router is unused at runtime, so `pnpm test` is unaffected.

- [ ] **Step 3: Commit**

```bash
git add src/router.tsx
git commit -m "feat(web): add code-based TanStack route tree (unwired)"
```

---

## Task 3: Atomic switch to TanStack Router

All edits in this task land together. Do **not** expect green tests/build until Step 13 (the verify step). Make every edit, then verify once, then commit once.

**Files:**
- Rewrite: `src/App.tsx`, `src/test-utils.tsx`
- Modify: `src/pages/Layout.tsx`, `src/pages/SessionDetail.tsx`, `src/pages/Sessions.tsx`, `src/pages/Settings.tsx`, `src/components/NavSpine.tsx`, `src/components/NewSessionForm.tsx`, `src/components/SessionManifest.tsx`, `src/components/UserChip.tsx`, `src/components/settings/ProfilePanel.tsx`
- Delete: `src/auth/RequireAdmin.tsx`

- [ ] **Step 1: Rewrite `src/App.tsx`**

Replace the entire file with:

```tsx
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { RouterProvider } from '@tanstack/react-router';
import { AuthProvider, useAuth } from './auth/AuthProvider';
import { router } from './router';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 0,
      refetchOnWindowFocus: false,
    },
  },
});

// AuthProvider resolves the principal (GET /me) and renders boot/not-member/
// error screens until it does — so by the time InnerApp mounts the router,
// `auth` is fully resolved and safe to hand to the router context that
// beforeLoad admin guards read.
function InnerApp() {
  const auth = useAuth();
  return <RouterProvider router={router} context={{ auth }} />;
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <InnerApp />
      </AuthProvider>
    </QueryClientProvider>
  );
}
```

- [ ] **Step 2: Delete `src/auth/RequireAdmin.tsx`**

```bash
git rm src/auth/RequireAdmin.tsx
```

(Its guard logic now lives in `requireAdmin` in `router.tsx`. Confirm nothing else imports it — Step 13 build will catch any stragglers; the only importer was `App.tsx`, now rewritten.)

- [ ] **Step 3: Edit `src/pages/Layout.tsx`**

Change the import line only:

```tsx
import { Outlet } from '@tanstack/react-router';
```

(Everything else in the file is unchanged.)

- [ ] **Step 4: Edit `src/pages/SessionDetail.tsx`**

Change the import (line 1) from:

```tsx
import { useParams } from 'react-router-dom';
```

to:

```tsx
import { useParams } from '@tanstack/react-router';
```

And change the params read (around line 23) from:

```tsx
  const { id } = useParams<{ id: string }>();
```

to:

```tsx
  const { id } = useParams({ from: '/sessions/$id' });
```

(`id` is now typed `string` — no longer `string | undefined`. `useSession(id)` accepts a string, so no further change.)

- [ ] **Step 5: Edit `src/pages/Sessions.tsx`**

Change the import (line 3) from:

```tsx
import { useNavigate } from 'react-router-dom';
```

to:

```tsx
import { useNavigate } from '@tanstack/react-router';
```

Change the token-nudge navigate (around line 111) from:

```tsx
            onClick={() => navigate('/settings/tokens')}
```

to:

```tsx
            onClick={() => navigate({ to: '/settings/tokens' })}
```

Change the post-create navigate (around line 127) from:

```tsx
              navigate(`/sessions/${id}`);
```

to:

```tsx
              navigate({ to: '/sessions/$id', params: { id } });
```

- [ ] **Step 6: Edit `src/components/NewSessionForm.tsx`**

Change the import (line 3) from:

```tsx
import { Link } from 'react-router-dom';
```

to:

```tsx
import { Link } from '@tanstack/react-router';
```

(The `<Link to="/settings/tokens">` static link at line ~262 needs no other change.)

- [ ] **Step 7: Edit `src/components/UserChip.tsx`**

Change the import (line 3) from:

```tsx
import { Link } from 'react-router-dom';
```

to:

```tsx
import { Link } from '@tanstack/react-router';
```

(The `<Link to="/settings/profile">` static link at line ~113 needs no other change.)

- [ ] **Step 8: Edit `src/components/settings/ProfilePanel.tsx`**

Change the import (line 6) from:

```tsx
import { Link } from 'react-router-dom';
```

to:

```tsx
import { Link } from '@tanstack/react-router';
```

(Both `<Link to="/settings/tokens">` static links need no other change.)

- [ ] **Step 9: Edit `src/components/SessionManifest.tsx`**

Change the import (line 2) from:

```tsx
import { Link } from 'react-router-dom';
```

to:

```tsx
import { Link } from '@tanstack/react-router';
```

Change the dynamic session link (around line 44) from:

```tsx
      <Link
        to={`/sessions/${session.id}`}
        className={`session-row${showOwner ? ' has-owner' : ''}`}
      >
```

to:

```tsx
      <Link
        to="/sessions/$id"
        params={{ id: session.id }}
        className={`session-row${showOwner ? ' has-owner' : ''}`}
      >
```

- [ ] **Step 10: Edit `src/components/NavSpine.tsx`**

Replace the import (line 1) from:

```tsx
import { Link, useLocation, useParams } from 'react-router-dom';
```

to:

```tsx
import { Link, useParams, useRouterState } from '@tanstack/react-router';
```

Change the `SURFACES` declaration so `to` is typed as a valid router path. Add a `LinkProps` type import to the existing TanStack import line:

```tsx
import { Link, useParams, useRouterState, type LinkProps } from '@tanstack/react-router';
```

Then change the `SURFACES` array type annotation so each `to` is a `LinkProps['to']`:

```tsx
const SURFACES: { to: LinkProps['to']; label: string; adminOnly: boolean; match: (p: string) => boolean }[] = [
  { to: '/', label: 'Sessions', adminOnly: false, match: (p: string) => p === '/' || p.startsWith('/sessions') },
  { to: '/fleet', label: 'Fleet', adminOnly: true, match: (p: string) => p.startsWith('/fleet') },
  { to: '/storage', label: 'Storage', adminOnly: true, match: (p: string) => p.startsWith('/storage') },
  { to: '/settings', label: 'Settings', adminOnly: false, match: (p: string) => p.startsWith('/settings') },
];
```

Replace the hook reads inside `NavSpine` (around lines 35-36) from:

```tsx
  const { pathname } = useLocation();
  const params = useParams();
```

to:

```tsx
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const params = useParams({ strict: false });
```

(`useParams({ strict: false })` returns partial params; `params.id` stays valid and optional, matching the existing `params.id` read. The `<Link to="/">`, the `<Link to={s.to}>` map, and the `aria-current={s.match(pathname) ? 'page' : undefined}` logic are otherwise unchanged.)

- [ ] **Step 11: Edit `src/pages/Settings.tsx`**

Replace the import (line 2) from:

```tsx
import { NavLink, Outlet, useLocation } from 'react-router-dom';
```

to:

```tsx
import { Link, Outlet, useLocation, type LinkProps } from '@tanstack/react-router';
```

Change the `Tab` interface and the two tab arrays so each tab carries an absolute `to` plus a `seg` used only for hint-keying. Replace the `interface Tab`, `YOU_TABS`, `DEPLOYMENT_TABS`, and `ALL_HINTS` blocks with:

```tsx
interface Tab {
  to: LinkProps['to'];
  seg: string;
  label: string;
  hint: string;
}

const YOU_TABS: Tab[] = [
  {
    to: '/settings/profile',
    seg: 'profile',
    label: 'Profile',
    hint: 'Your signed-in identity & what your role can do',
  },
  {
    to: '/settings/tokens',
    seg: 'tokens',
    label: 'Tokens',
    hint: 'Your service tokens — sealed & used automatically for every session',
  },
];

const DEPLOYMENT_TABS: Tab[] = [
  {
    to: '/settings/members',
    seg: 'members',
    label: 'Members',
    hint: 'Everyone in this deployment — roles & access',
  },
  {
    to: '/settings/images',
    seg: 'images',
    label: 'Images',
    hint: 'Curated OCI image URIs sessions may reference',
  },
  {
    to: '/settings/registries',
    seg: 'registries',
    label: 'Registries',
    hint: 'Docker registry credentials, sealed under the deployment KEK',
  },
];

const ALL_HINTS: Record<string, string> = {
  ...Object.fromEntries(YOU_TABS.map((t) => [t.seg, t.hint])),
  ...Object.fromEntries(DEPLOYMENT_TABS.map((t) => [t.seg, t.hint])),
};
```

Update the two `SettingsTab` call sites (in the `YOU_TABS.map` and `DEPLOYMENT_TABS.map`) to pass `to` from the tab:

```tsx
              {YOU_TABS.map((tab) => (
                <SettingsTab key={tab.seg} to={tab.to} label={tab.label} />
              ))}
```

```tsx
                {DEPLOYMENT_TABS.map((tab) => (
                  <SettingsTab key={tab.seg} to={tab.to} label={tab.label} />
                ))}
```

`useLocation()` from TanStack returns a location object with `.pathname`, so `const location = useLocation();` and `location.pathname.split('/').pop()` are unchanged. The `<Outlet />` is unchanged.

Replace the `SettingsTab` component at the bottom of the file with the TanStack `Link` form using `activeProps`/`inactiveProps`:

```tsx
function SettingsTab({ to, label }: { to: LinkProps['to']; label: string }) {
  return (
    <Link
      to={to}
      className="settings-tab"
      activeProps={{
        className: 'settings-tab tab-active',
        style: {
          color: 'var(--color-ink)',
          borderBottom: '1px solid var(--color-ink)',
        },
      }}
      inactiveProps={{
        className: 'settings-tab tab-inactive',
        style: {
          color: 'var(--color-ink-quiet)',
          borderBottom: '1px solid transparent',
        },
      }}
    >
      {label}
    </Link>
  );
}
```

- [ ] **Step 12: Rewrite `src/test-utils.tsx`**

Replace the entire file with a memory-history TanStack router. The root route renders the passed `ui`; a splat child route makes any `<Link to>` in the component-under-test resolve cleanly during `buildLocation` (so links to `/settings/tokens` etc. don't error even though we're not exercising those pages). The `auth` context mirrors the app's `RouterContext` so any guard logic behaves.

```tsx
// Render-with-providers helper for component tests. Each test gets its own
// QueryClient so React-Query cache state doesn't leak across tests.
//
// Components under test call TanStack Router's <Link> / useParams / useRouterState,
// which require a RouterProvider context. We build a throwaway memory-history
// router whose root route renders the test `ui`, plus a splat child so any
// <Link to="..."> the component renders resolves without erroring. The router
// context carries `auth` (mirrors src/router.tsx's RouterContext) seeded from a
// test principal, bypassing the /me query.

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import {
  createMemoryHistory,
  createRootRouteWithContext,
  createRoute,
  createRouter,
  RouterProvider,
} from '@tanstack/react-router';
import { render, type RenderOptions } from '@testing-library/react';
import { type ReactElement } from 'react';
import { AuthContextProvider, type AuthState } from './auth/AuthProvider';
import type { Principal } from './types';

interface TestRouterContext {
  auth: AuthState;
}

/** Default test principal: a local admin with a saved token, matching the
 * dev synthetic admin. Override via `renderWithProviders({ principal })`. */
const DEFAULT_PRINCIPAL: Principal = {
  email: 'dev@engram.local',
  display_name: 'Local Admin',
  role: 'admin',
  is_admin: true,
  has_claude_token: true,
  can_sign_out: true,
};

export interface RenderWithProvidersOptions extends Omit<RenderOptions, 'wrapper'> {
  /** Reuse a caller-supplied client (rare — for multi-step tests that need
   * cache continuity). Default: a fresh client per call. */
  queryClient?: QueryClient;
  /** Principal injected into the auth context (bypasses the /me query so tests
   * don't each need a fetch mock). Defaults to a local admin. */
  principal?: Principal;
}

export function renderWithProviders(
  ui: ReactElement,
  {
    queryClient,
    principal = DEFAULT_PRINCIPAL,
    ...renderOptions
  }: RenderWithProvidersOptions = {},
) {
  const client =
    queryClient ??
    new QueryClient({
      defaultOptions: {
        queries: { retry: false, gcTime: 0, staleTime: 0 },
        mutations: { retry: false },
      },
    });

  const authValue: AuthState = {
    principal,
    isAdmin: principal.is_admin,
    refresh: () => {},
  };

  const rootRoute = createRootRouteWithContext<TestRouterContext>()({
    // Wrap in AuthContextProvider + QueryClientProvider so components that read
    // those contexts (most of them) work, while the router supplies Link/params.
    component: () => (
      <QueryClientProvider client={client}>
        <AuthContextProvider value={authValue}>{ui}</AuthContextProvider>
      </QueryClientProvider>
    ),
  });

  // Splat child: any <Link to="..."> resolves to a real match during
  // buildLocation, so rendering links to app paths we aren't exercising
  // doesn't throw.
  const splatRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: '$',
    component: () => null,
  });

  const router = createRouter({
    routeTree: rootRoute.addChildren([splatRoute]),
    history: createMemoryHistory({ initialEntries: ['/'] }),
    context: { auth: authValue },
  });

  return {
    ...render(<RouterProvider router={router} />, renderOptions),
    queryClient: client,
  };
}
```

Note: `AuthState` must be exported from `src/auth/AuthProvider.tsx`. It already is (`export interface AuthState`). If a future edit un-exports it, re-export it.

- [ ] **Step 13: Verify the whole suite + build are green**

Run:

```bash
pnpm test
```

Expected: PASS — all 6 test files (`UserChip`, `NewSessionForm`, `TerminalPane`, `Transcript`, `settings/ImagesPanel`, `settings/RegistriesPanel`) pass unchanged.

Then run:

```bash
pnpm build
```

Expected: clean `tsc -b && vite build` — no type errors, no remaining `react-router-dom` import errors.

If a test fails because a TanStack `<Link to="...">` throws during render (the §5 risk), the splat route in Step 12 should already prevent it; if it does not, the fallback is to register the specific linked paths as stub child routes (`/settings/tokens`, `/settings/profile`, `/sessions/$id`) instead of (or in addition to) the splat. Apply that only if the splat proves insufficient.

- [ ] **Step 14: Commit**

```bash
git add -A
git commit -m "feat(web): migrate routing to TanStack Router (code-based, beforeLoad guards)"
```

---

## Task 4: Remove `react-router-dom`

**Files:**
- Modify: `package.json`, `pnpm-lock.yaml`

- [ ] **Step 1: Confirm zero remaining references**

Run:

```bash
grep -rn "react-router" src
```

Expected: no output. If any line prints, migrate that import (same patterns as Task 3) before continuing.

- [ ] **Step 2: Remove the dependency**

```bash
pnpm remove react-router-dom
```

- [ ] **Step 3: Verify still green**

```bash
pnpm test && pnpm build
```

Expected: PASS / clean.

- [ ] **Step 4: Commit**

```bash
git add package.json pnpm-lock.yaml
git commit -m "build(web): drop react-router-dom"
```

---

## Task 5: Manual smoke check

**Files:** none (runtime verification).

- [ ] **Step 1: Start the dev server**

Run:

```bash
pnpm dev
```

- [ ] **Step 2: Walk every route as an admin (dev synthetic admin)**

In the browser at `http://localhost:5173`:

- `/` — Sessions list renders; nav tabs show Sessions · Fleet · Storage · Settings; "Sessions" tab has the active underline.
- Click a session → `/sessions/<id>` — SessionDetail renders; nav sub-crumb shows the short id; "Sessions" tab still active.
- `/fleet` and `/storage` — render (admin); their tabs go active.
- `/settings` — redirects to `/settings/profile`; the Profile tab is active.
- `/settings/tokens`, `/settings/members`, `/settings/images`, `/settings/registries` — each renders; the matching settings sub-tab shows the active border.

Expected: all routes load; active styling matches pre-migration behavior.

- [ ] **Step 3: Verify the admin redirect for a non-admin**

Temporarily simulate a non-admin principal (e.g. point at a member account, or stub `is_admin: false`) and deep-link to `/fleet` and `/settings/members`.

Expected: both `beforeLoad` guards bounce to `/settings/profile`. Fleet/Storage tabs are hidden in `NavSpine` (admin-filtered), and the Deployment settings group is hidden.

- [ ] **Step 4: Confirm and finish**

No commit needed (no code change). If everything passes, the migration is complete. Revert any temporary non-admin stub used in Step 3.

---

## Self-review notes

- **Spec coverage:** deps add/remove (T1/T4), `router.tsx` w/ context + `requireAdmin` + register (T2), `App.tsx` `InnerApp`/`RouterProvider` + `RequireAdmin` delete (T3.1/T3.2), all component swaps incl. `:id`→`$id`, `useNavigate` object form, `useParams({from})`, `NavLink`→`activeProps`, `Outlet`/`useLocation` (T3.3–T3.11), `test-utils` rewrite (T3.12), verification (T3.13/T4/T5). All §-sections of the spec map to a task.
- **§5 open detail** (isolated test router + Link resolution) is addressed by the splat route in T3.12 with an explicit fallback in T3.13.
- **Type consistency:** `RouterContext.auth: AuthState` (router.tsx) and `TestRouterContext.auth: AuthState` (test-utils) both use the exported `AuthState`; `requireAdmin` reads `context.auth.isAdmin`; `LinkProps['to']` typing used consistently in NavSpine and Settings for `to` values passed to `<Link>`.
- **Atomicity** is called out up front so the executor doesn't expect green between Task 3's edit steps.
