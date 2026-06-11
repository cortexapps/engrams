import { test, expect } from "@playwright/test";

// The single linear journey from ADR 0039 "Preparation". Black-box at the
// browser: these assertions must hold identically before and after the
// orchestration-tier migration.
test("create a session and watch it come alive", async ({ page }) => {
  await page.goto("/"); // no login (SyntheticAdmin today); redirects to /sessions
  await page.getByTestId("new-session").click();

  // Explicitly select the no-harness demo image — images[0] is auto-selected
  // otherwise and may be a Claude image (token-gated submit) on dev boxes.
  // The picker is a radix Select (shadcn), not a native <select>: open the
  // trigger, then click the option. The demo image lives under
  // integration-test/ ("demo" alone also matches the Claude demo image).
  await page.getByTestId("image-select").click();
  await page
    .getByRole("option", { name: /integration/ })
    .first()
    .click();
  // Pin that we really picked the harness-less image (the token-gated Link
  // never appears for it): the dialog echoes the image's harness state.
  await expect(page.getByText(/no baked harness/i)).toBeVisible();
  await page.getByTestId("start-session").click();

  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+/, { timeout: 30_000 });
  const id = page.url().split("/sessions/")[1];

  // The stream is live: the event count climbs above zero.
  await expect
    .poll(async () => Number(await page.getByTestId("event-count").textContent()), {
      timeout: 60_000,
    })
    .toBeGreaterThan(0);

  // Status advances toward active.
  await expect(page.getByTestId("session-status")).toHaveText(/active|running/i, {
    timeout: 90_000,
  });

  // RAW tab renders event rows.
  await page.getByTestId("tab-raw").click();
  await expect(page.getByTestId("event-row").first()).toBeVisible();

  // SHELL tab's WebSocket connects. Predicate-filter (vite HMR also opens a
  // websocket); 'websocket' fires on creation, so also await a received
  // frame (ttyd handshakes promptly) to pin "connected".
  const wsPromise = page.waitForEvent("websocket", {
    predicate: (ws) => ws.url().includes("/shell"),
    timeout: 30_000,
  });
  await page.getByTestId("tab-shell").click();
  const ws = await wsPromise;
  await ws.waitForEvent("framereceived", { timeout: 15_000 });

  // The new row appears in the list. (Post-migration this list shows TASKS —
  // the testid survives the noun change; assert on testid, never copy.)
  await page.goto("/");
  await expect(page.getByTestId("session-row").filter({ hasText: id.slice(0, 8) })).toBeVisible();
});
