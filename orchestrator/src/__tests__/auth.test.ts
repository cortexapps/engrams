/**
 * better-auth integration tests (bun test) — ADR 0051 §5 / Task 16.
 *
 * Two test groups:
 *
 * 1. Non-gated: /api/auth/* mount responds (the better-auth handler,
 *    not Hono's 404). Requires only that the server starts — no live DB
 *    needed. Verifies the route is wired and better-auth owns the prefix.
 *
 * 2. Live (ORCHESTRATOR_DATABASE_URL + reachable DB): sign-up → get-session
 *    round-trip via fetch with a cookie jar. Validates the full stack:
 *    auth.handler → drizzle adapter → postgres → session cookie.
 *
 * Pattern follows db.test.ts: top-level await for the gate, test.skipIf for
 * honest SKIP reporting (not silent pass), unique email per run to avoid
 * collisions on repeated runs against the same DB.
 */

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { checkDb } from "../db/client.ts";
import authRoute from "../routes/auth.ts";
import health from "../routes/health.ts";
import type { AddressInfo } from "net";

// ---------------------------------------------------------------------------
// Gate: reachability probe at module load (top-level await, fine in Bun)
// ---------------------------------------------------------------------------

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

// ---------------------------------------------------------------------------
// Shared ephemeral server (both test groups share it)
// ---------------------------------------------------------------------------

let baseUrl: string;
let srv: ReturnType<typeof buildServer>;

beforeAll(async () => {
  // NOTE: BETTER_AUTH_SECRET is no longer set here — betterAuth() is called
  // eagerly at module-load time (when better-auth.ts is imported via server.ts),
  // so any env mutation here arrives too late. config.ts now handles the
  // test-mode escape: when NODE_ENV==='test' and the var is absent it injects a
  // placeholder at module load. Setting it here would be a no-op.

  const app = new Hono();
  app.route("/", health);
  app.route("/", authRoute);
  app.notFound((c) => c.json({ error: "not found" }, 404));
  srv = buildServer(app);

  await new Promise<void>((resolve) => {
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address() as AddressInfo;
      baseUrl = `http://127.0.0.1:${addr.port}`;
      resolve();
    });
  });
});

afterAll(async () => {
  if (!srv) return;
  await new Promise<void>((resolve, reject) => {
    srv.close((err) => (err ? reject(err) : resolve()));
  });
});

// ---------------------------------------------------------------------------
// 1. Non-gated: /api/auth/* mount is owned by better-auth, not Hono's 404
//
// We hit a well-known better-auth endpoint (GET /api/auth/get-session) with
// no cookie. better-auth returns a non-404 response (200 with null session).
// Hono's generic 404 {"error":"not found"} would indicate the route is not
// wired. We don't assert the exact body — just that it is NOT Hono's 404.
// ---------------------------------------------------------------------------

describe("auth route — non-gated (no live DB required)", () => {
  test("GET /api/auth/get-session → better-auth responds (not Hono 404)", async () => {
    const res = await fetch(`${baseUrl}/api/auth/get-session`);

    // better-auth returns 200 (null session) or 401 — not 404.
    // Hono's notFound handler returns 404 {"error":"not found"}.
    // Any non-404 proves the mount is correct.
    expect(res.status).not.toBe(404);

    // Belt-and-suspenders: if it is 404 the body must not be Hono's shape.
    if (res.status === 404) {
      const body = await res.json();
      expect((body as Record<string, unknown>)["error"]).not.toBe("not found");
    }
  });
});

// ---------------------------------------------------------------------------
// 2. Live round-trip: sign-up → get-session
//
// Uses a manual cookie jar so we can verify the session cookie carries auth
// across requests. better-auth sets `better-auth.session_token` (httpOnly).
// ---------------------------------------------------------------------------

describe("auth live round-trip (requires ORCHESTRATOR_DATABASE_URL)", () => {
  // Unique email per run — avoid "email already taken" on repeated runs.
  const testEmail = `test-${Date.now()}@engram.local`;
  const testPassword = "test-password-abc-123";
  const testName = "Test User";

  // Manual cookie jar: we need to capture Set-Cookie from sign-up and replay
  // it on get-session. Bun's fetch does NOT auto-manage cookies across
  // requests, so we extract them manually.
  let sessionCookie = "";

  test.skipIf(!dbReachable)(
    "sign-up/email → 200 + set-cookie",
    async () => {
      const res = await fetch(`${baseUrl}/api/auth/sign-up/email`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          // Simulate the browser's origin so better-auth's CSRF check passes.
          origin: "http://localhost:5173",
        },
        body: JSON.stringify({
          email: testEmail,
          password: testPassword,
          name: testName,
        }),
      });

      expect(res.status).toBe(200);

      // Extract all Set-Cookie headers — better-auth may set multiple.
      // In Node/Bun getSetCookie() returns an array of each header value.
      const cookies = res.headers.getSetCookie?.() ?? [];
      expect(cookies.length).toBeGreaterThan(0);

      // Join all cookies for replay (browser behaviour for multiple Set-Cookie).
      sessionCookie = cookies
        .map((c) => c.split(";")[0]) // strip Path=/ etc., keep name=value
        .join("; ");
    },
  );

  test.skipIf(!dbReachable)(
    "get-session with session cookie → user record returned",
    async () => {
      // Fail explicitly if sign-up didn't produce a cookie — a silent return
      // would mask regressions where sign-up broke but the DB gate passed.
      expect(sessionCookie).toBeTruthy();

      const res = await fetch(`${baseUrl}/api/auth/get-session`, {
        headers: {
          cookie: sessionCookie,
          origin: "http://localhost:5173",
        },
      });

      expect(res.status).toBe(200);
      const body = (await res.json()) as Record<string, unknown>;

      // better-auth returns { session: {...}, user: {...} } on an authenticated request.
      expect(body["user"]).toBeTruthy();
      const user = body["user"] as Record<string, unknown>;
      expect(user["email"]).toBe(testEmail);
      expect(user["name"]).toBe(testName);

      // Freshly signed-up user has role 'user' (or null — admin plugin may
      // not set it until setRole is called; both are acceptable).
      const role = user["role"];
      expect(role === "user" || role === null || role === undefined).toBe(true);
    },
  );
});
