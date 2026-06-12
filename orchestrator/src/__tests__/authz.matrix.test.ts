/**
 * Authz matrix tests (ADR 0039 Task 18, Step 7).
 *
 * Verifies the CASL ability × policy gate outcomes for key cases:
 *
 *   - unauthenticated caller → 401 Unauthenticated
 *   - member accessing own session (GetSession) → allowed (passes through)
 *   - member accessing another user's session → 404 NotFound
 *     (anti-enumeration: we don't leak whether the session exists)
 *   - member calling admin-only FleetService.ListHosts → 403 PermissionDenied
 *   - admin calling any method → allowed
 *   - method with no policy entry → 403 PermissionDenied (fail-closed)
 *   - member reading image catalog → allowed
 *   - POLICY structural: every Session-subject entry carries sessionIdField
 *   - fail-closed: no-policy-entry → PermissionDenied, zero upstream calls
 *
 * Both getSession and resolveOwner are injected so no DB is needed.
 * Clients use the /rpc base path since the server routes /rpc/* to Connect.
 */

import { expect, test, describe, afterEach } from "bun:test";

import {
  createClient,
  ConnectError,
  Code,
  createRouterTransport,
} from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { ConnectRouter, Transport } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";

import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { registerPassthrough } from "../rpc/passthrough.ts";
import type { GetSession, ResolveOwner } from "../rpc/passthrough.ts";
import { SURFACE } from "../rpc/surface.ts";
import { POLICY } from "../authz/policy-map.ts";
import { clearOwnerCache } from "../authz/resolve.ts";

import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import { FleetService } from "../gen/engram/app/v1/fleet_pb.ts";
import { ImageService } from "../gen/engram/app/v1/image_pb.ts";
import {
  GetSessionResponseSchema,
  ListSessionsResponseSchema,
} from "../gen/engram/app/v1/session_pb.ts";
import {
  ListHostsResponseSchema,
} from "../gen/engram/app/v1/fleet_pb.ts";
import {
  ListEnabledImagesResponseSchema,
} from "../gen/engram/app/v1/image_pb.ts";

// ---------------------------------------------------------------------------
// Test IDs
// ---------------------------------------------------------------------------

const MEMBER_A = "member-a-id";
const MEMBER_B = "member-b-id";
const ADMIN_ID  = "admin-id";
const SESSION_OF_A = "session-owned-by-a";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Build a getSession stub for the given user (or return null for anon). */
function makeGetSession(
  userId: string | null,
  role: "user" | "admin" = "user",
): GetSession {
  return async (_headers) => {
    if (!userId) return null;
    return { user: { id: userId, role, email: `${userId}@test.invalid` } };
  };
}

/**
 * Fake owner resolver: SESSION_OF_A → MEMBER_A, everything else → null.
 * No DB required.
 */
const fakeResolveOwner: ResolveOwner = async (sessionId) => {
  if (sessionId === SESSION_OF_A) return MEMBER_A;
  return null;
};

interface TestOrchestrator {
  /** Base URL WITHOUT /rpc suffix — use makeTransport() which adds /rpc. */
  serverUrl: string;
  close: () => Promise<void>;
  upstreamCallCount: { value: number };
}

async function spawnOrchestrator(
  getSession: GetSession,
): Promise<TestOrchestrator> {
  const upstreamCallCount = { value: 0 };

  const upstream = createRouterTransport((router: ConnectRouter) => {
    router.service(SessionService, {
      getSession: (_req: unknown) => {
        upstreamCallCount.value++;
        return create(GetSessionResponseSchema, {});
      },
      listSessions: (_req: unknown) => {
        upstreamCallCount.value++;
        return create(ListSessionsResponseSchema, { sessions: [] });
      },
    });
    router.service(FleetService, {
      listHosts: (_req: unknown) => {
        upstreamCallCount.value++;
        return create(ListHostsResponseSchema, { hosts: [] });
      },
    });
    router.service(ImageService, {
      listEnabledImages: (_req: unknown) => {
        upstreamCallCount.value++;
        return create(ListEnabledImagesResponseSchema, { images: [] });
      },
    });
  });

  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));

  const server = buildServer(app, (router: ConnectRouter) => {
    registerPassthrough(router, SURFACE, upstream, getSession, fakeResolveOwner);
  });

  return new Promise<TestOrchestrator>((resolve, reject) => {
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address() as import("net").AddressInfo;
      const serverUrl = `http://127.0.0.1:${addr.port}`;
      resolve({
        serverUrl,
        upstreamCallCount,
        close: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          }),
      });
    });
    server.on("error", reject);
  });
}

/**
 * Build a Connect transport pointing to the orchestrator's /rpc prefix.
 * The Connect adapter lives at /rpc, so the client baseUrl must include it.
 * Connect protocol (not gRPC-Web) avoids gRPC-Web trailer-frame requirements in tests.
 */
function makeTransport(serverUrl: string): Transport {
  return createConnectTransport({
    baseUrl: `${serverUrl}/rpc`,
    httpVersion: "1.1",
  });
}

/** Expect a Connect call to throw with a specific error code. */
async function expectCode(
  call: () => Promise<unknown>,
  code: Code,
): Promise<void> {
  try {
    await call();
    throw new Error(
      `Expected ConnectError with code ${Code[code]}, but no error was thrown`,
    );
  } catch (err) {
    if (!(err instanceof ConnectError)) {
      throw new Error(
        `Expected ConnectError with code ${Code[code]}, got: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
    expect(err.code).toBe(code);
  }
}

// ---------------------------------------------------------------------------
// Matrix
// ---------------------------------------------------------------------------

afterEach(() => {
  clearOwnerCache();
});

describe("authz.matrix — unauthenticated", () => {
  test("unauthenticated call to GetSession → Unauthenticated", async () => {
    const orch = await spawnOrchestrator(makeGetSession(null));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      await expectCode(
        () => client.getSession({ sessionId: SESSION_OF_A }),
        Code.Unauthenticated,
      );
      // Upstream must never be reached on auth failure.
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });

  test("unauthenticated call to ListHosts → Unauthenticated", async () => {
    const orch = await spawnOrchestrator(makeGetSession(null));
    try {
      const client = createClient(FleetService, makeTransport(orch.serverUrl));
      await expectCode(() => client.listHosts({}), Code.Unauthenticated);
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });
});

describe("authz.matrix — member accessing own session", () => {
  test("member A calls GetSession on their own session → allowed", async () => {
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      // fakeResolveOwner maps SESSION_OF_A → MEMBER_A, so member A can read it.
      const result = await client.getSession({ sessionId: SESSION_OF_A });
      expect(result).toBeDefined();
      expect(orch.upstreamCallCount.value).toBe(1);
    } finally {
      await orch.close();
    }
  });
});

describe("authz.matrix — member cross-session access (anti-enumeration)", () => {
  test("member B calling GetSession on session owned by A → NotFound", async () => {
    // Member B calls GetSession with SESSION_OF_A.
    // fakeResolveOwner returns MEMBER_A; CASL check: member B cannot 'read' Session
    // where createdByUserId = MEMBER_A. Returns NotFound (anti-enumeration).
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_B, "user"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      await expectCode(
        () => client.getSession({ sessionId: SESSION_OF_A }),
        Code.NotFound,
      );
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });

  test("member calling GetSession on unknown session → NotFound", async () => {
    // fakeResolveOwner returns null for unknown sessions.
    // null owner means no task row exists; notFound.
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      await expectCode(
        () => client.getSession({ sessionId: "nonexistent" }),
        Code.NotFound,
      );
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });
});

describe("authz.matrix — member accessing admin-only resource", () => {
  test("member calls FleetService.ListHosts → PermissionDenied", async () => {
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(FleetService, makeTransport(orch.serverUrl));
      await expectCode(() => client.listHosts({}), Code.PermissionDenied);
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });

  test("member calls SessionService.ListSessions (admin raw view) → PermissionDenied", async () => {
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      await expectCode(() => client.listSessions({}), Code.PermissionDenied);
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      await orch.close();
    }
  });
});

describe("authz.matrix — admin access", () => {
  test("admin calls FleetService.ListHosts → allowed", async () => {
    const orch = await spawnOrchestrator(makeGetSession(ADMIN_ID, "admin"));
    try {
      const client = createClient(FleetService, makeTransport(orch.serverUrl));
      const result = await client.listHosts({});
      expect(result).toBeDefined();
      expect(orch.upstreamCallCount.value).toBe(1);
    } finally {
      await orch.close();
    }
  });

  test("admin calls SessionService.ListSessions → allowed", async () => {
    const orch = await spawnOrchestrator(makeGetSession(ADMIN_ID, "admin"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      const result = await client.listSessions({});
      expect(result).toBeDefined();
      expect(orch.upstreamCallCount.value).toBe(1);
    } finally {
      await orch.close();
    }
  });

  test("admin calls GetSession on any session → allowed", async () => {
    const orch = await spawnOrchestrator(makeGetSession(ADMIN_ID, "admin"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      // Admin has manage:all — session ownership check still runs but passes.
      const result = await client.getSession({ sessionId: SESSION_OF_A });
      expect(result).toBeDefined();
      expect(orch.upstreamCallCount.value).toBe(1);
    } finally {
      await orch.close();
    }
  });
});

describe("authz.matrix — member reads image catalog", () => {
  test("member calls ImageService.ListEnabledImages → allowed (read:EnabledImage)", async () => {
    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(ImageService, makeTransport(orch.serverUrl));
      const result = await client.listEnabledImages({});
      expect(result).toBeDefined();
      expect(orch.upstreamCallCount.value).toBe(1);
    } finally {
      await orch.close();
    }
  });
});

// ---------------------------------------------------------------------------
// Fix 1b: POLICY structural guard — every Session-subject entry must carry
// sessionIdField, or the flat string-subject branch in the gate would be
// reached and CASL would return true for any member (latent fail-open).
// ---------------------------------------------------------------------------

describe("POLICY structural: every Session-subject entry carries sessionIdField", () => {
  test("no Session-subject entry lacks sessionIdField", () => {
    const violations: string[] = [];
    for (const [key, entry] of Object.entries(POLICY)) {
      if (entry.subject === "Session" && !entry.sessionIdField) {
        violations.push(key);
      }
    }
    expect(violations).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Fix 3: Fail-closed — a method with no POLICY entry must yield PermissionDenied
// with zero upstream calls. POLICY is a module constant so we can't inject a
// replacement map; instead we temporarily delete an entry, verify the gate
// fires, then restore it.
// ---------------------------------------------------------------------------

describe("authz.matrix — fail-closed: no POLICY entry → PermissionDenied (zero upstream)", () => {
  test("erasing a POLICY entry makes the gate deny with zero upstream calls", async () => {
    // Temporarily remove GetSession from POLICY to prove no-entry → denied.
    const KEY = "SessionService.GetSession";
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const saved = (POLICY as Record<string, any>)[KEY];
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    delete (POLICY as Record<string, any>)[KEY];

    const orch = await spawnOrchestrator(makeGetSession(MEMBER_A, "user"));
    try {
      const client = createClient(SessionService, makeTransport(orch.serverUrl));
      await expectCode(
        () => client.getSession({ sessionId: SESSION_OF_A }),
        Code.PermissionDenied,
      );
      // The upstream must never be reached when the policy entry is absent.
      expect(orch.upstreamCallCount.value).toBe(0);
    } finally {
      // Restore the entry so other tests are not affected.
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (POLICY as Record<string, any>)[KEY] = saved;
      await orch.close();
    }
  });
});
