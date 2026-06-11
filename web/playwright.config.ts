import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  globalSetup: "./e2e/global-setup.ts",
  // Generous: the journey's sequential assertion budgets (30+60+90+30s)
  // must fit inside this, or a slow boot dies as a generic test-timeout.
  timeout: 240_000,
  retries: 0, // characterization: flake is signal, not noise
  use: {
    baseURL: "http://localhost:5173",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },
});
