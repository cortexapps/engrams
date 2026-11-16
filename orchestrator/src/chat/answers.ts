/** Where the answer to a question card goes. A chat provider's click handler
 * reads the address from its card and calls `deliverAnswer`. */

import { DBOS } from "@dbos-inc/dbos-sdk";

import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import {
  CHOICE_ANSWER_SIGNAL,
  CHOICE_ID_PREFIX,
  deliverSessionAnswer,
} from "../automations/session-relay.ts";
import { log as rootLog } from "../log.ts";
import type { SourceAnswer } from "../workflows/communication-policy.ts";
import type { AnswerRoute } from "./chat.ts";

const log = rootLog.child({ component: "chat-answers" });

/** The answer to a choice card is a signal to its run. The answer to any
 * other card completes the tool call of the session that asked. A card with
 * neither address has nothing to take its answer. */
export function answerDelivery(
  route: AnswerRoute,
  answer: SourceAnswer,
):
  | { kind: "automation"; runId: string; message: AutomationInbox; idempotencyKey: string }
  | { kind: "session"; sessionId: string }
  | { kind: "unrouted" } {
  const choice = answer.toolCallId.startsWith(CHOICE_ID_PREFIX);
  if (!choice && route.sessionId) {
    return { kind: "session", sessionId: route.sessionId };
  }
  if (route.runId) {
    return {
      kind: "automation",
      runId: route.runId,
      message: {
        kind: "signal",
        name: CHOICE_ANSWER_SIGNAL,
        payload: { toolCallId: answer.toolCallId, answers: answer.answers },
      },
      // The prefix is part of the stored key of a signal: do not rename it.
      idempotencyKey: `slack-answer:${answer.toolCallId}`,
    };
  }
  return { kind: "unrouted" };
}

export async function deliverAnswer(route: AnswerRoute, answer: SourceAnswer): Promise<void> {
  const delivery = answerDelivery(route, answer);
  switch (delivery.kind) {
    case "unrouted":
      log.warn({ toolCallId: answer.toolCallId }, "answer to a question card with no address; ignored");
      return;
    case "session":
      if ((await deliverSessionAnswer(delivery.sessionId, answer)) === "no_relay") {
        log.warn(
          { toolCallId: answer.toolCallId, sessionId: delivery.sessionId },
          "answer for a session with no relay; ignored",
        );
      }
      return;
    case "automation":
      assertIdempotencyKey(delivery.idempotencyKey);
      await DBOS.send<AutomationInbox>(
        delivery.runId,
        delivery.message,
        AUTOMATION_TOPIC,
        delivery.idempotencyKey,
      );
  }
}
