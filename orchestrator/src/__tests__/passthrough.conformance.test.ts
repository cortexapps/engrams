/**
 * Passthrough conformance test (ADR 0051 Task 18, Step 6).
 *
 * For every method in SURFACE, verifies that:
 *   1. A request deterministically filled with sentinel values reaches the
 *      upstream byte-for-byte identical.
 *   2. The upstream response (server-streaming: exactly 2 messages) reaches
 *      the client unchanged — "≥1" would pass even on truncation.
 *   3. The upstream does NOT receive cookie/authorization headers (the
 *      upstreamHeaders() scrub). Note: the in-process createRouterTransport
 *      does not carry real HTTP bearer headers, so we assert the scrub
 *      (cookie/authorization absent) rather than asserting a forwarded token.
 *   4. The upstream's x-fake-upstream response header survives copyHeaders to
 *      the client (exercises copyHeaders, currently untested by the matrix).
 *
 * Run as an admin user so the policy gate passes — transport fidelity and
 * authz are exercised orthogonally (authz matrix lives in authz.matrix.test.ts).
 *
 * A future RPC added to SURFACE is covered automatically with zero new test
 * code.
 */

import { expect, test, describe } from "bun:test";

import type { AddressInfo } from "node:net";
import { Hono } from "hono";

import {
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { ConnectRouter, Transport } from "@connectrpc/connect";
import {
  create,
  toBinary,
  ScalarType,
  type DescMessage,
  type DescField,
} from "@bufbuild/protobuf";

import { buildServer } from "../server.ts";
import { SURFACE } from "../rpc/surface.ts";
import { registerPassthrough } from "../rpc/passthrough.ts";
import type { GetSession, ResolveOwner } from "../rpc/passthrough.ts";

// ---------------------------------------------------------------------------
// Deterministic message filler
// ---------------------------------------------------------------------------

const MAX_DEPTH = 3;

/**
 * Return a sentinel value for a scalar field based on its type.
 */
function scalarValue(scalar: ScalarType, fieldName: string): unknown {
  switch (scalar) {
    case ScalarType.STRING:
      return fieldName; // field name as value
    case ScalarType.BOOL:
      return true;
    case ScalarType.BYTES:
      return new Uint8Array([0xab]);
    case ScalarType.DOUBLE:
    case ScalarType.FLOAT:
      return 1.0;
    case ScalarType.INT32:
    case ScalarType.UINT32:
    case ScalarType.FIXED32:
    case ScalarType.SFIXED32:
    case ScalarType.SINT32:
      return 1;
    case ScalarType.INT64:
    case ScalarType.UINT64:
    case ScalarType.FIXED64:
    case ScalarType.SFIXED64:
    case ScalarType.SINT64:
      return BigInt(1);
    default:
      throw new Error(`Unhandled ScalarType: ${scalar}`);
  }
}

/**
 * Deterministically populate every field of a protobuf message.
 * Returns a plain object suitable for passing to `create(schema, obj)`.
 *
 * String   → field local name
 * Numbers  → field number (1)
 * Bool     → true
 * Bytes    → [0xAB]
 * Enum     → first non-zero enum value (or 0 if all are zero)
 * Repeated → one element
 * Map      → one entry ("key" → value)
 * Message  → recurse (depth-capped at MAX_DEPTH)
 * Oneof    → first case populated
 */
function populateInit(schema: DescMessage, depth = 0): Record<string, unknown> {
  const init: Record<string, unknown> = {};

  const handledOneofs = new Set<string>();

  for (const field of schema.fields) {
    const localName = field.localName;

    // Only handle one field per oneof group.
    if (field.oneof) {
      if (handledOneofs.has(field.oneof.name)) continue;
      handledOneofs.add(field.oneof.name);
    }

    init[localName] = fieldValue(field, depth);
  }

  return init;
}

function fieldValue(field: DescField, depth: number): unknown {
  switch (field.fieldKind) {
    case "scalar":
      return scalarValue(field.scalar, field.localName);

    case "enum": {
      // Pick first non-zero value, or 0 as fallback.
      const nonZero = field.enum.values.find((v) => v.number !== 0);
      return nonZero ? nonZero.number : 0;
    }

    case "message": {
      if (depth >= MAX_DEPTH) return undefined;
      const nested = populateInit(field.message, depth + 1);
      return create(field.message, nested);
    }

    case "list": {
      const listField = field;
      switch (listField.listKind) {
        case "scalar":
          return [scalarValue(listField.scalar, field.localName)];
        case "enum": {
          const nz = listField.enum.values.find((v) => v.number !== 0);
          return [nz ? nz.number : 0];
        }
        case "message": {
          if (depth >= MAX_DEPTH) return [];
          const nested = populateInit(listField.message, depth + 1);
          return [create(listField.message, nested)];
        }
      }
      break;
    }

    case "map": {
      const mapField = field;
      let keyVal: string | number | bigint = "key";
      // Map keys are scalar (non-float, non-bytes, non-bool in proto).
      if (mapField.mapKey === ScalarType.STRING) {
        keyVal = "key";
      } else if (
        mapField.mapKey === ScalarType.INT64 ||
        mapField.mapKey === ScalarType.UINT64 ||
        mapField.mapKey === ScalarType.FIXED64 ||
        mapField.mapKey === ScalarType.SFIXED64 ||
        mapField.mapKey === ScalarType.SINT64
      ) {
        keyVal = BigInt(1);
      } else {
        keyVal = 1;
      }

      switch (mapField.mapKind) {
        case "scalar":
          return { [String(keyVal)]: scalarValue(mapField.scalar, field.localName) };
        case "enum": {
          const nz = mapField.enum.values.find((v) => v.number !== 0);
          return { [String(keyVal)]: nz ? nz.number : 0 };
        }
        case "message": {
          if (depth >= MAX_DEPTH) return {};
          const nested = populateInit(mapField.message, depth + 1);
          return { [String(keyVal)]: create(mapField.message, nested) };
        }
      }
      break;
    }
  }

  throw new Error(
    `fieldValue: unhandled field kind '${field.fieldKind}' for field '${field.localName}'`,
  );
}

// ---------------------------------------------------------------------------
// Admin getSession stub
// ---------------------------------------------------------------------------

const ADMIN_USER_ID = "admin-conformance-user";

const adminGetSession: GetSession = async (_headers) => ({
  user: { id: ADMIN_USER_ID, role: "admin", email: "admin@test.invalid" },
});

/**
 * Stub resolver: returns a constant owner for any session.
 * Admins pass the CASL check regardless, so this just avoids DB access.
 */
const stubResolveOwner: ResolveOwner = async (_sessionId) => ADMIN_USER_ID;

// ---------------------------------------------------------------------------
// Server fixture helpers
// ---------------------------------------------------------------------------

interface ConformanceServer {
  baseUrl: string;
  close: () => Promise<void>;
}

async function spawnConformanceServer(
  upstream: Transport,
): Promise<ConformanceServer> {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));

  const server = buildServer(app, (router: ConnectRouter) => {
    registerPassthrough(router, SURFACE, upstream, adminGetSession, stubResolveOwner);
  });

  return new Promise<ConformanceServer>((resolve, reject) => {
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address() as AddressInfo;
      // /rpc is required: Connect adapter is mounted at requestPathPrefix=/rpc.
      const baseUrl = `http://127.0.0.1:${addr.port}/rpc`;
      resolve({
        baseUrl,
        close: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          }),
      });
    });
    server.on("error", reject);
  });
}

// ---------------------------------------------------------------------------
// Conformance suite
// ---------------------------------------------------------------------------

describe("passthrough conformance", () => {
  for (const { service, methods: allowedMethods } of SURFACE) {
    const methodsToTest = service.methods.filter(
      (m) =>
        (!allowedMethods || allowedMethods.includes(m.name)) &&
        (m.methodKind === "unary" || m.methodKind === "server_streaming"),
    );

    for (const m of methodsToTest) {
      const testName = `${service.typeName}/${m.name} passes through faithfully`;

      test(testName, async () => {
        // Build deterministic request + expected response messages.
        const reqInit = populateInit(m.input);
        const req = create(m.input, reqInit);

        const resInit = populateInit(m.output);
        const res = create(m.output, resInit);

        // Track what the fake upstream received.
        let capturedUpstreamReq: unknown = null;
        let capturedUpstreamHeaders: Headers | null = null;

        // Fake upstream: captures the inbound request, returns expectedRes.
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const fakeImpl: Record<string, any> = {};

        if (m.methodKind === "unary") {
          fakeImpl[m.localName] = (upstreamReq: unknown, ctx: { requestHeader: Headers; responseHeader: Headers }) => {
            capturedUpstreamReq = upstreamReq;
            capturedUpstreamHeaders = ctx.requestHeader;
            ctx.responseHeader.set("x-fake-upstream", "1");
            return res;
          };
        } else {
          fakeImpl[m.localName] = async function* (
            upstreamReq: unknown,
            ctx: { requestHeader: Headers; responseHeader: Headers },
          ) {
            capturedUpstreamReq = upstreamReq;
            capturedUpstreamHeaders = ctx.requestHeader;
            ctx.responseHeader.set("x-fake-upstream", "1");
            yield res;
            yield res;
          };
        }

        const fakeUpstreamTransport = createRouterTransport((router: ConnectRouter) => {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          router.service(service as any, fakeImpl);
        });

        // Spawn the orchestrator server pointing at the fake upstream.
        const srv = await spawnConformanceServer(fakeUpstreamTransport);

        try {
          // Create a Connect protocol client that talks to the orchestrator.
          // Connect protocol avoids gRPC-Web trailer-frame requirements in tests.
          const transport = createConnectTransport({
            baseUrl: srv.baseUrl,
            httpVersion: "1.1",
          });

          if (m.methodKind === "unary") {
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            const client = createClient(service as any, transport) as any;
            let capturedResponseHeader: Headers | null = null;
            const clientRes = await client[m.localName](req, {
              onHeader(h: Headers) { capturedResponseHeader = h; },
            });

            // The upstream received the request byte-for-byte.
            expect(toBinary(m.input, capturedUpstreamReq as never)).toEqual(
              toBinary(m.input, req),
            );

            // The client received the response byte-for-byte.
            expect(toBinary(m.output, clientRes as never)).toEqual(
              toBinary(m.output, res),
            );

            // (a) Scrub: upstream must NOT have seen cookie or authorization.
            // createRouterTransport is in-process and carries no real bearer,
            // so we assert absence rather than presence — the scrub still applies.
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedUpstreamHeaders!.has("cookie")).toBe(false);
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedUpstreamHeaders!.has("authorization")).toBe(false);

            // (b) copyHeaders: x-fake-upstream set by upstream must survive to client.
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedResponseHeader!.get("x-fake-upstream")).toBe("1");
          } else {
            // server_streaming: collect all messages.
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            const client = createClient(service as any, transport) as any;
            const messages: unknown[] = [];
            let capturedResponseHeader: Headers | null = null;
            for await (const msg of client[m.localName](req, {
              onHeader(h: Headers) { capturedResponseHeader = h; },
            })) {
              messages.push(msg);
            }

            // (c) Exact count: "≥1" would pass on truncation; the fake yields 2.
            expect(messages.length).toBe(2);

            // (a) Scrub: upstream must NOT have seen cookie or authorization.
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedUpstreamHeaders!.has("cookie")).toBe(false);
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedUpstreamHeaders!.has("authorization")).toBe(false);

            // (b) copyHeaders: x-fake-upstream must propagate to the client.
            // eslint-disable-next-line @typescript-eslint/no-non-null-assertion
            expect(capturedResponseHeader!.get("x-fake-upstream")).toBe("1");

            // Upstream saw the request.
            expect(toBinary(m.input, capturedUpstreamReq as never)).toEqual(
              toBinary(m.input, req),
            );

            // All yielded messages match the expected response.
            for (const msg of messages) {
              expect(toBinary(m.output, msg as never)).toEqual(
                toBinary(m.output, res),
              );
            }
          }
        } finally {
          await srv.close();
        }
      });
    }
  }
});
