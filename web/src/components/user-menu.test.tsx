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

test("shows who is signed in, with Settings, the theme and sign-out when opened", async () => {
  renderMenu();
  // The row itself carries the role and the workspace domain.
  expect(await screen.findByText("admin · engram.local")).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: /local admin/i }));
  // The email renders in the open dropdown label.
  expect((await screen.findAllByText("dev@engram.local")).length).toBeGreaterThan(0);
  // Settings lives under the monogram, with the gear, above the theme and
  // sign out — the menu is about this person.
  expect(screen.getByRole("menuitem", { name: /settings/i }).getAttribute("href")).toBe(
    "/settings",
  );
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
