import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { consumerCursor } from "../db/schema.ts";
import { makeMcpLifecycleEvents } from "../mcp/lifecycle-events.ts";

test("a failed event cannot be skipped and competing replicas advance only after cancellation", async () => {
  const db = getDb();
  const sessionId = crypto.randomUUID();
  const events = [
    { idx: 1n, kind: "run_interrupted", payloadJson: "{}" },
    { idx: 2n, kind: "resumed", payloadJson: "{}" },
  ];
  let fail = true;
  const canceled: bigint[] = [];
  const make = () =>
    makeMcpLifecycleEvents({
      db,
      now: () => new Date(),
      list: async (_id, after) => events.filter((event) => event.idx > after),
      reconcile: async (_id, event) => {
        if (event.idx === 1n && fail) throw Error("unavailable");
        canceled.push(event.idx);
      },
    });
  try {
    await expect(make().runOnce(sessionId)).rejects.toThrow("unavailable");
    expect(
      (await db.select().from(consumerCursor).where(eq(consumerCursor.sessionId, sessionId)))[0]!
        .lastIdx,
    ).toBe(-1n);
    expect(canceled).toEqual([]);
    fail = false;
    await Promise.all([make().runOnce(sessionId), make().runOnce(sessionId)]);
    await make().runOnce(sessionId);
    expect(
      (await db.select().from(consumerCursor).where(eq(consumerCursor.sessionId, sessionId)))[0]!
        .lastIdx,
    ).toBe(2n);
    expect(canceled).toContain(1n);
    expect(canceled).toContain(2n);
    expect(canceled.indexOf(1n)).toBeLessThan(canceled.indexOf(2n));
  } finally {
    await db.delete(consumerCursor).where(eq(consumerCursor.sessionId, sessionId));
  }
});

test("invalid event order cannot advance beyond the last committed event", async () => {
  const db = getDb();
  const sessionId = crypto.randomUUID();
  const reader = makeMcpLifecycleEvents({
    db,
    now: () => new Date(),
    list: async () => [
      { idx: 3n, kind: "run_interrupted", payloadJson: "{}" },
      { idx: 2n, kind: "resumed", payloadJson: "{}" },
    ],
    reconcile: async () => {},
  });
  try {
    await expect(reader.runOnce(sessionId)).rejects.toThrow("order");
    const [cursor] = await db
      .select()
      .from(consumerCursor)
      .where(eq(consumerCursor.sessionId, sessionId));
    expect(cursor!.lastIdx).toBe(-1n);
  } finally {
    await db.delete(consumerCursor).where(eq(consumerCursor.sessionId, sessionId));
  }
});
