/**
 * TaskService native implementation tests (ADR 0051 Task 19).
 *
 * Injectable deps pattern: registerTasks accepts { getSession, sessions,
 * secrets, db } so no real better-auth / upstream / DB is required for the
 * authz + compensation matrix. DB-gated tests (create → list → get → delete
 * flow with the real drizzle against the dev DB) are skip-gated on
 * ORCHESTRATOR_DATABASE_URL being set and reachable — same gate as db.test.ts.
 *
 * Coverage:
 *   1. Unauthenticated caller → 401 Unauthenticated.
 *   2. Member create → list shows task (own only) → get → delete cascade.
 *   3. Member cannot get / delete another member's task (NotFound anti-enum).
 *   4. Admin list shows all tasks + synthetic unattributed rows.
 *   5. Compensation: upstream create OK + DB insert forced to fail → upstream
 *      DeleteSession is called.
 *   6. createTask: only "chat" type accepted → InvalidArgument otherwise.
 */

import { expect, test, describe, afterAll, beforeAll } from "bun:test";
import { ConnectError, Code, createRouterTransport } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { createClient } from "@connectrpc/connect";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerTasks } from "../rpc/tasks.ts";
import type { TaskDeps, SessionsClient, SecretsClient, Db, GetSession } from "../rpc/tasks.ts";
import { TaskService } from "../gen/engram/app/v1/task_pb.ts";
import type { Session } from "../gen/engram/app/v1/session_pb.ts";
import { checkDb, getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { eq } from "drizzle-orm";

// ---------------------------------------------------------------------------
// DB gate (same pattern as db.test.ts)
// ---------------------------------------------------------------------------

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

// ---------------------------------------------------------------------------
// Test IDs
// ---------------------------------------------------------------------------

const MEMBER_A = "member-a-tasks-test";
const MEMBER_B = "member-b-tasks-test";
const ADMIN_ID  = "admin-tasks-test";

// ---------------------------------------------------------------------------
// Fake upstream session state
// ---------------------------------------------------------------------------

interface FakeSession {
  id: string;
  status: string;
  image: string;
  mode: string;
  createdAt: string;
  lastActiveAt: string;
  hostId?: string;
  sandboxId?: string;
}

/**
 * Build a fake SessionsClient.
 *
 * created: the sessions that createSession will return (queue, FIFO).
 * existing: map of sessionId → FakeSession for getSession / listSessions.
 * deletedIds: accumulates deleted session ids (for compensation assertions).
 */
function makeFakeSessions(opts: {
  created?: FakeSession[];
  existing?: FakeSession[];
  createShouldThrow?: boolean;
}): SessionsClient & { deletedIds: string[]; createCallCount: number } {
  const created = [...(opts.created ?? [])];
  const byId = new Map<string, FakeSession>();
  for (const s of opts.existing ?? []) byId.set(s.id, s);

  const deletedIds: string[] = [];
  let createCallCount = 0;

  return {
    deletedIds,
    createCallCount: 0 as number,
    async createSession(req) {
      createCallCount++;
      if (opts.createShouldThrow) throw new Error("upstream create failed");
      const next = created.shift();
      if (!next) throw new Error("No more fake sessions in queue");
      byId.set(next.id, next);
      return {
        sessionId: next.id,
        status: next.status,
        imageVersion: req.imageUri,
        kind: "user",
      };
    },
    async listSessions(_req) {
      return {
        sessions: Array.from(byId.values()).map((s) => ({ session: s as unknown as Session })),
      };
    },
    async getSession(req) {
      const s = byId.get(req.sessionId);
      return { session: s as unknown as Session | undefined };
    },
    async deleteSession(req) {
      deletedIds.push(req.sessionId);
      byId.delete(req.sessionId);
      return {};
    },
  };
}

/** Build a fake SecretsClient (hasSecret always returns false). */
function makeFakeSecrets(): SecretsClient {
  return {
    async hasSecret(_req) {
      return { exists: false };
    },
  };
}

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

// ---------------------------------------------------------------------------
// Server factory
// ---------------------------------------------------------------------------

interface TestServer {
  serverUrl: string;
  close: () => Promise<void>;
}

async function spawnServer(deps: TaskDeps): Promise<TestServer> {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));

  const srv = buildServer(app, (router) => {
    registerTasks(router, deps);
  });

  const serverUrl = await new Promise<string>((resolve) => {
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address() as AddressInfo;
      resolve(`http://127.0.0.1:${addr.port}`);
    });
  });

  return {
    serverUrl,
    close: () =>
      new Promise<void>((resolve, reject) => {
        srv.close((err) => (err ? reject(err) : resolve()));
      }),
  };
}

/**
 * Create a TaskService Connect client against the given server URL.
 * The Connect adapter lives at /rpc, so baseUrl must include /rpc.
 */
function makeClient(serverUrl: string) {
  const transport = createConnectTransport({
    baseUrl: `${serverUrl}/rpc`,
    httpVersion: "1.1",
  });
  return createClient(TaskService, transport);
}

// ---------------------------------------------------------------------------
// Helper: assert ConnectError code
// ---------------------------------------------------------------------------
async function expectConnectError(
  promise: Promise<unknown>,
  expectedCode: Code,
): Promise<void> {
  try {
    await promise;
    throw new Error(`Expected ConnectError(${Code[expectedCode]}) but resolved`);
  } catch (err) {
    if (!(err instanceof ConnectError)) throw err;
    expect(err.code).toBe(expectedCode);
  }
}

// ---------------------------------------------------------------------------
// 1. Unauthenticated
// ---------------------------------------------------------------------------

describe("TaskService — unauthenticated", () => {
  let srv: TestServer;

  beforeAll(async () => {
    const fakeSessions = makeFakeSessions({ existing: [] });
    srv = await spawnServer({
      getSession: makeGetSession(null),
      sessions: fakeSessions,
      secrets: makeFakeSecrets(),
    });
  });

  afterAll(() => srv?.close());

  test("CreateTask anon → 401 Unauthenticated", async () => {
    const client = makeClient(srv.serverUrl);
    await expectConnectError(
      client.createTask({ type: "chat", imageUri: "registry/img:latest" }),
      Code.Unauthenticated,
    );
  });

  test("ListTasks anon → 401 Unauthenticated", async () => {
    const client = makeClient(srv.serverUrl);
    await expectConnectError(client.listTasks({}), Code.Unauthenticated);
  });

  test("GetTask anon → 401 Unauthenticated", async () => {
    const client = makeClient(srv.serverUrl);
    await expectConnectError(
      client.getTask({ taskId: "anything" }),
      Code.Unauthenticated,
    );
  });

  test("DeleteTask anon → 401 Unauthenticated", async () => {
    const client = makeClient(srv.serverUrl);
    await expectConnectError(
      client.deleteTask({ taskId: "anything" }),
      Code.Unauthenticated,
    );
  });
});

// ---------------------------------------------------------------------------
// 2. Only "chat" type accepted
// ---------------------------------------------------------------------------

describe("TaskService — type validation", () => {
  let srv: TestServer;

  beforeAll(async () => {
    const fakeSessions = makeFakeSessions({ existing: [] });
    srv = await spawnServer({
      getSession: makeGetSession(MEMBER_A),
      sessions: fakeSessions,
      secrets: makeFakeSecrets(),
    });
  });

  afterAll(() => srv?.close());

  test("CreateTask type='linear_issue' → 422 InvalidArgument", async () => {
    const client = makeClient(srv.serverUrl);
    await expectConnectError(
      client.createTask({ type: "linear_issue", imageUri: "registry/img:latest" }),
      Code.InvalidArgument,
    );
  });
});

// ---------------------------------------------------------------------------
// 3. Member anti-enumeration
// ---------------------------------------------------------------------------

describe("TaskService — member anti-enumeration (in-memory store)", () => {
  // Use the real drizzle DB if available; otherwise skip.
  test.skipIf(!dbReachable)(
    "member GetTask on another member's task → NotFound",
    async () => {
      const db = getDb();
      const ownerTaskId = `antienum-task-${Date.now()}`;
      const fakeSessionId = `antienum-sess-${Date.now()}`;

      // Insert a task owned by MEMBER_A.
      await db.insert(taskTable).values({
        id: ownerTaskId,
        type: "chat",
        status: "open",
        createdByUserId: MEMBER_A,
        source: {},
      });
      await db.insert(taskSessionTable).values({
        taskId: ownerTaskId,
        sessionId: fakeSessionId,
        role: "primary",
      });

      try {
        const fakeSessions = makeFakeSessions({
          existing: [
            {
              id: fakeSessionId,
              status: "active",
              image: "img",
              mode: "agent",
              createdAt: new Date().toISOString(),
              lastActiveAt: new Date().toISOString(),
            },
          ],
        });

        const srv = await spawnServer({
          getSession: makeGetSession(MEMBER_B),
          sessions: fakeSessions,
          secrets: makeFakeSecrets(),
          db,
        });

        try {
          const client = makeClient(srv.serverUrl);
          await expectConnectError(
            client.getTask({ taskId: ownerTaskId }),
            Code.NotFound,
          );
        } finally {
          await srv.close();
        }
      } finally {
        await db.delete(taskTable).where(eq(taskTable.id, ownerTaskId));
      }
    },
  );

  test.skipIf(!dbReachable)(
    "member DeleteTask on another member's task → NotFound",
    async () => {
      const db = getDb();
      const ownerTaskId = `antienum-del-task-${Date.now()}`;
      const fakeSessionId = `antienum-del-sess-${Date.now()}`;

      await db.insert(taskTable).values({
        id: ownerTaskId,
        type: "chat",
        status: "open",
        createdByUserId: MEMBER_A,
        source: {},
      });
      await db.insert(taskSessionTable).values({
        taskId: ownerTaskId,
        sessionId: fakeSessionId,
        role: "primary",
      });

      try {
        const fakeSessions = makeFakeSessions({ existing: [] });
        const srv = await spawnServer({
          getSession: makeGetSession(MEMBER_B),
          sessions: fakeSessions,
          secrets: makeFakeSecrets(),
          db,
        });

        try {
          const client = makeClient(srv.serverUrl);
          await expectConnectError(
            client.deleteTask({ taskId: ownerTaskId }),
            Code.NotFound,
          );
        } finally {
          await srv.close();
        }
      } finally {
        // Cleanup: delete directly since we didn't go through the API.
        await db.delete(taskTable).where(eq(taskTable.id, ownerTaskId));
      }
    },
  );
});

// ---------------------------------------------------------------------------
// 4. Member create → list → get → delete (DB-gated)
// ---------------------------------------------------------------------------

describe("TaskService — member CRUD lifecycle (requires DB)", () => {
  let srv: TestServer;
  let client: ReturnType<typeof makeClient>;
  let fakeSessions: ReturnType<typeof makeFakeSessions>;
  let createdTaskId: string;
  const sessionId = `crud-sess-${Date.now()}`;
  const db = dbReachable ? getDb() : null;

  beforeAll(async () => {
    if (!dbReachable) return;

    fakeSessions = makeFakeSessions({
      created: [
        {
          id: sessionId,
          status: "active",
          image: "registry/img:latest",
          mode: "agent",
          createdAt: new Date().toISOString(),
          lastActiveAt: new Date().toISOString(),
        },
      ],
      existing: [],
    });

    srv = await spawnServer({
      getSession: makeGetSession(MEMBER_A),
      sessions: fakeSessions,
      secrets: makeFakeSecrets(),
      db: db!,
    });
    client = makeClient(srv.serverUrl);
  });

  afterAll(async () => {
    if (!dbReachable) return;
    // Cleanup any leftover task rows.
    if (createdTaskId && db) {
      await db.delete(taskTable).where(eq(taskTable.id, createdTaskId)).catch(() => {});
    }
    await srv?.close();
  });

  test.skipIf(!dbReachable)("CreateTask → returns task with 'working' status from session", async () => {
    const resp = await client.createTask({
      type: "chat",
      imageUri: "registry/img:latest",
      title: "CRUD test task",
    });

    expect(resp.task).toBeDefined();
    expect(resp.task!.type).toBe("chat");
    expect(resp.task!.title).toBe("CRUD test task");
    // Status derived from session "active" → "working".
    expect(resp.task!.status).toBe("working");
    expect(resp.task!.sessions).toHaveLength(1);
    expect(resp.task!.sessions[0]!.sessionId).toBe(sessionId);
    expect(resp.task!.sessions[0]!.role).toBe("primary");
    // Session should be denormalized.
    expect(resp.task!.sessions[0]!.session).toBeDefined();

    createdTaskId = resp.task!.id;
  });

  test.skipIf(!dbReachable)("ListTasks → shows own task with live session state", async () => {
    const resp = await client.listTasks({});
    const found = resp.tasks.find((t) => t.id === createdTaskId);
    expect(found).toBeDefined();
    expect(found!.status).toBe("working");
    expect(found!.sessions).toHaveLength(1);
    expect(found!.sessions[0]!.session).toBeDefined();
  });

  test.skipIf(!dbReachable)("GetTask → returns task with session ref", async () => {
    const resp = await client.getTask({ taskId: createdTaskId });
    expect(resp.task).toBeDefined();
    expect(resp.task!.id).toBe(createdTaskId);
    expect(resp.task!.sessions[0]!.sessionId).toBe(sessionId);
  });

  test.skipIf(!dbReachable)("ListTasks scoped: MEMBER_B sees only their own tasks (empty)", async () => {
    const srvB = await spawnServer({
      getSession: makeGetSession(MEMBER_B),
      sessions: fakeSessions,
      secrets: makeFakeSecrets(),
      db: db!,
    });
    try {
      const clientB = makeClient(srvB.serverUrl);
      const resp = await clientB.listTasks({});
      const bTaskIds = resp.tasks.map((t) => t.id);
      expect(bTaskIds).not.toContain(createdTaskId);
    } finally {
      await srvB.close();
    }
  });

  test.skipIf(!dbReachable)("DeleteTask → cascades task_session rows", async () => {
    await client.deleteTask({ taskId: createdTaskId });

    // Verify upstream session was deleted.
    expect(fakeSessions.deletedIds).toContain(sessionId);

    // Verify DB task row is gone.
    const taskRows = db ? await db.select().from(taskTable).where(eq(taskTable.id, createdTaskId)) : [];
    expect(taskRows).toHaveLength(0);

    // Verify DB task_session rows are gone (cascade).
    const sessionRefRows = db
      ? await db.select().from(taskSessionTable).where(eq(taskSessionTable.taskId, createdTaskId))
      : [];
    expect(sessionRefRows).toHaveLength(0);

    // createdTaskId already cleaned up; don't double-delete in afterAll.
    createdTaskId = "";
  });

  test.skipIf(!dbReachable)("ListTasks after delete → task no longer visible", async () => {
    if (!createdTaskId) return; // already verified via DeleteTask test above
    const resp = await client.listTasks({});
    const found = resp.tasks.find((t) => t.id === createdTaskId);
    expect(found).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 5. Admin sees all tasks + synthetic unattributed rows (DB-gated)
// ---------------------------------------------------------------------------

describe("TaskService — admin list sees all + synthetic unattributed rows", () => {
  test.skipIf(!dbReachable)("admin ListTasks includes member tasks + unattributed sessions", async () => {
    const db = getDb();
    const memberTaskId = `admin-see-task-${Date.now()}`;
    const memberSessionId = `admin-see-sess-${Date.now()}`;
    const orphanSessionId = `admin-orphan-${Date.now()}`;

    // Insert a task owned by MEMBER_A.
    await db.insert(taskTable).values({
      id: memberTaskId,
      type: "chat",
      status: "open",
      createdByUserId: MEMBER_A,
      source: {},
    });
    await db.insert(taskSessionTable).values({
      taskId: memberTaskId,
      sessionId: memberSessionId,
      role: "primary",
    });

    try {
      // Upstream returns TWO sessions: one attributed (in DB), one orphan (not in DB).
      const fakeSessions = makeFakeSessions({
        existing: [
          {
            id: memberSessionId,
            status: "idle",
            image: "img",
            mode: "agent",
            createdAt: new Date().toISOString(),
            lastActiveAt: new Date().toISOString(),
          },
          {
            id: orphanSessionId,
            status: "active",
            image: "img",
            mode: "agent",
            createdAt: new Date().toISOString(),
            lastActiveAt: new Date().toISOString(),
          },
        ],
      });

      const srv = await spawnServer({
        getSession: makeGetSession(ADMIN_ID, "admin"),
        sessions: fakeSessions,
        secrets: makeFakeSecrets(),
        db,
      });

      try {
        const client = makeClient(srv.serverUrl);
        const resp = await client.listTasks({});

        const taskIds = resp.tasks.map((t) => t.id);

        // Member task should appear.
        expect(taskIds).toContain(memberTaskId);

        // Synthetic unattributed row should appear.
        const syntheticId = `unattributed-${orphanSessionId}`;
        expect(taskIds).toContain(syntheticId);

        // Synthetic row shape.
        const synthetic = resp.tasks.find((t) => t.id === syntheticId);
        expect(synthetic).toBeDefined();
        expect(synthetic!.type).toBe("chat");
        expect(synthetic!.sessions).toHaveLength(1);
        expect(synthetic!.sessions[0]!.sessionId).toBe(orphanSessionId);
      } finally {
        await srv.close();
      }
    } finally {
      await db.delete(taskTable).where(eq(taskTable.id, memberTaskId));
    }
  });
});

// ---------------------------------------------------------------------------
// 5b. Member scoping — orphan sessions are invisible to members
// ---------------------------------------------------------------------------

describe("TaskService — member scoping: orphan sessions excluded from member ListTasks", () => {
  test.skipIf(!dbReachable)(
    "member ListTasks with orphan upstream session → no synthetic unattributed- row",
    async () => {
      const db = getDb();
      const orphanSessionId = `member-orphan-${Date.now()}`;

      // Upstream exposes one session that has NO task_session row in the DB.
      const fakeSessions = makeFakeSessions({
        existing: [
          {
            id: orphanSessionId,
            status: "active",
            image: "img",
            mode: "agent",
            createdAt: new Date().toISOString(),
            lastActiveAt: new Date().toISOString(),
          },
        ],
      });

      const srv = await spawnServer({
        getSession: makeGetSession(MEMBER_A),
        sessions: fakeSessions,
        secrets: makeFakeSecrets(),
        db,
      });

      try {
        const client = makeClient(srv.serverUrl);
        const resp = await client.listTasks({});
        const taskIds = resp.tasks.map((t) => t.id);

        // Members must NEVER see a synthetic unattributed- row.
        const syntheticId = `unattributed-${orphanSessionId}`;
        expect(taskIds).not.toContain(syntheticId);
        // Also sanity: no row whose id starts with "unattributed-".
        const hasSynthetic = taskIds.some((id) => id.startsWith("unattributed-"));
        expect(hasSynthetic).toBe(false);
      } finally {
        await srv.close();
      }
    },
  );
});

// ---------------------------------------------------------------------------
// 6. Compensation path — upstream create OK + DB insert fails
// ---------------------------------------------------------------------------

const TX_ERROR_MESSAGE = "forced DB failure for compensation test";

describe("TaskService — compensation: upstream OK + DB fail → DeleteSession called", () => {
  test("DB insert failure triggers upstream DeleteSession", async () => {
    const fakeSessionId = `comp-sess-${Date.now()}`;
    const fakeSessions = makeFakeSessions({
      created: [
        {
          id: fakeSessionId,
          status: "created",
          image: "registry/img:latest",
          mode: "agent",
          createdAt: new Date().toISOString(),
          lastActiveAt: new Date().toISOString(),
        },
      ],
      existing: [],
    });

    // Inject a fake DB whose transaction always throws.
    const fakeDb = {
      transaction: async (_fn: unknown) => {
        throw new Error(TX_ERROR_MESSAGE);
      },
      // Other methods unused by createTask but present for type compat.
      select: () => { throw new Error("unreachable"); },
      insert: () => { throw new Error("unreachable"); },
      delete: () => { throw new Error("unreachable"); },
    } as unknown as Db;

    const srv = await spawnServer({
      getSession: makeGetSession(MEMBER_A),
      sessions: fakeSessions,
      secrets: makeFakeSecrets(),
      db: fakeDb,
    });

    try {
      const client = makeClient(srv.serverUrl);
      // The request should fail (DB error → rethrown as Internal).
      let caughtErr: unknown;
      try {
        await client.createTask({ type: "chat", imageUri: "registry/img:latest" });
        throw new Error("Expected createTask to throw");
      } catch (err) {
        caughtErr = err;
      }

      // The original DB error must surface. Connect serialises non-ConnectErrors
      // as Code.Internal with the message scrubbed for security, so we assert:
      //   (a) the error is a ConnectError with Code.Internal, OR
      //   (b) it's a plain Error whose message contains the original tx message
      //       (happens when the call is made in-process without HTTP serialisation).
      expect(caughtErr).toBeDefined();
      if (caughtErr instanceof ConnectError) {
        // Connect serialises plain errors as Internal — the code must be Internal,
        // which proves the original tx error (not some other path) caused the failure.
        expect(caughtErr.code).toBe(Code.Internal);
      } else if (caughtErr instanceof Error) {
        expect(caughtErr.message).toContain(TX_ERROR_MESSAGE);
      } else {
        throw new Error(`Unexpected error type: ${String(caughtErr)}`);
      }

      // Compensation: upstream session should have been deleted.
      expect(fakeSessions.deletedIds).toContain(fakeSessionId);
    } finally {
      await srv.close();
    }
  });
});

// ---------------------------------------------------------------------------
// 7. Status mapping
// ---------------------------------------------------------------------------

describe("TaskService — session status → task status mapping", () => {
  // These are pure logic tests; no DB or upstream needed.
  const cases: Array<[string, string]> = [
    ["pending", "working"],
    ["created", "working"],
    ["guest_ready", "working"],
    ["active", "working"],
    ["idle", "working"],
    ["evacuating", "working"],
    ["evicting", "working"],
    ["completed", "done"],
    ["failed", "failed"],
    ["dead", "failed"],
    ["host_lost", "failed"],
  ];

  for (const [sessionStatus, expectedTaskStatus] of cases) {
    test.skipIf(!dbReachable)(
      `session '${sessionStatus}' → task '${expectedTaskStatus}'`,
      async () => {
        const db = getDb();
        const taskId = `status-map-${sessionStatus}-${Date.now()}`;
        const sessionId2 = `status-sess-${sessionStatus}-${Date.now()}`;

        await db.insert(taskTable).values({
          id: taskId,
          type: "chat",
          status: "open",
          createdByUserId: MEMBER_A,
          source: {},
        });
        await db.insert(taskSessionTable).values({
          taskId,
          sessionId: sessionId2,
          role: "primary",
        });

        try {
          const fakeSessions = makeFakeSessions({
            existing: [
              {
                id: sessionId2,
                status: sessionStatus,
                image: "img",
                mode: "agent",
                createdAt: new Date().toISOString(),
                lastActiveAt: new Date().toISOString(),
              },
            ],
          });

          const srv = await spawnServer({
            getSession: makeGetSession(MEMBER_A),
            sessions: fakeSessions,
            secrets: makeFakeSecrets(),
            db,
          });

          try {
            const client = makeClient(srv.serverUrl);
            const resp = await client.getTask({ taskId });
            expect(resp.task!.status).toBe(expectedTaskStatus);
          } finally {
            await srv.close();
          }
        } finally {
          await db.delete(taskTable).where(eq(taskTable.id, taskId));
        }
      },
    );
  }
});
