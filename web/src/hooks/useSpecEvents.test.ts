import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { SseHandlers } from "@/sse";

const subscription = vi.hoisted(() => ({
  specId: "",
  since: 0,
  handlers: null as SseHandlers | null,
  close: vi.fn(),
}));

vi.mock("@/sse", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/sse")>()),
  subscribeSpec: (specId: string, handlers: SseHandlers, since = -1) => {
    subscription.specId = specId;
    subscription.handlers = handlers;
    subscription.since = since;
    return subscription.close;
  },
}));

import { useSpecEvents } from "./useSpecEvents";

beforeEach(() => {
  subscription.specId = "";
  subscription.since = 0;
  subscription.handlers = null;
  subscription.close.mockReset();
});

describe("useSpecEvents", () => {
  test("subscribes to the whole spec feed and merges replayed events by index", () => {
    const { result } = renderHook(() => useSpecEvents("spec-1"));
    expect(subscription.specId).toBe("spec-1");
    expect(subscription.since).toBe(-1);

    const first = {
      idx: 2,
      event: { type: "harness_idle" as const, at: "2026-08-13T10:00:02.000Z" },
    };
    const earlier = {
      idx: 1,
      event: { type: "harness_idle" as const, at: "2026-08-13T10:00:01.000Z" },
    };
    act(() => subscription.handlers!.onEvent(first));
    act(() => subscription.handlers!.onEvent(earlier));
    act(() => subscription.handlers!.onEvent(first));

    expect(result.current.events.map(({ idx }) => idx)).toEqual([1, 2]);
  });

  test("reports lag and closes the subscription on unmount", () => {
    const { result, unmount } = renderHook(() => useSpecEvents("spec-1"));
    act(() => subscription.handlers!.onLagged?.(3));
    expect(result.current.missed).toBe(3);
    unmount();
    expect(subscription.close).toHaveBeenCalledOnce();
  });

  test("keeps live chunks separate until the durable agent message arrives", () => {
    const { result } = renderHook(() => useSpecEvents("spec-1"));
    act(() =>
      subscription.handlers!.onDelta?.({ runId: "run-1", messageId: "agent-1", chunk: "Hel" }),
    );
    act(() =>
      subscription.handlers!.onDelta?.({ runId: "run-1", messageId: "agent-1", chunk: "lo" }),
    );
    expect(result.current.streamingText).toBe("Hello");
    expect(result.current.events).toHaveLength(0);

    act(() =>
      subscription.handlers!.onEvent({
        idx: 1,
        event: {
          type: "agent_message",
          run_id: "run-1",
          message_id: "agent-1",
          role: "assistant",
          text: "Hello",
          at: "2026-08-13T10:00:00.000Z",
        },
      }),
    );
    expect(result.current.streamingText).toBe("");
    expect(result.current.events).toHaveLength(1);
  });
});
