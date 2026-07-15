import { describe, expect, test } from "bun:test";
import { PgDialect } from "drizzle-orm/pg-core";
import type { SQL } from "drizzle-orm";

import {
  makeInMemoryLeaseStore,
  makeLeaseStore,
  type LeaseExecutor,
  type LeaseStore,
} from "../lease-store.ts";

function contractStore() {
  let nowMs = 1_000;
  const store = makeInMemoryLeaseStore(() => new Date(nowMs));
  return {
    store,
    advance(ms: number) {
      nowMs += ms;
    },
  };
}

async function seed(store: LeaseStore, sessionId = "session-1"): Promise<void> {
  await store.ensureRow(sessionId);
}

describe("LeaseStore contract", () => {
  test("acquires an unowned listener row", async () => {
    const { store } = contractStore();
    await seed(store);

    expect(await store.tryAcquire("session-1", "owner-a", 1_000)).toBe(true);
  });

  test("rejects acquisition while another live owner holds the lease", async () => {
    const { store } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);

    expect(await store.tryAcquire("session-1", "owner-b", 1_000)).toBe(false);
  });

  test("steals an expired lease", async () => {
    const { store, advance } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);
    advance(1_001);

    expect(await store.tryAcquire("session-1", "owner-b", 1_000)).toBe(true);
  });

  test("renew returns false after ownership is lost", async () => {
    const { store, advance } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);
    advance(1_001);
    await store.tryAcquire("session-1", "owner-b", 1_000);

    expect(await store.renew("session-1", "owner-a", 1_000)).toBe(false);
  });

  test("release clears ownership and terminal rows can never be acquired", async () => {
    const { store } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);
    await store.release("session-1", "owner-a");
    expect(await store.tryAcquire("session-1", "owner-b", 1_000)).toBe(true);

    await store.markTerminal("session-1");
    await store.release("session-1", "owner-b");
    expect(await store.tryAcquire("session-1", "owner-c", 1_000)).toBe(false);
    expect(await store.listDesired()).toEqual([]);
  });
});

describe("Drizzle lease implementation", () => {
  test("tryAcquire is one conditional UPDATE statement", async () => {
    const statements: SQL[] = [];
    const executor: LeaseExecutor = {
      execute: async (statement) => {
        statements.push(statement);
        return { rowCount: 1, rows: [] };
      },
    };
    const store = makeLeaseStore(executor);

    expect(await store.tryAcquire("session-1", "owner-a", 30_000)).toBe(true);
    expect(statements).toHaveLength(1);
    const rendered = new PgDialect().sqlToQuery(statements[0]!).sql;
    expect(rendered).toContain("update \"session_listeners\"");
    expect(rendered).toContain("\"terminal_at\" is null");
    expect(rendered).toContain("\"owner\" is null or \"lease_expires_at\" < now()");
  });
});
