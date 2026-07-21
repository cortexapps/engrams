import { describe, expect, test } from "bun:test";

import { makePendingToolCallStore, type PendingToolCallDb } from "../pending-tool-calls.ts";

type DbAction =
  | { kind: "insert"; values: Record<string, unknown> }
  | { kind: "conflict-do-nothing" }
  | { kind: "update"; values: Record<string, unknown> };

/** House-style recording Drizzle fake: expose only the fluent methods used by
 *  the store and capture their values, with no database connection. */
function recordingDb(actions: DbAction[]): PendingToolCallDb {
  return {
    insert: () => ({
      values: (values: Record<string, unknown>) => {
        actions.push({ kind: "insert", values });
        return {
          onConflictDoNothing: async () => void actions.push({ kind: "conflict-do-nothing" }),
        };
      },
    }),
    update: () => ({
      set: (values: Record<string, unknown>) => {
        actions.push({ kind: "update", values });
        return { where: async () => {} };
      },
    }),
  } as unknown as PendingToolCallDb;
}

describe("PendingToolCallStore", () => {
  test("request insert is idempotent on session-scoped tool_call_id", async () => {
    const actions: DbAction[] = [];
    const store = makePendingToolCallStore(recordingDb(actions));
    const requestedAt = new Date("2026-07-13T12:00:00.000Z");

    await store.recordRequested({
      sessionId: "session-1",
      toolCallId: "call-1",
      toolName: "ask_user_question",
      handling: "session",
      requestedAt,
    });

    expect(actions).toEqual([
      {
        kind: "insert",
        values: {
          sessionId: "session-1",
          toolCallId: "call-1",
          toolName: "ask_user_question",
          handling: "session",
          requestedAt,
        },
      },
      { kind: "conflict-do-nothing" },
    ]);
  });

  test("submitted and completed timestamps update their dedicated columns", async () => {
    const actions: DbAction[] = [];
    const store = makePendingToolCallStore(recordingDb(actions));
    const submittedAt = new Date("2026-07-13T12:01:00.000Z");
    const completedAt = new Date("2026-07-13T12:02:00.000Z");

    await store.markSubmitted("session-1", "call-1", submittedAt);
    await store.markCompleted("session-1", "call-1", completedAt);

    expect(actions).toEqual([
      { kind: "update", values: { submittedAt } },
      { kind: "update", values: { completedAt } },
    ]);
  });
});
