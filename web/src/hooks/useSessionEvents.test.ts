// Phase 1c (ADR 0052) merge-safety tests for the live-streaming overlay.
// The invariant under test: ephemeral token chunks accumulate into a SEPARATE
// overlay (`streamingText`) and the durable event log ALWAYS wins — a chunk
// can never corrupt or double the persisted transcript, even under a crash,
// a late/out-of-order delta, or a pod cycle mid-message.

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";
import type { SseHandlers } from "../sse";
import type { SessionEvent } from "../lib/types";

// Capture the handlers `useSessionEvents` registers, so the test can drive
// onEvent / onDelta directly. `vi.hoisted` makes the holder available inside
// the hoisted `vi.mock` factory.
const h = vi.hoisted(() => ({ handlers: null as SseHandlers | null }));
vi.mock("../sse", () => ({
  subscribeSession: (_sessionId: string, handlers: SseHandlers) => {
    h.handlers = handlers;
    return () => {
      h.handlers = null;
    };
  },
}));

import { useSessionEvents } from "./useSessionEvents";

const AT = "2026-06-02T12:00:00.000Z";
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

beforeEach(() => {
  h.handlers = null;
});

describe("useSessionEvents — Phase 1c streaming overlay merge-safety", () => {
  test("chunks accumulate into streamingText and stay OUT of the durable events", () => {
    const { result } = renderHook(() => useSessionEvents("s1"));
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "hel" }));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "lo" }));
    expect(result.current.streamingText).toBe("hello");
    // chunks are ephemeral — never appended to the durable log
    expect(result.current.events).toHaveLength(1);
  });

  test("the durable agent_message supersedes the overlay and drops a late straggler", () => {
    const { result } = renderHook(() => useSessionEvents("s1"));
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

  test("a run terminal clears a still-open overlay (crash mid-stream)", () => {
    const { result } = renderHook(() => useSessionEvents("s1"));
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onDelta!({ runId: "r1", messageId: "a1", chunk: "partial…" }));
    expect(result.current.streamingText).toBe("partial…");
    // crash: the run closes with NO terminal agent_message for a1
    act(() =>
      h.handlers!.onEvent(ev(1, { type: "run_completed", run_id: "r1", ok: false, at: AT })),
    );
    expect(result.current.streamingText).toBe("");
  });

  test("a replayed durable event (same idx) is de-duped", () => {
    const { result } = renderHook(() => useSessionEvents("s1"));
    act(() => h.handlers!.onEvent(ev(0, runStarted())));
    act(() => h.handlers!.onEvent(ev(0, runStarted()))); // replay on reconnect
    expect(result.current.events).toHaveLength(1);
  });
});
