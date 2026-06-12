/**
 * Orchestrator live smoke test (ADR 0039 Task 18).
 *
 * Env-gated: set SMOKE=1 to enable.
 * Requires a full stack running:
 *   - Coordinator on localhost:50061 with ENGRAM_APP_GRPC_TOKENS=dev-app-grpc-token
 *   - Orchestrator on ORCHESTRATOR_URL (default http://127.0.0.1:8091)
 *   - A valid better-auth session cookie in SMOKE_SESSION_COOKIE
 *
 * Exercises:
 *   1. Unauthenticated GetSession → Unauthenticated (401 / gRPC Unauthenticated)
 *   2. Member cookie + ListHosts → PermissionDenied (403 / gRPC PermissionDenied)
 *   3. Member cookie + ListEnabledImages → OK (image catalog is member-readable)
 *   4. (Extended by Task 19/20): CreateTask → SSE stream → delete flow.
 *
 * Extended by Tasks 19 and 20 once those RPCs land.
 */

import { expect, test, describe } from "bun:test";

const SMOKE = process.env["SMOKE"] === "1";

const ORCHESTRATOR_URL =
  process.env["ORCHESTRATOR_URL"] ?? "http://127.0.0.1:8091";
const SMOKE_SESSION_COOKIE = process.env["SMOKE_SESSION_COOKIE"] ?? "";

describe("orchestrator live smoke (SMOKE=1 to enable)", () => {
  test.skipIf(!SMOKE)(
    "unauthenticated GetSession RPC → 401/Unauthenticated",
    async () => {
      // Connect protocol POST with no session cookie.
      const res = await fetch(
        `${ORCHESTRATOR_URL}/rpc/engram.app.v1.SessionService/GetSession`,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/connect+json",
            "Connect-Protocol-Version": "1",
          },
          body: JSON.stringify({ session_id: "nonexistent-session" }),
        },
      );

      // Connect returns non-2xx or a JSON error body with code = "unauthenticated".
      const body = await res.json();
      expect(body).toHaveProperty("code");
      expect(["unauthenticated", "permission_denied"]).toContain(
        body.code as string,
      );
      console.log("Smoke 1/3 PASS: unauthenticated → denied");
    },
  );

  test.skipIf(!SMOKE || !SMOKE_SESSION_COOKIE)(
    "member cookie + ListHosts → PermissionDenied",
    async () => {
      const res = await fetch(
        `${ORCHESTRATOR_URL}/rpc/engram.app.v1.FleetService/ListHosts`,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/connect+json",
            "Connect-Protocol-Version": "1",
            Cookie: SMOKE_SESSION_COOKIE,
          },
          body: JSON.stringify({}),
        },
      );

      const body = await res.json();
      expect(body).toHaveProperty("code");
      expect(body.code as string).toBe("permission_denied");
      console.log("Smoke 2/3 PASS: member + ListHosts → permission_denied");
    },
  );

  test.skipIf(!SMOKE || !SMOKE_SESSION_COOKIE)(
    "member cookie + ListEnabledImages → 200 (images list)",
    async () => {
      const res = await fetch(
        `${ORCHESTRATOR_URL}/rpc/engram.app.v1.ImageService/ListEnabledImages`,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/connect+json",
            "Connect-Protocol-Version": "1",
            Cookie: SMOKE_SESSION_COOKIE,
          },
          body: JSON.stringify({}),
        },
      );

      expect(res.status).toBe(200);
      const body = await res.json();
      // images field is present (may be empty in dev).
      expect(body).toHaveProperty("images");
      expect(Array.isArray(body.images)).toBe(true);
      console.log(
        `Smoke 3/3 PASS: ListEnabledImages → ${(body.images as unknown[]).length} image(s)`,
      );
    },
  );
});
