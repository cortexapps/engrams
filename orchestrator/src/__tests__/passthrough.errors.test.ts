/**
 * Passthrough error-propagation test (enable-image incident regression).
 *
 * Motivating incident: a user enabled an image; the coordinator returned a
 * precise gRPC error — `registry pull … failed: OCI distribution error: …
 * Not authorized` with code InvalidArgument — but the browser only saw
 * `ConnectError: [internal] HTTP 400`. The real message must survive the
 * coord-gRPC → orchestrator passthrough → web chain.
 *
 * This test pins the orchestrator half: when the control-plane upstream
 * rejects an RPC with a ConnectError(code, message), the passthrough MUST
 * re-surface it as a Connect-framed error whose body carries the SAME
 * `code` and `message` — never a generic Internal / opaque "HTTP 400".
 *
 * We assert at two levels:
 *   1. The raw HTTP response the browser's connect-web transport would
 *      receive is a Connect-framed JSON error `{code, message}` (the exact
 *      shape connect-web parses; a non-Connect body is what degrades to
 *      "[internal] HTTP 400").
 *   2. A real Connect client observes the upstream code + message.
 */

import { expect, test, describe } from "bun:test";
import type { AddressInfo } from "node:net";
import { Hono } from "hono";
import {
  createClient,
  ConnectError,
  Code,
} from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { ConnectRouter, Transport } from "@connectrpc/connect";

import { buildServer } from "../server.ts";
import { SURFACE } from "../rpc/surface.ts";
import { registerPassthrough } from "../rpc/passthrough.ts";
import type { GetSession, ResolveOwner } from "../rpc/passthrough.ts";
import { ImageService } from "../gen/engram/app/v1/image_pb.ts";

const adminGetSession: GetSession = async () => ({
  user: { id: "admin", role: "admin", email: "admin@test.invalid" },
});
const stubResolveOwner: ResolveOwner = async () => "admin";

// The exact incident message + code the coordinator returns.
const INCIDENT_CODE = Code.InvalidArgument;
const INCIDENT_MSG =
  "registry pull for `reg.example/x:tag` failed: OCI distribution error: " +
  "response status 401 Unauthorized: Not authorized. Check that a matching " +
  "registry credential exists if the registry requires auth.";

/** Spawn the orchestrator pointed at a fake upstream whose EnableImage throws. */
async function spawnWithFailingUpstream() {
  // Model the exact error object createGrpcTransport returns. A router
  // transport sanitizes its own protocol metadata before this boundary and
  // therefore cannot reproduce the production failure.
  const upstream = {
    unary() {
      throw new ConnectError(INCIDENT_MSG, INCIDENT_CODE, {
        "content-type": "application/grpc",
        "grpc-status": String(INCIDENT_CODE),
        "grpc-message": INCIDENT_MSG,
        "x-request-id": "incident-request",
      });
    },
    stream() {
      throw new Error("unexpected streaming call");
    },
  } as Transport;

  const app = new Hono();
  const server = buildServer(app, (router: ConnectRouter) => {
    registerPassthrough(router, SURFACE, upstream, adminGetSession, stubResolveOwner);
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const addr = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${addr.port}/rpc`,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

describe("passthrough error propagation (enable-image incident)", () => {
  test("raw response is a Connect-framed {code,message} error, not opaque HTTP 400", async () => {
    const srv = await spawnWithFailingUpstream();
    try {
      // Exactly what connect-web sends over HTTP/1.1 for a unary Connect call.
      const resp = await fetch(
        `${srv.baseUrl}/engram.app.v1.ImageService/EnableImage`,
        {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "connect-protocol-version": "1",
          },
          body: JSON.stringify({ imageUri: "reg.example/x:tag" }),
        },
      );
      // InvalidArgument → HTTP 400 in the Connect protocol; the BODY must
      // be the Connect error envelope (this is what stops connect-web from
      // collapsing to "[internal] HTTP 400").
      expect(resp.headers.get("content-type")).toContain("json");
      expect(resp.headers.get("content-type")).not.toContain("application/grpc");
      expect(resp.headers.get("grpc-status")).toBeNull();
      expect(resp.headers.get("x-request-id")).toBe("incident-request");
      const body = (await resp.json()) as { code?: string; message?: string };
      expect(body.code).toBe("invalid_argument");
      expect(body.message).toBe(INCIDENT_MSG);
      expect(body.message).toContain("Not authorized");
    } finally {
      await srv.close();
    }
  });

  test("a Connect client observes the upstream code + message verbatim", async () => {
    const srv = await spawnWithFailingUpstream();
    try {
      const transport = createConnectTransport({
        baseUrl: srv.baseUrl,
        httpVersion: "1.1",
      });
      const client = createClient(ImageService, transport);
      let caught: unknown;
      try {
        await client.enableImage({ imageUri: "reg.example/x:tag" });
      } catch (e) {
        caught = e;
      }
      const ce = ConnectError.from(caught);
      // Code preserved (NOT collapsed to Internal).
      expect(ce.code).toBe(INCIDENT_CODE);
      expect(ce.code).not.toBe(Code.Internal);
      // Message preserved verbatim (rawMessage strips the "[code]" prefix
      // that ConnectError.toString() adds).
      expect(ce.rawMessage).toBe(INCIDENT_MSG);
      expect(ce.rawMessage).toContain("Not authorized");
    } finally {
      await srv.close();
    }
  });
});
