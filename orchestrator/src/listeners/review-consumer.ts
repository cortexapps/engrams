import { DBOS } from "@dbos-inc/dbos-sdk";

import type { ReviewSessionBinding } from "../db/review-sessions.ts";
import { makeReviewSessionStore } from "../db/review-sessions.ts";
import {
  REVIEW_TOPIC,
  type ReviewInbox,
} from "../workflows/review-inbox.ts";
import type { SessionConsumer } from "./consumer.ts";

export type ReviewMailboxSend = (
  destinationId: string,
  message: ReviewInbox,
  topic: string,
  idempotencyKey: string,
) => Promise<void>;

export interface ReviewConsumerDeps {
  findReviewSession(sessionId: string): Promise<ReviewSessionBinding | null>;
  send: ReviewMailboxSend;
}

export function makeReviewConsumer(deps: ReviewConsumerDeps): SessionConsumer {
  let binding: ReviewSessionBinding | null | undefined;

  const destination = (): ReviewSessionBinding => {
    if (!binding) throw new Error("Review consumer has no workflow binding");
    return binding;
  };

  return {
    name: "review",
    interestedIn: () => true,
    async appliesTo(sessionId) {
      if (binding === undefined) {
        binding = await deps.findReviewSession(sessionId);
      }
      return binding !== null;
    },
    async handle() {
      // Review findings and verdicts arrive through tools; the workflow only
      // needs the terminal signal from this event stream.
    },
    async onTerminal(outcome, ctx) {
      const { reviewWorkflowId, role } = destination();
      await deps.send(
        reviewWorkflowId,
        { kind: "session_ended", role, sessionId: ctx.sessionId, outcome },
        REVIEW_TOPIC,
        `review:${ctx.sessionId}:terminal`,
      );
    },
  };
}

export function makeProductionReviewConsumer(): SessionConsumer {
  const store = makeReviewSessionStore();
  return makeReviewConsumer({
    findReviewSession: (sessionId) => store.find(sessionId),
    send: async (destinationId, message, topic, idempotencyKey) => {
      await DBOS.send<ReviewInbox>(
        destinationId,
        message,
        topic,
        idempotencyKey,
      );
    },
  });
}
