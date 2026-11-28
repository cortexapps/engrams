import { expect, test } from "bun:test";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { canonicalDigest } from "../mcp/hash.ts";

// Real isolated PostgreSQL; schema success below is an explicit trusted-worker fixture.
test("schema lease is released before approval and execute gets a distinct generation", async () => {
  const f = await reviewFixture();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    await expect(f.journal.requestApproval(call.id, f.deploymentId, f.now)).rejects.toThrow(
      "schema",
    );
    expect(await f.journal.admitSchema(call.id, f.deploymentId, f.now)).toBe("admitted");
    const schema = (await f.journal.getAdmission(call.id))!;
    await f.journal.finishSchema(
      call.id,
      schema.reservationId,
      schema.expiresAt,
      call.requestDigest,
      true,
      f.now,
    );
    expect((await f.journal.getAdmission(call.id))!.state).toBe("released");
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    expect(approval.expiresAt.getTime()).toBe(f.now.getTime() + 600_000);
    expect((await f.journal.getAdmission(call.id))!.schemaDigest).toBe(call.requestDigest);
    await expect(
      f.journal.finishSchema(
        call.id,
        schema.reservationId,
        schema.expiresAt,
        canonicalDigest({ wrong: true }),
        true,
        f.now,
      ),
    ).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
});

test("released failed schema preserves first queue expiry across replacement generations", async () => {
  const f = await reviewFixture();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    await f.journal.admitSchema(call.id, f.deploymentId, f.now);
    const first = (await f.journal.getAdmission(call.id))!;
    await f.journal.finishSchema(
      call.id,
      first.reservationId,
      first.expiresAt,
      call.requestDigest,
      false,
      f.now,
    );
    const later = new Date(f.now.getTime() + 20_000);
    await f.journal.admitSchema(call.id, f.deploymentId, later);
    const second = (await f.journal.getAdmission(call.id))!;
    expect(second.reservationId).not.toBe(first.reservationId);
    expect(second.schemaQueueExpiresAt).toEqual(first.schemaQueueExpiresAt);
    await expect(
      f.journal.finishSchema(
        call.id,
        first.reservationId,
        first.expiresAt,
        call.requestDigest,
        true,
        later,
      ),
    ).rejects.toThrow();
    await f.journal.finishSchema(
      call.id,
      second.reservationId,
      second.expiresAt,
      call.requestDigest,
      false,
      later,
    );
    await expect(
      f.journal.admitSchema(call.id, f.deploymentId, new Date(f.now.getTime() + 30_001)),
    ).rejects.toThrow("queue");
  } finally {
    await f.cleanup();
  }
});

test("execute replacement preserves first queue and tool expiries after approval wait", async () => {
  const f = await reviewFixture();
  try {
    const call = await f.journal.create(
      { ...f.input, deadline: new Date(f.now.getTime() + 900_000) },
      { kind: "approval" },
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
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    const { reviewContext } = await import("../mcp/journal-review.ts");
    const afterWait = new Date(f.now.getTime() + 300_000);
    const approved = await f.journal.decide(
      call.id,
      f.actorId,
      true,
      reviewContext(call, approval),
      afterWait,
    );
    await f.journal.admit(call.id, f.deploymentId, afterWait);
    const first = (await f.journal.getAdmission(call.id))!;
    expect(first.reservationId).not.toBe(schema.reservationId);
    expect(first.expiresAt.getTime()).toBe(afterWait.getTime() + 60_000);
    expect(first.executeQueueExpiresAt!.getTime()).toBe(afterWait.getTime() + 30_000);
    const proof = {
      executionId: call.id,
      requestDigest: call.requestDigest,
      bindingHash: call.bindingHash,
      principalId: call.principalId,
      expectedJournalRevision: approved.call.version - 1,
      reservationId: first.reservationId,
      reservationExpiresAt: first.expiresAt,
    };
    await expect(
      f.journal.dispatch(
        call.id,
        { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: first.expiresAt },
        afterWait,
        proof,
      ),
    ).rejects.toThrow("stale");
    await f.journal.admit(call.id, f.deploymentId, new Date(afterWait.getTime() + 20_000));
    const replacement = (await f.journal.getAdmission(call.id))!;
    expect(replacement.reservationId).not.toBe(first.reservationId);
    expect(replacement.expiresAt).toEqual(first.expiresAt);
    expect(replacement.executeQueueExpiresAt).toEqual(first.executeQueueExpiresAt);
    await expect(
      f.journal.finishSchema(
        call.id,
        schema.reservationId,
        schema.expiresAt,
        call.requestDigest,
        true,
        afterWait,
      ),
    ).rejects.toThrow();
    expect((await f.journal.getAdmission(call.id))!.reservationId).toBe(replacement.reservationId);
  } finally {
    await f.cleanup();
  }
});

test("schema capacity is shared across database clients and released capacity promotes without renewing queue", async () => {
  const fixtures = await Promise.all(Array.from({ length: 5 }, () => reviewFixture()));
  const first = fixtures[0]!;
  try {
    const calls = await Promise.all(
      fixtures.map((f) => f.journal.create(f.input, { kind: "approval" }, f.now)),
    );
    const states = await Promise.all(
      fixtures.map((f, i) => f.journal.admitSchema(calls[i]!.id, first.deploymentId, f.now)),
    );
    expect(states.filter((state) => state === "admitted")).toHaveLength(4);
    const queuedIndex = states.indexOf("queued"),
      activeIndex = states.indexOf("admitted");
    const waiting = (await first.journal.getAdmission(calls[queuedIndex]!.id))!;
    const active = (await first.journal.getAdmission(calls[activeIndex]!.id))!;
    await first.journal.finishSchema(
      calls[activeIndex]!.id,
      active.reservationId,
      active.expiresAt,
      calls[activeIndex]!.requestDigest,
      true,
      first.now,
    );
    expect(
      await first.journal.admitSchema(
        calls[queuedIndex]!.id,
        first.deploymentId,
        new Date(first.now.getTime() + 5_000),
      ),
    ).toBe("admitted");
    expect(
      (await first.journal.getAdmission(calls[queuedIndex]!.id))!.schemaQueueExpiresAt,
    ).toEqual(waiting.schemaQueueExpiresAt);
  } finally {
    const { task } = await import("../db/schema.ts");
    const { eq } = await import("drizzle-orm");
    for (const f of fixtures) await f.db.delete(task).where(eq(task.id, f.taskId));
    for (const f of fixtures) await f.cleanup();
  }
});

test("capacity reads exclude retained history but count expired active leases until release", async () => {
  const f = await reviewFixture();
  const { drizzle } = await import("drizzle-orm/node-postgres");
  const { getPool } = await import("../db/client.ts");
  const schema = await import("../db/schema.ts");
  const { makeMcpAdmission } = await import("../mcp/admission.ts");
  const capacityReads: { query: string; params: unknown[] }[] = [];
  const capacity = makeMcpAdmission(
    drizzle(getPool(), {
      schema,
      logger: {
        logQuery(query, params) {
          if (
            query.startsWith("select") &&
            query.includes('from "mcp_admission"') &&
            query.includes('"mcp_admission"."deployment_id" =')
          ) {
            capacityReads.push({ query, params });
          }
        },
      },
    }),
  );
  try {
    const calls = [];
    for (let i = 0; i < 7; i++) {
      const toolCallId = `capacity-${i}`;
      const call = await f.journal.create(
        {
          ...f.input,
          toolCallId,
          requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId },
        },
        { kind: "approval" },
        f.now,
      );
      calls.push(call);
      if (i === 6) continue;
      await f.journal.admitSchema(call.id, f.deploymentId, f.now);
      if (i < 2) {
        const lease = (await f.journal.getAdmission(call.id))!;
        await f.journal.finishSchema(
          call.id,
          lease.reservationId,
          lease.expiresAt,
          call.requestDigest,
          i === 0,
          f.now,
        );
      }
    }
    const successfulHistory = (await f.journal.getAdmission(calls[0]!.id))!;
    const failedHistory = (await f.journal.getAdmission(calls[1]!.id))!;
    const later = new Date(f.now.getTime() + 15_000);
    // The four active leases are expired, but still own capacity until explicit release.
    expect(await capacity.reserve(calls[6]!.id, f.deploymentId, "schema", later)).toBe("queued");
    const waiting = (await f.journal.getAdmission(calls[6]!.id))!;
    expect(capacityReads).toHaveLength(1);
    const read = capacityReads[0]!;
    // Replay the actual emitted SQL: four active rows plus the newly queued row, no history.
    const population = await getPool().query<{ state: string; execution_id: string }>(
      read.query,
      read.params,
    );
    expect(population.rows).toHaveLength(5);
    expect(population.rows.filter((row) => row.state === "admitted")).toHaveLength(4);
    expect(population.rows.filter((row) => row.state === "queued")).toHaveLength(1);
    expect(population.rows.map((row) => row.execution_id)).not.toContain(calls[0]!.id);
    expect(population.rows.map((row) => row.execution_id)).not.toContain(calls[1]!.id);
    expect(await f.journal.getAdmission(calls[0]!.id)).toEqual(successfulHistory);
    expect(await f.journal.getAdmission(calls[1]!.id)).toEqual(failedHistory);
    const active = (await f.journal.getAdmission(calls[2]!.id))!;
    await f.journal.finishSchema(
      calls[2]!.id,
      active.reservationId,
      active.expiresAt,
      calls[2]!.requestDigest,
      false,
      later,
    );
    const promotionTime = new Date(f.now.getTime() + 20_000);
    expect(await capacity.reserve(calls[6]!.id, f.deploymentId, "schema", promotionTime)).toBe(
      "admitted",
    );
    expect((await f.journal.getAdmission(calls[6]!.id))!.schemaQueueExpiresAt).toEqual(
      waiting.schemaQueueExpiresAt,
    );
    // The separately fetched released row still carries its original retry deadline.
    expect(await capacity.reserve(calls[1]!.id, f.deploymentId, "schema", promotionTime)).toBe(
      "queued",
    );
    const replacement = (await f.journal.getAdmission(calls[1]!.id))!;
    expect(replacement.schemaQueueExpiresAt).toEqual(failedHistory.schemaQueueExpiresAt);
    expect(replacement.reservationId).not.toBe(failedHistory.reservationId);
    expect((await f.journal.getAdmission(calls[0]!.id))!.schemaDigest).toBe(
      calls[0]!.requestDigest,
    );
  } finally {
    await f.cleanup();
  }
});
