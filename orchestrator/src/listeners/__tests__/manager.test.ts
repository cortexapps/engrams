import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { LeaseStore } from "../lease-store.ts";
import { ListenerManager, type ListenerHandle } from "../manager.ts";

function fixture(initialDesired: string[] = [], initialDormant: string[] = []) {
  let desired = new Set(initialDesired);
  const dormant = new Set(initialDormant);
  const owners = new Map<string, string>();
  const released: string[] = [];
  const woken: string[] = [];
  const terminal: string[] = [];
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
    markTerminal: async (sessionId) => {
      desired.delete(sessionId);
      dormant.delete(sessionId);
      terminal.push(sessionId);
    },
    listDesired: async () => [...desired],
    ensureRow: async (sessionId) => void desired.add(sessionId),
    markDormant: async (sessionId) => {
      desired.delete(sessionId);
      dormant.add(sessionId);
      return true;
    },
    wake: async (sessionId) => {
      if (dormant.delete(sessionId)) desired.add(sessionId);
      woken.push(sessionId);
    },
    listDormant: async () => [...dormant],
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
    woken,
    terminal,
    setDesired(values: string[]) {
      desired = new Set(values);
    },
  };
}

describe("ListenerManager.reconcileDormantOnce", () => {
  test("wakes a dormant row whose session is no longer parked and retires a gone one", async () => {
    const f = fixture([], ["parked", "resumed", "gone", "flaky"]);
    const probed: string[] = [];
    const manager = new ListenerManager({
      owner: "manager-1",
      ttlMs: 30_000,
      leaseStore: f.leases,
      createListener: () => ({ start: () => new Promise<void>(() => {}), stop: async () => {} }),
      probeStatus: async (sessionId) => {
        probed.push(sessionId);
        if (sessionId === "parked") return "parked";
        if (sessionId === "resumed") return "active";
        if (sessionId === "gone") throw new ConnectError("no such session", Code.NotFound);
        throw new Error("coordinator unreachable");
      },
    });

    await manager.reconcileDormantOnce();

    expect(probed).toEqual(["parked", "resumed", "gone", "flaky"]);
    expect(f.woken).toEqual(["resumed"]);
    expect(f.terminal).toEqual(["gone"]);
    // The resumed session is desired again; the parked and the flaky ones
    // stay dormant until the next reconcile.
    expect(await f.leases.listDesired()).toEqual(["resumed"]);
    expect(await f.leases.listDormant()).toEqual(["parked", "flaky"]);
  });

  test("without a status probe the reconcile is a no-op", async () => {
    const f = fixture([], ["parked"]);
    await f.manager.reconcileDormantOnce();
    expect(f.woken).toEqual([]);
    expect(await f.leases.listDormant()).toEqual(["parked"]);
  });
});

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
