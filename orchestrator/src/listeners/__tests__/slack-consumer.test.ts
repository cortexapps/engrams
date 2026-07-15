import { describe, expect, test } from "bun:test";

import { THREAD_TOPIC, type ThreadInbox } from "../../workflows/thread-inbox.ts";
import {
  makeSlackConsumer,
  type SlackMailboxSend,
} from "../slack-consumer.ts";

function sender() {
  const calls: Array<{
    destinationId: string;
    message: ThreadInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const send: SlackMailboxSend = async (
    destinationId,
    message,
    topic,
    idempotencyKey,
  ) => void calls.push({ destinationId, message, topic, idempotencyKey });
  return { send, calls };
}

describe("Slack consumer", () => {
  test("sends an event to the bound mailbox with its per-event idempotency key", async () => {
    const sent = sender();
    const consumer = makeSlackConsumer({
      findThreadWorkflow: async () => "thread-wf-1",
      send: sent.send,
    });
    const event = { idx: 42n, kind: "run_started", payloadJson: "{}" };

    expect(await consumer.appliesTo("session-1")).toBe(true);
    await consumer.handle(event, { sessionId: "session-1" });

    expect(sent.calls).toEqual([{
      destinationId: "thread-wf-1",
      message: { kind: "session_event", event },
      topic: THREAD_TOPIC,
      idempotencyKey: "slack:session-1:42",
    }]);
  });

  test("terminal sends no lastMessage and uses the terminal idempotency key", async () => {
    const sent = sender();
    const consumer = makeSlackConsumer({
      findThreadWorkflow: async () => "thread-wf-1",
      send: sent.send,
    });

    await consumer.appliesTo("session-1");
    await consumer.onTerminal?.("completed", { sessionId: "session-1" });

    expect(sent.calls).toEqual([{
      destinationId: "thread-wf-1",
      message: { kind: "session_terminal", outcome: "completed" },
      topic: THREAD_TOPIC,
      idempotencyKey: "slack:session-1:terminal",
    }]);
  });

  test("does not apply when the session has no Slack binding", async () => {
    const sent = sender();
    let lookups = 0;
    const consumer = makeSlackConsumer({
      findThreadWorkflow: async () => {
        lookups++;
        return null;
      },
      send: sent.send,
    });

    expect(await consumer.appliesTo("session-1")).toBe(false);
    expect(lookups).toBe(1);
    expect(sent.calls).toEqual([]);
  });
});
