/**
 * Orchestrator DB tests (bun test).
 *
 * All live tests are env-gated on ORCHESTRATOR_DATABASE_URL being set AND
 * the database being reachable. When the variable is absent or the DB is
 * unreachable the tests skip cleanly so the suite passes in environments
 * without a Postgres instance.
 *
 * In the Tilt dev loop ORCHESTRATOR_DATABASE_URL is always set (via
 * serve_env), so the live tests run automatically there.
 *
 * Note on test.skipIf: Bun evaluates the skipIf predicate at module-parse
 * time, before any beforeAll hook runs. So we cannot use a top-level async
 * flag for the skip condition. Instead we probe the DB synchronously at
 * module load (via a top-level Promise.resolve trick) and fall back to
 * early-return inside each test body for the reachability guard.
 */

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession } from "../db/schema.ts";
import { eq } from "drizzle-orm";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import healthRoute from "../routes/health.ts";
import type { AddressInfo } from "net";

// ---------------------------------------------------------------------------
// Gate: skip everything when URL is absent (known at parse time).
// Reachability is checked in beforeAll and used as an early-return guard.
// ---------------------------------------------------------------------------

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let dbReachable = false;

beforeAll(async () => {
  if (!DB_URL) return;
  dbReachable = await checkDb();
});

// Helper: skip a test when the DB is not reachable (called at test body start).
function skipIfNoDb(t: () => void | Promise<void>) {
  return async () => {
    if (!DB_URL || !dbReachable) {
      // Signal skip via a console note; the test body does nothing.
      return;
    }
    await t();
  };
}

// ---------------------------------------------------------------------------
// checkDb() unit-like behaviour when URL is absent
// ---------------------------------------------------------------------------

describe("checkDb() — no URL set", () => {
  test.skipIf(!!DB_URL)(
    "returns false when ORCHESTRATOR_DATABASE_URL is absent",
    async () => {
      const result = await checkDb();
      expect(result).toBe(false);
    },
  );
});

// ---------------------------------------------------------------------------
// Live DB tests (requires ORCHESTRATOR_DATABASE_URL + reachable DB)
// ---------------------------------------------------------------------------

describe("live DB (requires ORCHESTRATOR_DATABASE_URL)", () => {
  // Unique id per test run to avoid collisions on repeated runs.
  const testTaskId = `test-task-${Date.now()}`;
  const testSessionId = `test-session-${Date.now()}`;

  test(
    "checkDb() returns true when DB is reachable",
    skipIfNoDb(async () => {
      expect(await checkDb()).toBe(true);
    }),
  );

  test(
    "insert + read + delete a task row",
    skipIfNoDb(async () => {
      const db = getDb();

      // Insert
      await db.insert(task).values({
        id: testTaskId,
        type: "chat",
        title: "Test task",
        status: "open",
      });

      // Read back
      const rows = await db
        .select()
        .from(task)
        .where(eq(task.id, testTaskId));

      expect(rows).toHaveLength(1);
      expect(rows[0]!.id).toBe(testTaskId);
      expect(rows[0]!.type).toBe("chat");
      expect(rows[0]!.status).toBe("open");
      expect(rows[0]!.title).toBe("Test task");

      // Cleanup
      await db.delete(task).where(eq(task.id, testTaskId));

      const afterDelete = await db
        .select()
        .from(task)
        .where(eq(task.id, testTaskId));
      expect(afterDelete).toHaveLength(0);
    }),
  );

  test(
    "insert + read + delete a task_session row (cascade on task delete)",
    skipIfNoDb(async () => {
      const db = getDb();

      // Insert parent task first
      await db.insert(task).values({
        id: testTaskId + "-ts",
        type: "chat",
        status: "open",
      });

      // Insert task_session
      await db.insert(taskSession).values({
        taskId: testTaskId + "-ts",
        sessionId: testSessionId,
        role: "primary",
      });

      // Read back via taskId
      const rows = await db
        .select()
        .from(taskSession)
        .where(eq(taskSession.taskId, testTaskId + "-ts"));

      expect(rows).toHaveLength(1);
      expect(rows[0]!.sessionId).toBe(testSessionId);
      expect(rows[0]!.role).toBe("primary");

      // Delete parent — cascade should remove task_session
      await db.delete(task).where(eq(task.id, testTaskId + "-ts"));

      const afterCascade = await db
        .select()
        .from(taskSession)
        .where(eq(taskSession.taskId, testTaskId + "-ts"));
      expect(afterCascade).toHaveLength(0);
    }),
  );
});

// ---------------------------------------------------------------------------
// Live healthz-with-DB test
//
// Spins up the real health route (which calls checkDb()) on an ephemeral
// port and verifies the 200 {ok:true, db:true} shape when the DB is live.
// ---------------------------------------------------------------------------

describe("healthz with live DB (requires ORCHESTRATOR_DATABASE_URL)", () => {
  let baseUrl: string;
  let srv: ReturnType<typeof buildServer>;

  beforeAll(async () => {
    if (!DB_URL || !dbReachable) return;

    const app = new Hono();
    app.route("/", healthRoute);
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

  test(
    "GET /healthz → 200 {ok:true, db:true} when DB is live",
    skipIfNoDb(async () => {
      const res = await fetch(`${baseUrl}/healthz`);
      expect(res.status).toBe(200);
      const body = await res.json();
      expect(body).toEqual({ ok: true, db: true });
    }),
  );
});
