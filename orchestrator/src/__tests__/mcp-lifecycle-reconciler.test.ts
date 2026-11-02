import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeMcpLifecycleReconciler } from "../mcp/lifecycle-reconciler.ts";

test("independent keyset scan cancels stale accepted work without provider replay", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    f.setFence(false);
    const scanner = makeMcpLifecycleReconciler({ db: f.db, driver: f.driver });
    await scanner.runOnce();
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("recovery lifecycle pass runs event catch-up and independent currentness", async () => {
  const { makeMcpRecovery } = await import("../mcp/recovery.ts");
  const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
  const { consumerCursor } = await import("../db/schema.ts");
  const { eq } = await import("drizzle-orm");
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      listLifecycleEvents: async (_id, after) =>
        after < 1n ? [{ idx: 1n, kind: "resumed", payloadJson: "{}" }] : [],
    });
    const scanner = makeMcpRecoveryScanner({
      deploymentId: f.deploymentId,
      runtime: () => runtime,
    });
    await scanner.runLifecycleOnce();
    expect((await f.journal.get(call.id))!.state).toBe("authorized");
    expect(
      (
        await f.db.select().from(consumerCursor).where(eq(consumerCursor.sessionId, f.sessionId))
      )[0]!.lastIdx,
    ).toBe(1n);
    f.setFence(false);
    await scanner.runLifecycleOnce();
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.db.delete(consumerCursor).where(eq(consumerCursor.sessionId, f.sessionId));
    await f.cleanup();
  }
}, 20000);

test("a stalled authority check times out and aborts local I/O", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const canceled: string[] = [];
    const scanner = makeMcpLifecycleReconciler({
      db: f.db,
      timeoutMs: 10,
      driver: {
        reconcile: () => new Promise<void>(() => {}),
        cancel: (id) => {
          canceled.push(id);
        },
      },
    });
    await expect(scanner.runOnce()).rejects.toThrow("unavailable");
    expect(canceled).toContain(call.id);
    expect((await f.journal.get(call.id))!.state).toBe("authorized");
  } finally {
    await f.cleanup();
  }
}, 20000);

test("a timed-out stale check cannot cancel later when its response arrives", async () => {
  const f = await effectFixture({ classification: "read" });
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    f.setFence(false);
    f.beforeFence(() => blocked);
    const scanner = makeMcpLifecycleReconciler({ db: f.db, driver: f.driver, timeoutMs: 10 });
    await expect(scanner.runOnce()).rejects.toThrow("unavailable");
    release();
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect((await f.journal.get(call.id))!.state).toBe("authorized");
  } finally {
    release();
    await f.cleanup();
  }
}, 20000);

test("the recovery scheduler runs lifecycle catch-up without a workflow delivery", async () => {
  const { makeMcpRecovery } = await import("../mcp/recovery.ts");
  const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
  const { consumerCursor } = await import("../db/schema.ts");
  const { eq } = await import("drizzle-orm");
  const f = await effectFixture({ classification: "read" });
  let scanner: ReturnType<typeof makeMcpRecoveryScanner> | undefined;
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      listLifecycleEvents: async (_id, after) =>
        after < 1n ? [{ idx: 1n, kind: "resumed", payloadJson: "{}" }] : [],
    });
    scanner = makeMcpRecoveryScanner({
      deploymentId: f.deploymentId,
      journal: f.journal,
      runtime: () => runtime,
      startWorkflow: async () => {},
    });
    await scanner.start();
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline) {
      const [current] = await f.db
        .select()
        .from(consumerCursor)
        .where(eq(consumerCursor.sessionId, f.sessionId));
      if (current?.lastIdx === 1n) break;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const [cursor] = await f.db
      .select()
      .from(consumerCursor)
      .where(eq(consumerCursor.sessionId, f.sessionId));
    expect(cursor?.lastIdx).toBe(1n);
    expect((await f.journal.get(call.id))!.state).toBe("authorized");
  } finally {
    await scanner?.stop();
    await f.db.delete(consumerCursor).where(eq(consumerCursor.sessionId, f.sessionId));
    await f.cleanup();
  }
}, 20000);

test("active keyset uses the partial index and advances past a full page", async () => {
  const { eq, sql } = await import("drizzle-orm");
  const { mcpExecution } = await import("../db/schema.ts");
  const { MCP_LIMITS } = await import("../mcp/limits.ts");
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const [base] = await f.db.select().from(mcpExecution).where(eq(mcpExecution.id, call.id));
    const copies = Array.from({ length: MCP_LIMITS.queueSize }, (_, i) => ({
      ...base!,
      id: crypto.randomUUID(),
      toolCallId: `scan-${i}-${crypto.randomUUID()}`,
    }));
    await f.db.insert(mcpExecution).values(copies);
    const seen = new Set<string>();
    const scanner = makeMcpLifecycleReconciler({
      db: f.db,
      driver: {
        async reconcile(id) {
          seen.add(id);
        },
        cancel() {},
      },
    });
    await scanner.runOnce();
    expect(seen.size).toBe(MCP_LIMITS.queueSize);
    await scanner.runOnce();
    expect(seen.size).toBe(MCP_LIMITS.queueSize + 1);
    const plan = await f.db.transaction(async (tx) => {
      await tx.execute(sql`SET LOCAL enable_seqscan = off`);
      return tx.execute(
        sql`EXPLAIN SELECT id FROM mcp_execution WHERE state IN ('waiting_approval','authorized','dispatching') AND id > '' ORDER BY id LIMIT 128`,
      );
    });
    expect(JSON.stringify(plan.rows)).toContain("mcp_execution_lifecycle_active_idx");
  } finally {
    await f.cleanup();
  }
}, 20000);
