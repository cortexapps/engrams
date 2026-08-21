import { describe, expect, test } from "bun:test";

import { AUTOMATION_TOPIC, type AutomationInbox } from "../../automations/engine/inbox.ts";
import {
  makeAutomationConsumer,
  type AutomationMailboxSend,
} from "../automation-consumer.ts";

function sender() {
  const calls: Array<{
    destinationId: string;
    message: AutomationInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const send: AutomationMailboxSend = async (destinationId, message, topic, idempotencyKey) =>
    void calls.push({ destinationId, message, topic, idempotencyKey });
  return { send, calls };
}

const RUN_ID = "autorun:auto-1:webhook:d1";

describe("Automation consumer", () => {
  test("does not apply to an unbound session and memoizes the lookup", async () => {
    let lookups = 0;
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => {
        lookups += 1;
        return null;
      },
      send: sent.send,
    });
    expect(await consumer.appliesTo("session-1")).toBe(false);
    expect(await consumer.appliesTo("session-1")).toBe(false);
    expect(lookups).toBe(1);
  });

  test("run_completed becomes session_idle with an idx-scoped key", async () => {
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID }),
      send: sent.send,
    });
    expect(await consumer.appliesTo("session-1")).toBe(true);

    await consumer.handle(
      { idx: 7n, kind: "run_completed", payloadJson: `{"ok":true}` },
      { sessionId: "session-1" },
    );
    await consumer.handle(
      { idx: 12n, kind: "run_completed", payloadJson: `{"ok":false}` },
      { sessionId: "session-1" },
    );
    // Non-run events are ignored.
    await consumer.handle(
      { idx: 13n, kind: "agent_message", payloadJson: "{}" },
      { sessionId: "session-1" },
    );

    expect(sent.calls).toEqual([
      {
        destinationId: RUN_ID,
        message: { kind: "session_idle", sessionId: "session-1" },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: "autorun:session-1:idle:7",
      },
      {
        destinationId: RUN_ID,
        message: { kind: "session_idle", sessionId: "session-1", runFailed: true },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: "autorun:session-1:idle:12",
      },
    ]);
  });

  test("malformed run_completed payload counts as a clean run", async () => {
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID }),
      send: sent.send,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle(
      { idx: 1n, kind: "run_completed", payloadJson: "not json" },
      { sessionId: "session-1" },
    );
    expect(sent.calls[0]!.message).toEqual({ kind: "session_idle", sessionId: "session-1" });
  });

  test("terminal outcome becomes session_ended", async () => {
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID }),
      send: sent.send,
    });
    await consumer.appliesTo("session-1");
    await consumer.onTerminal?.("failed", { sessionId: "session-1" });
    expect(sent.calls).toEqual([
      {
        destinationId: RUN_ID,
        message: { kind: "session_ended", sessionId: "session-1", outcome: "failed" },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: "autorun:session-1:terminal",
      },
    ]);
  });
});
