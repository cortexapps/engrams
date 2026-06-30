// Router guard regression tests.
//
// Uses the actual routeTree from router.tsx so that removing or changing
// loginRoute's beforeLoad will break these tests. The authenticated app
// shell components (RootLayout, SessionsLayout) are stubbed with Outlet
// passthroughs to avoid pulling in AuthProvider / Connect RPC hooks.

import { expect, test, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { abilityFor } from "./lib/ability";
import type { AuthState } from "./auth/AuthProvider";

// Stub the app shell so the route tree resolves without AuthProvider or RPCs.
// Outlet passthroughs let child routes (sessionsLayoutRoute → StartScreen, the
// /sessions index) render.
vi.mock("./pages/RootLayout", async () => {
  const { Outlet } = await import("@tanstack/react-router");
  return { RootLayout: () => <Outlet /> };
});
vi.mock("./pages/sessions/SessionsLayout", async () => {
  const { Outlet } = await import("@tanstack/react-router");
  return { SessionsLayout: () => <Outlet /> };
});
vi.mock("./pages/sessions/StartScreen", () => ({
  StartScreen: () => <div data-testid="sessions" />,
}));

import { routeTree } from "./router";

const AUTH: AuthState = {
  principal: {
    email: "user@example.com",
    display_name: "Test User",
    role: "member",
    is_admin: false,
    can_sign_out: true,
  },
  isAdmin: false,
  ability: abilityFor({ id: "user-1", role: "user" }),
};

function makeTestRouter(auth: AuthState | null) {
  return createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: ["/login"] }),
    context: { auth },
  });
}

test("renders sign-in form for unauthenticated visitor at /login", async () => {
  render(<RouterProvider router={makeTestRouter(null)} />);
  await screen.findByLabelText(/email/i);
});

test("redirects authenticated user away from /login to /sessions", async () => {
  render(<RouterProvider router={makeTestRouter(AUTH)} />);
  await screen.findByTestId("sessions");
  expect(screen.queryByLabelText(/email/i)).toBeNull();
});
