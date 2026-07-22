import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type {
  BoundedRead,
  CuratedEvent,
  WireEvent,
} from "../../control-plane/session-events.ts";
import type { SessionConsumer } from "../consumer.ts";
import { makeInMemoryCursorStore, type CursorStore } from "../cursor-store.ts";
import { makeInMemoryLeaseStore, type LeaseStore } from "../lease-store.ts";
import {
  SessionListener,
  type OpenedSessionStream,
  type SessionListenerDeps,
} from "../session-listener.ts";

const never = () => new Promise<void>(() => {});

function event(idx: bigint, kind = "run_started"): CuratedEvent {
  return { idx, kind, payloadJson: "{}" };
}

function terminal(idx: bigint, outcome = "completed"): WireEvent {
  const to = outcome === "completed" ? "completed" : outcome === "failed" ? "failed" : "dead";
  return { idx, kind: "status_changed", payloadJson: JSON.stringify({ to }) };
}

function page(
  events: CuratedEvent[],
  nextAfter: bigint,
  outcome?: "completed" | "failed" | "neutral",
): BoundedRead {
  return {
    events,
    nextAfter,
    ...(outcome ? { terminal: { outcome } } : {}),
  };
}

function opened(
  frames: WireEvent[],
  options: { throwAfter?: number; onClose?: () => void; waitAtEnd?: boolean } = {},
): OpenedSessionStream {
  let closed = false;
  let wakeClosed: (() => void) | undefined;
  const closedSignal = new Promise<void>((resolve) => {
    wakeClosed = resolve;
  });
  return {
    events: {
      async *[Symbol.asyncIterator]() {
        for (let index = 0; index < frames.length; index++) {
          if (closed) return;
          yield frames[index]!;
          if (options.throwAfter === index + 1) throw new Error("stream dropped");
        }
        if (options.waitAtEnd) await closedSignal;
      },
    },
    close() {
      if (closed) return;
      closed = true;
      wakeClosed?.();
      options.onClose?.();
    },
  };
}

function recordingConsumer(options: {
  name?: string;
  interestedIn?: (kind: string) => boolean;
  handle?: (ev: CuratedEvent) => Promise<void>;
} = {}) {
  const events: CuratedEvent[] = [];
  const terminals: string[] = [];
  const consumer: SessionConsumer = {
    name: options.name ?? "consumer",
    interestedIn: options.interestedIn ?? (() => true),
    appliesTo: async () => true,
    handle: async (ev) => {
      events.push(ev);
      await options.handle?.(ev);
    },
    onTerminal: async (outcome) => void terminals.push(outcome),
  };
  return { consumer, events, terminals };
}

async function acquiredLease(): Promise<LeaseStore> {
  const store = makeInMemoryLeaseStore();
  await store.ensureRow("session-1");
  expect(await store.tryAcquire("session-1", "owner-1", 30_000)).toBe(true);
  return store;
}

async function listener(
  overrides: Partial<SessionListenerDeps>,
  consumers: SessionConsumer[],
  cursorStore: CursorStore = makeInMemoryCursorStore(),
): Promise<SessionListener> {
  return new SessionListener({
    sessionId: "session-1",
    owner: "owner-1",
    ttlMs: 30_000,
    leaseStore: await acquiredLease(),
    cursorStore,
    consumers,
    readPage: async (_sessionId, after) => page([], after),
    openStream: async () => opened([], { waitAtEnd: true }),
    sleep: never,
    ...overrides,
  });
}

async function waitFor(check: () => Promise<boolean> | boolean): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await check()) return;
    await Bun.sleep(1);
  }
  throw new Error("condition was not reached");
}

describe("SessionListener", () => {
  test("cold catch-up delivers all logged events in order and advances the cursor", async () => {
    const rec = recordingConsumer();
    let reads = 0;
    const cursors = makeInMemoryCursorStore();
    const subject = await listener({
      cursorStore: cursors,
      readPage: async () => {
        reads++;
        return page([event(0n), event(1n), event(2n)], 3n, "completed");
      },
    }, [rec.consumer], cursors);

    await subject.run();

    expect(reads).toBe(1);
    expect(rec.events.map((ev) => ev.idx)).toEqual([0n, 1n, 2n]);
    expect(await cursors.get("session-1", "consumer")).toBe(2n);
  });

  test("a failing handler leaves the cursor unmoved until the same event succeeds", async () => {
    let attempts = 0;
    const observedCursors: bigint[] = [];
    const cursors = makeInMemoryCursorStore();
    const rec = recordingConsumer({
      handle: async () => {
        observedCursors.push(await cursors.get("session-1", "consumer"));
        attempts++;
        if (attempts === 1) throw new Error("transient");
      },
    });
    const subject = await listener({
      cursorStore: cursors,
      readPage: async () => page([event(4n)], 5n, "completed"),
      sleep: async (ms) => {
        if (ms >= 10_000) await never();
      },
    }, [rec.consumer], cursors);

    await subject.run();

    expect(rec.events.map((ev) => ev.idx)).toEqual([4n, 4n]);
    expect(observedCursors).toEqual([-1n, -1n]);
    expect(await cursors.get("session-1", "consumer")).toBe(4n);
  });

  test("stream frames at or behind the consumer cursor are not redelivered", async () => {
    const rec = recordingConsumer();
    const cursors = makeInMemoryCursorStore();
    await cursors.set("session-1", "consumer", 5n);
    const subject = await listener({
      cursorStore: cursors,
      openStream: async () => opened([
        event(4n),
        event(5n),
        event(6n),
        terminal(7n),
      ]),
    }, [rec.consumer], cursors);

    await subject.run();

    expect(rec.events.map((ev) => ev.idx)).toEqual([6n]);
    expect(await cursors.get("session-1", "consumer")).toBe(6n);
  });

  test("a filtered curated event advances the consumer cursor without handling", async () => {
    const rec = recordingConsumer({ interestedIn: () => false });
    const cursors = makeInMemoryCursorStore();
    const subject = await listener({
      cursorStore: cursors,
      readPage: async () => page([event(0n)], 1n, "completed"),
    }, [rec.consumer], cursors);

    await subject.run();

    expect(rec.events).toEqual([]);
    expect(await cursors.get("session-1", "consumer")).toBe(0n);
  });

  test("non-curated stream frames advance only the reconnect cursor", async () => {
    const rec = recordingConsumer();
    const since: bigint[] = [];
    let opens = 0;
    const subject = await listener({
      openStream: async (_sessionId, cursor) => {
        since.push(cursor);
        opens++;
        return opens === 1
          ? opened([{ idx: 5n, kind: "stdout", payloadJson: "{}" }], {
              throwAfter: 1,
            })
          : opened([terminal(6n)]);
      },
      sleep: async (ms) => {
        if (ms >= 10_000) await never();
      },
    }, [rec.consumer]);

    await subject.run();

    expect(since).toEqual([-1n, 5n]);
    expect(rec.events).toEqual([]);
  });

  test("a stream error reconnects from the durable cursor without a gap or duplicate", async () => {
    const rec = recordingConsumer();
    const since: bigint[] = [];
    let opens = 0;
    const subject = await listener({
      openStream: async (_sessionId, cursor) => {
        since.push(cursor);
        opens++;
        return opens === 1
          ? opened([event(0n)], { throwAfter: 1 })
          : opened([event(1n), terminal(2n)]);
      },
      sleep: async (ms) => {
        if (ms >= 10_000) await never();
      },
    }, [rec.consumer]);

    await subject.run();

    expect(since).toEqual([-1n, 0n]);
    expect(rec.events.map((ev) => ev.idx)).toEqual([0n, 1n]);
  });

  test("a catch-up read that never settles hits the rpc deadline, retries, and recovers", async () => {
    const rec = recordingConsumer();
    let reads = 0;
    let deadlineFires = 0;
    const subject = await listener({
      rpcDeadlineMs: 55_555,
      readPage: async () => {
        reads++;
        if (reads === 1) return new Promise<never>(() => {}); // orphaned call (issue #704)
        return page([event(4n)], 5n, "completed");
      },
      sleep: async (ms) => {
        if (ms === 55_555) {
          if (deadlineFires++ === 0) return; // first deadline fires; later ones park
          await never();
        } else if (ms < 10_000) {
          return; // reconnect backoff is instant
        } else {
          await never();
        }
      },
    }, [rec.consumer]);

    await subject.run();

    expect(reads).toBe(2);
    expect(deadlineFires).toBeGreaterThanOrEqual(1);
    expect(rec.events.map((ev) => ev.idx)).toEqual([4n]);
    expect(rec.terminals).toEqual(["completed"]);
  });

  test("a wedged listener stops renewing so the lease can pass to a replacement", async () => {
    const base = await acquiredLease();
    let renews = 0;
    const leases: LeaseStore = {
      ...base,
      renew: async (sessionId, owner, ttlMs) => {
        renews++;
        return base.renew(sessionId, owner, ttlMs);
      },
    };
    let clock = 0;
    const subject = await listener({
      leaseStore: leases,
      staleGraceMs: 15_000,
      now: () => clock,
      // The catch-up read parks forever and even the rpc deadline never fires
      // (a hypothetical wedge beyond layer-one protection): only the heartbeat
      // staleness check can save this session.
      readPage: () => new Promise<never>(() => {}),
      sleep: async (ms) => {
        if (ms === 10_000) {
          clock += ms; // ttl/3 heartbeat cadence drives the virtual clock
          return;
        }
        await never();
      },
    }, [recordingConsumer().consumer]);

    await subject.run(); // resolves at all only because the lease is abandoned

    expect(renews).toBe(1); // tick 1 renews (10s stale); tick 2 abandons (20s > 15s)
    expect(await base.tryAcquire("session-1", "owner-2", 30_000)).toBe(true);
  });

  test("a silent stream that fell behind the durable log reconnects after two probe sightings", async () => {
    const rec = recordingConsumer();
    let reads = 0;
    let opens = 0;
    let closed = 0;
    let probeSleeps = 0;
    const subject = await listener({
      probeIntervalMs: 77_777,
      readPage: async (_sessionId, after) => {
        reads++;
        if (reads === 1) return page([], after); // initial catch-up: empty log
        if (reads <= 3) return page([], 5n); // probes: the log is ahead, the stream silent
        return page([event(5n)], 6n, "completed"); // post-reconnect catch-up delivers
      },
      openStream: async () => {
        opens++;
        return opened([], { waitAtEnd: true, onClose: () => closed++ });
      },
      sleep: async (ms) => {
        if (ms === 77_777) {
          if (probeSleeps++ < 2) return; // two probe intervals elapse, then quiet
          await never();
        } else if (ms < 10_000) {
          return; // reconnect backoff is instant
        } else {
          await never();
        }
      },
    }, [rec.consumer]);

    await subject.run();

    expect(opens).toBe(1); // the stalled stream is closed; catch-up reaches terminal first
    expect(closed).toBe(1);
    expect(reads).toBe(4);
    expect(rec.events.map((ev) => ev.idx)).toEqual([5n]);
    expect(rec.terminals).toEqual(["completed"]);
  });

  test("a genuinely idle session probes the log without reconnecting", async () => {
    const rec = recordingConsumer();
    let reads = 0;
    let opens = 0;
    let probeSleeps = 0;
    const subject = await listener({
      probeIntervalMs: 77_777,
      readPage: async (_sessionId, after) => {
        reads++;
        return page([], after); // the log never moves: genuine idleness
      },
      openStream: async () => {
        opens++;
        return opened([], { waitAtEnd: true });
      },
      sleep: async (ms) => {
        if (ms === 77_777) {
          if (probeSleeps++ < 2) return;
          await never();
        } else {
          await never();
        }
      },
    }, [rec.consumer]);

    const running = subject.run();
    await waitFor(() => reads === 3); // catch-up + two idle probes
    await subject.stop();
    await running;

    expect(opens).toBe(1);
    expect(rec.events).toEqual([]);
    expect(rec.terminals).toEqual([]);
  });

  test("an actively delivering stream re-arms its probe timer per frame and never probes the log", async () => {
    const rec = recordingConsumer();
    let reads = 0; // readPage calls: catch-up only — a probe read would add more
    let probeArmings = 0; // sleep(probeIntervalMs) calls: one re-arm per frame
    const subject = await listener({
      probeIntervalMs: 77_777,
      readPage: async (_sessionId, after) => {
        reads++;
        return page([], after); // catch-up empty; must never be consulted again
      },
      openStream: async () =>
        opened([event(0n), event(1n), event(2n), terminal(3n)]),
      sleep: async (ms) => {
        if (ms === 77_777) probeArmings++;
        // The probe timer never fires: every delivered frame re-arms it, so a
        // delivering stream proves liveness via frames and issues no probe read.
        await never();
      },
    }, [rec.consumer]);

    await subject.run();

    expect(rec.events.map((ev) => ev.idx)).toEqual([0n, 1n, 2n]);
    expect(rec.terminals).toEqual(["completed"]);
    expect(reads).toBe(1); // just the initial catch-up — no probe reads
    expect(probeArmings).toBe(4); // re-armed once per delivered frame, not armed once
  });

  test("an idx-less lag frame closes the stream and catches up without loss", async () => {
    const rec = recordingConsumer();
    const after: bigint[] = [];
    let reads = 0;
    let closed = 0;
    const subject = await listener({
      readPage: async (_sessionId, cursor) => {
        after.push(cursor);
        reads++;
        if (reads === 1) return page([], cursor);
        return page([event(1n)], 2n, "completed");
      },
      openStream: async () => opened([
        event(0n),
        { kind: "lagged", payloadJson: "{}" },
      ], { onClose: () => closed++ }),
    }, [rec.consumer]);

    await subject.run();

    expect(closed).toBe(1);
    expect(after).toEqual([-1n, 0n]);
    expect(rec.events.map((ev) => ev.idx)).toEqual([0n, 1n]);
  });

  test("terminal drains queues, calls onTerminal, marks terminal, releases, and exits", async () => {
    const rec = recordingConsumer();
    const base = await acquiredLease();
    const actions: string[] = [];
    const leases: LeaseStore = {
      ...base,
      markTerminal: async (sessionId) => {
        actions.push(`terminal:${sessionId}`);
        await base.markTerminal(sessionId);
      },
      release: async (sessionId, owner) => {
        actions.push(`release:${sessionId}:${owner}`);
        await base.release(sessionId, owner);
      },
    };
    const subject = await listener({
      leaseStore: leases,
      readPage: async () => page([event(0n)], 1n, "failed"),
    }, [rec.consumer]);

    await subject.run();

    expect(rec.terminals).toEqual(["failed"]);
    expect(actions).toEqual(["terminal:session-1", "release:session-1:owner-1"]);
    expect(await base.tryAcquire("session-1", "owner-2", 30_000)).toBe(false);
  });

  test("a session already in a terminal status finishes with the mapped outcome without reading the log", async () => {
    const rec = recordingConsumer();
    const base = await acquiredLease();
    let reads = 0;
    const subject = await listener({
      leaseStore: base,
      fetchStatus: async () => "failed",
      readPage: async () => {
        reads++;
        return page([], -1n);
      },
    }, [rec.consumer]);

    await subject.run();

    expect(reads).toBe(0);
    expect(rec.terminals).toEqual(["failed"]);
    expect(await base.listDesired()).toEqual([]);
  });

  test("a live status probe proceeds to normal delivery", async () => {
    const rec = recordingConsumer();
    const cursors = makeInMemoryCursorStore();
    const subject = await listener({
      fetchStatus: async () => "active",
      readPage: async (_sessionId, after) =>
        after < 1n ? page([event(0n), event(1n)], 1n, "completed") : page([], after),
    }, [rec.consumer], cursors);

    await subject.run();

    expect(rec.events.map((ev) => ev.idx)).toEqual([0n, 1n]);
    expect(rec.terminals).toEqual(["completed"]);
  });

  test("a session unknown to the coordinator marks terminal and exits instead of retrying", async () => {
    const rec = recordingConsumer();
    const base = await acquiredLease();
    const actions: string[] = [];
    const leases: LeaseStore = {
      ...base,
      markTerminal: async (sessionId) => {
        actions.push(`terminal:${sessionId}`);
        await base.markTerminal(sessionId);
      },
      release: async (sessionId, owner) => {
        actions.push(`release:${sessionId}:${owner}`);
        await base.release(sessionId, owner);
      },
    };
    let reads = 0;
    const subject = await listener({
      leaseStore: leases,
      sleep: async () => {},
      readPage: async () => {
        reads++;
        throw new ConnectError("metadata row not found", Code.NotFound);
      },
    }, [rec.consumer]);

    await subject.run();

    expect(reads).toBe(1);
    expect(actions).toEqual(["terminal:session-1", "release:session-1:owner-1"]);
    // The outcome is unknown — no fabricated terminal reaches consumers.
    expect(rec.terminals).toEqual([]);
    expect(await base.listDesired()).toEqual([]);
  });

  test("a failed lease renewal stops before delivering further events", async () => {
    const rec = recordingConsumer();
    const base = await acquiredLease();
    let renewals = 0;
    let allowRead: (() => void) | undefined;
    const readGate = new Promise<void>((resolve) => {
      allowRead = resolve;
    });
    const leases: LeaseStore = {
      ...base,
      renew: async () => {
        renewals++;
        allowRead?.();
        return false;
      },
    };
    const subject = await listener({
      leaseStore: leases,
      sleep: async () => {},
      readPage: async () => {
        await readGate;
        return page([event(0n)], 1n);
      },
    }, [rec.consumer]);

    await subject.run();

    expect(renewals).toBe(1);
    expect(rec.events).toEqual([]);
  });

  test("a saturated retrying consumer does not block another consumer's cursor", async () => {
    const cursors = makeInMemoryCursorStore();
    const slow = recordingConsumer({
      name: "slow",
      handle: async () => {
        throw new Error("still broken");
      },
    });
    const fast = recordingConsumer({ name: "fast" });
    const subject = await listener({
      cursorStore: cursors,
      readPage: async (_sessionId, after) => {
        const events = [event(0n), event(1n), event(2n), event(3n)]
          .filter((candidate) => candidate.idx > after);
        return page(events, events.length > 0 ? 3n : after);
      },
      queueCapacity: 1,
      sleep: never,
      openStream: async () => opened([], { waitAtEnd: true }),
    }, [slow.consumer, fast.consumer], cursors);

    const running = subject.run();
    await waitFor(async () => (await cursors.get("session-1", "fast")) === 3n);
    expect(await cursors.get("session-1", "slow")).toBe(-1n);
    expect(fast.events.map((ev) => ev.idx)).toEqual([0n, 1n, 2n, 3n]);

    await subject.stop();
    await running;
  });
});
