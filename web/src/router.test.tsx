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

// ADR 0118 regression (prod, 2026-08-16): every session-app link bounced the
// human to the dashboard. The preview edge 302s an unauthenticated navigation
// to `/login?next=<app url>`, but this gate fired first and threw
// `redirect({ to: "/" })`, discarding `next` — so Login's bounce-back, which is
// the only code that can validate a cross-origin destination, never ran.
// Falling through to the page is the fix, so assert the gate does NOT redirect.
test("keeps an authenticated /login?next= arrival on the page so it can bounce back", async () => {
  const router = makeTestRouter(
    AUTH,
    "/login?next=" + encodeURIComponent("https://tilt-azure-bold-quokkas.preview.example.com/"),
  );
  await router.load();
  expect(router.state.location.pathname).toBe("/login");
});

test("still short-circuits an authenticated /login with no next", async () => {
  const router = makeTestRouter(AUTH, "/login");
  await router.load();
  expect(router.state.location.pathname).not.toBe("/login");
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

test("renders a spec inside the app chrome", async () => {
  // The document surface used to render chromeless behind its own narrow
  // spine. The spine carried half the destinations the sidebar does, so a
  // person lost navigation to read a spec.
  render(<RouterProvider router={makeTestRouter(AUTH, "/specs/spec-1")} />);
  await screen.findByTestId("spec-shell");
  expect(screen.getByTestId("app-sidebar")).toBeTruthy();
});

test("renders new-spec creation inside the app chrome", async () => {
  render(<RouterProvider router={makeTestRouter(AUTH, "/specs/new")} />);
  await screen.findByTestId("new-spec");
  expect(screen.getByTestId("app-sidebar")).toBeTruthy();
});

test.each(["/specs/spec-1", "/specs/new"])("requires auth for %s", async (path) => {
  const router = makeTestRouter(null, path);
  await router.load();
  expect(router.state.location.pathname).toBe("/login");
});
