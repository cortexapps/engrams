import { describe, expect, test } from "bun:test";

import type { PendingToolCallRow, PendingToolCallStore } from "../pending-tool-calls.ts";
import {
  SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS,
  findStaleSessionToolCalls,
  runPendingToolCallWatchdog,
} from "../watchdog.ts";

const NOW = new Date("2026-07-13T12:00:00.000Z");

function row(
  toolCallId: string,
  options: Partial<PendingToolCallRow> = {},
): PendingToolCallRow {
  return {
    sessionId: "session-1",
    toolCallId,
    toolName: "ask_user_question",
    handling: "session",
    requestedAt: new Date(NOW.getTime() - SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS - 1),
    submittedAt: null,
    completedAt: null,
    ...options,
  };
}

describe("pending tool-call watchdog", () => {
  test("finds only unsubmitted session-handled calls older than 24 hours", () => {
    const stale = row("stale");
    const fresh = row("fresh", {
      requestedAt: new Date(NOW.getTime() - SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS + 1),
    });
    const submitted = row("submitted", { submittedAt: new Date(NOW.getTime() - 1_000) });
    const handled = row("handled", { handling: "handled" });

    expect(findStaleSessionToolCalls([stale, fresh, submitted, handled], () => NOW)).toEqual([stale]);
  });

  test("runner logs each stale row and invokes the future-alert hook", async () => {
    const stale = row("stale");
    const queriedCutoffs: Date[] = [];
    const logs: Array<{ row: PendingToolCallRow; ageMs: number }> = [];
    const hookCalls: PendingToolCallRow[][] = [];
    const pendingCalls: PendingToolCallStore = {
      recordRequested: async () => {},
      markSubmitted: async () => {},
      markCompleted: async () => {},
      find: async () => null,
      listUnsubmittedSessionCallsBefore: async (cutoff) => {
        queriedCutoffs.push(cutoff);
        return [stale];
      },
    };

    const result = await runPendingToolCallWatchdog({
      pendingCalls,
      now: () => NOW,
      logStale: (flagged, ageMs) => void logs.push({ row: flagged, ageMs }),
      onStale: async (flagged) => void hookCalls.push(flagged),
    });

    expect(queriedCutoffs).toEqual([
      new Date(NOW.getTime() - SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS),
    ]);
    expect(result).toEqual([stale]);
    expect(logs).toEqual([
      { row: stale, ageMs: SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS + 1 },
    ]);
    expect(hookCalls).toEqual([[stale]]);
  });
});
