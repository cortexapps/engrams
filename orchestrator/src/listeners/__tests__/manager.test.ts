import { describe, expect, test } from "bun:test";

import type { LeaseStore } from "../lease-store.ts";
import { ListenerManager, type ListenerHandle } from "../manager.ts";

function fixture(initialDesired: string[] = []) {
  let desired = new Set(initialDesired);
  const owners = new Map<string, string>();
  const released: string[] = [];
  const leases: LeaseStore = {
    tryAcquire: async (sessionId, owner) => {
      if (owners.has(sessionId)) return false;
      owners.set(sessionId, owner);
      return true;
    },
    renew: async (sessionId, owner) => owners.get(sessionId) === owner,
    release: async (sessionId, owner) => {
      if (owners.get(sessionId) === owner) owners.delete(sessionId);
      released.push(sessionId);
    },
    markTerminal: async (sessionId) => void desired.delete(sessionId),
    listDesired: async () => [...desired],
    ensureRow: async (sessionId) => void desired.add(sessionId),
  };
  const started: string[] = [];
  const stopped: string[] = [];
  const create = (sessionId: string): ListenerHandle => ({
    start() {
      started.push(sessionId);
      return neverEnding;
    },
    async stop() {
      stopped.push(sessionId);
    },
  });
  const neverEnding = new Promise<void>(() => {});
  const manager = new ListenerManager({
    owner: "manager-1",
    ttlMs: 30_000,
    leaseStore: leases,
    createListener: create,
  });
  return {
    manager,
    leases,
    started,
    stopped,
    released,
    setDesired(values: string[]) {
      desired = new Set(values);
    },
  };
}

describe("ListenerManager.scanOnce", () => {
  test("desired {A,B} with B already running starts only A", async () => {
    const f = fixture(["B"]);
    await f.manager.scanOnce();
    f.started.length = 0;
    f.setDesired(["A", "B"]);

    await f.manager.scanOnce();

    expect(f.started).toEqual(["A"]);
  });

  test("a running session removed from desired is stopped and released", async () => {
    const f = fixture(["A"]);
    await f.manager.scanOnce();
    f.setDesired([]);

    await f.manager.scanOnce();

    expect(f.stopped).toEqual(["A"]);
    expect(f.released).toContain("A");
  });

  test("double scan creates one listener", async () => {
    const f = fixture(["A"]);

    await f.manager.scanOnce();
    await f.manager.scanOnce();

    expect(f.started).toEqual(["A"]);
  });

  test("one session failing to start does not prevent another", async () => {
    const f = fixture(["A", "B"]);
    const manager = new ListenerManager({
      owner: "manager-1",
      ttlMs: 30_000,
      leaseStore: f.leases,
      createListener(sessionId) {
        if (sessionId === "A") throw new Error("broken session");
        return {
          start() {
            f.started.push(sessionId);
            return new Promise<void>(() => {});
          },
          async stop() {},
        };
      },
    });

    await manager.scanOnce();

    expect(f.started).toEqual(["B"]);
    expect(f.released).toContain("A");
  });
});
