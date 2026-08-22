import { describe, expect, test } from "bun:test";
import { Error as DBOSErrors } from "@dbos-inc/dbos-sdk";

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

  test("a send to a finished run is a no-op; any other send failure still propagates", async () => {
    // A kept session outlives its run: DBOS rejects the send. The event is
    // dropped (not retried forever), so the listener's cursor advances.
    let calls = 0;
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: async () => {
        calls += 1;
        throw new DBOSErrors.DBOSNonExistentWorkflowError(RUN_ID);
      },
    });
    expect(await consumer.appliesTo("session-1")).toBe(true);
    await consumer.handle(
      { idx: 7n, kind: "run_completed", payloadJson: `{"ok":true}` },
      { sessionId: "session-1" },
    );
    await consumer.onTerminal?.("failed", { sessionId: "session-1" });
    expect(calls).toBe(3); // session_event, session_idle, session_ended — each dropped

    const broken = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID }),
      send: async () => {
        throw new Error("postgres down");
      },
    });
    await broken.appliesTo("session-2");
    await expect(
      broken.handle({ idx: 1n, kind: "run_completed", payloadJson: "{}" }, { sessionId: "session-2" }),
    ).rejects.toThrow("postgres down");
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


describe("Automation consumer — relay gate (ADR 0119 phase 4.5)", () => {
  test("a relay-bound session forwards every curated event with an idx-scoped key; a plain one forwards none", async () => {
    const sent = sender();
    const relayBound = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
    });
    expect(await relayBound.appliesTo("session-1")).toBe(true);
    await relayBound.handle(
      { idx: 5n, kind: "agent_message", payloadJson: `{"role":"assistant","text":"hi"}` },
      { sessionId: "session-1" },
    );
    expect(sent.calls).toEqual([
      {
        destinationId: RUN_ID,
        message: {
          kind: "session_event",
          sessionId: "session-1",
          event: { idx: 5n, kind: "agent_message", payloadJson: `{"role":"assistant","text":"hi"}` },
        },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: "autorun:session-1:event:5",
      },
    ]);

    const plain = sender();
    const unbound = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: false }),
      send: plain.send,
    });
    await unbound.appliesTo("session-1");
    await unbound.handle(
      { idx: 6n, kind: "agent_message", payloadJson: "{}" },
      { sessionId: "session-1" },
    );
    expect(plain.calls).toEqual([]);
  });

  test("run_completed on a relay-bound session emits BOTH the curated event and the idle", async () => {
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle(
      { idx: 7n, kind: "run_completed", payloadJson: `{"ok":true}` },
      { sessionId: "session-1" },
    );
    expect(sent.calls.map((c) => c.message.kind)).toEqual(["session_event", "session_idle"]);
  });

  test("the relay flag is re-read per event, so a flip after install takes effect", async () => {
    let relay = false;
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay }),
      send: sent.send,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle({ idx: 1n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    expect(sent.calls).toEqual([]);
    relay = true; // the relay block installed
    await consumer.handle({ idx: 2n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    expect(sent.calls.map((c) => c.message.kind)).toEqual(["session_event"]);
  });
});
