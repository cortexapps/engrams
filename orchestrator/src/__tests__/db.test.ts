/**
 * Orchestrator DB tests (bun test).
 *
 * All live tests are env-gated on ORCHESTRATOR_DATABASE_URL being set AND
 * the database being reachable. When the variable is absent or the DB is
 * unreachable the tests are reported as SKIP (not silent pass) so CI surfaces
 * the gap honestly.
 *
 * Bun test files support top-level await, so we probe reachability once at
 * module load and pass the result directly to test.skipIf — no beforeAll trick
 * needed, and no early-return wrapper that masks skips as passes.
 *
 * In the Tilt dev loop ORCHESTRATOR_DATABASE_URL is always set (via
 * serve_env), so the live tests run automatically there.
 */

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession, profile } from "../db/schema.ts";
import { eq } from "drizzle-orm";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import healthRoute from "../routes/health.ts";
import type { AddressInfo } from "net";

// ---------------------------------------------------------------------------
// Gate: compute reachability once at module load (top-level await is fine in
// Bun test files). test.skipIf receives the resolved boolean so skipped tests
// are reported as SKIP, not as silent passes.
// ---------------------------------------------------------------------------

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

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

  test.skipIf(!dbReachable)(
    "checkDb() returns true when DB is reachable",
    async () => {
      expect(await checkDb()).toBe(true);
    },
  );

  test.skipIf(!dbReachable)(
    "insert + read + delete a task row",
    async () => {
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
    },
  );

  test.skipIf(!dbReachable)(
    "insert + read + delete a task_session row (cascade on task delete)",
    async () => {
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
    },
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
    if (!dbReachable) return;

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

  test.skipIf(!dbReachable)(
    "GET /healthz → 200 {ok:true, db:true} when DB is live",
    async () => {
      const res = await fetch(`${baseUrl}/healthz`);
      expect(res.status).toBe(200);
      const body = await res.json();
      expect(body).toEqual({ ok: true, db: true });
    },
  );
});

// ---------------------------------------------------------------------------
// Session profiles (ADR 0052)
//
// Round-trips the orchestrator-only `profile` table and the nullable
// `task_session.profile_id` intra-DB FK that records which profile started a
// session. Exercises both the table (incl. jsonb env_vars + boolean +
// null deleted_at defaults) and the FK column.
// ---------------------------------------------------------------------------

describe("profile table (ADR 0052)", () => {
  test.skipIf(!dbReachable)(
    "insert profile + task_session.profile_id round-trips",
    async () => {
      const db = getDb();
      const pid = `profile-rt-${Date.now()}`;
      const tid = `profile-rt-task-${Date.now()}`;
      const sid = `profile-rt-sess-${Date.now()}`;
      try {
        await db.insert(profile).values({
          id: pid,
          name: "Round-trip",
          description: "d",
          icon: "Bot",
          imageId: "img-logical-id",
          includeUserTokens: true,
          envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
        });
        await db.insert(task).values({
          id: tid,
          type: "chat",
          status: "open",
          createdByUserId: "u",
          source: {},
        });
        await db.insert(taskSession).values({
          taskId: tid,
          sessionId: sid,
          role: "primary",
          profileId: pid,
        });

        const rows = await db
          .select()
          .from(profile)
          .where(eq(profile.id, pid));
        expect(rows[0]!.includeUserTokens).toBe(true);
        expect(rows[0]!.envVars).toEqual({ ANTHROPIC_MODEL: "claude-opus-4-8" });
        expect(rows[0]!.deletedAt).toBeNull();

        const refs = await db
          .select()
          .from(taskSession)
          .where(eq(taskSession.taskId, tid));
        expect(refs[0]!.profileId).toBe(pid);
      } finally {
        await db
          .delete(task)
          .where(eq(task.id, tid))
          .catch(() => {});
        await db
          .delete(profile)
          .where(eq(profile.id, pid))
          .catch(() => {});
      }
    },
  );
});
