/** Live-PG tests for the listener lease store's dormancy statements: the
 * fenced stand-down, the wake, and the desired/dormant listings against the
 * real `session_listeners` schema. Gated on ORCHESTRATOR_DATABASE_URL + a
 * reachability probe; rows use unique ids so the suite is parallel-safe. */

import { afterAll, describe, expect, test } from "bun:test";
import { sql } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { makeLeaseStore } from "../listeners/lease-store.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;
const runId = `leasetest-${Date.now()}`;

afterAll(async () => {
  if (!dbReachable) return;
  await getDb().execute(sql`delete from "session_listeners" where "session_id" like ${`${runId}-%`}`);
});

describe("listener lease store with live Postgres", () => {
  test.skipIf(!dbReachable)("a stand-down is fenced by the owner and the wake grace, and a wake re-arms", async () => {
    const store = makeLeaseStore();
    const sessionId = `${runId}-parked`;
    await store.ensureRow(sessionId);
    expect(await store.tryAcquire(sessionId, "owner-a", 60_000)).toBe(true);

    // Not the owner: refused.
    expect(await store.markDormant(sessionId, "owner-b", 120_000)).toBe(false);
    // A fresh wake: refused, the lease stays.
    await store.wake(sessionId);
    expect(await store.markDormant(sessionId, "owner-a", 120_000)).toBe(false);
    expect(await store.renew(sessionId, "owner-a", 60_000)).toBe(true);
    // Past the grace: the row goes dormant and the lease is released.
    await getDb().execute(sql`
      update "session_listeners" set "woken_at" = now() - interval '3 minutes'
      where "session_id" = ${sessionId}
    `);
    expect(await store.markDormant(sessionId, "owner-a", 120_000)).toBe(true);
    expect(await store.renew(sessionId, "owner-a", 60_000)).toBe(false);
    expect(await store.tryAcquire(sessionId, "owner-b", 60_000)).toBe(false);

    const mine = (ids: string[]) => ids.filter((id) => id.startsWith(runId));
    expect(mine(await store.listDesired())).toEqual([]);
    expect(mine(await store.listDormant())).toEqual([sessionId]);

    await store.wake(sessionId);
    expect(mine(await store.listDesired())).toEqual([sessionId]);
    expect(mine(await store.listDormant())).toEqual([]);
    expect(await store.tryAcquire(sessionId, "owner-b", 60_000)).toBe(true);

    // A terminal row never wakes back into the desired set.
    await store.markTerminal(sessionId);
    await store.wake(sessionId);
    expect(mine(await store.listDesired())).toEqual([]);
    expect(mine(await store.listDormant())).toEqual([]);
  });
});
