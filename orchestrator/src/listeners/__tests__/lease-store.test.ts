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

describe("LeaseStore dormancy", () => {
  test("a dormant row is not desired and cannot be acquired until a wake", async () => {
    const { store } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);

    expect(await store.markDormant("session-1", "owner-a", 1_000)).toBe(true);
    expect(await store.listDesired()).toEqual([]);
    expect(await store.listDormant()).toEqual(["session-1"]);
    // The stand-down gave the lease up in the same step.
    expect(await store.renew("session-1", "owner-a", 1_000)).toBe(false);
    expect(await store.tryAcquire("session-1", "owner-b", 1_000)).toBe(false);

    await store.wake("session-1");
    expect(await store.listDesired()).toEqual(["session-1"]);
    expect(await store.listDormant()).toEqual([]);
    expect(await store.tryAcquire("session-1", "owner-b", 1_000)).toBe(true);
  });

  test("a stand-down inside the wake grace is refused and the lease is kept", async () => {
    const { store, advance } = contractStore();
    await seed(store);
    await store.wake("session-1");
    await store.tryAcquire("session-1", "owner-a", 10_000);

    expect(await store.markDormant("session-1", "owner-a", 1_000)).toBe(false);
    expect(await store.listDesired()).toEqual(["session-1"]);
    expect(await store.renew("session-1", "owner-a", 10_000)).toBe(true);

    advance(1_001);
    expect(await store.markDormant("session-1", "owner-a", 1_000)).toBe(true);
  });

  test("only the lease owner can stand a row down, and a terminal row never wakes", async () => {
    const { store } = contractStore();
    await seed(store);
    await store.tryAcquire("session-1", "owner-a", 1_000);
    expect(await store.markDormant("session-1", "owner-b", 1_000)).toBe(false);
    expect(await store.listDesired()).toEqual(["session-1"]);

    await store.markTerminal("session-1");
    await store.wake("session-1");
    expect(await store.listDesired()).toEqual([]);
    expect(await store.listDormant()).toEqual([]);
  });
});

describe("Drizzle lease implementation", () => {
  function recording() {
    const statements: SQL[] = [];
    const executor: LeaseExecutor = {
      execute: async (statement) => {
        statements.push(statement);
        return { rowCount: 1, rows: [] };
      },
    };
    const render = (index: number) => new PgDialect().sqlToQuery(statements[index]!).sql;
    return { store: makeLeaseStore(executor), statements, render };
  }

  test("markDormant is one fenced UPDATE that also releases the lease", async () => {
    const { store, statements, render } = recording();
    expect(await store.markDormant("session-1", "owner-a", 120_000)).toBe(true);
    expect(statements).toHaveLength(1);
    const rendered = render(0);
    expect(rendered).toContain("set \"dormant_at\" = now(), \"owner\" = null, \"lease_expires_at\" = null");
    expect(rendered).toContain("\"owner\" = $2");
    expect(rendered).toContain("\"woken_at\" is null");
    expect(rendered).toContain("\"woken_at\" < now() - ($3 * interval '1 millisecond')");
  });

  test("wake clears dormancy, stamps the wake, and skips terminal rows", async () => {
    const { store, render } = recording();
    await store.wake("session-1");
    const rendered = render(0);
    expect(rendered).toContain("set \"dormant_at\" = null, \"woken_at\" = now()");
    expect(rendered).toContain("\"terminal_at\" is null");
  });

  test("desired rows exclude dormant ones; acquisition does too", async () => {
    const { store, render } = recording();
    await store.listDesired();
    expect(render(0)).toContain("\"dormant_at\" is null");
    await store.tryAcquire("session-1", "owner-a", 30_000);
    expect(render(1)).toContain("\"dormant_at\" is null");
    await store.listDormant();
    expect(render(2)).toContain("\"dormant_at\" is not null");
  });

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
