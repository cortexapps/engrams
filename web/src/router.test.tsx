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
  return {
    RootLayout: () => (
      <>
        <nav data-testid="app-sidebar" />
        <Outlet />
      </>
    ),
  };
});
vi.mock("./pages/sessions/SessionsLayout", async () => {
  const { Outlet } = await import("@tanstack/react-router");
  return { SessionsLayout: () => <Outlet /> };
});
vi.mock("./pages/sessions/StartScreen", () => ({
  StartScreen: () => <div data-testid="sessions" />,
}));
vi.mock("./pages/specs/SpecsLayout", async () => {
  const { Outlet } = await import("@tanstack/react-router");
  return { SpecsLayout: () => <Outlet /> };
});
vi.mock("./pages/specs/SpecsList", () => ({
  SpecsList: () => <div data-testid="tech-specs" />,
}));
vi.mock("./pages/specs/SpecTemplates", () => ({
  SpecTemplates: () => <div data-testid="spec-templates" />,
}));
vi.mock("./pages/specmode/SpecShellPage", () => ({
  SpecShellPage: () => <div data-testid="spec-shell" />,
}));
vi.mock("./pages/specmode/NewSpecPage", () => ({
  NewSpecPage: () => <div data-testid="new-spec" />,
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

const ADMIN_AUTH: AuthState = {
  principal: {
    email: "admin@example.com",
    display_name: "Test Admin",
    role: "admin",
    is_admin: true,
    can_sign_out: true,
  },
  isAdmin: true,
  ability: abilityFor({ id: "admin-1", role: "admin" }),
};

function makeTestRouter(auth: AuthState | null, path = "/login") {
  return createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: [path] }),
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

test("routes an authenticated member to the Tech Specs list", async () => {
  render(<RouterProvider router={makeTestRouter(AUTH, "/specs")} />);
  await screen.findByTestId("tech-specs");
  expect(screen.getByTestId("app-sidebar")).toBeTruthy();
});

test("routes an admin to the Tech Specs list", async () => {
  render(<RouterProvider router={makeTestRouter(ADMIN_AUTH, "/specs")} />);
  await screen.findByTestId("tech-specs");
});

test("routes an admin to the Templates catalog", async () => {
  render(<RouterProvider router={makeTestRouter(ADMIN_AUTH, "/specs/templates")} />);
  await screen.findByTestId("spec-templates");
});

test("renders a spec in its chromeless full-window route", async () => {
  render(<RouterProvider router={makeTestRouter(AUTH, "/specs/spec-1")} />);
  await screen.findByTestId("spec-shell");
  expect(screen.queryByTestId("app-sidebar")).toBeNull();
});

test("renders new-spec creation outside the app chrome", async () => {
  render(<RouterProvider router={makeTestRouter(AUTH, "/specs/new")} />);
  await screen.findByTestId("new-spec");
  expect(screen.queryByTestId("app-sidebar")).toBeNull();
});

test.each(["/specs/spec-1", "/specs/new"])("requires auth for %s", async (path) => {
  const router = makeTestRouter(null, path);
  await router.load();
  expect(router.state.location.pathname).toBe("/login");
});
