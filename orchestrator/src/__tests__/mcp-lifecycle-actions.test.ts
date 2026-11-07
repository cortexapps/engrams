import { expect, test } from "bun:test";
import { schemaVerifiedFixture } from "./mcp-review-fixture.ts";
import { reviewContext } from "../mcp/journal-review.ts";
import { persistVerifiedOutcome } from "../mcp/effect-driver.ts";

async function dispatch(f: Awaited<ReturnType<typeof schemaVerifiedFixture>>) {
  const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
  const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
  const approved = (
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), f.now)
  ).call;
  await f.journal.admit(call.id, f.deploymentId, f.now);
  const lease = (await f.journal.getAdmission(call.id))!;
  return f.journal.dispatch(
    call.id,
    { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt },
    f.now,
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
}

test("cancel requires the observed revision and returns a durable receipt without undo", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const call = await dispatch(f);
    await expect(f.journal.cancel(call.id, f.actorId, f.now, call.version - 1)).rejects.toThrow(
      "stale",
    );
    expect((await f.journal.get(call.id))!.state).toBe("dispatching");
    const canceled = await f.journal.cancel(call.id, f.actorId, f.now, call.version);
    expect(canceled.state).toBe("outcome_unknown");
    const data = await f.journal.readSession(f.sessionId, f.actorId, { limit: 16 });
    expect(data.rows[0]!.cancellation).toMatchObject({
      actorId: f.actorId,
      expectedJournalRevision: call.version,
      journalRevision: call.version + 1,
      state: "outcome_unknown",
    });
    expect(await f.journal.hasUnknown(f.sessionId, f.connection.id)).toBe(true);
  } finally {
    await f.cleanup();
  }
});

test("resolution requires exact unknown context and retains later verified truth", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const dispatched = await dispatch(f);
    const call = await f.journal.cancel(dispatched.id, f.actorId, f.now, dispatched.version);
    const expected = {
      executionId: call.id,
      journalRevision: call.version,
      requestDigest: call.requestDigest,
      bindingHash: call.bindingHash,
      intentId: call.intentId!,
    };
    await expect(
      f.journal.resolveUnknown(call.id, f.actorId, "acknowledged", f.now, {
        ...expected,
        intentId: "other",
      }),
    ).rejects.toThrow("stale");
    expect(await f.journal.hasUnknown(f.sessionId, f.connection.id)).toBe(true);
    const receipt = await f.journal.resolveUnknown(
      call.id,
      f.actorId,
      "acknowledged",
      f.now,
      expected,
    );
    expect(receipt.expectedContext).toEqual(expected);
    expect(await f.journal.hasUnknown(f.sessionId, f.connection.id)).toBe(false);
    const result = await persistVerifiedOutcome(
      f.journal,
      dispatched,
      {
        state: "succeeded",
        result: { outcome: "succeeded", delivery: "available", output: { content: [] } },
        resultUnavailable: false,
      },
      () => f.now,
    );
    expect(result.state).toBe("succeeded");
    expect(result.intentId).toBe(dispatched.intentId);
    const data = await f.journal.readSession(f.sessionId, f.actorId, { limit: 16 });
    expect(data.rows[0]!.resolution).toEqual(receipt);
    expect(data.rows[0]!.call.result).toEqual(result.result);
  } finally {
    await f.cleanup();
  }
});

test("verified result before resolution makes the unknown context stale", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const dispatched = await dispatch(f);
    const unknown = await f.journal.cancel(dispatched.id, f.actorId, f.now, dispatched.version);
    const expected = {
      executionId: unknown.id,
      journalRevision: unknown.version,
      requestDigest: unknown.requestDigest,
      bindingHash: unknown.bindingHash,
      intentId: unknown.intentId!,
    };
    const result = await persistVerifiedOutcome(
      f.journal,
      dispatched,
      {
        state: "succeeded",
        result: { outcome: "succeeded", delivery: "available", output: { content: [] } },
        resultUnavailable: false,
      },
      () => f.now,
    );
    expect(result.state).toBe("succeeded");
    await expect(
      f.journal.resolveUnknown(unknown.id, f.actorId, "acknowledged", f.now, expected),
    ).rejects.toThrow("stale");
    const data = await f.journal.readSession(f.sessionId, f.actorId, { limit: 16 });
    expect(data.rows[0]!.resolution).toBeNull();
    expect(data.rows[0]!.call.result).toEqual(result.result);
  } finally {
    await f.cleanup();
  }
});

test("concurrent resolution and verified result preserve the committed evidence", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const dispatched = await dispatch(f);
    const unknown = await f.journal.cancel(dispatched.id, f.actorId, f.now, dispatched.version);
    const expected = {
      executionId: unknown.id,
      journalRevision: unknown.version,
      requestDigest: unknown.requestDigest,
      bindingHash: unknown.bindingHash,
      intentId: unknown.intentId!,
    };
    const [resolution, result] = await Promise.allSettled([
      f.journal.resolveUnknown(unknown.id, f.actorId, "acknowledged", f.now, expected),
      persistVerifiedOutcome(
        f.journal,
        dispatched,
        {
          state: "succeeded",
          result: { outcome: "succeeded", delivery: "available", output: { content: [] } },
          resultUnavailable: false,
        },
        () => f.now,
      ),
    ]);
    expect(result.status).toBe("fulfilled");
    const data = await f.journal.readSession(f.sessionId, f.actorId, { limit: 16 });
    expect(data.rows[0]!.call.state).toBe("succeeded");
    expect(data.rows[0]!.call.result).toMatchObject({ outcome: "succeeded" });
    if (resolution.status === "fulfilled")
      expect(data.rows[0]!.resolution).toEqual(resolution.value);
    else {
      expect(String(resolution.reason)).toContain("stale");
      expect(data.rows[0]!.resolution).toBeNull();
    }
  } finally {
    await f.cleanup();
  }
});

test("an unverified read failure cannot overwrite a concurrent cancellation", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const dispatched = await dispatch(f);
    let first = true;
    const journal = {
      ...f.journal,
      async get(id: string) {
        const before = await f.journal.get(id);
        if (first) {
          first = false;
          await f.journal.cancel(id, f.actorId, f.now, dispatched.version);
        }
        return before;
      },
    };
    const result = await persistVerifiedOutcome(
      journal,
      dispatched,
      {
        state: "failed",
        result: { outcome: "failed", delivery: "unavailable" },
        resultUnavailable: false,
      },
      () => f.now,
      false,
    );
    expect(result.state).toBe("outcome_unknown");
  } finally {
    await f.cleanup();
  }
});
