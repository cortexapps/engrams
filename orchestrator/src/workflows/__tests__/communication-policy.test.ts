/**
 * Framework event-classification (ADR 0059 P1.4).
 *
 * `routeSessionEvent` is the pure decision the thread workflow makes for each
 * curated session event: which `CommunicationPolicy` method to invoke (and,
 * for questions, the `tool_call_id` correlation token). Pure → unit-tested
 * here in isolation; the workflow just executes the chosen effect as a step.
 */

import { expect, test, describe } from "bun:test";
import { routeSessionEvent } from "../communication-policy.ts";
import type { CuratedEvent } from "../../control-plane/session-events.ts";

const ev = (kind: string, payloadJson = "{}"): CuratedEvent => ({ idx: 0n, kind, payloadJson });

describe("routeSessionEvent()", () => {
  test("user_question → question, carrying the tool_call_id", () => {
    expect(routeSessionEvent(ev("user_question", '{"tool_call_id":"t1"}'))).toEqual({
      kind: "question",
      toolCallId: "t1",
    });
  });

  test("question_answered → answered, carrying the tool_call_id", () => {
    expect(routeSessionEvent(ev("question_answered", '{"tool_call_id":"t1"}'))).toEqual({
      kind: "answered",
      toolCallId: "t1",
    });
  });

  test("integration_asset and file_shared → asset", () => {
    expect(routeSessionEvent(ev("integration_asset"))).toEqual({ kind: "asset" });
    expect(routeSessionEvent(ev("file_shared"))).toEqual({ kind: "asset" });
  });

  test("run_started / run_completed → ignore (curated but not rendered as content)", () => {
    expect(routeSessionEvent(ev("run_started"))).toEqual({ kind: "ignore" });
    // run_completed is NOT terminal (Invariant 2) and renders nothing.
    expect(routeSessionEvent(ev("run_completed"))).toEqual({ kind: "ignore" });
  });

  test("a question with an unparseable payload still routes (toolCallId undefined)", () => {
    expect(routeSessionEvent(ev("user_question", "not json"))).toEqual({
      kind: "question",
      toolCallId: undefined,
    });
  });
});
