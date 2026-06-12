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
//
// ADR 0039 Task 28: the enabled-images precondition probe was changed from
//   GET /api/v1/enabled-images   (coordinator REST — no longer reachable from the browser)
// to
//   POST /rpc/engram.app.v1.ImageService/ListEnabledImages   (orchestrator Connect/JSON)
// The probe signs in first (the request-context flow already exists) so the
// ListEnabledImages RPC passes the CASL gate (member can read enabled images).
//
// The coordinator /healthz probe is kept: it tests the stack precondition
// (coordinator must be running for the full platform to work), not browser
// traffic. The coordinator's REST routes remain live for engram-cli/scripts;
// the browser just never hits them directly after Task 28.

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
 *  HARD-FAILS if psql does not confirm "UPDATE 1". A silent failure here yields
 *  opaque downstream locator timeouts in admin-gated tests — we fail loudly
 *  with a fix-it message instead.
 *
 *  Uses a dynamic import of child_process to avoid the tsconfig needing
 *  full @types/node (which would leak Node globals into src/).  */
async function promoteE2eUserToAdmin(): Promise<void> {
  const { execSync } = await import("child_process");
  // Playwright runs with cwd=web/ — the compose file lives at the repo root.
  let stdout: string;
  try {
    const result = execSync(
      [
        "docker compose",
        "-f ../deploy/docker-compose.dev.yml",
        "exec -T postgres",
        "psql -U engram -d engram_orchestrator",
        `-c "UPDATE \\"user\\" SET role='admin' WHERE email='${E2E_EMAIL}'"`,
      ].join(" "),
      { stdio: "pipe" },
    );
    stdout = result.toString();
  } catch (err) {
    throw new Error(
      `[global-setup] psql promote failed — is docker compose up?\n` +
        `  Run: just dev\n  Error: ${String(err)}`,
    );
  }

  // psql prints "UPDATE 1" on success, "UPDATE 0" when no row matched.
  if (!stdout.includes("UPDATE 1")) {
    throw new Error(
      `[global-setup] e2e user promote returned "${stdout.trim()}" instead of "UPDATE 1".\n` +
        `  The user row does not exist yet — did the sign-up step above succeed?\n` +
        `  Check: docker compose -f ../deploy/docker-compose.dev.yml exec -T postgres ` +
        `psql -U engram -d engram_orchestrator -c 'SELECT email,role FROM "user"'`,
    );
  }
}

export default async function globalSetup() {
  // ---- Precondition gate --------------------------------------------------
  await mustFetch(`${WEB}/`, "run `just dev` first");
  // Stack precondition: coordinator must be running for the platform to work.
  // This is NOT browser traffic — just a liveness check. The coordinator's REST
  // routes stay live for engram-cli/integration scripts (Tasks 29/32 scope).
  await mustFetch(
    `${COORD}/healthz`,
    "coordinator down/unhealthy — check Tilt (http://localhost:10350)",
  );

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

  // Step d: probe enabled-images via the orchestrator Connect/JSON RPC.
  // ADR 0039 Task 28: was GET /api/v1/enabled-images (coordinator REST).
  // Now: POST /rpc/engram.app.v1.ImageService/ListEnabledImages (orchestrator
  // passthrough). The session cookie from step c rides on this request so the
  // CASL gate passes. harnessName is camelCase in proto JSON mapping.
  const imagesRes = await ctx.post("/rpc/engram.app.v1.ImageService/ListEnabledImages", {
    data: {},
    headers: { "Content-Type": "application/json" },
  });
  if (!imagesRes.ok()) {
    const respBody = await imagesRes.text().catch(() => "(unreadable)");
    throw new Error(
      `ListEnabledImages probe failed (${imagesRes.status()}) — orchestrator/coordinator up?\n${respBody}`,
    );
  }
  const imagesBody = (await imagesRes.json()) as {
    images?: { imageUri: string; harnessName?: string | null }[];
  };
  // Connect JSON omits unset optional fields (emitDefaults=false): a
  // no-harness image has harnessName ABSENT, not null — loose check.
  if (!imagesBody.images?.some((i) => i.harnessName == null)) {
    throw new Error("no NO-HARNESS image enabled — run `just integration-session` once");
  }

  // Step e: persist the session cookie so tests start pre-authenticated.
  await ctx.storageState({ path: "e2e/.auth-state.json" });
  await ctx.dispose();
}
