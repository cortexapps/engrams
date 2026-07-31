/**
 * Frame-taxonomy contract test (ADR 0108 B).
 *
 * Enumerates every frame kind the coordinator can emit on the app-gRPC
 * `StreamEvents` RPC and pins how the SessionListener must classify each:
 *
 *  - "lag":       the `lagged` sentinel — the ONE idx-less frame that means
 *                 "the bus dropped events; close the stream and catch up
 *                 from the durable log". Reconnects with no backoff.
 *  - "ephemeral": idx-less content (never persisted, never replayed) — the
 *                 listener skips it and keeps consuming the stream. It must
 *                 NOT reconnect: before ADR 0108 B, treating every idx-less
 *                 frame as lag reconnected ~5/s for every generation.
 *  - "durable":   an idx-carrying log event — advances the reconnect cursor.
 *
 * The durable list mirrors `SESSION_EVENT_KINDS` in web/src/sse.ts (the
 * browser's explicit subscription list); the mirror is verified against
 * that file below. A NEW kind on the wire must be added to WIRE_FRAME_KINDS
 * with an explicit classification — the mirror check and the exhaustiveness
 * checks fail until it is.
 */

import { describe, expect, test } from "bun:test";

import type {
  BoundedRead,
  CuratedEvent,
  WireEvent,
} from "../../control-plane/session-events.ts";
import type { SessionConsumer } from "../consumer.ts";
import { makeInMemoryCursorStore } from "../cursor-store.ts";
import { makeInMemoryLeaseStore } from "../lease-store.ts";
import {
  SessionListener,
  type OpenedSessionStream,
  type SessionListenerDeps,
} from "../session-listener.ts";

type FrameClass = "durable" | "ephemeral" | "lag";

/** THE taxonomy: every frame kind the coordinator emits on StreamEvents,
 * each with its classification decision. Additions without a decision fail
 * the tests below. */
const WIRE_FRAME_KINDS: Readonly<Record<string, FrameClass>> = {
  // Durable log events — mirror of web/src/sse.ts SESSION_EVENT_KINDS.
  status_changed: "durable",
  exec_started: "durable",
  exec_completed: "durable",
  stdout: "durable",
  stderr: "durable",
  snapshot_taken: "durable",
  evicted: "durable",
  resumed: "durable",
  resume_started: "durable",
  run_started: "durable",
  agent_message: "durable",
  tool_call_started: "durable",
  tool_call_completed: "durable",
  browser_activity: "durable",
  tool_call_requested: "durable",
  tool_result_submitted: "durable",
  run_completed: "durable",
  run_interrupted: "durable",
  harness_idle: "durable",
  prompt_queued: "durable",
  prompt_edited: "durable",
  prompt_dequeued: "durable",
  prompt_steered: "durable",
  integration_asset: "durable",
  file_shared: "durable",
  recovered_from_checkpoint: "durable",
  durability_rollback: "durable",
  user_question: "durable",
  question_answered: "durable",
  file_changed: "durable",
  // Ephemeral: Phase 1c token chunks (ADR 0052) — idx-less, never
  // persisted. Suppressed server-side when durableOnly is set, but an old
  // coordinator under deploy skew still sends them.
  agent_message_chunk: "ephemeral",
  // The broadcast-lag sentinel (api/events.rs merged_to_parts).
  lagged: "lag",
};

const KINDS_OF = (cls: FrameClass): string[] =>
  Object.entries(WIRE_FRAME_KINDS)
    .filter(([, c]) => c === cls)
    .map(([kind]) => kind);

// ---------------------------------------------------------------------------
// Minimal listener harness (same shape as session-listener.test.ts).
// ---------------------------------------------------------------------------

const never = () => new Promise<void>(() => {});

function page(events: CuratedEvent[], nextAfter: bigint): BoundedRead {
  return { events, nextAfter };
}

function opened(
  frames: WireEvent[],
  options: { throwAfter?: number } = {},
): OpenedSessionStream {
  let closed = false;
  return {
    events: {
      async *[Symbol.asyncIterator]() {
        for (let index = 0; index < frames.length; index++) {
          if (closed) return;
          yield frames[index]!;
          if (options.throwAfter === index + 1) throw new Error("stream dropped");
        }
      },
    },
    close() {
      closed = true;
    },
  };
}

interface RunOutcome {
  since: bigint[];
  opens: number;
  backoffs: number[];
  handled: CuratedEvent[];
  terminals: string[];
}

/** Run one listener against scripted stream opens until it finishes.
 * `streams` receives the reconnect cursor and the 1-based open count. */
async function runListener(
  streams: (since: bigint, open: number) => OpenedSessionStream,
  overrides: Partial<SessionListenerDeps> = {},
): Promise<RunOutcome> {
  const outcome: RunOutcome = {
    since: [],
    opens: 0,
    backoffs: [],
    handled: [],
    terminals: [],
  };
  const consumer: SessionConsumer = {
    name: "taxonomy",
    interestedIn: () => true,
    appliesTo: async () => true,
    handle: async (ev) => void outcome.handled.push(ev),
    onTerminal: async (terminal) => void outcome.terminals.push(terminal),
  };
  const leaseStore = makeInMemoryLeaseStore();
  await leaseStore.ensureRow("session-1");
  expect(await leaseStore.tryAcquire("session-1", "owner-1", 30_000)).toBe(true);
  const rawSleep = async (ms: number): Promise<void> => {
    // Heartbeat (ttl/3) and probe timers park; reconnect backoffs are
    // recorded and resolve at once so a retry proceeds.
    if (ms >= 10_000) return never();
    outcome.backoffs.push(ms);
  };
  const sleep = (ms: number, signal?: AbortSignal): Promise<void> => {
    if (!signal) return rawSleep(ms);
    if (signal.aborted) return Promise.resolve();
    return new Promise<void>((resolve) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        signal.removeEventListener("abort", done);
        resolve();
      };
      void rawSleep(ms).then(done);
      signal.addEventListener("abort", done, { once: true });
    });
  };
  const listener = new SessionListener({
    sessionId: "session-1",
    owner: "owner-1",
    ttlMs: 30_000,
    leaseStore,
    cursorStore: makeInMemoryCursorStore(),
    consumers: [consumer],
    readPage: async (_sessionId, after) => page([], after),
    openStream: async (_sessionId, since) => {
      outcome.since.push(since);
      outcome.opens++;
      return streams(since, outcome.opens);
    },
    ...overrides,
    sleep,
  });
  await listener.run();
  return outcome;
}

const terminalFrame = (idx: bigint): WireEvent => ({
  idx,
  kind: "status_changed",
  payloadJson: JSON.stringify({ to: "completed" }),
});

// ---------------------------------------------------------------------------
// The contract.
// ---------------------------------------------------------------------------

describe("StreamEvents frame taxonomy (ADR 0108 B)", () => {
  test("the durable list mirrors web/src/sse.ts SESSION_EVENT_KINDS", async () => {
    // web/src/sse.ts is the browser's explicit subscription list — the
    // co-maintained enumeration of durable kinds. Read it from source so a
    // kind added there without a classification decision here fails loudly.
    const ssePath = `${import.meta.dir}/../../../../web/src/sse.ts`;
    const source = await Bun.file(ssePath).text();
    const arrayMatch = source.match(
      /SESSION_EVENT_KINDS[^=]*=\s*\[([\s\S]*?)\];/,
    );
    if (!arrayMatch) {
      throw new Error(
        `could not find SESSION_EVENT_KINDS in ${ssePath} — if it moved, update this contract test`,
      );
    }
    const webKinds = [...arrayMatch[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
    expect(webKinds.length).toBeGreaterThan(0);

    const durable = new Set(KINDS_OF("durable"));
    for (const kind of webKinds) {
      if (!WIRE_FRAME_KINDS[kind]) {
        throw new Error(
          `frame kind "${kind}" (web/src/sse.ts) has no classification decision — ` +
            `add it to WIRE_FRAME_KINDS as durable/ephemeral/lag (ADR 0108 B)`,
        );
      }
    }
    for (const kind of durable) {
      if (!webKinds.includes(kind)) {
        throw new Error(
          `durable frame kind "${kind}" is not in web/src/sse.ts SESSION_EVENT_KINDS — ` +
            `the mirror drifted; reconcile the two lists`,
        );
      }
    }
  });

  test("the taxonomy is exhaustive and disjoint", () => {
    // "lagged" must be exactly the listener's lag key, and idx-less
    // non-lagged kinds must exist (or the skip path is untested).
    expect(KINDS_OF("lag")).toEqual(["lagged"]);
    expect(KINDS_OF("ephemeral")).toEqual(["agent_message_chunk"]);
    for (const [kind, cls] of Object.entries(WIRE_FRAME_KINDS)) {
      if (cls !== "durable" && cls !== "ephemeral" && cls !== "lag") {
        throw new Error(
          `frame kind "${kind}" has invalid classification "${String(cls)}"`,
        );
      }
    }
  });

  test("every durable kind advances the reconnect cursor", async () => {
    for (const kind of KINDS_OF("durable")) {
      // Stream 1 delivers the kind at idx 5 then drops; the reconnect must
      // resume from 5 — proof the frame's idx advanced the cursor. A
      // non-terminal payload keeps status_changed in-loop. Keyed on the
      // open count so a cursor bug shows as a wrong `since`, not a hang.
      const outcome = await runListener((_since, open) =>
        open === 1
          ? opened([{ idx: 5n, kind, payloadJson: "{}" }], { throwAfter: 1 })
          : opened([terminalFrame(6n)]),
      );
      if (outcome.since.length !== 2 || outcome.since[1] !== 5n) {
        throw new Error(
          `durable frame kind "${kind}" did not advance the cursor: ` +
            `reconnect since=${String(outcome.since)} (expected [-1, 5])`,
        );
      }
    }
  });

  test("every ephemeral kind is skipped in place — no close, no reconnect", async () => {
    for (const kind of KINDS_OF("ephemeral")) {
      // The idx-less frame arrives mid-stream; the listener must keep
      // consuming the SAME stream and reach the terminal frame behind it.
      // A misclassification (the pre-ADR-0108 lag treatment) reconnects;
      // the second open terminates the run so the error is a clear
      // assertion, not a test timeout.
      const outcome = await runListener((_since, open) =>
        open === 1
          ? opened([{ kind, payloadJson: "{}" }, terminalFrame(0n)])
          : opened([terminalFrame(0n)]),
      );
      if (outcome.opens !== 1) {
        throw new Error(
          `ephemeral frame kind "${kind}" caused a reconnect (${outcome.opens} opens) — ` +
            `idx-less non-lagged frames must be skipped (ADR 0108 B)`,
        );
      }
      expect(outcome.terminals).toEqual(["completed"]);
      expect(outcome.backoffs).toEqual([]);
    }
  });

  test("the lagged sentinel takes the lag path: immediate reconnect, no backoff", async () => {
    for (const kind of KINDS_OF("lag")) {
      // The cursor does not advance across a lag, so the script keys on
      // the open count, not on `since`.
      const outcome = await runListener((_since, open) =>
        open === 1
          ? opened([{ kind, payloadJson: JSON.stringify({ missed: 3 }) }])
          : opened([terminalFrame(0n)]),
      );
      if (outcome.opens !== 2) {
        throw new Error(
          `lag frame kind "${kind}" did not reconnect (${outcome.opens} opens)`,
        );
      }
      // Lag is flow control, not a failure: no backoff sleep on the way back.
      expect(outcome.backoffs).toEqual([]);
      expect(outcome.terminals).toEqual(["completed"]);
    }
  });
});
