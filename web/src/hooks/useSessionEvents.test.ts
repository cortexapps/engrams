// Phase 1c (ADR 0052) merge-safety tests for the live-streaming overlay, plus
// the windowed open sequence (lib/sessionWindow.ts).
//
// The invariant under test: ephemeral token chunks accumulate into a SEPARATE
// overlay (`streamingText`) and the durable event log ALWAYS wins — a chunk
// can never corrupt or double the persisted transcript, even under a crash,
// a late/out-of-order delta, or a pod cycle mid-message.
//
// The window is READ before the stream opens, so these also pin that seam: the
// unary pages fill `events`, and the SSE subscribe carries `since = <highest
// idx held>` so the live tail continues without a gap.

import { renderHook, act, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";
import type { SseHandlers } from "../sse";
import type { SessionEvent } from "../lib/types";

const AT = "2026-06-02T12:00:00.000Z";

// Capture the handlers `useSessionEvents` registers, so the test can drive
// onEvent / onDelta directly, plus the unary pages it reads. `vi.hoisted` makes
// the holder available inside the hoisted `vi.mock` factories.
const h = vi.hoisted(() => ({
  handlers: null as SseHandlers | null,
  since: -1,
  /** The fixture log every page is served from. */
  log: [] as { idx: bigint; kind: string; payloadJson: string }[],
  requests: [] as { afterIdx?: bigint; beforeIdx?: bigint; limit: bigint; kinds: string[] }[],
}));

vi.mock("../sse", () => ({
  subscribeSession: (_sessionId: string, handlers: SseHandlers, since: number) => {
    h.handlers = handlers;
    h.since = since;
    return () => {
      h.handlers = null;
    };
  },
}));

vi.mock("@connectrpc/connect-query", () => ({ useTransport: () => ({}) }));

// A stand-in coordinator over `h.log`, with the page semantics the real
// ListSessionEvents has: forward from `after_idx`, backward from `before_idx`
// (oldest-first), a kind filter, and `next_after_idx` as the forward cursor.
vi.mock("@connectrpc/connect", () => ({
  createClient: () => ({
    listSessionEvents: (req: {
      afterIdx?: bigint;
      beforeIdx?: bigint;
      limit: bigint;
      kinds: string[];
    }) => {
      h.requests.push(req);
      let rows = h.log;
      if (req.kinds.length > 0) rows = rows.filter((r) => req.kinds.includes(r.kind));
      if (req.afterIdx !== undefined) rows = rows.filter((r) => r.idx > req.afterIdx!);
      if (req.beforeIdx !== undefined) rows = rows.filter((r) => r.idx < req.beforeIdx!);
      const limit = Number(req.limit);
      const page = req.beforeIdx !== undefined ? rows.slice(-limit) : rows.slice(0, limit);
      const last = page[page.length - 1];
      return Promise.resolve({
        events: page,
        nextAfterIdx: last ? last.idx : (req.afterIdx ?? -1n),
      });
    },
  }),
}));

import { useSessionEvents } from "./useSessionEvents";

const ev = (idx: number, event: SessionEvent) => ({ idx, event });
const runStarted = (): SessionEvent => ({
  type: "run_started",
  run_id: "r1",
  prompt_summary: null,
  at: AT,
});
const asstMsg = (text: string, message_id = "a1"): SessionEvent => ({
  type: "agent_message",
  run_id: "r1",
  message_id,
  role: "assistant",
  text,
  at: AT,
});

/** One durable row as the wire carries it. */
function row(idx: number, kind: string, payload: Record<string, unknown> = {}) {
  return { idx: BigInt(idx), kind, payloadJson: JSON.stringify({ at: AT, ...payload }) };
}

/** A fixture session of `runs` turns, each with `tools` tool-call pairs — the
 *  kinds the window defers. */
function fixtureLog(runs: number, tools: number) {
  const log: ReturnType<typeof row>[] = [];
  for (let r = 0; r < runs; r++) {
    log.push(row(log.length, "run_started", { run_id: `r${r}`, prompt_summary: `p${r}` }));
    log.push(
      row(log.length, "agent_message", {
        run_id: `r${r}`,
        message_id: `m${r}`,
        role: "assistant",
        text: `answer ${r}`,
      }),
    );
    for (let i = 0; i < tools; i++) {
      log.push(
        row(log.length, "tool_call_started", {
          run_id: `r${r}`,
          tool_call_id: `t${r}-${i}`,
          tool_name: "Read",
          args_summary: null,
        }),
      );
      log.push(
        row(log.length, "tool_call_completed", {
          run_id: `r${r}`,
          tool_call_id: `t${r}-${i}`,
          tool_name: "Read",
          ok: true,
          duration_ms: 1,
          result_summary: "ok",
        }),
      );
    }
    log.push(row(log.length, "run_completed", { run_id: `r${r}`, ok: true }));
  }
  return log;
}

/** Mount the hook and wait for the open sequence to reach the subscribe. */
async function mounted() {
  const hook = renderHook(() => useSessionEvents("s1"));
  await waitFor(() => expect(h.handlers).not.toBeNull());
  return hook;
}

beforeEach(() => {
  h.handlers = null;
  h.since = -1;
  h.log = [];
  h.requests = [];
});

describe("useSessionEvents — Phase 1c streaming overlay merge-safety", () => {
  test("chunks accumulate into streamingText and stay OUT of the durable events", async () => {
    const { result } = await mounted();
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "hel" }));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "lo" }));
    expect(result.current.streamingText).toBe("hello");
    // chunks are ephemeral — never appended to the durable log
    expect(result.current.events).toHaveLength(1);
  });

  test("the durable agent_message supersedes the overlay and drops a late straggler", async () => {
    const { result } = await mounted();
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "hello" }));
    expect(result.current.streamingText).toBe("hello");
    // terminal durable message lands → overlay for a1 is pruned
    act(() => h.handlers!.onEvent(ev(1, asstMsg("hello"))));
    expect(result.current.streamingText).toBe("");
    expect(result.current.events).toHaveLength(2);
    // a late, out-of-order chunk for the finalized message is ignored
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "X" }));
    expect(result.current.streamingText).toBe("");
  });

  test("a run terminal clears a still-open overlay (crash mid-stream)", async () => {
    const { result } = await mounted();
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "partial…" }));
    expect(result.current.streamingText).toBe("partial…");
    // crash: the run closes with NO terminal agent_message for a1
    act(() =>
      h.handlers!.onEvent(ev(1, { type: "run_completed", run_id: "r1", ok: false, at: AT })),
    );
    expect(result.current.streamingText).toBe("");
  });

  test("a replayed durable event (same idx) is de-duped", async () => {
    const { result } = await mounted();
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onEvent(ev(0, runStarted()))); // replay on reconnect
    expect(result.current.events).toHaveLength(1);
  });
});

describe("useSessionEvents — the windowed open sequence", () => {
  test("a short session loads whole, subscribes from its tail, and has nothing to backfill", async () => {
    h.log = fixtureLog(2, 3);
    const { result } = await mounted();
    expect(result.current.events.map((e) => e.idx)).toEqual(h.log.map((r) => Number(r.idx)));
    // The stream continues from the highest idx held — no replay, no gap.
    expect(h.since).toBe(h.log.length - 1);
    expect(result.current.hasMore).toBe(false);
    expect(result.current.oldestIdx).toBe(0);
  });

  test("a long session opens on the tail window plus the whole spine", async () => {
    h.log = fixtureLog(12, 20); // 516 events — well over one window
    const { result } = await mounted();

    const idxs = result.current.events.map((e) => e.idx);
    expect(idxs).toEqual([...idxs].sort((a, b) => a - b));
    expect(new Set(idxs).size).toBe(idxs.length);
    expect(result.current.hasMore).toBe(true);
    const floor = result.current.oldestIdx!;
    expect(floor).toBeGreaterThan(0);

    // Below the floor the spine carries the conversation; the heavy tool kinds
    // are deferred to the window.
    const below = result.current.events.filter((e) => e.idx < floor);
    expect(below.length).toBeGreaterThan(0);
    expect(below.some((e) => e.event.type === "run_started")).toBe(true);
    expect(below.some((e) => e.event.type === "tool_call_completed")).toBe(false);
    // The window edge sits ON a run boundary, so no turn is cut in half.
    expect(result.current.events.find((e) => e.idx === floor)!.event.type).toBe("run_started");
    expect(h.since).toBe(h.log.length - 1);
  });

  test("loadOlder prepends the next window and moves the oldest idx down", async () => {
    h.log = fixtureLog(12, 20);
    const { result } = await mounted();
    const firstFloor = result.current.oldestIdx!;
    const before = result.current.events.length;

    act(() => result.current.loadOlder());
    await waitFor(() => expect(result.current.loadingOlder).toBe(false));

    expect(result.current.oldestIdx!).toBeLessThan(firstFloor);
    expect(result.current.events.length).toBeGreaterThan(before);
    const idxs = result.current.events.map((e) => e.idx);
    expect(new Set(idxs).size).toBe(idxs.length);
    expect(idxs).toEqual([...idxs].sort((a, b) => a - b));
  });
});
