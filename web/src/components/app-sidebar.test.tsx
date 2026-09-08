import { expect, test } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { SidebarProvider } from "@/components/ui/sidebar";
import { renderWithProviders } from "../test-utils";
import { ThemeProvider } from "./theme-provider";
import { MainSidebar } from "./app-sidebar";

test("admin sees the four products, and Settings as a row", async () => {
  renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <MainSidebar />
      </SidebarProvider>
    </ThemeProvider>,
  );
  // Router defers the initial render to a microtask — await the first match.
  // Exact-string names target the destination links (not the logo link, whose
  // accessible name also contains "tasks").
  expect(await screen.findByRole("link", { name: "Tasks" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Reviews" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Artifacts" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Automations" })).toBeTruthy();
  // Settings is a spine destination at the foot, not an avatar-menu item.
  expect(screen.getByRole("link", { name: "Settings" })).toBeTruthy();
  // The Operator and Kaizen hats retired into Settings.
  expect(screen.queryByRole("link", { name: "Operator" })).toBeNull();
  expect(screen.queryByRole("link", { name: "Kaizen" })).toBeNull();
  // Tech Specs is unreleased: out of the spine for admins too, not just members.
  expect(screen.queryByRole("link", { name: "Tech Specs" })).toBeNull();
});

test("member sees shared products but not Automations", async () => {
  renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <MainSidebar />
      </SidebarProvider>
    </ThemeProvider>,
    {
      principal: {
        email: "m@e.local",
        display_name: "Mem",
        role: "member",
        is_admin: false,
        can_sign_out: false,
      },
    },
  );
  expect(await screen.findByRole("link", { name: "Tasks" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Artifacts" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Settings" })).toBeTruthy();
  expect(screen.queryByRole("link", { name: "Automations" })).toBeNull();
  expect(screen.queryByRole("link", { name: "Tech Specs" })).toBeNull();
});

test("the spine has a visible toggle that folds it to icons and back", async () => {
  renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <MainSidebar />
      </SidebarProvider>
    </ThemeProvider>,
  );
  await screen.findByRole("link", { name: "Tasks" });
  const sidebar = document.querySelector('[data-slot="sidebar"][data-state]')!;
  expect(sidebar.getAttribute("data-state")).toBe("expanded");

  await userEvent.click(screen.getByRole("button", { name: "Collapse the sidebar" }));
  expect(sidebar.getAttribute("data-state")).toBe("collapsed");

  await userEvent.click(screen.getByRole("button", { name: "Expand the sidebar" }));
  expect(sidebar.getAttribute("data-state")).toBe("expanded");
});
