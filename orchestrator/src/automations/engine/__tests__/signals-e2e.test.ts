import { describe, expect, test } from "bun:test";

import { makeAutomationConsumer } from "../../../listeners/automation-consumer.ts";
import type { AutomationInbox } from "../inbox.ts";

/** End-to-end shape check (phase 1.4): the messages the stream consumer
 * produces are exactly what the interpreter's session waits match. The
 * interpreter suite already proves matching from scripted messages; this
 * proves the consumer emits that same shape, so the two sides cannot drift.
 */

describe("consumer → interpreter signal pipe", () => {
  test("run_completed produces the session_idle message a run_end wait consumes", async () => {
    const captured: AutomationInbox[] = [];
    const consumer = makeAutomationConsumer({
      findSessionBinding: async () => ({ runId: "autorun:auto-1:webhook:d1" }),
      send: async (_dest, message) => void captured.push(message),
    });
    expect(await consumer.appliesTo("s-launch")).toBe(true);
    await consumer.handle(
      { idx: 3n, kind: "run_completed", payloadJson: `{"ok":true}` },
      { sessionId: "s-launch" },
    );
    await consumer.onTerminal?.("completed", { sessionId: "s-launch" });

    expect(captured).toEqual([
      { kind: "session_idle", sessionId: "s-launch" },
      { kind: "session_ended", sessionId: "s-launch", outcome: "completed" },
    ]);
    // These literals are the wait-matcher inputs exercised in
    // interpreter.test.ts ("linear graph", "a failed session run fails the
    // run") — same discriminants, same field names.
  });
});
