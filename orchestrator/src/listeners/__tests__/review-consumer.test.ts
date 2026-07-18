import { describe, expect, test } from "bun:test";

import { REVIEW_TOPIC, type ReviewInbox } from "../../workflows/review-inbox.ts";
import {
  makeReviewConsumer,
  type ReviewMailboxSend,
} from "../review-consumer.ts";

function sender() {
  const calls: Array<{
    destinationId: string;
    message: ReviewInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const send: ReviewMailboxSend = async (
    destinationId,
    message,
    topic,
    idempotencyKey,
  ) => void calls.push({ destinationId, message, topic, idempotencyKey });
  return { send, calls };
}

describe("Review consumer", () => {
  test("applies to a bound session and sends its terminal outcome", async () => {
    const sent = sender();
    const consumer = makeReviewConsumer({
      findReviewSession: async () => ({
        reviewWorkflowId: "review-wf-1",
        role: "verifier",
      }),
      send: sent.send,
    });

    expect(await consumer.appliesTo("session-1")).toBe(true);
    await consumer.onTerminal?.("completed", { sessionId: "session-1" });

    expect(sent.calls).toEqual([{
      destinationId: "review-wf-1",
      message: {
        kind: "session_ended",
        role: "verifier",
        sessionId: "session-1",
        outcome: "completed",
      },
      topic: REVIEW_TOPIC,
      idempotencyKey: "review:session-1:terminal",
    }]);
  });

  test("does not apply when the session has no review binding", async () => {
    const sent = sender();
    let lookups = 0;
    const consumer = makeReviewConsumer({
      findReviewSession: async () => {
        lookups++;
        return null;
      },
      send: sent.send,
    });

    expect(await consumer.appliesTo("session-1")).toBe(false);
    expect(await consumer.appliesTo("session-1")).toBe(false);
    expect(lookups).toBe(1);
    expect(sent.calls).toEqual([]);
  });

  test("sends run completion as the idle fallback with a stable key", async () => {
    const sent = sender();
    const consumer = makeReviewConsumer({
      findReviewSession: async () => ({
        reviewWorkflowId: "review-wf-1",
        role: "finder",
      }),
      send: sent.send,
    });

    await consumer.appliesTo("session-1");
    await consumer.handle(
      { idx: 7n, kind: "run_completed", payloadJson: "{}" },
      { sessionId: "session-1" },
    );

    expect(sent.calls).toEqual([{
      destinationId: "review-wf-1",
      message: { kind: "session_idle", role: "finder" },
      topic: REVIEW_TOPIC,
      idempotencyKey: "review:session-1:idle",
    }]);
  });

  test("ignores curated events other than run completion", async () => {
    const sent = sender();
    const consumer = makeReviewConsumer({
      findReviewSession: async () => ({
        reviewWorkflowId: "review-wf-1",
        role: "finder",
      }),
      send: sent.send,
    });

    await consumer.appliesTo("session-1");
    await consumer.handle(
      { idx: 7n, kind: "run_started", payloadJson: "{}" },
      { sessionId: "session-1" },
    );

    expect(sent.calls).toEqual([]);
  });
});
