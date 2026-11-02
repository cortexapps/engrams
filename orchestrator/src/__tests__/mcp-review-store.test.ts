import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession, user } from "../db/schema.ts";
import { makeMcpJournal } from "../mcp/journal.ts";

const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
test.skipIf(!reachable)(
  "empty approval reads still require a current human owner or admin",
  async () => {
    const db = getDb();
    const actorId = crypto.randomUUID(),
      taskId = crypto.randomUUID(),
      sessionId = crypto.randomUUID();
    try {
      await db.insert(user).values({
        id: actorId,
        name: "Review reader",
        email: `${actorId}@example.test`,
        role: "user",
        createdAt: new Date(),
        updatedAt: new Date(),
      });
      await db.insert(task).values({ id: taskId, type: "chat", createdByUserId: "another-owner" });
      await db.insert(taskSession).values({ taskId, sessionId });
      const journal = makeMcpJournal(db);
      // A cached administrator role must not grant access, even when no calls exist.
      await expect(journal.pending(sessionId, actorId, new Date())).rejects.toThrow(
        "owner or administrator",
      );
    } finally {
      await db.delete(task).where(eq(task.id, taskId));
      await db.delete(user).where(eq(user.id, actorId));
    }
  },
);

import { reviewFixture } from "./mcp-review-fixture.ts";
test.skipIf(!reachable)(
  "journal freezes original bytes and rejects argument hash mismatch",
  async () => {
    const f = await reviewFixture();
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      expect(call.originalArguments).toBe(f.input.originalArguments);
      await expect(
        f.journal.create(
          { ...f.input, toolCallId: "changed", originalArguments: '{"x":2}' },
          { kind: "approval" },
          f.now,
        ),
      ).rejects.toThrow("argument");
    } finally {
      await f.cleanup();
    }
  },
);

import { sql } from "drizzle-orm";
import { mcpApproval, mcpAdmission, mcpExecution, profile } from "../db/schema.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import { reviewContext, requestPolicyContext } from "../mcp/journal-review.ts";

async function pending(f: Awaited<ReturnType<typeof reviewFixture>>) {
  const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
  const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
  return { call, approval, expected: reviewContext(call, approval) };
}
test.skipIf(!reachable)(
  "same request races keep the first bytes and immutable complete context",
  async () => {
    const f = await reviewFixture();
    try {
      const calls = await Promise.all(
        Array.from({ length: 4 }, () => f.journal.create(f.input, { kind: "approval" }, f.now)),
      );
      expect(new Set(calls.map((c) => c.id)).size).toBe(1);
      const original = calls[0]!;
      expect(requestPolicyContext(original.requestContext!).acceptedPolicyRef.policyRevision).toBe(
        1n,
      );
      const changed = {
        ...f.input,
        originalArguments: '{"x":2}',
        argumentHash: canonicalDigest({ x: 2 }),
      };
      await expect(f.journal.create(changed, { kind: "approval" }, f.now)).rejects.toThrow(
        "digest",
      );
      await expect(
        f.journal.create({ ...f.input, originalArguments: '{"x":1}' }, { kind: "approval" }, f.now),
      ).rejects.toThrow("digest");
      await expect(
        Promise.resolve(
          f.db
            .update(mcpExecution)
            .set({ originalArguments: '{"x":2}', state: "canceled", version: 2 })
            .where(eq(mcpExecution.id, original.id)),
        ),
      ).rejects.toThrow();
      expect((await f.journal.get(original.id))!.originalArguments).toBe(f.input.originalArguments);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "every expected review field is exact and concurrent decisions have one winner",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      for (const key of Object.keys(expected) as (keyof typeof expected)[]) {
        const value = expected[key];
        await expect(
          f.journal.decide(
            call.id,
            f.actorId,
            true,
            { ...expected, [key]: typeof value === "number" ? value + 1 : `${value}-changed` },
            f.now,
          ),
        ).rejects.toThrow("stale");
      }
      const results = await Promise.allSettled(
        [true, false].map((approve) =>
          f.journal.decide(call.id, f.actorId, approve, expected, f.now),
        ),
      );
      expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
      const [approval] = await f.db
        .select()
        .from(mcpApproval)
        .where(eq(mcpApproval.executionId, call.id));
      expect(approval!.actorId).toBe(f.actorId);
      expect(approval!.consumedAt).toBeNull();
    } finally {
      await f.cleanup();
    }
  },
);
async function waitForBlocked(db: ReturnType<typeof getDb>, blocker: number) {
  const deadline = Date.now() + 1000;
  while (Date.now() < deadline) {
    const rows = await db.execute<{ waiting: boolean }>(
      sql`SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE ${blocker} = ANY(pg_blocking_pids(pid))) AS waiting`,
    );
    if (rows.rows[0]?.waiting === true) return;
    await Bun.sleep(5);
  }
  throw new Error("decision did not reach the held authorization row");
}
test.skipIf(!reachable)(
  "decisions serialize with current owner, admin role, ban and service identity changes",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      const changes = ["owner", "role", "ban", "service"];
      for (const change of changes) {
        await f.db
          .update(task)
          .set({ createdByUserId: change === "role" ? "other-owner" : f.actorId })
          .where(eq(task.id, f.taskId));
        await f.db
          .update(user)
          .set({
            role: change === "role" ? "admin" : "user",
            banned: false,
            email: `${f.actorId}@example.test`,
          })
          .where(eq(user.id, f.actorId));
        let unlock!: () => void, locked!: (pid: number) => void;
        const release = new Promise<void>((resolve) => {
          unlock = resolve;
        });
        const ready = new Promise<number>((resolve) => {
          locked = resolve;
        });
        const mutation = f.db.transaction(async (tx) => {
          if (change === "owner")
            await tx
              .update(task)
              .set({ createdByUserId: "other-owner" })
              .where(eq(task.id, f.taskId));
          else
            await tx
              .update(user)
              .set(
                change === "role"
                  ? { role: "user" }
                  : change === "ban"
                    ? { banned: true }
                    : { email: `apikey+${f.actorId}@service.local` },
              )
              .where(eq(user.id, f.actorId));
          const rows = await tx.execute<{ pid: number }>(sql`SELECT pg_backend_pid() AS pid`);
          locked(Number(rows.rows[0]!.pid));
          await release;
        });
        const blocker = await ready;
        const decision = f.journal.decide(call.id, f.actorId, true, expected, f.now).then(
          () => "allowed",
          () => "denied",
        );
        try {
          await waitForBlocked(f.db, blocker);
        } finally {
          unlock();
        }
        await mutation;
        expect(await decision).toBe("denied");
        expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
      }
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "local profile changes and replaced bindings invalidate review",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      await f.db.update(profile).set({ integrationGrants: [] }).where(eq(profile.id, f.profileId));
      await expect(f.journal.decide(call.id, f.actorId, true, expected, f.now)).rejects.toThrow(
        "stale",
      );
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "failed barrier releases reservation without consuming approval",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      const decision = await f.journal.decide(call.id, f.actorId, true, expected, f.now);
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const authorization = {
        executionId: call.id,
        requestDigest: call.requestDigest,
        bindingHash: call.bindingHash,
        principalId: call.principalId,
        expectedJournalRevision: decision.call.version - 1,
        reservationId: (await f.journal.getAdmission(call.id))!.reservationId,
        reservationExpiresAt: (await f.journal.getAdmission(call.id))!.expiresAt,
      };
      await expect(
        f.journal.dispatch(
          call.id,
          {
            bootId: "boot-a",
            intentId: crypto.randomUUID(),
            expiresAt: new Date(f.now.getTime() + 1000),
          },
          f.now,
          authorization,
        ),
      ).rejects.toThrow("stale");
      expect(
        await f.db.select().from(mcpAdmission).where(eq(mcpAdmission.executionId, call.id)),
      ).toHaveLength(0);
      const [approval] = await f.db
        .select()
        .from(mcpApproval)
        .where(eq(mcpApproval.executionId, call.id));
      expect(approval!.consumedAt).toBeNull();
      expect((await f.journal.get(call.id))!.state).toBe("authorized");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "unknown write blocks are global to the session across state pages",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      const approved = await f.journal.decide(call.id, f.actorId, true, expected, f.now);
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const dispatched = await f.journal.dispatch(
        call.id,
        {
          bootId: "boot-a",
          intentId: crypto.randomUUID(),
          expiresAt: new Date(f.now.getTime() + 1000),
        },
        f.now,
        {
          executionId: call.id,
          requestDigest: call.requestDigest,
          bindingHash: call.bindingHash,
          principalId: call.principalId,
          expectedJournalRevision: approved.call.version,
          reservationId: (await f.journal.getAdmission(call.id))!.reservationId,
          reservationExpiresAt: (await f.journal.getAdmission(call.id))!.expiresAt,
        },
      );
      await f.journal.finish(call.id, dispatched.version, "outcome_unknown", null, false, f.now);
      const page = await f.journal.readSession(f.sessionId, f.actorId, { after: "zzzz" });
      expect(page.rows).toHaveLength(0);
      expect(page.unknownWriteBlocks).toEqual([f.connection.id]);
      await f.journal.resolveUnknown(call.id, f.actorId, "acknowledged", f.now);
      expect((await f.journal.readSession(f.sessionId, f.actorId)).unknownWriteBlocks).toHaveLength(
        0,
      );
      expect(
        (await f.journal.readSession(f.sessionId, f.actorId)).rows[0]!.resolution?.outcome,
      ).toBe("acknowledged");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "legacy requests cannot gain review authority and frozen read-only requests stay blocked",
  async () => {
    const f = await reviewFixture();
    try {
      const { call } = await pending(f);
      const legacyId = crypto.randomUUID();
      await f.db.insert(mcpExecution).values({
        ...call,
        id: legacyId,
        toolCallId: "legacy",
        originalArguments: null,
        requestContext: null,
      });
      await f.journal.requestApproval(legacyId, f.deploymentId, f.now);
      const current = await f.journal.pending(f.sessionId, f.actorId, f.now);
      expect(current.map((row) => row.call.id)).not.toContain(legacyId);
      await expect(
        f.journal.decide(
          legacyId,
          f.actorId,
          true,
          reviewContext(call, current[0]!.approval),
          f.now,
        ),
      ).rejects.toThrow("context unavailable");
      const readonly = await f.journal.create(
        {
          ...f.input,
          toolCallId: "read-only",
          requestPolicyContext: {
            ...f.input.requestPolicyContext,
            toolCallId: "read-only",
            acceptedWriteEligibility: "read_only",
          },
        },
        { kind: "approval" },
        f.now,
      );
      const approval = await f.journal.requestApproval(readonly.id, f.deploymentId, f.now);
      await expect(
        f.journal.decide(readonly.id, f.actorId, true, reviewContext(readonly, approval), f.now),
      ).rejects.toThrow("policy denies");
      expect((await f.journal.get(readonly.id))!.state).toBe("waiting_approval");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "ancestor profile revocation invalidates a child approval without changing the child profile",
  async () => {
    const f = await reviewFixture();
    const childTaskId = crypto.randomUUID(),
      childSessionId = crypto.randomUUID(),
      childProfileId = crypto.randomUUID();
    try {
      const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
      const { bindingSnapshotColumns } = await import("../mcp/binding-store.ts");
      const live = { ...f.authority, profileId: childProfileId };
      const snapshot = compileSessionBindings({
        launchPolicy: { profileId: childProfileId, integrationGrants: live.profileGrants },
        liveAuthority: live,
        bindingEpoch: 0,
        requestedGrants: live.profileGrants,
        parentAuthority: { sessionId: f.sessionId, snapshot: f.snapshot },
      });
      await f.db.insert(profile).values({
        id: childProfileId,
        name: "Child review profile",
        imageId: "image",
        harness: "claude",
        integrationGrants: live.profileGrants,
      });
      await f.db.insert(task).values({
        id: childTaskId,
        type: "chat",
        createdByUserId: f.actorId,
        spawningSessionId: f.sessionId,
      });
      await f.db.insert(taskSession).values({
        taskId: childTaskId,
        sessionId: childSessionId,
        ...bindingSnapshotColumns(snapshot, [
          {
            id: f.connection.id,
            alias: f.connection.alias,
            provider: "mcp",
            displayName: "Tools",
            config: f.connection.config,
          },
        ]),
      });
      const binding = snapshot.immutableBindings[0]!,
        bound = binding.tools[0]!;
      const call = await f.journal.create(
        {
          ...f.input,
          taskId: childTaskId,
          sessionId: childSessionId,
          bindingHash: snapshot.bindingHash,
          resolvedTool: {
            ...f.input.resolvedTool,
            bindingHash: snapshot.bindingHash,
            binding,
            effectiveGrant: bound.grant,
            ancestorAuthorityRefs: snapshot.ancestorAuthorityRefs,
          },
          requestPolicyContext: {
            ...f.input.requestPolicyContext,
            sessionId: childSessionId,
            acceptedPolicyRef: {
              ...f.input.requestPolicyContext.acceptedPolicyRef,
              sessionId: childSessionId,
            },
            ancestorPolicyRefs: [f.input.requestPolicyContext.acceptedPolicyRef],
          },
        },
        { kind: "approval" },
        f.now,
      );
      const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      await f.db.update(profile).set({ integrationGrants: [] }).where(eq(profile.id, f.profileId));
      await expect(
        f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), f.now),
      ).rejects.toThrow("stale");
      expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
    } finally {
      await f.db.delete(task).where(eq(task.id, childTaskId));
      await f.db.delete(profile).where(eq(profile.id, childProfileId));
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "competing first requests with different argument bytes cannot share a call",
  async () => {
    const f = await reviewFixture();
    try {
      const inputs = [
        f.input,
        { ...f.input, originalArguments: '{"x":2}', argumentHash: canonicalDigest({ x: 2 }) },
      ];
      const outcomes = await Promise.allSettled(
        inputs.map((input) => f.journal.create(input, { kind: "approval" }, f.now)),
      );
      expect(outcomes.filter((result) => result.status === "fulfilled")).toHaveLength(1);
      expect(outcomes.filter((result) => result.status === "rejected")).toHaveLength(1);
      const state = await f.journal.readSession(f.sessionId, f.actorId);
      expect(state.rows).toHaveLength(1);
      const stored = state.rows[0]!.call;
      expect(canonicalDigest(JSON.parse(stored.originalArguments!))).toBe(stored.argumentHash);
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "a stale reservation attempt cannot release a replacement at the same clock time",
  async () => {
    const f = await reviewFixture();
    try {
      const { call, expected } = await pending(f);
      const approved = await f.journal.decide(call.id, f.actorId, true, expected, f.now);
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const old = (await f.journal.getAdmission(call.id))!;
      const authorization = {
        executionId: call.id,
        requestDigest: call.requestDigest,
        bindingHash: call.bindingHash,
        principalId: call.principalId,
        expectedJournalRevision: approved.call.version,
        reservationId: old.reservationId,
        reservationExpiresAt: old.expiresAt,
      };
      const intent = {
        bootId: "boot-a",
        intentId: crypto.randomUUID(),
        expiresAt: new Date(f.now.getTime() + 1000),
      };
      await expect(
        f.journal.dispatch(call.id, intent, f.now, {
          ...authorization,
          expectedJournalRevision: 0,
        }),
      ).rejects.toThrow();
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const replacement = (await f.journal.getAdmission(call.id))!;
      expect(replacement.createdAt).toEqual(old.createdAt);
      expect(replacement.expiresAt).toEqual(old.expiresAt);
      expect(replacement.reservationId).not.toBe(old.reservationId);
      await expect(f.journal.dispatch(call.id, intent, f.now, authorization)).rejects.toThrow();
      expect((await f.journal.getAdmission(call.id))?.reservationId).toBe(
        replacement.reservationId,
      );
    } finally {
      await f.cleanup();
    }
  },
);
