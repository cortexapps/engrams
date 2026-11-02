import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { McpRebindDraining } from "../mcp/rebind.ts";
import { coldFixture } from "./mcp-cold-fixture.ts";

test("a rebind cannot expose a new binding while the old dispatch is in its final fence", async () => {
  const f = await effectFixture({ classification: "read", bindingEpoch: 1 });
  const cold = await coldFixture(f, f.operatorPolicies);
  let unblock!: () => void;
  const blocked = new Promise<void>((resolve) => {
    unblock = resolve;
  });
  let notify!: () => void;
  const entered = new Promise<void>((resolve) => {
    notify = resolve;
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let afterLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases > 0 && ++afterLeaseFences === 2) {
        notify();
        await blocked;
      }
    });
    const pending = f.driver.driveOnce(call.id);
    await entered;
    const service = cold.service;
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    expect(candidate.drain?.reason).toBe("dispatch_in_progress");
    expect(candidate.drain?.retryAfter).toBeTruthy();
    const { mcpOperation } = await import("../mcp/contracts.ts");
    const { McpRebindDrainSchema, McpRebindDrainReason } =
      await import("../gen/engram/app/v1/integration_pb.ts");
    const { ConnectError } = await import("@connectrpc/connect");
    try {
      await mcpOperation(() =>
        service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
      );
      throw Error("expected drain");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      expect(
        (error as InstanceType<typeof ConnectError>).findDetails(McpRebindDrainSchema)[0]?.reason,
      ).toBe(McpRebindDrainReason.DISPATCH_IN_PROGRESS);
    }
    const attempt = await service
      .commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId)
      .then(
        () => "committed",
        (error: Error) => error.message,
      );
    unblock();
    await pending;
    expect(attempt).toContain("dispatch in progress");
    expect((await f.command("stats")).calls).toBe(1);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toBeInstanceOf(McpRebindDraining);
    cold.start();
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    expect((await cold.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);

test("cancel and manual resolution do not waive the original in-flight intent", async () => {
  const f = await effectFixture({ classification: "read", bindingEpoch: 1 });
  const cold = await coldFixture(f, f.operatorPolicies);
  let unblock!: () => void;
  const blocked = new Promise<void>((resolve) => {
    unblock = resolve;
  });
  let notify!: () => void;
  const entered = new Promise<void>((resolve) => {
    notify = resolve;
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let afterLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases > 0 && ++afterLeaseFences === 2) {
        notify();
        await blocked;
      }
    });
    const pending = f.driver.driveOnce(call.id);
    await entered;
    const dispatched = (await f.journal.get(call.id))!;
    const unknown = await f.journal.cancel(call.id, f.actorId, new Date(), dispatched.version);
    const context = {
      executionId: call.id,
      journalRevision: unknown.version,
      requestDigest: unknown.requestDigest,
      bindingHash: unknown.bindingHash,
      intentId: unknown.intentId!,
    };
    await f.journal.resolveUnknown(call.id, f.actorId, "acknowledged", new Date(), context);
    const service = cold.service;
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    expect(candidate.drain?.reason).toBe("unknown_intent");
    expect(candidate.drain?.retryAfter).toBeTruthy();
    const attempt = await service
      .commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId)
      .then(
        () => "committed",
        (error: Error) => error.message,
      );
    unblock();
    await pending;
    expect(attempt).toContain("dispatch in progress");
    expect((await f.command("stats")).calls).toBe(1);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toBeInstanceOf(McpRebindDraining);
    cold.start();
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    expect((await cold.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);

test("an expired original intent permits rebind and cannot send afterward", async () => {
  const f = await effectFixture({ classification: "read", bindingEpoch: 1 });
  const cold = await coldFixture(f, f.operatorPolicies);
  let unblock!: () => void;
  const blocked = new Promise<void>((resolve) => {
    unblock = resolve;
  });
  let notify!: () => void;
  const entered = new Promise<void>((resolve) => {
    notify = resolve;
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let afterLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases > 0 && ++afterLeaseFences === 2) {
        notify();
        await blocked;
      }
    });
    const pending = f.driver.driveOnce(call.id);
    await entered;
    const dispatched = (await f.journal.get(call.id))!;
    await f.journal.cancel(call.id, f.actorId, new Date(), dispatched.version);
    const service = cold.service;
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    cold.advanceColdNow(310_000);
    f.advanceNow(310_000);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toBeInstanceOf(McpRebindDraining);
    cold.start();
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    unblock();
    const outcome = await pending;
    expect(outcome.state).toBe("outcome_unknown");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);

test("a different-scope barrier commits before the waiting rebind reads its drain", async () => {
  const { and, eq, sql } = await import("drizzle-orm");
  const { mcpExecution } = await import("../db/schema.ts");
  const { reviewContext } = await import("../mcp/journal-review.ts");
  const f = await effectFixture({ classification: "write", bindingEpoch: 1 });
  const cold = await coldFixture(f, f.operatorPolicies);
  let release!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  let locked!: () => void;
  const ready = new Promise<void>((resolve) => {
    locked = resolve;
  });
  let blocker: Promise<unknown> | undefined;
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    const approved = (
      await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date())
    ).call;
    await f.journal.admit(call.id, f.deploymentId, new Date());
    const lease = (await f.journal.getAdmission(call.id))!;
    const service = cold.service;
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    blocker = f.db.transaction(async (tx) => {
      await tx.select().from(mcpExecution).where(eq(mcpExecution.id, call.id)).for("update");
      locked();
      await held;
    });
    await ready;
    const barrier = f.journal.dispatch(
      call.id,
      { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt },
      new Date(),
      {
        executionId: call.id,
        requestDigest: call.requestDigest,
        bindingHash: call.bindingHash,
        principalId: call.principalId,
        expectedJournalRevision: approved.version,
        reservationId: lease.reservationId,
        reservationExpiresAt: lease.expiresAt,
      },
    );
    async function waitForLocks(minimum: number) {
      const deadline = Date.now() + 3000;
      while (Date.now() < deadline) {
        const result = await f.db.execute(
          sql`select count(*)::integer as n from pg_stat_activity where datname = current_database() and wait_event_type = 'Lock' and state = 'active'`,
        );
        if (Number(result.rows[0]?.n) >= minimum) return;
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      throw Error("expected lock wait unavailable");
    }
    await waitForLocks(1);
    const commit = service
      .commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId)
      .then(
        () => "committed",
        (error: Error) => error.message,
      );
    await waitForLocks(2);
    release();
    await blocker;
    expect((await barrier).state).toBe("dispatching");
    expect(await commit).not.toBe("committed");
    expect(cold.stats().applies).toBe(0);
  } finally {
    release();
    await blocker;
    await f.cleanup();
  }
}, 20000);

test("an accepted old call cannot cross the barrier after rebind", async () => {
  const f = await effectFixture({ classification: "read", bindingEpoch: 1 });
  const cold = await coldFixture(f, f.operatorPolicies);
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const service = cold.service;
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toBeInstanceOf(McpRebindDraining);
    cold.start();
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    await f.driver.driveOnce(call.id).catch(() => undefined);
    expect((await f.command("stats")).calls).toBe(0);
    expect((await f.journal.get(call.id))!.state).not.toBe("dispatching");
  } finally {
    await f.cleanup();
  }
}, 20000);

test("a held cold host call allows another session to acquire a real worker credential lease", async () => {
  // The cold fixture has no process. Only the other session starts one isolated worker fixture.
  const first = await coldFixture();
  const other = await effectFixture({ classification: "read" });
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  let enter!: () => void;
  const entered = new Promise<void>((resolve) => {
    enter = resolve;
  });
  let pending: Promise<unknown> | undefined;
  try {
    const candidate = await first.service.prepare(
      first.sessionId,
      first.snapshot.bindingHash,
      first.actorId,
    );
    first.beforeApply(async () => {
      enter();
      await blocked;
    });
    pending = first.service
      .commit(first.sessionId, first.snapshot.bindingHash, candidate.bindingHash, first.actorId)
      .catch((error) => error);
    await entered;
    const call = await other.driver.acceptToolRequest(other.request);
    const result = await other.driver.driveOnce(call.id);
    expect(other.leases).toBe(1);
    expect((await other.command("stats")).calls).toBe(1);
    expect(result.state).toBe("succeeded");
    release();
    expect(await pending).toBeInstanceOf(McpRebindDraining);
  } finally {
    release();
    await pending;
    await other.cleanup();
    await first.cleanup();
  }
}, 20000);
