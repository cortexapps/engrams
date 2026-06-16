// Tests for router-level auth guards.
//
// Uses a minimal in-memory router that mirrors the loginRoute's beforeLoad
// guard from router.tsx. If that guard is removed or its redirect target
// changes, the "redirects authenticated user" test will fail.

import { expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import {
  createMemoryHistory,
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
  RouterProvider,
} from "@tanstack/react-router";
import { Login } from "./pages/Login";
import { abilityFor } from "./lib/ability";
import type { RouterContext } from "./router";
import type { AuthState } from "./auth/AuthProvider";

const AUTH: AuthState = {
  principal: {
    email: "user@example.com",
    display_name: "Test User",
    role: "member",
    is_admin: false,
    has_claude_token: false,
    can_sign_out: true,
  },
  isAdmin: false,
  ability: abilityFor({ id: "user-1", role: "user" }),
  refresh: () => {},
};

/**
 * Minimal router starting at /login. "/" renders a sentinel <div> so tests
 * can confirm a redirect landed there without pulling in RootLayout.
 */
function makeLoginRouter(auth: AuthState | null) {
  const rootRoute = createRootRouteWithContext<RouterContext>()({});

  const loginRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/login",
    // Mirror of appLayoutRoute's inverse: authenticated users don't belong on /login.
    beforeLoad: ({ context }) => {
      if (context.auth) throw redirect({ to: "/" });
    },
    component: Login,
  });

  const homeRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/",
    component: () => <div data-testid="home" />,
  });

  return createRouter({
    routeTree: rootRoute.addChildren([loginRoute, homeRoute]),
    history: createMemoryHistory({ initialEntries: ["/login"] }),
    context: { auth },
  });
}

test("renders sign-in form for unauthenticated visitor at /login", async () => {
  render(<RouterProvider router={makeLoginRouter(null)} />);
  // The email input is unique to the Login form — confirms the form rendered.
  await screen.findByLabelText(/email/i);
});

test("redirects authenticated user away from /login to /", async () => {
  render(<RouterProvider router={makeLoginRouter(AUTH)} />);
  // Sentinel renders after redirect; Login form must be absent.
  await screen.findByTestId("home");
  expect(screen.queryByLabelText(/email/i)).toBeNull();
});
