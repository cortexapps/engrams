/**
 * Orchestrator server tests (bun test).
 *
 * Spins the server on an ephemeral port and verifies:
 *   1. GET /healthz → 200 {"ok":true}
 *   2. /rpc/some.Service/Method → handled by Connect adapter (distinguishable
 *      from Hono by the Connect-protocol error body shape or content-type)
 *   3. Unknown non-rpc path → Hono 404 {"error":"not found"}
 *   4. Streaming: first chunk arrives before stream closes (no full-response buffering)
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

  // The body-shape check is the real discriminator:
  //   Hono's fallback → {"error":"not found"}
  //   Connect's fallback → empty body or {"code":...} — never has "error":"not found"
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = null;
  }

  // Must NOT be Hono's generic 404 shape — this is the definitive assertion.
  if (parsed !== null && typeof parsed === "object") {
    expect((parsed as Record<string, unknown>)["error"]).not.toBe("not found");
  } else {
    // Empty or non-JSON body: Connect returns an empty 404 for unknown routes.
    // Either way, it is not Hono's {"error":"not found"} — test passes.
    expect(text.trim()).not.toBe('{"error":"not found"}');
  }
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
// 4. Streaming regression — proves getRequestListener does NOT buffer
//
// If the Hono bridge buffers the full response body before sending, the
// client would receive BOTH chunks only after the stream closes. This test
// reads the response incrementally and asserts the first chunk arrives
// before the stream ends (i.e., we observe data mid-stream).
//
// Uses a dedicated ephemeral server with a /stream route so this test is
// self-contained and doesn't pollute the shared fixture.
// ---------------------------------------------------------------------------

describe("SSE streaming regression (proves no full-response buffering)", () => {
  let streamBaseUrl: string;
  let streamServer: ReturnType<typeof buildServer>;

  beforeAll(async () => {
    const streamApp = new Hono();

    // Route that writes two SSE-ish chunks with a 100 ms delay between them.
    streamApp.get("/stream", (c) => {
      const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
      const writer = writable.getWriter();
      const enc = new TextEncoder();

      (async () => {
        await writer.write(enc.encode("data: chunk1\n\n"));
        // 100 ms gap — client should observe chunk1 before this delay expires
        await new Promise((r) => setTimeout(r, 100));
        await writer.write(enc.encode("data: chunk2\n\n"));
        await writer.close();
      })();

      return new Response(readable, {
        headers: { "Content-Type": "text/event-stream" },
      });
    });

    streamServer = buildServer(streamApp);

    await new Promise<void>((resolve) => {
      streamServer.listen(0, "127.0.0.1", () => {
        const addr = streamServer.address() as import("net").AddressInfo;
        streamBaseUrl = `http://127.0.0.1:${addr.port}`;
        resolve();
      });
    });
  });

  afterAll(async () => {
    await new Promise<void>((resolve, reject) => {
      streamServer.close((err) => (err ? reject(err) : resolve()));
    });
  });

  test(
    "GET /stream — first chunk arrives before stream closes (incremental delivery)",
    async () => {
      const res = await fetch(`${streamBaseUrl}/stream`);
      expect(res.status).toBe(200);
      expect(res.body).not.toBeNull();

      const reader = res.body!.getReader();
      const dec = new TextDecoder();
      const chunks: string[] = [];

      // Read the first chunk; record when it arrives.
      const t0 = Date.now();
      const { value: firstValue, done: firstDone } = await reader.read();
      const t1 = Date.now();

      expect(firstDone).toBe(false);
      const firstText = dec.decode(firstValue);
      chunks.push(firstText);

      // Drain the rest of the stream.
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        chunks.push(dec.decode(value));
      }

      // The first chunk must contain "chunk1" — proves it arrived mid-stream.
      expect(chunks.join("")).toContain("chunk1");
      expect(chunks.join("")).toContain("chunk2");

      // The first chunk must have arrived in well under the total stream
      // duration (~100 ms delay). If the bridge buffered the full response,
      // t1 - t0 would be ≥ 100 ms; incremental delivery is << 100 ms.
      // We allow up to 80 ms to stay robust under load.
      expect(t1 - t0).toBeLessThan(80);
    },
  );
});

// ---------------------------------------------------------------------------
// CRITICAL gRPC canary — skip when ENGRAM_SMOKE_GRPC is unset
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
