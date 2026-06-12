// Preconditions for the e2e characterization net (ADR 0039 "Preparation").
// 1. web dev server up  2. control plane healthy  3. a no-harness demo
// image enabled. We deliberately do NOT auto-bake one (multi-minute build).
//
// ADR 0039 Task 22: auth entry moves to better-auth on the orchestrator.
// After the precondition gate, this setup:
//   a. Signs up the e2e user (idempotent — 422 on duplicate is swallowed).
//   b. Promotes the user to admin via psql so admin-gated surfaces are reachable.
//      (Promote BEFORE sign-in so the role is stamped on the initial session.)
//   c. Signs in via /api/auth/sign-in/email to acquire the HttpOnly session cookie.
//   d. Persists the storage state (cookie jar) to e2e/.auth-state.json so every
//      test page.goto() starts pre-authenticated.

import { request } from "@playwright/test";

const WEB = "http://localhost:5173";
const COORD = process.env.ENGRAM_COORDINATOR_URL ?? "http://127.0.0.1:8090";

// e2e test credentials — stable constants.
// Change these if the orchestrator DB is reset.
export const E2E_EMAIL = "e2e@engram.local";
export const E2E_PW = "e2e-playwright-pw";

async function mustFetch(url: string, fixit: string): Promise<Response> {
  let res: Response;
  try {
    res = await fetch(url);
  } catch {
    throw new Error(`${url} not reachable — ${fixit}`);
  }
  if (!res.ok) throw new Error(`${url} returned ${res.status} — ${fixit}`);
  return res;
}

/** Promote the e2e user to admin directly in the orchestrator DB.
 *  Uses docker compose psql (the Task 16 recipe). Idempotent — the UPDATE
 *  is safe to repeat. Must run BEFORE sign-in so the role is baked into
 *  the initial session.
 *
 *  Uses a dynamic import of child_process to avoid the tsconfig needing
 *  full @types/node (which would leak Node globals into src/).  */
async function promoteE2eUserToAdmin(): Promise<void> {
  try {
    // Dynamic import avoids the ambient @types/node requirement across all files.
    const { execSync } = await import("child_process");
    execSync(
      [
        "docker compose",
        "-f deploy/docker-compose.dev.yml",
        "exec -T postgres",
        "psql -U engram -d engram_orchestrator",
        `-c "UPDATE \\"user\\" SET role='admin' WHERE email='${E2E_EMAIL}'"`,
      ].join(" "),
      { stdio: "pipe" },
    );
  } catch (err) {
    // Non-fatal: if docker isn't up or the user row doesn't exist yet,
    // log a warning but don't abort — the sign-up above may have raced.
    console.warn(`[global-setup] Could not promote e2e user to admin: ${String(err)}`);
  }
}

export default async function globalSetup() {
  // ---- Precondition gate --------------------------------------------------
  await mustFetch(`${WEB}/`, "run `just dev` first");
  await mustFetch(
    `${COORD}/healthz`,
    "coordinator down/unhealthy — check Tilt (http://localhost:10350)",
  );
  const res = await mustFetch(`${WEB}/api/v1/enabled-images`, "run `just dev` first");
  // Shape: ListEnabledImagesResponse (web/src/types.ts) — { images: [...] }
  const body = (await res.json()) as {
    images?: { image_uri: string; harness_name: string | null }[];
  };
  if (!body.images?.some((i) => i.harness_name === null)) {
    throw new Error("no NO-HARNESS image enabled — run `just integration-session` once");
  }

  // ---- better-auth entry (ADR 0039 §5 / Task 22) --------------------------
  // Use a Playwright request context so the cookie jar is handled for us.
  const ctx = await request.newContext({ baseURL: WEB });

  // Step a: sign up (idempotent — swallow 422 duplicate-email error).
  await ctx
    .post("/api/auth/sign-up/email", {
      data: { email: E2E_EMAIL, password: E2E_PW, name: "e2e" },
    })
    .catch(() => {
      // duplicate-email → 422; swallow so setup stays idempotent
    });

  // Step b: promote to admin BEFORE sign-in so the role is in the session.
  // Uses the Task 16 recipe (docker compose psql UPDATE on engram_orchestrator).
  await promoteE2eUserToAdmin();

  // Step c: sign in — must succeed or the suite is dead.
  const signInRes = await ctx.post("/api/auth/sign-in/email", {
    data: { email: E2E_EMAIL, password: E2E_PW },
  });
  if (!signInRes.ok()) {
    const respBody = await signInRes.text().catch(() => "(unreadable)");
    throw new Error(
      `better-auth sign-in failed (${signInRes.status()}) — orchestrator up on :8787?\n${respBody}`,
    );
  }

  // Step d: persist the session cookie so tests start pre-authenticated.
  await ctx.storageState({ path: "e2e/.auth-state.json" });
  await ctx.dispose();
}
