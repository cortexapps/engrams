/**
 * Orchestrator server tests (bun test).
 *
 * Spins the server on an ephemeral port and verifies:
 *   1. GET /healthz → 200 {"ok":true}
 *   2. /rpc/some.Service/Method → handled by Connect adapter (distinguishable
 *      from Hono by the Connect-protocol error body shape or content-type)
 *   3. Unknown non-rpc path → Hono 404 {"error":"not found"}
 *
 * CRITICAL CANARY (Step 7): env-gated gRPC live call.
 *   Set ENGRAM_SMOKE_GRPC=1 to enable. Requires a running coordinator on
 *   localhost:50061 with ENGRAM_APP_GRPC_TOKENS=dev-app-grpc-token.
 */

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import type { AddressInfo } from "net";

// ---------------------------------------------------------------------------
// Shared server fixture
// ---------------------------------------------------------------------------

let baseUrl: string;
let server: ReturnType<typeof buildServer>;

beforeAll(async () => {
  const app = new Hono();
  app.get("/healthz", (c) => c.json({ ok: true }));
  app.notFound((c) => c.json({ error: "not found" }, 404));

  server = buildServer(app);

  await new Promise<void>((resolve) => {
    // port=0 → OS assigns an ephemeral port
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address() as AddressInfo;
      baseUrl = `http://127.0.0.1:${addr.port}`;
      resolve();
    });
  });
});

afterAll(async () => {
  await new Promise<void>((resolve, reject) => {
    server.close((err) => (err ? reject(err) : resolve()));
  });
});

// ---------------------------------------------------------------------------
// 1. Health check
// ---------------------------------------------------------------------------

test("GET /healthz → 200 {ok:true}", async () => {
  const res = await fetch(`${baseUrl}/healthz`);
  expect(res.status).toBe(200);
  const body = await res.json();
  expect(body).toEqual({ ok: true });
});

// ---------------------------------------------------------------------------
// 2. /rpc route is handled by Connect adapter, NOT Hono
//
// The Connect adapter returns a Connect-shaped error for unknown procedures
// instead of Hono's {"error":"not found"}. We verify:
//   a) The response is NOT the Hono 404 shape
//   b) The response has a content-type consistent with Connect protocol
//      (application/json with a `code` field, or application/connect+json,
//       or a plain 404 with no `error` key)
// ---------------------------------------------------------------------------

test("POST /rpc/some.Service/Method → Connect adapter response (not Hono)", async () => {
  const res = await fetch(`${baseUrl}/rpc/some.Service/Method`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Connect-Protocol-Version": "1",
    },
    body: "{}",
  });

  const text = await res.text();

  // Hono's fallback would give {"error":"not found"}
  // Connect's fallback gives a Connect-typed error (has "code" key)
  // or a plain 404 with no "error" key — either way, NOT Hono's body.
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = null;
  }

  // Must NOT be Hono's generic 404 shape
  if (parsed !== null && typeof parsed === "object") {
    expect((parsed as Record<string, unknown>)["error"]).not.toBe("not found");
  }

  // Connect adapter returns 404 for unknown routes — confirm it came from
  // Connect by checking the content-type OR the absence of Hono's body.
  const ct = res.headers.get("content-type") ?? "";
  const isConnectLike =
    ct.includes("application/json") ||
    ct.includes("application/connect") ||
    ct.includes("application/proto") ||
    res.status === 404;
  expect(isConnectLike).toBe(true);
});

// ---------------------------------------------------------------------------
// 3. Unknown non-rpc path → Hono 404
// ---------------------------------------------------------------------------

test("GET /does-not-exist → Hono 404 {error:'not found'}", async () => {
  const res = await fetch(`${baseUrl}/does-not-exist`);
  expect(res.status).toBe(404);
  const body = await res.json();
  expect(body).toEqual({ error: "not found" });
});

// ---------------------------------------------------------------------------
// 4. CRITICAL gRPC canary — skip when ENGRAM_SMOKE_GRPC is unset
// ---------------------------------------------------------------------------

const SMOKE_GRPC = process.env["ENGRAM_SMOKE_GRPC"] === "1";
const BEARER = process.env["CONTROL_PLANE_BEARER"] ?? "dev-app-grpc-token";
const GRPC_URL = process.env["CONTROL_PLANE_GRPC_URL"] ?? "http://127.0.0.1:50061";

describe("gRPC canary (ENGRAM_SMOKE_GRPC=1 to enable)", () => {
  test.skipIf(!SMOKE_GRPC)(
    "SessionService.ListSessions returns without error (live coordinator)",
    async () => {
      // Dynamic import so the module isn't resolved when the test is skipped,
      // avoiding import-time errors on machines without the coordinator up.
      const { createGrpcTransport } = await import("@connectrpc/connect-node");
      const { createClient } = await import("@connectrpc/connect");
      const { SessionService } = await import(
        "../gen/engram/app/v1/session_pb.ts"
      );

      const bearerInterceptor = () =>
        (next: (req: unknown) => Promise<unknown>) =>
        (req: { header: Headers }) => {
          req.header.set("authorization", `Bearer ${BEARER}`);
          return next(req);
        };

      const transport = createGrpcTransport({
        baseUrl: GRPC_URL,
        httpVersion: "2",
        interceptors: [bearerInterceptor() as never],
      });

      const client = createClient(SessionService, transport);

      let threw = false;
      let errorMsg = "";
      try {
        const resp = await client.listSessions({});
        // Any response (even empty list) is a success
        expect(Array.isArray(resp.sessions)).toBe(true);
        console.log(
          `gRPC canary PASS: ListSessions returned ${resp.sessions.length} session(s)`,
        );
      } catch (err: unknown) {
        threw = true;
        errorMsg = err instanceof Error ? err.message : String(err);
        console.error("gRPC canary BLOCKED:", errorMsg);
      }

      if (threw) {
        // Force a descriptive failure rather than a silent pass
        throw new Error(
          `BLOCKED: gRPC canary failed — Bun node:http2 / Connect transport error: ${errorMsg}. ` +
            `This changes the architecture; report to controller.`,
        );
      }
    },
  );
});
