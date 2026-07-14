import { describe, expect, test } from "bun:test";

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

  test("a retrying consumer does not block another consumer's cursor", async () => {
    const cursors = makeInMemoryCursorStore();
    const slow = recordingConsumer({
      name: "slow",
      handle: async () => {
        throw new Error("still broken");
      },
    });
    const fast = recordingConsumer({ name: "fast" });
    let reads = 0;
    const subject = await listener({
      cursorStore: cursors,
      readPage: async (_sessionId, after) => {
        reads++;
        return reads === 1 ? page([event(0n), event(1n)], 1n) : page([], after);
      },
      sleep: never,
      openStream: async () => opened([], { waitAtEnd: true }),
    }, [slow.consumer, fast.consumer], cursors);

    const running = subject.run();
    await waitFor(async () => (await cursors.get("session-1", "fast")) === 1n);
    expect(await cursors.get("session-1", "slow")).toBe(-1n);
    expect(fast.events.map((ev) => ev.idx)).toEqual([0n, 1n]);

    await subject.stop();
    await running;
  });
});
