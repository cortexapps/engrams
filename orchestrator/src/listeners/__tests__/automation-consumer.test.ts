import { describe, expect, test } from "bun:test";
import { Error as DBOSErrors } from "@dbos-inc/dbos-sdk";

import { AUTOMATION_TOPIC, inboxKeys, type AutomationInbox } from "../../automations/engine/inbox.ts";
import type { CuratedEvent, TerminalOutcome } from "../../control-plane/session-events.ts";
import {
  makeAutomationConsumer,
  type AutomationMailboxSend,
  type AutomationRelayRenderer,
} from "../automation-consumer.ts";

/** A recording session relay (contract 5): what the consumer asked it to
 * render and to close. */
function relay(options: { throwOnRender?: boolean; throwOnClose?: boolean } = {}) {
  const rendered: Array<{ sessionId: string; idx: bigint; kind: string }> = [];
  const closed: Array<{ sessionId: string; outcome: TerminalOutcome }> = [];
  const renderer: AutomationRelayRenderer = {
    async render(sessionId: string, event: CuratedEvent) {
      if (options.throwOnRender) throw new Error("slack down");
      rendered.push({ sessionId, idx: event.idx, kind: event.kind });
    },
    async close(sessionId: string, outcome: TerminalOutcome) {
      if (options.throwOnClose) throw new Error("slack down");
      closed.push({ sessionId, outcome });
    },
  };
  return { renderer, rendered, closed };
}

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
    expect(calls).toBe(2); // session_idle, session_ended — each dropped

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

  test("run_interrupted ends the run as failed, rendered into the relay first", async () => {
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    await consumer.appliesTo("session-1");
    const event = { idx: 14n, kind: "run_interrupted", payloadJson: '{"run_id":"run-1"}' };
    await consumer.handle(event, { sessionId: "session-1" });
    expect(r.rendered).toEqual([{ sessionId: "session-1", idx: 14n, kind: "run_interrupted" }]);
    expect(sent.calls).toEqual([
      {
        destinationId: RUN_ID,
        message: { kind: "session_idle", sessionId: "session-1", runFailed: true },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: inboxKeys.sessionIdle("session-1", 14n),
      },
    ]);
  });

  test("run_continued does not end the run", async () => {
    const sent = sender();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID }),
      send: sent.send,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle(
      { idx: 15n, kind: "run_continued", payloadJson: '{"run_id":"run-1"}' },
      { sessionId: "session-1" },
    );
    expect(sent.calls).toEqual([]);
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


describe("Automation consumer — session relay (contract 5)", () => {
  test("a relayed session has every curated event rendered; a plain one has none", async () => {
    const sent = sender();
    const r = relay();
    const relayed = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    expect(await relayed.appliesTo("session-1")).toBe(true);
    await relayed.handle(
      { idx: 5n, kind: "agent_message", payloadJson: `{"role":"assistant","text":"hi"}` },
      { sessionId: "session-1" },
    );
    expect(r.rendered).toEqual([{ sessionId: "session-1", idx: 5n, kind: "agent_message" }]);
    // A rendered message is no mailbox traffic: no run is waiting for it.
    expect(sent.calls).toEqual([]);

    const plain = relay();
    const unrelayed = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: false }),
      send: sent.send,
      relay: plain.renderer,
    });
    await unrelayed.appliesTo("session-1");
    await unrelayed.handle({ idx: 6n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    expect(plain.rendered).toEqual([]);
  });

  test("run_completed on a relayed session is rendered AND becomes the idle", async () => {
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle({ idx: 7n, kind: "run_completed", payloadJson: `{"ok":true}` }, { sessionId: "session-1" });
    expect(r.rendered.map((e) => e.kind)).toEqual(["run_completed"]);
    expect(sent.calls.map((c) => c.message.kind)).toEqual(["session_idle"]);
  });

  test("the session's end closes the relay, then tells the run", async () => {
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    await consumer.appliesTo("session-1");
    await consumer.onTerminal!("completed", { sessionId: "session-1" });
    expect(r.closed).toEqual([{ sessionId: "session-1", outcome: "completed" }]);
    expect(sent.calls.map((c) => c.message.kind)).toEqual(["session_ended"]);
  });

  test("a relay that throws never pins the listener: the event and the end still reach the run", async () => {
    const sent = sender();
    const r = relay({ throwOnRender: true, throwOnClose: true });
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle({ idx: 1n, kind: "run_completed", payloadJson: "{}" }, { sessionId: "session-1" });
    await consumer.onTerminal!("failed", { sessionId: "session-1" });
    expect(sent.calls.map((c) => c.message.kind)).toEqual(["session_idle", "session_ended"]);
  });

  test("the destination is re-read per event: a kept session adopted by a later run of its workstream routes there", async () => {
    // r-old answered the thread and ended; the session was kept. A later
    // mention opened r-new, which adopted the session (D11). The listener
    // outlived both runs — it must not keep sending to r-old's mailbox.
    let runId = "autorun:auto-1:main:i-1:slack:Ev-old";
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId, relay: true }),
      send: sent.send,
      relay: r.renderer,
    });
    expect(await consumer.appliesTo("session-1")).toBe(true);
    await consumer.handle({ idx: 1n, kind: "run_completed", payloadJson: "{}" }, { sessionId: "session-1" });
    runId = "autorun:auto-1:main:i-1:slack:Ev-new"; // the resumed run adopted the session
    await consumer.handle({ idx: 2n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    await consumer.handle({ idx: 3n, kind: "run_completed", payloadJson: "{}" }, { sessionId: "session-1" });
    await consumer.onTerminal!("completed", { sessionId: "session-1" });
    expect(sent.calls.map((c) => [c.destinationId.slice(-6), c.message.kind])).toEqual([
      ["Ev-old", "session_idle"],
      ["Ev-new", "session_idle"],
      ["Ev-new", "session_ended"],
    ]);
    expect(r.rendered.map((e) => e.idx)).toEqual([1n, 2n, 3n]);
  });

  test("a binding that is gone drops the event instead of throwing", async () => {
    let bound = true;
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => (bound ? { runId: RUN_ID, relay: true } : null),
      send: sent.send,
      relay: r.renderer,
    });
    expect(await consumer.appliesTo("session-1")).toBe(true);
    bound = false;
    await consumer.handle({ idx: 1n, kind: "run_completed", payloadJson: "{}" }, { sessionId: "session-1" });
    await consumer.onTerminal!("completed", { sessionId: "session-1" });
    expect(sent.calls).toEqual([]);
    expect(r.rendered).toEqual([]);
    expect(r.closed).toEqual([]);
  });

  test("the relay flag is re-read per event, so an install after the session exists takes effect", async () => {
    let installed = false;
    const sent = sender();
    const r = relay();
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: RUN_ID, relay: installed }),
      send: sent.send,
      relay: r.renderer,
    });
    await consumer.appliesTo("session-1");
    await consumer.handle({ idx: 1n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    expect(r.rendered).toEqual([]);
    installed = true; // the relay block ran
    await consumer.handle({ idx: 2n, kind: "agent_message", payloadJson: "{}" }, { sessionId: "session-1" });
    expect(r.rendered.map((e) => e.idx)).toEqual([2n]);
  });
});
