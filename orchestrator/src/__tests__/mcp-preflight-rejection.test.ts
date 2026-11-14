import { expect, test } from "bun:test";
import { schemaVerifiedFixture } from "./mcp-review-fixture.ts";
import { reviewContext } from "../mcp/journal-review.ts";

test("preflight rejection cannot release another generation or a committed dispatch", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    const approved = (
      await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), f.now)
    ).call;
    await f.journal.admit(call.id, f.deploymentId, f.now);
    const lease = (await f.journal.getAdmission(call.id))!;
    const rejection = {
      requestDigest: call.requestDigest,
      version: approved.version,
      reservation: { reservationId: lease.reservationId, expiresAt: lease.expiresAt },
    };
    await expect(
      f.journal.rejectBeforeDispatch(
        call.id,
        { ...rejection, reservation: { ...rejection.reservation, reservationId: "stale" } },
        f.now,
      ),
    ).rejects.toThrow();
    expect((await f.journal.getAdmission(call.id))!.reservationId).toBe(lease.reservationId);
    const proof = {
      executionId: call.id,
      requestDigest: call.requestDigest,
      bindingHash: call.bindingHash,
      principalId: call.principalId,
      expectedJournalRevision: approved.version,
      reservationId: lease.reservationId,
      reservationExpiresAt: lease.expiresAt,
    };
    const results = await Promise.allSettled([
      f.journal.dispatch(
        call.id,
        { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt },
        f.now,
        proof,
      ),
      f.journal.rejectBeforeDispatch(call.id, rejection, f.now),
    ]);
    expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
    const row = (await f.journal.get(call.id))!;
    if (row.state === "dispatching")
      expect((await f.journal.getAdmission(call.id))!.reservationId).toBe(lease.reservationId);
    else {
      expect(row.state).toBe("failed");
      expect(row.intentId).toBeNull();
      expect(await f.journal.getAdmission(call.id)).toBeNull();
    }
  } finally {
    await f.cleanup();
  }
});

import { persistVerifiedOutcome } from "../mcp/effect-driver.ts";
test("a verified late success survives cancellation between result read and result commit", async () => {
  const f = await schemaVerifiedFixture();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    const approved = (
      await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), f.now)
    ).call;
    await f.journal.admit(call.id, f.deploymentId, f.now);
    const lease = (await f.journal.getAdmission(call.id))!;
    const dispatched = await f.journal.dispatch(
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
    let commits = 0;
    const result = await persistVerifiedOutcome(
      {
        ...f.journal,
        async finish(...args) {
          if (++commits === 1)
            await f.journal.cancel(
              call.id,
              f.actorId,
              f.now,
              (await f.journal.get(call.id))!.version,
            );
          return f.journal.finish(...args);
        },
      },
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
    expect(commits).toBe(2);
    expect(await f.journal.getAdmission(call.id)).toBeNull();
  } finally {
    await f.cleanup();
  }
});
