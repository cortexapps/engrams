// Signed-out characterisation suite — the auth flows the journey spec cannot
// cover because it always runs pre-authenticated via storageState.
//
// These tests drop the session cookie entirely (storageState: empty) so they
// start as a genuinely anonymous browser. They verify:
//   (a) Any URL signed-out → redirected to /login (no reload loop).
//   (b) Sign in via the form → lands in the app, session list visible.
//   (c) Sign out via the user menu → back at /login, stable (no loop).
//
// The spec keeps its own storageState so it doesn't inherit the global one
// from playwright.config.ts. It imports E2E_EMAIL / E2E_PW from global-setup
// so the credentials are a single source of truth.

import { test, expect } from "@playwright/test";
import { E2E_EMAIL, E2E_PW } from "./global-setup";

// Run every test in this file signed-out (no cookies, no origins).
test.use({ storageState: { cookies: [], origins: [] } });

test("(a) signed-out / → /login — email input visible, no reload loop", async ({ page }) => {
  // Count navigations to detect a redirect loop (more than 1 redirect = loop).
  let navCount = 0;
  page.on("framenavigated", (frame) => {
    if (frame === page.mainFrame()) navCount++;
  });

  await page.goto("/");

  // Should land on /login without a loop.
  await expect(page).toHaveURL(/\/login/, { timeout: 5_000 });
  await expect(page.getByLabel("Email")).toBeVisible({ timeout: 5_000 });

  // Wait 2s and confirm the login form is stable (no further redirects).
  await page.waitForTimeout(2_000);
  await expect(page.getByLabel("Email")).toBeVisible();

  // Sanity-check: the navigation count should be small — one load + at most
  // one SPA redirect (/ → /login), not an endless loop.
  expect(navCount).toBeLessThan(5);
});

test("(b) sign-in via form → app sessions list visible", async ({ page }) => {
  await page.goto("/login");
  await expect(page.getByLabel("Email")).toBeVisible({ timeout: 5_000 });

  // Fill and submit the sign-in form using the e2e credentials.
  await page.getByLabel("Email").fill(E2E_EMAIL);
  await page.getByLabel("Password").fill(E2E_PW);
  await page.getByRole("button", { name: /sign in/i }).click();

  // After sign-in the hard-navigate lands on / → /sessions.
  await expect(page).toHaveURL(/\/sessions/, { timeout: 10_000 });

  // The sessions list (or at least the new-session button) must be visible —
  // confirms we're inside the authenticated app shell.
  await expect(page.getByTestId("new-session")).toBeVisible({ timeout: 10_000 });
});

test("(c) sign-out → back at /login, stable (no loop)", async ({ page }) => {
  // Start signed-out → sign in first so we can then sign out.
  await page.goto("/login");
  await expect(page.getByLabel("Email")).toBeVisible({ timeout: 5_000 });
  await page.getByLabel("Email").fill(E2E_EMAIL);
  await page.getByLabel("Password").fill(E2E_PW);
  await page.getByRole("button", { name: /sign in/i }).click();
  await expect(page).toHaveURL(/\/sessions/, { timeout: 10_000 });

  // Open the user menu and click sign out.
  // The user menu is triggered by the user avatar/chip in the sidebar.
  await page.getByTestId("user-menu-trigger").click();
  await page.getByRole("menuitem", { name: /sign out/i }).click();

  // Should land on /login, stable.
  await expect(page).toHaveURL(/\/login/, { timeout: 5_000 });
  await expect(page.getByLabel("Email")).toBeVisible({ timeout: 5_000 });

  // 2s stability check — no further redirects.
  await page.waitForTimeout(2_000);
  await expect(page.getByLabel("Email")).toBeVisible();
});
