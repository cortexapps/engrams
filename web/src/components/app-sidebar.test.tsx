import { expect, test } from "vitest";
import { screen } from "@testing-library/react";
import { SidebarProvider } from "@/components/ui/sidebar";
import { renderWithProviders } from "../test-utils";
import { ThemeProvider } from "./theme-provider";
import { MainSidebar } from "./app-sidebar";

test("admin sees the two hats: Tasks and Operator", async () => {
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
  expect(screen.getByRole("link", { name: "Artifacts" })).toBeTruthy();
  expect(screen.getByRole("link", { name: "Operator" })).toBeTruthy();
  // Settings is not a rail destination; it lives in the avatar menu.
  expect(screen.queryByRole("link", { name: "Settings" })).toBeNull();
});

test("member sees only Tasks in the rail", async () => {
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
  expect(screen.queryByRole("link", { name: "Operator" })).toBeNull();
});
