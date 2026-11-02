import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeMcpRebind } from "../mcp/rebind.ts";

test("a rebind cannot expose a new binding while the old dispatch is in its final fence", async () => {
  const f = await effectFixture({ classification: "read" });
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
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      readFrozenNativeManifest: async () => "[]",
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
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
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);

test("cancel and manual resolution do not waive the original in-flight intent", async () => {
  const f = await effectFixture({ classification: "read" });
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
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      readFrozenNativeManifest: async () => "[]",
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
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
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);

test("an expired original intent permits rebind and cannot send afterward", async () => {
  const f = await effectFixture({ classification: "read" });
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
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    let offset = 0;
    const service = makeMcpRebind({
      readFrozenNativeManifest: async () => "[]",
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      now: () => new Date(Date.now() + offset),
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    offset = 310_000;
    f.advanceNow(offset);
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
  const f = await effectFixture({ classification: "write" });
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
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    let applied = 0;
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        applied++;
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      readFrozenNativeManifest: async () => "[]",
      db: f.db,
      deploymentId: crypto.randomUUID(),
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
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
    expect(applied).toBe(0);
  } finally {
    release();
    await blocker;
    await f.cleanup();
  }
}, 20000);

test("an accepted old call cannot cross the barrier after rebind", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      readFrozenNativeManifest: async () => "[]",
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId);
    await f.driver.driveOnce(call.id).catch(() => undefined);
    expect((await f.command("stats")).calls).toBe(0);
    expect((await f.journal.get(call.id))!.state).not.toBe("dispatching");
  } finally {
    await f.cleanup();
  }
}, 20000);

test("a held host apply does not block another session broker in the same deployment", async () => {
  const { profile } = await import("../db/schema.ts");
  const { canonicalDigest } = await import("../mcp/hash.ts");
  const { eq } = await import("drizzle-orm");
  const first = await effectFixture({ classification: "read" });
  const other = await effectFixture({ classification: "read" });
  let release!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  let entered!: () => void;
  const ready = new Promise<void>((resolve) => {
    entered = resolve;
  });
  let commit: Promise<unknown> | undefined;
  let releaseLease!: () => void;
  const leaseHeld = new Promise<void>((resolve) => {
    releaseLease = resolve;
  });
  let leased!: () => void;
  const leaseReady = new Promise<void>((resolve) => {
    leased = resolve;
  });
  let drive: Promise<unknown> | undefined;
  async function waitWithin(promise: Promise<void>) {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        promise,
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(Error("test gate timed out")), 3000);
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
  try {
    const [firstProfile] = await first.db
      .select()
      .from(profile)
      .where(eq(profile.id, first.profileId));
    const [otherProfile] = await first.db
      .select()
      .from(profile)
      .where(eq(profile.id, other.profileId));
    const grants = [...firstProfile!.integrationGrants, ...otherProfile!.integrationGrants];
    await first.db
      .update(profile)
      .set({
        integrationGrants: grants,
        mcpGrantPublication: {
          revision: 2,
          actorId: first.actorId,
          grantsHash: canonicalDigest(grants.filter((grant) => grant.mcp)),
        },
      })
      .where(eq(profile.id, first.profileId));
    let state = {
      sessionId: first.sessionId,
      bindingEpoch: first.snapshot.bindingEpoch,
      bindingHash: first.snapshot.bindingHash,
      policy: first.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return state;
      },
      async compareAndApply(
        _prev: typeof state,
        ack: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof state.policy,
      ) {
        entered();
        await held;
        state = { ...state, bindingHash: ack.bindingHash, policy };
        return ack;
      },
    };
    const service = makeMcpRebind({
      db: first.db,
      deploymentId: other.deploymentId,
      host,
      readFrozenNativeManifest: async () => "[]",
      readOperatorPolicies: async () => first.operatorPolicies,
      readBindingEpoch: async () => first.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(
      first.sessionId,
      first.snapshot.bindingHash,
      first.actorId,
    );
    const call = await other.driver.acceptToolRequest(other.request);
    other.afterLease(async () => {
      if (other.leases === 1) {
        leased();
        await leaseHeld;
      }
    });
    drive = other.driver.driveOnce(call.id);
    await waitWithin(leaseReady);
    commit = service.commit(
      first.sessionId,
      first.snapshot.bindingHash,
      candidate.bindingHash,
      first.actorId,
    );
    await waitWithin(ready);
    releaseLease();
    const deadline = Date.now() + 2000;
    let calls = 0;
    while (Date.now() < deadline && calls === 0) {
      calls = (await other.command("stats")).calls;
      if (!calls) await new Promise((resolve) => setTimeout(resolve, 10));
    }
    expect(calls).toBe(1);
    release();
    await commit;
    expect(((await drive) as { state: string }).state).toBe("succeeded");
  } finally {
    release();
    releaseLease();
    await commit?.catch(() => {});
    await drive?.catch(() => {});
    await other.cleanup();
    await first.cleanup();
  }
}, 20000);
