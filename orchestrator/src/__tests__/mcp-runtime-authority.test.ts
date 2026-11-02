import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { taskSession, user } from "../db/schema.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { reviewContext } from "../mcp/journal-review.ts";

async function ready() {
  const f = await reviewFixture();
  await f.db
    .update(taskSession)
    .set({ integrationPrincipalId: f.actorId })
    .where(eq(taskSession.sessionId, f.sessionId));
  const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
  await f.journal.admitSchema(call.id, f.deploymentId, f.now);
  const schema = (await f.journal.getAdmission(call.id))!;
  await f.journal.finishSchema(
    call.id,
    schema.reservationId,
    schema.expiresAt,
    call.requestDigest,
    true,
    f.now,
  );
  const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
  const approved = await f.journal.decide(
    call.id,
    f.actorId,
    true,
    reviewContext(call, approval),
    f.now,
  );
  await f.journal.admit(call.id, f.deploymentId, f.now);
  const lease = (await f.journal.getAdmission(call.id))!;
  const proof = {
    executionId: call.id,
    requestDigest: call.requestDigest,
    bindingHash: call.bindingHash,
    principalId: call.principalId,
    expectedJournalRevision: approved.call.version,
    reservationId: lease.reservationId,
    reservationExpiresAt: lease.expiresAt,
  };
  const intent = { bootId: "boot-a", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt };
  return { ...f, call, proof, intent };
}
test("barrier checks the current immutable principal and ban in its transaction", async () => {
  const f = await ready();
  try {
    await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    await expect(f.journal.dispatch(f.call.id, f.intent, f.now, f.proof)).rejects.toThrow();
    expect((await f.journal.get(f.call.id))!.state).toBe("authorized");
  } finally {
    await f.cleanup();
  }
});
test("barrier denies a principal changed after review", async () => {
  const f = await ready();
  try {
    await f.db
      .update(taskSession)
      .set({ integrationPrincipalId: "other" })
      .where(eq(taskSession.sessionId, f.sessionId));
    await expect(f.journal.dispatch(f.call.id, f.intent, f.now, f.proof)).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
});

test("successful barrier and both live assertions retain exact lease generation", async () => {
  const f = await ready();
  try {
    expect((await f.journal.dispatch(f.call.id, f.intent, f.now, f.proof)).state).toBe(
      "dispatching",
    );
    expect((await f.journal.assertDispatchCurrent(f.call.id, f.proof, f.now)).state).toBe(
      "dispatching",
    );
    await expect(
      f.journal.assertDispatchCurrent(
        f.call.id,
        { ...f.proof, reservationId: "other-generation" },
        f.now,
      ),
    ).rejects.toThrow("stale");
    await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    await expect(f.journal.assertDispatchCurrent(f.call.id, f.proof, f.now)).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
});

test("automation requires exact live linkage and published grant; creator role is not authority", async () => {
  const { automation, automationRun, automationSession, task } = await import("../db/schema.ts");
  const { makeProfileStore } = await import("../db/profiles.ts");
  const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
  const { bindingSnapshotColumns } = await import("../mcp/binding-store.ts");
  for (const publish of [false, true]) {
    const f = await reviewFixture();
    const automationId = crypto.randomUUID(),
      runId = crypto.randomUUID(),
      principalId = `automation:${automationId}`;
    try {
      const profiles = makeProfileStore(f.db);
      if (publish) {
        await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
        await profiles.update(f.profileId, (await profiles.get(f.profileId))!, {
          actorId: f.actorId,
          expectedRevision: 0,
        });
        await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
      }
      const publication = (await profiles.get(f.profileId))!.mcpGrantPublication ?? null;
      const snapshot = compileSessionBindings({
        launchPolicy: { profileId: f.profileId, integrationGrants: f.authority.profileGrants },
        liveAuthority: { ...f.authority, profilePublication: publication },
        bindingEpoch: 0,
      });
      await f.db
        .update(taskSession)
        .set({
          ...bindingSnapshotColumns(snapshot, [
            {
              id: f.connection.id,
              alias: f.connection.alias,
              provider: "mcp",
              displayName: f.connection.displayName,
              config: f.connection.config,
            },
          ]),
          integrationPrincipalId: principalId,
        })
        .where(eq(taskSession.sessionId, f.sessionId));
      await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
      await f.db
        .insert(automation)
        .values({ id: automationId, name: "Test automation", createdByUserId: f.actorId });
      await f.db.insert(automationRun).values({
        id: runId,
        automationId,
        taskId: f.taskId,
        sessionId: f.sessionId,
        status: "running",
        trigger: { source: "manual", receivedAt: f.now.toISOString() },
      });
      await f.db
        .insert(automationSession)
        .values({ sessionId: f.sessionId, runId, blockId: "start" });
      const input = {
        ...f.input,
        principalId,
        bindingHash: snapshot.bindingHash,
        resolvedTool: {
          ...f.input.resolvedTool,
          bindingHash: snapshot.bindingHash,
          binding: snapshot.immutableBindings[0]!,
          profilePublication: publication,
        },
      };
      const call = await f.journal.create(
        input,
        { kind: "automation", grant: input.resolvedTool.effectiveGrant },
        f.now,
      );
      await f.journal.admitSchema(call.id, f.deploymentId, f.now);
      const schema = (await f.journal.getAdmission(call.id))!;
      await f.journal.finishSchema(
        call.id,
        schema.reservationId,
        schema.expiresAt,
        call.requestDigest,
        true,
        f.now,
      );
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const lease = (await f.journal.getAdmission(call.id))!;
      const proof = {
        executionId: call.id,
        requestDigest: call.requestDigest,
        bindingHash: call.bindingHash,
        principalId,
        expectedJournalRevision: call.version,
        reservationId: lease.reservationId,
        reservationExpiresAt: lease.expiresAt,
      };
      const intent = { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt };
      if (!publish)
        await expect(f.journal.dispatch(call.id, intent, f.now, proof)).rejects.toThrow(
          "published",
        );
      else {
        expect((await f.journal.dispatch(call.id, intent, f.now, proof)).state).toBe("dispatching");
        await f.db
          .update(automation)
          .set({ enabled: false })
          .where(eq(automation.id, automationId));
        await expect(f.journal.assertDispatchCurrent(call.id, proof, f.now)).rejects.toThrow(
          "automation",
        );
      }
    } finally {
      await f.db.delete(automation).where(eq(automation.id, automationId));
      await f.cleanup();
    }
  }
});

test("competing barriers commit once and losing cleanup preserves the committed lease", async () => {
  const f = await ready();
  try {
    const results = await Promise.allSettled([
      f.journal.dispatch(f.call.id, f.intent, f.now, f.proof),
      f.journal.dispatch(f.call.id, f.intent, f.now, f.proof),
    ]);
    expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
    expect((await f.journal.getAdmission(f.call.id))!.reservationId).toBe(f.proof.reservationId);
    expect((await f.journal.assertDispatchCurrent(f.call.id, f.proof, f.now)).state).toBe(
      "dispatching",
    );
  } finally {
    await f.cleanup();
  }
});

test("barrier and human cancellation share a safe lock order", async () => {
  const f = await ready();
  try {
    const results = await Promise.allSettled([
      f.journal.dispatch(f.call.id, f.intent, f.now, f.proof),
      f.journal.cancel(f.call.id, f.actorId, f.now),
    ]);
    expect(results[1]!.status).toBe("fulfilled");
    expect(["canceled", "outcome_unknown"]).toContain((await f.journal.get(f.call.id))!.state);
    expect(await f.journal.getAdmission(f.call.id)).toBeNull();
    await expect(f.journal.assertDispatchCurrent(f.call.id, f.proof, f.now)).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
});
