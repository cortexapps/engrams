import { expect, test } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { SidebarProvider } from "@/components/ui/sidebar";
import { renderWithProviders } from "../test-utils";
import { ThemeProvider } from "./theme-provider";
import { UserMenu } from "./user-menu";

function renderMenu() {
  return renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <UserMenu />
      </SidebarProvider>
    </ThemeProvider>,
  );
}

test("shows who is signed in, with the theme and sign-out actions when opened", async () => {
  renderMenu();
  // The row itself carries the role and the workspace domain.
  expect(await screen.findByText("admin · engram.local")).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: /local admin/i }));
  // The email renders in the open dropdown label.
  expect((await screen.findAllByText("dev@engram.local")).length).toBeGreaterThan(0);
  // Settings is a spine destination now, not a menu item; the menu is about
  // this person's session — theme + sign out.
  expect(screen.queryByRole("menuitem", { name: /settings/i })).toBeNull();
  expect(screen.getByRole("menuitem", { name: /dark theme/i })).toBeTruthy();
  expect(screen.getByRole("menuitem", { name: /sign out/i })).toBeTruthy();
});

test("the theme item toggles the dark class on the document root", async () => {
  document.documentElement.classList.remove("dark");
  renderMenu();
  await userEvent.click(await screen.findByRole("button", { name: /local admin/i }));
  expect(document.documentElement.classList.contains("dark")).toBe(false);
  await userEvent.click(screen.getByRole("menuitem", { name: /dark theme/i }));
  expect(document.documentElement.classList.contains("dark")).toBe(true);
});
