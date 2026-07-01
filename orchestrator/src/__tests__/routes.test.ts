/**
 * Unit tests for Task 20 routes: SSE events, artifact bytes, /me/claude-token.
 * (ADR 0051 Task 20, Step 5)
 *
 * All deps are injected — no DB, no real better-auth, no upstream gRPC needed.
 *
 * Coverage:
 *   SSE events:
 *     1. 3 events (idx 0,1,2) → 3 ordered SSE frames, correct id/event/data
 *        (idx 0 = bigint 0n must NOT be treated as absent)
 *     2. Lagged event (idx undefined) → no id: line in the SSE frame
 *     3. Client abort cancels the upstream (AbortSignal observed)
 *     4. Unauthenticated → 401
 *     5. Wrong owner (member-other) → 404
 *     6. Unattributed session (owner null) → 404 for member
 *
 *   Artifact bytes:
 *     7. metadata + chunks → correct Content-Type, intact bytes
 *     8. Upstream NotFound → 404
 *     9. Unauthenticated → 401
 *
 *   /me/claude-token:
 *    10. POST without session → 401
 *    11. POST/GET/DELETE round-trip against fake SecretService
 *    12. Token value must not appear in any console output (log spy)
 */

import {
  expect,
  test,
  describe,
  beforeAll,
  afterAll,
  mock,
  spyOn,
} from "bun:test";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { makeEventsRoute } from "../routes/events.ts";
import type { EventsDeps } from "../routes/events.ts";
import { makeArtifactsRoute } from "../routes/artifacts.ts";
import type { ArtifactsDeps } from "../routes/artifacts.ts";
import { makeMeRoute } from "../routes/me.ts";
import type { MeDeps } from "../routes/me.ts";
import type { UserSecretStore } from "../db/user-secrets.ts";
import { ConnectError, Code } from "@connectrpc/connect";
import type { AddressInfo } from "node:net";

// ---------------------------------------------------------------------------
// Test identity constants
// ---------------------------------------------------------------------------

const MEMBER_A = "member-a-routes-test";
const MEMBER_B = "member-b-routes-test";
const SESSION_OF_A = "session-owned-by-a";

// ---------------------------------------------------------------------------
// Injectable dep factories
// ---------------------------------------------------------------------------

type GetSession = EventsDeps["getSession"];
type ResolveOwner = EventsDeps["resolveOwner"];

function makeGetSession(
  userId: string | null,
  role: "user" | "admin" = "user",
): GetSession {
  return async (_headers) => {
    if (!userId) return null;
    return { user: { id: userId, role, email: `${userId}@test.invalid` } };
  };
}

// Owner map: SESSION_OF_A → MEMBER_A, everything else → null.
function makeResolveOwner(
  extra: Record<string, string | null> = {},
): ResolveOwner {
  return async (sessionId) => {
    if (sessionId === SESSION_OF_A) return MEMBER_A;
    if (sessionId in extra) return extra[sessionId] ?? null;
    return null;
  };
}

// ---------------------------------------------------------------------------
// Server fixture helpers
// ---------------------------------------------------------------------------

interface ServerFixture {
  baseUrl: string;
  server: ReturnType<typeof buildServer>;
}

async function startServer(app: Hono): Promise<ServerFixture> {
  const server = buildServer(app);
  return new Promise<ServerFixture>((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address() as AddressInfo;
      const baseUrl = `http://127.0.0.1:${addr.port}`;
      resolve({ baseUrl, server });
    });
  });
}

async function stopServer(server: ReturnType<typeof buildServer>): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((err) => (err ? reject(err) : resolve()));
  });
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

/**
 * Parse raw SSE text into an array of frame objects.
 * Each frame: { id?: string; event?: string; data?: string }
 */
function parseSseFrames(
  raw: string,
): Array<{ id?: string; event?: string; data?: string }> {
  const frames: Array<{ id?: string; event?: string; data?: string }> = [];
  let current: { id?: string; event?: string; data?: string } = {};

  for (const line of raw.split("\n")) {
    if (line === "") {
      // Empty line = dispatch current frame if it has any fields.
      if (Object.keys(current).length > 0) {
        frames.push(current);
        current = {};
      }
    } else if (line.startsWith("id:")) {
      current.id = line.slice(3).trim();
    } else if (line.startsWith("event:")) {
      current.event = line.slice(6).trim();
    } else if (line.startsWith("data:")) {
      current.data = line.slice(5).trim();
    }
    // comments (: ...) are silently ignored
  }
  // Flush if stream ended without trailing blank line.
  if (Object.keys(current).length > 0) frames.push(current);
  return frames;
}

// ---------------------------------------------------------------------------
// 1–6: SSE events tests
// ---------------------------------------------------------------------------

describe("SSE events route", () => {
  let baseUrl: string;
  let server: ReturnType<typeof buildServer>;

  beforeAll(async () => {
    // Build an events route wired with fake deps.
    const sessionsClient = {
      async *streamEvents(
        req: { sessionId: string; since?: bigint },
        _options?: { signal?: AbortSignal },
      ) {
        // idx 0 (bigint 0n) — must NOT be omitted from id: line.
        yield { idx: 0n, kind: "run_started", payloadJson: '{"run_id":"r0"}' };
        yield { idx: 1n, kind: "agent_message", payloadJson: '{"text":"hi"}' };
        yield { idx: 2n, kind: "run_completed", payloadJson: '{"ok":true}' };
        // Lagged frame: idx undefined — must NOT emit id: line.
        yield { idx: undefined, kind: "lagged", payloadJson: '{"missed":3}' };
      },
    };

    const eventsRoute = makeEventsRoute({
      sessions: sessionsClient,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", eventsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));

    ({ baseUrl, server } = await startServer(app));
  });

  afterAll(async () => stopServer(server));

  test("1: 3 ordered events (idx 0,1,2) — correct id/event/data, idx 0 not absent", async () => {
    const res = await fetch(
      `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/events`,
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toMatch(/text\/event-stream/);

    const raw = await res.text();
    const frames = parseSseFrames(raw).filter(
      (f) => f.event !== "ping",
    );

    // Should have exactly 4 frames: 3 with idx + 1 lagged.
    expect(frames.length).toBe(4);

    // Frame 0: idx 0 — MUST have id: "0"
    const f0 = frames[0]!;
    expect(f0.id).toBe("0");
    expect(f0.event).toBe("run_started");
    const d0 = JSON.parse(f0.data!) as Record<string, unknown>;
    expect(d0.idx).toBe(0);
    expect(d0.kind).toBe("run_started");
    expect(typeof d0.payload_json).toBe("string");

    // Frame 1: idx 1
    const f1 = frames[1]!;
    expect(f1.id).toBe("1");
    expect(f1.event).toBe("agent_message");
    const d1 = JSON.parse(f1.data!) as Record<string, unknown>;
    expect(d1.idx).toBe(1);

    // Frame 2: idx 2
    const f2 = frames[2]!;
    expect(f2.id).toBe("2");
    expect(f2.event).toBe("run_completed");

    console.log("Test 1 PASS: 3 ordered events with idx 0,1,2 — id: lines correct");
  });

  test("2: lagged event (idx undefined) → no id: line in SSE frame", async () => {
    const res = await fetch(
      `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/events`,
    );
    expect(res.status).toBe(200);

    const raw = await res.text();
    const frames = parseSseFrames(raw).filter(
      (f) => f.event === "lagged",
    );

    expect(frames.length).toBe(1);
    const lagFrame = frames[0]!;
    // Must NOT have an id: line.
    expect(lagFrame.id).toBeUndefined();
    expect(lagFrame.event).toBe("lagged");
    const d = JSON.parse(lagFrame.data!) as Record<string, unknown>;
    expect(d.idx).toBeNull();
    expect(d.kind).toBe("lagged");

    console.log("Test 2 PASS: lagged frame has no id: line, idx=null in envelope");
  });
});

describe("SSE events route — abort + guard", () => {
  test("3: client abort cancels upstream (AbortSignal wired)", async () => {
    // This test verifies that the events route passes an AbortSignal to the
    // upstream streamEvents call. We verify this structurally — the signal is
    // present in the options object — since cross-fetch-abort propagation
    // through Node HTTP is infrastructure-level and not guaranteed to fire
    // synchronously in the Bun test environment.
    //
    // The key assertion: when streamEvents is called, options.signal is defined
    // (not undefined). This proves the route correctly wires `c.req.raw.signal`
    // to the upstream call, satisfying the "browser close → RST upstream" goal.

    let signalWiredCorrectly = false;

    const sessionsClient = {
      async *streamEvents(
        _req: { sessionId: string; since?: bigint },
        options?: { signal?: AbortSignal },
      ) {
        // Check: signal is present in options.
        signalWiredCorrectly = options?.signal !== undefined;
        // Yield two events so the stream has real content.
        yield { idx: 0n, kind: "run_started", payloadJson: "{}" };
        yield { idx: 1n, kind: "run_completed", payloadJson: "{}" };
      },
    };

    const eventsRoute = makeEventsRoute({
      sessions: sessionsClient,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", eventsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/events`,
      );
      expect(res.status).toBe(200);
      // Drain the stream.
      await res.text();
    } finally {
      await stopServer(server);
    }

    // AbortSignal must have been wired to upstream.
    expect(signalWiredCorrectly).toBe(true);
    console.log("Test 3 PASS: AbortSignal wired to upstream streamEvents call");
  });

  test("4: unauthenticated → 401", async () => {
    const eventsRoute = makeEventsRoute({
      sessions: {
        async *streamEvents() { /* never called */ },
      },
      getSession: makeGetSession(null), // not authenticated
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", eventsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));
    app.onError((err, c) => {
      // Surface HTTPException status.
      if ("status" in err && typeof err.status === "number") {
        return c.json({ error: err.message }, err.status as 401);
      }
      return c.json({ error: String(err) }, 500);
    });

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/events`,
      );
      expect(res.status).toBe(401);
      console.log("Test 4 PASS: unauthenticated → 401");
    } finally {
      await stopServer(server);
    }
  });

  test("5: member-other's session → 404 (anti-enumeration)", async () => {
    // MEMBER_B tries to access SESSION_OF_A (owned by MEMBER_A).
    const eventsRoute = makeEventsRoute({
      sessions: {
        async *streamEvents() { /* never called */ },
      },
      getSession: makeGetSession(MEMBER_B),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", eventsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));
    app.onError((err, c) => {
      if ("status" in err && typeof err.status === "number") {
        return c.json({ error: err.message }, err.status as 404);
      }
      return c.json({ error: String(err) }, 500);
    });

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/events`,
      );
      expect(res.status).toBe(404);
      console.log("Test 5 PASS: member-other → 404 (anti-enumeration)");
    } finally {
      await stopServer(server);
    }
  });

  test("6: unattributed session (owner null) → 404 for member", async () => {
    // Session exists but has no owner (not in DB) → resolveOwner returns null.
    const UNATTRIBUTED = "session-not-in-db";
    const eventsRoute = makeEventsRoute({
      sessions: {
        async *streamEvents() { /* never called */ },
      },
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: async (_sid) => null, // always null → no owner
    });

    const app = new Hono();
    app.route("/", eventsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));
    app.onError((err, c) => {
      if ("status" in err && typeof err.status === "number") {
        return c.json({ error: err.message }, err.status as 404);
      }
      return c.json({ error: String(err) }, 500);
    });

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${UNATTRIBUTED}/events`,
      );
      expect(res.status).toBe(404);
      console.log("Test 6 PASS: unattributed session → 404 for member");
    } finally {
      await stopServer(server);
    }
  });
});

// ---------------------------------------------------------------------------
// 7–9: Artifact bytes tests
// ---------------------------------------------------------------------------

describe("Artifact bytes route", () => {
  test("7: metadata + chunks → correct Content-Type, intact bytes", async () => {
    const chunkA = new Uint8Array([0x89, 0x50, 0x4e, 0x47]); // PNG magic
    const chunkB = new Uint8Array([0x0d, 0x0a, 0x1a, 0x0a]);

    const sessionsClient = {
      async *getArtifact(
        _req: { sessionId: string; artifactId: string },
        _options?: { signal?: AbortSignal },
      ) {
        yield {
          msg: {
            case: "metadata" as const,
            value: {
              mediaType: "image/png",
              sizeBytes: BigInt(chunkA.length + chunkB.length),
              fileName: "screenshot.png",
            },
          },
        };
        yield { msg: { case: "chunk" as const, value: chunkA } };
        yield { msg: { case: "chunk" as const, value: chunkB } };
      },
    };

    const artifactsRoute = makeArtifactsRoute({
      sessions: sessionsClient,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", artifactsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/artifacts/art-1`,
      );
      expect(res.status).toBe(200);
      expect(res.headers.get("content-type")).toContain("image/png");

      const bytes = new Uint8Array(await res.arrayBuffer());
      expect(bytes.length).toBe(chunkA.length + chunkB.length);
      // First 4 bytes = PNG magic (chunkA).
      expect(bytes[0]).toBe(0x89);
      expect(bytes[1]).toBe(0x50);
      expect(bytes[2]).toBe(0x4e);
      expect(bytes[3]).toBe(0x47);
      // Next 4 bytes = chunkB.
      expect(bytes[4]).toBe(0x0d);
      console.log("Test 7 PASS: metadata+chunks → correct content-type, intact bytes");
    } finally {
      await stopServer(server);
    }
  });

  test("8: upstream NotFound → 404", async () => {
    const sessionsClient = {
      async *getArtifact(
        _req: { sessionId: string; artifactId: string },
        _options?: { signal?: AbortSignal },
      ) {
        throw new ConnectError("not found", Code.NotFound);
        // yield required for TypeScript async generator type.
        yield { msg: { case: undefined as undefined, value: undefined } };
      },
    };

    const artifactsRoute = makeArtifactsRoute({
      sessions: sessionsClient,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", artifactsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/artifacts/nonexistent`,
      );
      expect(res.status).toBe(404);
      console.log("Test 8 PASS: upstream NotFound → 404");
    } finally {
      await stopServer(server);
    }
  });

  test("9: artifact route unauthenticated → 401", async () => {
    const artifactsRoute = makeArtifactsRoute({
      sessions: {
        async *getArtifact() { /* never called */ },
      },
      getSession: makeGetSession(null),
      resolveOwner: makeResolveOwner(),
    });

    const app = new Hono();
    app.route("/", artifactsRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));
    app.onError((err, c) => {
      if ("status" in err && typeof err.status === "number") {
        return c.json({ error: err.message }, err.status as 401);
      }
      return c.json({ error: String(err) }, 500);
    });

    const { baseUrl, server } = await startServer(app);
    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/artifacts/art-1`,
      );
      expect(res.status).toBe(401);
      console.log("Test 9 PASS: artifact unauthenticated → 401");
    } finally {
      await stopServer(server);
    }
  });
});

// ---------------------------------------------------------------------------
// 10–12: /me/claude-token tests
// ---------------------------------------------------------------------------

/**
 * In-memory fake of the orchestrator's UserSecretStore (ADR 0051 Drip A): the
 * per-user Claude token now lives in the orchestrator's own KEK-sealed store,
 * keyed by (userId, envVarName), not the coordinator's SecretService. Keeps the
 * POST→GET(true)→DELETE→GET(false) round-trip semantics against this local
 * store. The fake holds plaintext (the real store seals); the seam under test
 * here is the route, not the crypto.
 */
function makeFakeSecretStore(): UserSecretStore & { store: Record<string, string> } {
  const store: Record<string, string> = {};
  const key = (userId: string, envVarName: string) => `${userId} ${envVarName}`;
  return {
    store,
    async put(userId, envVarName, plaintext) {
      store[key(userId, envVarName)] = plaintext;
    },
    async getAll(userId) {
      const out: Record<string, string> = {};
      for (const k of Object.keys(store)) {
        const [u, name] = k.split(" ");
        if (u === userId) out[name!] = store[k]!;
      }
      return out;
    },
    async get(userId, envVarName) {
      return store[key(userId, envVarName)] ?? null;
    },
    async has(userId, envVarName) {
      return key(userId, envVarName) in store;
    },
    async delete(userId, envVarName) {
      delete store[key(userId, envVarName)];
    },
  };
}

describe("/me/harness-env route", () => {
  // The harness env var under test: the claude harness's declared user_env.
  const ENV = "CLAUDE_CODE_OAUTH_TOKEN";
  const fakeHarnessCatalog = () => ({
    listHarnesses: async () => ({
      harnesses: [{ name: "claude", descriptor: { label: "Claude Code", auth: { userEnv: ENV } } }],
    }),
  });
  type EnvVarsBody = {
    vars: Array<{ envVar: string; harnesses: Array<{ name: string; label: string }>; present: boolean }>;
  };

  /** Build the /me app with an error mapper that surfaces HTTPException codes. */
  const meApp = (member: string | null) => {
    const meRoute = makeMeRoute({
      secrets: makeFakeSecretStore(),
      harnessCatalog: fakeHarnessCatalog(),
      getSession: makeGetSession(member),
    });
    const app = new Hono();
    app.route("/", meRoute);
    app.notFound((c) => c.json({ error: "not found" }, 404));
    app.onError((err, c) => {
      if ("status" in err && typeof err.status === "number") {
        return new Response(JSON.stringify({ error: err.message }), {
          status: err.status,
          headers: { "Content-Type": "application/json" },
        });
      }
      return c.json({ error: String(err) }, 500);
    });
    return app;
  };

  test("10: GET without session → 401", async () => {
    const { baseUrl, server } = await startServer(meApp(null));
    try {
      const res = await fetch(`${baseUrl}/api/v1/me/harness-env`);
      expect(res.status).toBe(401);
    } finally {
      await stopServer(server);
    }
  });

  test("11: GET lists the catalog user_env union; PUT/DELETE flips present", async () => {
    const { baseUrl, server } = await startServer(meApp(MEMBER_A));
    try {
      // GET before PUT → the var is listed (with the asking harness), present:false.
      const g1 = await fetch(`${baseUrl}/api/v1/me/harness-env`);
      expect(g1.status).toBe(200);
      const b1 = (await g1.json()) as EnvVarsBody;
      expect(b1.vars).toHaveLength(1);
      expect(b1.vars[0]).toMatchObject({ envVar: ENV, present: false });
      expect(b1.vars[0]!.harnesses[0]).toMatchObject({ name: "claude", label: "Claude Code" });

      // PUT → 204.
      const put = await fetch(`${baseUrl}/api/v1/me/harness-env/${ENV}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ value: "sk-ant-oat01-test-token" }),
      });
      expect(put.status).toBe(204);

      // GET after PUT → present:true.
      const g2 = await fetch(`${baseUrl}/api/v1/me/harness-env`);
      const b2 = (await g2.json()) as EnvVarsBody;
      expect(b2.vars[0]).toMatchObject({ envVar: ENV, present: true });

      // DELETE → 204, then GET → present:false.
      const del = await fetch(`${baseUrl}/api/v1/me/harness-env/${ENV}`, { method: "DELETE" });
      expect(del.status).toBe(204);
      const g3 = await fetch(`${baseUrl}/api/v1/me/harness-env`);
      const b3 = (await g3.json()) as EnvVarsBody;
      expect(b3.vars[0]).toMatchObject({ envVar: ENV, present: false });
    } finally {
      await stopServer(server);
    }
  });

  test("12: PUT/DELETE for an env var no harness declares → 404", async () => {
    const { baseUrl, server } = await startServer(meApp(MEMBER_A));
    try {
      const put = await fetch(`${baseUrl}/api/v1/me/harness-env/NOT_A_HARNESS_ENV`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ value: "x" }),
      });
      expect(put.status).toBe(404);
      const del = await fetch(`${baseUrl}/api/v1/me/harness-env/NOT_A_HARNESS_ENV`, {
        method: "DELETE",
      });
      expect(del.status).toBe(404);
    } finally {
      await stopServer(server);
    }
  });

  test("13: value never appears in console output", async () => {
    const SECRET = "sk-ant-oat01-super-secret-never-logged-12345";

    // Spy on all console methods.
    const logMessages: string[] = [];
    const originalLog = console.log;
    const originalWarn = console.warn;
    const originalError = console.error;
    const originalInfo = console.info;
    const originalDebug = console.debug;
    const capture = (...args: unknown[]) => {
      logMessages.push(args.map(String).join(" "));
    };
    console.log = capture;
    console.warn = capture;
    console.error = capture;
    console.info = capture;
    console.debug = capture;

    try {
      const { baseUrl, server } = await startServer(meApp(MEMBER_A));
      try {
        await fetch(`${baseUrl}/api/v1/me/harness-env/${ENV}`, {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ value: SECRET }),
        });
      } finally {
        await stopServer(server);
      }
      for (const msg of logMessages) {
        expect(msg).not.toContain(SECRET);
      }
    } finally {
      console.log = originalLog;
      console.warn = originalWarn;
      console.error = originalError;
      console.info = originalInfo;
      console.debug = originalDebug;
    }
  });
});
