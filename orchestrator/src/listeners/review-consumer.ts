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

/** Whether a `run_completed` payload reports a failed harness run (`ok:false`).
 *  A malformed or absent flag is treated as a clean run — the same tolerant
 *  default the pre-`ok` behaviour had. */
function runCompletedFailed(payloadJson: string): boolean {
  try {
    return (JSON.parse(payloadJson) as { ok?: unknown }).ok === false;
  } catch {
    return false;
  }
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
    async handle(event, ctx) {
      // A harness run returns the reusable session to Idle instead of
      // terminating it. `run_completed` is the curated event for that
      // transition and backs up the authoritative in-band tool signal. Its
      // `ok` flag tells a clean turn (`ok:true`) apart from a run that ERRORED
      // (`ok:false` — e.g. the agent could not authenticate); the workflow must
      // never report a failed run as a "no findings" completion.
      if (event.kind !== "run_completed") return;
      const runFailed = runCompletedFailed(event.payloadJson);
      const { reviewWorkflowId, role } = destination();
      await deps.send(
        reviewWorkflowId,
        { kind: "session_idle", role, ...(runFailed ? { runFailed: true } : {}) },
        REVIEW_TOPIC,
        `review:${ctx.sessionId}:idle`,
      );
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
