// Render-with-providers helper for component tests. Each test gets
// its own QueryClient so React-Query cache state doesn't leak across
// tests (default `gcTime` would otherwise keep entries warm long
// enough to interfere with the next test).
//
// MemoryRouter is the default because the components we test call
// useLocation / Link / NavLink — without a router context they
// throw at render time.

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render, type RenderOptions } from '@testing-library/react';
import { type ReactElement, type ReactNode } from 'react';
import { MemoryRouter, type MemoryRouterProps } from 'react-router-dom';

export interface RenderWithProvidersOptions extends Omit<RenderOptions, 'wrapper'> {
  /** Initial URL stack for the router. Defaults to ["/"]. */
  initialEntries?: MemoryRouterProps['initialEntries'];
  /** Reuse a caller-supplied client (rare — for multi-step tests
   * that need cache continuity). Default: a fresh client per call. */
  queryClient?: QueryClient;
}

export function renderWithProviders(
  ui: ReactElement,
  { initialEntries = ['/'], queryClient, ...renderOptions }: RenderWithProvidersOptions = {},
) {
  // Disable retries + caching in tests. Real failures should fail
  // tests immediately, not retry-storm.
  const client =
    queryClient ??
    new QueryClient({
      defaultOptions: {
        queries: { retry: false, gcTime: 0, staleTime: 0 },
        mutations: { retry: false },
      },
    });

  function Wrapper({ children }: { children: ReactNode }) {
    return (
      <QueryClientProvider client={client}>
        <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
      </QueryClientProvider>
    );
  }

  return { ...render(ui, { wrapper: Wrapper, ...renderOptions }), queryClient: client };
}
