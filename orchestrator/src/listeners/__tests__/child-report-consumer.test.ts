import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { CuratedEvent } from "../../control-plane/session-events.ts";
import {
  childReportPromptId,
  makeChildReportConsumer,
  REPORT_MESSAGE_CHARS,
  renderChildReport,
  type SpawnedChild,
} from "../child-report-consumer.ts";

const CHILD = "11111111-1111-4111-8111-111111111111";
const PARENT = "22222222-2222-4222-8222-222222222222";
const ctx = { sessionId: CHILD };

const openChild: SpawnedChild = {
  canonicalTaskName: "build/tests",
  taskStatus: "working",
  parentSessionId: PARENT,
};

function assistant(idx: bigint, runId: string, text: string): CuratedEvent {
  return {
    idx,
    kind: "agent_message",
    payloadJson: JSON.stringify({ run_id: runId, role: "assistant", text }),
  };
}

function userEcho(idx: bigint, text: string): CuratedEvent {
  return {
    idx,
    kind: "agent_message",
    payloadJson: JSON.stringify({ run_id: "", role: "user", text }),
  };
}

function runCompleted(idx: bigint, runId: string, ok = true): CuratedEvent {
  return { idx, kind: "run_completed", payloadJson: JSON.stringify({ run_id: runId, ok }) };
}

function harness(
  child: SpawnedChild | null,
  send: (input: { sessionId: string; promptId: string; text: string }) => Promise<void> = async () => {},
) {
  const sent: Array<{ sessionId: string; promptId: string; text: string }> = [];
  const consumer = makeChildReportConsumer({
    findSpawnedChild: async () => child,
    sendPrompt: async (input) => {
      await send(input);
      sent.push(input);
    },
  });
  return { consumer, sent };
}

describe("child report consumer", () => {
  test("a child run end sends one prompt to the parent with the last assistant message", async () => {
    const { consumer, sent } = harness(openChild);
    expect(await consumer.appliesTo(CHILD)).toBe(true);

    await consumer.handle(userEcho(1n, "run the tests"), ctx);
    await consumer.handle(assistant(2n, "r1", "Starting."), ctx);
    await consumer.handle({ idx: 3n, kind: "tool_call_completed", payloadJson: "{}" }, ctx);
    await consumer.handle(assistant(4n, "r1", "All 12 tests pass."), ctx);
    await consumer.handle(runCompleted(5n, "r1"), ctx);

    expect(sent).toEqual([
      {
        sessionId: PARENT,
        promptId: childReportPromptId(CHILD, "r1"),
        text:
          `Child session "build/tests" (${CHILD}) finished a turn (completed).\n\n` +
          "All 12 tests pass.\n\nUse read_session for its full output.",
      },
    ]);
  });

  test("a failed run and an interrupted run name their outcome", async () => {
    const { consumer, sent } = harness(openChild);
    await consumer.handle(runCompleted(1n, "r1", false), ctx);
    await consumer.handle({ idx: 2n, kind: "run_interrupted", payloadJson: '{"run_id":"r2"}' }, ctx);
    expect(sent.map((s) => s.text.split("\n")[0])).toEqual([
      `Child session "build/tests" (${CHILD}) finished a turn (failed).`,
      `Child session "build/tests" (${CHILD}) finished a turn (interrupted).`,
    ]);
    expect(sent[0]!.text).toContain("The child sent no assistant message in this turn.");
  });

  test("the cursor holds before an unreported message, so a restart replays it", async () => {
    const { consumer } = harness(openChild);
    expect(await consumer.handle(userEcho(1n, "go"), ctx)).toBeUndefined();
    expect(await consumer.handle(assistant(2n, "r1", "Working."), ctx)).toBe(1n);
    expect(await consumer.handle({ idx: 3n, kind: "file_changed", payloadJson: "{}" }, ctx)).toBe(1n);
    expect(await consumer.handle(assistant(4n, "r1", "Done."), ctx)).toBe(3n);
    expect(await consumer.handle(runCompleted(5n, "r1"), ctx)).toBeUndefined();
  });

  test("a redelivered run end after a restart sends the same prompt id and text", async () => {
    const first = harness(openChild);
    await first.consumer.handle(assistant(4n, "r1", "Done."), ctx);
    await first.consumer.handle(runCompleted(5n, "r1"), ctx);

    // A fresh consumer resumes from the held floor (idx 3).
    const replay = harness(openChild);
    await replay.consumer.handle(assistant(4n, "r1", "Done."), ctx);
    await replay.consumer.handle(runCompleted(5n, "r1"), ctx);

    expect(replay.sent).toEqual(first.sent);
  });

  test("a message from an earlier run is not reported for a later run", async () => {
    const { consumer, sent } = harness(openChild);
    await consumer.handle(assistant(1n, "r1", "old"), ctx);
    await consumer.handle(runCompleted(2n, "r2"), ctx);
    expect(sent[0]!.text).not.toContain("old");
  });

  test("a cancelled child sends nothing", async () => {
    const { consumer, sent } = harness({ ...openChild, taskStatus: "cancelled" });
    await consumer.handle(assistant(1n, "r1", "Done."), ctx);
    await consumer.handle(runCompleted(2n, "r1"), ctx);
    expect(sent).toEqual([]);
  });

  test("a session that is not a spawned child does not apply and sends nothing", async () => {
    const { consumer, sent } = harness(null);
    expect(await consumer.appliesTo(CHILD)).toBe(false);
    await consumer.handle(runCompleted(1n, "r1"), ctx);
    expect(sent).toEqual([]);
  });

  test("a parent that has ended is skipped without a retry", async () => {
    let calls = 0;
    const { consumer } = harness(openChild, async () => {
      calls += 1;
      throw new ConnectError("session is completed", Code.FailedPrecondition);
    });
    await consumer.handle(runCompleted(1n, "r1"), ctx);
    expect(calls).toBe(1);
  });

  test("a transient send error retries, then gives up without failing the child", async () => {
    let calls = 0;
    const { consumer } = harness(openChild, async () => {
      calls += 1;
      throw new ConnectError("unavailable", Code.Unavailable);
    });
    for (let attempt = 1; attempt < 5; attempt++) {
      await expect(consumer.handle(runCompleted(1n, "r1"), ctx)).rejects.toThrow("unavailable");
    }
    await consumer.handle(runCompleted(1n, "r1"), ctx);
    expect(calls).toBe(5);
  });
});

describe("renderChildReport", () => {
  test("truncates a long message and says so", () => {
    const text = renderChildReport({
      canonicalTaskName: "a",
      childSessionId: CHILD,
      outcome: "completed",
      lastMessage: "x".repeat(REPORT_MESSAGE_CHARS + 500),
    });
    const note = `[Truncated: the first ${REPORT_MESSAGE_CHARS} of ${REPORT_MESSAGE_CHARS + 500} characters.]`;
    expect(text).toContain(`${"x".repeat(REPORT_MESSAGE_CHARS)}\n\n${note}`);
    expect(text).not.toContain("x".repeat(REPORT_MESSAGE_CHARS + 1));
    expect(text.endsWith("Use read_session for its full output.")).toBe(true);
  });

  test("keeps a message at the limit whole", () => {
    const text = renderChildReport({
      canonicalTaskName: "a",
      childSessionId: CHILD,
      outcome: "completed",
      lastMessage: "y".repeat(REPORT_MESSAGE_CHARS),
    });
    expect(text).not.toContain("[Truncated");
  });
});
