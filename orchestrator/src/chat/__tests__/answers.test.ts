import { describe, expect, test } from "bun:test";

import { answerDelivery } from "../answers.ts";

const RUN_ROUTE = { runId: "autorun:auto-1:slack:E1" };

describe("answerDelivery", () => {
  test("a route carrying runId delivers a slack_answer signal to that run; a card with no address is unrouted", () => {
    const answer = { toolCallId: "tc1", answers: { Pick: ["A"] } };
    expect(answerDelivery(RUN_ROUTE, answer)).toEqual({
      kind: "automation",
      runId: "autorun:auto-1:slack:E1",
      message: {
        kind: "signal",
        name: "slack_answer",
        payload: { toolCallId: "tc1", answers: { Pick: ["A"] } },
      },
      idempotencyKey: "slack-answer:tc1",
    });
    expect(answerDelivery({}, answer)).toEqual({ kind: "unrouted" });
  });

  test("a route carrying sessionId is a session question's: its answer goes to the session, not a run", () => {
    const sessionRoute = { ...RUN_ROUTE, sessionId: "s-1" };
    const answer = { toolCallId: "tc1", answers: { Pick: ["A"] } };
    expect(answerDelivery(sessionRoute, answer)).toEqual({ kind: "session", sessionId: "s-1" });
    // A choice card (its answer id is prefixed) is the run's even when the
    // route also names the session.
    const choice = { toolCallId: "choice:ask", answers: { Pick: ["A"] } };
    expect(answerDelivery(sessionRoute, choice)).toMatchObject({ kind: "automation", runId: RUN_ROUTE.runId });
  });
});
