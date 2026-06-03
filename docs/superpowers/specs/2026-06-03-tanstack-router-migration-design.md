# Design: Migrate engrams-web from react-router-dom to TanStack Router

**Date:** 2026-06-03
**Scope:** `web/` (engrams-web SPA)
**Status:** Approved for planning

## Goal

Replace `react-router-dom` v7 with `@tanstack/react-router`, using **code-based**
routing and **`beforeLoad` route guards** for admin gating. Update all component
tests (testing-library/react + vitest) to work with the new router.

## Decisions (confirmed)

1. **Routing style:** code-based (`createRootRoute` / `createRoute`), not file-based.
   No `@tanstack/router-plugin` / generated route tree.
2. **Auth guards:** idiomatic `beforeLoad` guards that `throw redirect(...)`, reading
   `isAdmin` from typed router context. `RequireAdmin` wrapper component is deleted.

## Current state

react-router-dom v7. Route tree (in `src/App.tsx`):

- `/` → `Sessions` (inside `Layout`)
- `/sessions/:id` → `SessionDetail`
- `/fleet` → `Fleet` (admin)
- `/storage` → `Storage` (admin)
- `/settings` → `Settings` (renders `<Outlet/>`)
  - index → `<Navigate to="profile" replace/>`
  - `profile` → `ProfilePanel`
  - `tokens` → `TokensPanel`
  - `members` → `Members` (admin)
  - `images` → `ImagesPanel` (admin)
  - `registries` → `RegistriesPanel` (admin)

Router API usage:

- `Link`: NavSpine, NewSessionForm, SessionManifest, UserChip, ProfilePanel
- `NavLink`: Settings
- `Outlet`: Settings, Layout
- `Navigate`: App (settings index), RequireAdmin
- `useNavigate`: Sessions
- `useParams`: NavSpine, SessionDetail
- `useLocation`: NavSpine, Settings
- `MemoryRouter`: test-utils

Auth model: `AuthProvider` owns the `GET /me` query and gates the entire app —
children render only once the principal resolves (boot / not-member / error screens
otherwise). `useIsAdmin()` reads from context. `RequireAdmin` is a UX-only render
guard; the coordinator's `require_admin` layer is the real gate.

Tests: 6 `*.test.tsx` files. **None use `initialEntries`** — every
`renderWithProviders` call renders at `/`, and the tested components
(UserChip, NewSessionForm, ImagesPanel, RegistriesPanel, Transcript) only need
`<Link>` to have a router context. No tested component uses `useParams`/`useLocation`.

## Target design

### 1. Dependencies

- Add `@tanstack/react-router`.
- Add `@tanstack/react-router-devtools` (dev dep, optional — gated behind dev).
- Remove `react-router-dom`.
- No Vite plugin (code-based).

Note: `tsconfig` uses `verbatimModuleSyntax` + `isolatedModules` — type-only imports
must use `import type`.

### 2. `src/router.tsx` (new)

Defines the route tree code-based, mirroring the current tree 1:1, plus a typed
context carrying auth:

```ts
interface RouterContext { auth: AuthState }

const rootRoute = createRootRoute({ component: Layout })

const requireAdmin = ({ context }: { context: RouterContext }) => {
  if (!context.auth.isAdmin) throw redirect({ to: '/settings/profile' })
}

// children: sessionsRoute('/'), sessionDetailRoute('/sessions/$id'),
//   fleetRoute('/fleet', beforeLoad: requireAdmin),
//   storageRoute('/storage', beforeLoad: requireAdmin),
//   settingsRoute('/settings', component: Settings) with children:
//     settingsIndexRoute (beforeLoad: throw redirect to '/settings/profile'),
//     profile, tokens,
//     members/images/registries (beforeLoad: requireAdmin)

export const router = createRouter({
  routeTree,
  context: { auth: undefined! as AuthState }, // populated at <RouterProvider/>
})

declare module '@tanstack/react-router' {
  interface Register { router: typeof router }
}
```

- `:id` → `$id`.
- Settings index redirect handled by an index route with `beforeLoad` throwing
  `redirect({ to: '/settings/profile' })`.
- The five admin routes share `requireAdmin`.

### 3. `src/App.tsx` (rewrite)

`AuthProvider` still gates the app and resolves the principal **before** the router
renders, so `context.auth` is always populated when `beforeLoad` runs.

```tsx
function InnerApp() {
  const auth = useAuth()
  return <RouterProvider router={router} context={{ auth }} />
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <InnerApp />
      </AuthProvider>
    </QueryClientProvider>
  )
}
```

`src/auth/RequireAdmin.tsx` is **deleted**.

### 4. Component API swaps

| File | Change |
|---|---|
| NavSpine | `Link`, active state from TanStack; replace `useLocation`/`useParams`-based active detection with `Link` active props / `useRouterState` as needed |
| NewSessionForm | `Link` import → `@tanstack/react-router` |
| SessionManifest | `Link` import → `@tanstack/react-router` (dynamic `to`+`params`) |
| UserChip | `Link` import → `@tanstack/react-router` |
| ProfilePanel | `Link` import → `@tanstack/react-router` |
| Sessions | `useNavigate` → `navigate({ to: '/sessions/$id', params: { id } })` |
| SessionDetail | `useParams<{id}>()` → `useParams({ from: '/sessions/$id' })` |
| Settings | `NavLink` → TanStack `Link` with `activeProps` / `data-status` for active styling; `Outlet` + `useLocation` from TanStack |
| Layout | `Outlet` from TanStack |

Dynamic links use `to`/`params` form: `<Link to="/sessions/$id" params={{ id }}>`.
Static links keep string `to`.

Trickiest: `NavLink` active styling in Settings and the `useLocation`+`useParams`
active detection in NavSpine. Convert both to TanStack's built-in active state.

### 5. Tests — `src/test-utils.tsx` (rewrite)

`renderWithProviders(ui, { principal, queryClient })` builds a minimal
memory-history TanStack router whose root route renders the passed `ui`, seeds
`context.auth` from the test principal, and renders `<RouterProvider/>`:

```ts
const rootRoute = createRootRoute({ component: () => ui })
const router = createRouter({
  routeTree: rootRoute,
  history: createMemoryHistory({ initialEntries: ['/'] }),
  context: { auth: authValue },
})
render(<RouterProvider router={router} />, ...)
```

- Public signature unchanged except the now-unused `initialEntries` option is dropped.
- `principal` injection and per-call fresh `QueryClient` preserved.
- All 6 test files keep working unchanged at their call sites.

Open detail to resolve in implementation: TanStack `<Link to="/settings/tokens">`
inside an isolated test router (whose route tree doesn't register that path) must
still render an anchor at runtime. If it warns/throws, fall back to a small stub
route tree that registers the handful of linked paths. Verify during the test step.

### 6. Verification

- `pnpm test` (vitest) green.
- `pnpm build` (`tsc -b && vite build`) clean — no type errors from the router
  registration / typed `to`.
- Manual `pnpm dev` smoke: each route loads; admin redirect works for a non-admin
  principal; settings index redirects to profile; `/sessions/:id` deep link resolves.

## Out of scope

- File-based routing / route-tree codegen.
- Loader-based data fetching (React Query stays as-is inside components).
- Search-param typing beyond what existing routes need (none use search params today).
- Any unrelated refactor of pages/components.

## Risks

- `beforeLoad` reads `context.auth`; context is only populated once `AuthProvider`
  resolves the principal. Since `AuthProvider` gates rendering of `InnerApp`, this is
  safe — but the router-context wiring must not render `<RouterProvider/>` before auth
  resolves. Covered by keeping `AuthProvider` above `InnerApp`.
- Test router for isolated components (see §5 open detail).
- NavSpine / Settings active-state styling parity — must visually match current behavior.
