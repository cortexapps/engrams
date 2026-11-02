import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession, user } from "../db/schema.ts";
import { authorizePolicyActor } from "../mcp/session-policy.ts";
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
test.skipIf(!reachable)(
  "policy authority uses current human owner, role, and ban state",
  async () => {
    const db = getDb(),
      actor = crypto.randomUUID(),
      tid = crypto.randomUUID(),
      sid = crypto.randomUUID();
    try {
      await db
        .insert(user)
        .values({
          id: actor,
          name: "Policy owner",
          email: `${actor}@example.test`,
          createdAt: new Date(),
          updatedAt: new Date(),
          role: "user",
        });
      await db.insert(task).values({ id: tid, type: "chat", createdByUserId: actor });
      await db.insert(taskSession).values({ taskId: tid, sessionId: sid });
      expect((await authorizePolicyActor(sid, actor)).isAdmin).toBe(false);
      await db.update(task).set({ createdByUserId: "another-owner" }).where(eq(task.id, tid));
      await expect(authorizePolicyActor(sid, actor)).rejects.toThrow("owner or administrator");
      await db.update(user).set({ role: "admin" }).where(eq(user.id, actor));
      expect((await authorizePolicyActor(sid, actor)).isAdmin).toBe(true);
      await db.update(user).set({ role: "user" }).where(eq(user.id, actor));
      await expect(authorizePolicyActor(sid, actor)).rejects.toThrow();
      await db.update(task).set({ createdByUserId: actor }).where(eq(task.id, tid));
      await db.update(user).set({ banned: true }).where(eq(user.id, actor));
      await expect(authorizePolicyActor(sid, actor)).rejects.toThrow();
      await db
        .update(user)
        .set({ banned: false, email: `apikey+${actor}@service.local` })
        .where(eq(user.id, actor));
      await expect(authorizePolicyActor(sid, actor)).rejects.toThrow();
    } finally {
      await db.delete(task).where(eq(task.id, tid));
      await db.delete(user).where(eq(user.id, actor));
    }
  },
);
