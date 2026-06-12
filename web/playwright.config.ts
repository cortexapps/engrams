import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  globalSetup: "./e2e/global-setup.ts",
  // Generous: the journey's sequential assertion budgets (30+60+90+30s)
  // must fit inside this, or a slow boot dies as a generic test-timeout.
  timeout: 240_000,
  retries: 0, // characterization: flake is signal, not noise
  // Serial: the suite drives ONE dev host-agent. Parallel workers make the
  // journey's CreateTask race auth-flow's session churn on the shared
  // single-host stack (bare 500s through the proxy) — the same class of
  // single-host contention that serialized the Rust grpc_smoke. Concurrent
  // multi-user behavior is a real follow-up question, but not this net's job.
  workers: 1,
  use: {
    baseURL: "http://localhost:5173",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    // ADR 0039 Task 22: load the better-auth session cookie so tests start
    // pre-authenticated. The state file is written by global-setup.ts and
    // gitignored (contains sensitive session data — never commit it).
    storageState: "e2e/.auth-state.json",
  },
});
