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
