import { expect, test } from "bun:test";
import { z } from "zod";

import type { PendingToolCallStore } from "../pending-tool-calls.ts";
import { createToolRegistry } from "../registry.ts";

test("tools.complete rejects an invalid output before the completer is called", async () => {
  const completerCalls: Array<{
    sessionId: string;
    toolCallId: string;
    resultJson: string;
  }> = [];
  const pendingCalls: PendingToolCallStore = {
    recordRequested: async () => {},
    markSubmitted: async () => {},
    markCompleted: async () => {},
    find: async () => ({
      sessionId: "session-1",
      toolCallId: "call-1",
      toolName: "save_memory",
      handling: "handled",
      requestedAt: new Date(0),
      submittedAt: null,
      completedAt: null,
    }),
    listUnsubmittedSessionCallsBefore: async () => [],
  };
  const registry = createToolRegistry({
    completion: {
      pendingCalls,
      completer: {
        completeToolCall: async (request) => void completerCalls.push(request),
      },
      now: () => new Date("2026-07-13T12:00:00.000Z"),
    },
  });
  registry.register({
    name: "save_memory",
    description: "Save a note.",
    input: z.object({ text: z.string() }),
    output: z.object({ saved: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async () => ({ saved: true }),
  });

  await expect(
    registry.complete("session-1", "call-1", { saved: "yes" }),
  ).rejects.toThrow("invalid result for tool save_memory");
  expect(completerCalls).toEqual([]);
});
