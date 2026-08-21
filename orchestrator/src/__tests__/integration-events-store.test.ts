/** integration_event store — live-PG lanes (ADR 0119 D5).
 *
 * Env-gated on ORCHESTRATOR_DATABASE_URL + reachability, per the db.test.ts
 * convention: absent/unreachable reports SKIP, never a silent pass. CI's
 * orchestrator lane migrates then tests, so these run there.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { eq } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  INTEGRATION_EVENT_RETENTION_PER_KEY,
  makeIntegrationEventStore,
} from "../db/integration-events.ts";
import { integrationConnection, integrationEvent } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const CONNECTION_ID = `test-int-events-${Date.now()}`;

describe("integration-event store (live PG)", () => {
  beforeAll(async () => {
    if (!dbReachable) return;
    await getDb()
      .insert(integrationConnection)
      .values({
        id: CONNECTION_ID,
        alias: CONNECTION_ID,
        provider: "github",
        displayName: "Ledger test connection",
      })
      .onConflictDoNothing();
  });

  afterAll(async () => {
    if (!dbReachable) return;
    // Cascade removes the ledger rows.
    await getDb().delete(integrationConnection).where(eq(integrationConnection.id, CONNECTION_ID));
  });

  const input = (deliveryId: string, eventKey = "pull_request.opened") => ({
    provider: "github",
    connectionId: CONNECTION_ID,
    eventKey,
    deliveryId,
    payload: { n: deliveryId },
    scopeValue: "acme/repo",
    receivedAt: new Date(),
  });

  test.skipIf(!dbReachable)("a redelivery is a no-op on the delivery unique", async () => {
    const store = makeIntegrationEventStore();
    const first = await store.record(input(`dup-${Date.now()}`));
    expect(first.recorded).toBe(true);
    const again = await store.record({ ...input("x"), deliveryId: (await store.list(CONNECTION_ID, undefined, 1))[0]!.deliveryId });
    expect(again.recorded).toBe(false);
  });

  test.skipIf(!dbReachable)("write-time retention keeps the newest 20 per (connection, key)", async () => {
    const store = makeIntegrationEventStore();
    const key = `retention-${Date.now()}`;
    for (let i = 0; i < INTEGRATION_EVENT_RETENTION_PER_KEY + 3; i += 1) {
      await store.record({
        ...input(`r-${key}-${i}`, key),
        receivedAt: new Date(Date.now() + i * 1000),
      });
    }
    const rows = await store.list(CONNECTION_ID, key, 100);
    expect(rows).toHaveLength(INTEGRATION_EVENT_RETENTION_PER_KEY);
    // Newest first; the pruned rows are the oldest three.
    expect(rows[0]!.deliveryId).toBe(`r-${key}-${INTEGRATION_EVENT_RETENTION_PER_KEY + 2}`);
    const latest = await store.getLatest(CONNECTION_ID, key);
    expect(latest?.deliveryId).toBe(rows[0]!.deliveryId);
  });

  test.skipIf(!dbReachable)("sweepExpired removes only rows past the 7-day window", async () => {
    const store = makeIntegrationEventStore();
    const key = `sweep-${Date.now()}`;
    await store.record({ ...input(`old-${key}`, key), receivedAt: new Date(Date.now() - 8 * 24 * 3600 * 1000) });
    await store.record({ ...input(`new-${key}`, key), receivedAt: new Date() });
    const swept = await store.sweepExpired(new Date());
    expect(swept).toBeGreaterThanOrEqual(1);
    const rows = await store.list(CONNECTION_ID, key, 10);
    expect(rows.map((r) => r.deliveryId)).toEqual([`new-${key}`]);
  });

  test.skipIf(!dbReachable)("observed event keys are distinct and sorted", async () => {
    const store = makeIntegrationEventStore();
    const stamp = Date.now();
    await store.record(input(`k1-${stamp}`, `zz-${stamp}`));
    await store.record(input(`k2-${stamp}`, `aa-${stamp}`));
    await store.record(input(`k3-${stamp}`, `aa-${stamp}`));
    const keys = await store.listObservedEventKeys(CONNECTION_ID);
    const mine = keys.filter((k) => k.endsWith(String(stamp)));
    expect(mine).toEqual([`aa-${stamp}`, `zz-${stamp}`]);
  });

  test.skipIf(!dbReachable)("connection delete cascades the ledger", async () => {
    const db = getDb();
    const store = makeIntegrationEventStore();
    const tempConnection = `${CONNECTION_ID}-cascade`;
    await db
      .insert(integrationConnection)
      .values({ id: tempConnection, alias: tempConnection, provider: "github", displayName: "t" })
      .onConflictDoNothing();
    await store.record({ ...input("cascade-1"), connectionId: tempConnection });
    await db.delete(integrationConnection).where(eq(integrationConnection.id, tempConnection));
    const orphans = await db
      .select()
      .from(integrationEvent)
      .where(eq(integrationEvent.connectionId, tempConnection));
    expect(orphans).toHaveLength(0);
  });
});
