import { test } from "@playwright/test";

// Screenshot harness for the visual validation protocol (plan header).
//   SNAP_PATHS="/,/sessions/abc?tab=raw" pnpm exec playwright test snap
// Writes web/e2e/__shots__/<slug>.png; an agent then READS the images and
// compares against e2e/__shots__/baseline/.
//
// Affordances beyond plain goto (SPA state isn't all URL-addressable):
//   ?tab=<id>   after load, click data-testid="tab-<id>" (session detail tabs)
//   #new        after load, click data-testid="new-session" (open the dialog)
//
// Self-skips when SNAP_PATHS is unset so `just e2e` (the characterization
// net) never runs the harness; `just snap` always sets it.
//
//   SNAP_STATE=<path>  override the suite's storageState (e.g. a member's
//   auth state for the member-baseline captures — globalSetup re-writes the
//   default admin state on every run, so a file swap can't work).
const raw = process.env.SNAP_PATHS;
const paths = (raw ?? "/").split(",").map((p) => p.trim());

const stateOverride = process.env.SNAP_STATE;
if (stateOverride) test.use({ storageState: stateOverride });

for (const p of paths) {
  const slug = p === "/" ? "root" : p.replace(/[^a-z0-9]+/gi, "-").replace(/^-|-$/g, "");
  test(`snap ${p}`, async ({ page }) => {
    test.skip(!raw, "SNAP_PATHS unset — snap harness only runs via `just snap`");
    const tab = /[?&]tab=([a-z0-9_-]+)/i.exec(p)?.[1];
    const openNew = p.includes("#new");
    await page.goto(p);
    await page.waitForLoadState("networkidle");
    if (openNew) {
      await page.getByTestId("new-session").click();
      // Wait on the image picker, not start-session: when images[0] is a
      // Claude-harness image and the user has no saved token, the submit
      // button is replaced by a save-token link (by design).
      await page.getByTestId("image-select").waitFor({ timeout: 15_000 });
    }
    if (tab) {
      await page.getByTestId(`tab-${tab}`).click();
      // The shell tab boots a WASM terminal over a fresh WebSocket — give it
      // a few seconds to render real pixels before the shot.
      await page.waitForLoadState("networkidle");
      if (tab === "shell") await page.waitForTimeout(4_000);
    }
    // Settle: SSE/polled data (event counts, COW state) lands after
    // networkidle, and radix dialogs fade in — don't baseline a half-render.
    await page.waitForTimeout(1_000);
    await page.screenshot({ path: `e2e/__shots__/${slug}.png`, fullPage: true });
  });
}
