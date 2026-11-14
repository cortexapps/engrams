import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import { makeManagedLifecycleStore } from "../mcp/managed-lifecycle-store.ts";
import type { ManagedLifecycle } from "../mcp/managed-lifecycle.ts";
import { taskSession } from "../db/schema.ts";
await verifyMcpTestDatabases();
test("managed lifecycle retains exact nonsecret identity through lost-response and concurrent CAS", async () => {
  const f = await reviewFixture();
  try {
    const store = makeManagedLifecycleStore(f.db);
    const initial: ManagedLifecycle = {
      version: 1,
      identity: {
        taskId: f.taskId,
        sessionId: f.sessionId,
        actorId: f.actorId,
        inputId: "input",
        inputDigest: `sha256:${"a".repeat(64)}`,
        writeEligibility: "read_only",
      },
      nativeVersion: 1,
      nativeDigest: `sha256:${"b".repeat(64)}`,
      phase: "reserving",
      bindingHash: f.snapshot.bindingHash,
    };
    await store.initialize(initial);
    await store.initialize(initial);
    expect((await store.read(f.sessionId)).session.mcpManagedLifecycle).toEqual(initial);
    const next = { ...initial, phase: "held" as const };
    const outcomes = await Promise.allSettled([
      store.compareAndSet(initial, next),
      store.compareAndSet(initial, { ...initial, phase: "preparing" }),
    ]);
    expect(outcomes.filter((o) => o.status === "fulfilled")).toHaveLength(1);
    await expect(store.compareAndSet(initial, next)).rejects.toThrow("changed");
    await expect(
      f.db
        .update(taskSession)
        .set({ mcpManagedLifecycle: null })
        .where(eq(taskSession.sessionId, f.sessionId))
        .execute(),
    ).rejects.toThrow();
    await expect(
      f.db
        .update(taskSession)
        .set({ mcpManagedLifecycle: { ...next, nativeDigest: `sha256:${"c".repeat(64)}` } })
        .where(eq(taskSession.sessionId, f.sessionId))
        .execute(),
    ).rejects.toThrow();
    expect(await store.pending()).toContainEqual({ sessionId: f.sessionId });
  } finally {
    await f.cleanup();
  }
});
