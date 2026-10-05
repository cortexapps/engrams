/**
 * Slack interactivity endpoint (ADR 0060 P2.9).
 *
 *   POST /api/v1/integrations/slack/interactivity
 *
 * The single inbound surface for AskUserQuestion answers: an inline option
 * click, an "Answer…" click (→ open the answer modal), or the modal submission.
 * Verifies every request with the SDK's `isValidSlackRequest` over the raw form
 * body, classifies the `payload` field via the Block Kit contract, and:
 *   - answer    → a `slack_answer` signal on the automation run whose relay
 *                 posted the question (idempotent on tool_call_id, so a
 *                 double-click delivers once);
 *   - open_modal → `views.open` with the trigger id (must be within ~3s).
 *
 * Routing never reads `payload.channel`/`payload.message` — the run id rides
 * the Block Kit value (and the modal's private_metadata), so a
 * view_submission (which has no channel) routes too. The DBOS/Slack
 * side-effects are injected so the handler is unit-testable without the engine.
 */

import { Hono } from "hono";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { isValidSlackRequest } from "@slack/bolt";
import type { ModalView } from "@slack/types";

import { log as rootLog } from "../log.ts";
import { getSlackClient, getSlackSigningSecret } from "../integrations/slack.ts";
import { parseInteractivity, type ThreadRoute } from "../integrations/slack-blocks.ts";
import type { SourceAnswer } from "../workflows/communication-policy.ts";
import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import { SLACK_ANSWER_SIGNAL } from "../automations/engine/blocks/relay.ts";

export interface SlackInteractivityDeps {
  signingSecret?: () => Promise<string>;
  /** Deliver an answer to the run whose relay posted the question. Default =
   *  DBOS.send of a `slack_answer` signal (idempotent on tool_call_id). */
  deliverAnswer?: (route: ThreadRoute, answer: SourceAnswer) => Promise<void>;
  /** Open the answer modal. Default = Slack `views.open`. */
  openModal?: (triggerId: string, view: ModalView) => Promise<void>;
}

/** Which mailbox an answer belongs to. A question posted by an automation
 *  run's relay carries `runId` in its Block Kit route; its answer is a
 *  `slack_answer` signal on that run. A card without one predates the engine
 *  (the thread workflow retired in ADR 0119 phase 4.8): nothing can take its
 *  answer. Pure, so the split is testable without DBOS. */
export function answerDelivery(
  route: ThreadRoute,
  answer: SourceAnswer,
):
  | { kind: "automation"; runId: string; message: AutomationInbox; idempotencyKey: string }
  | { kind: "unrouted" } {
  if (route.runId) {
    return {
      kind: "automation",
      runId: route.runId,
      message: {
        kind: "signal",
        name: SLACK_ANSWER_SIGNAL,
        payload: { toolCallId: answer.toolCallId, answers: answer.answers },
      },
      idempotencyKey: `slack-answer:${answer.toolCallId}`,
    };
  }
  return { kind: "unrouted" };
}

/** Default delivery (see `answerDelivery`). */
async function defaultDeliverAnswer(route: ThreadRoute, answer: SourceAnswer): Promise<void> {
  const delivery = answerDelivery(route, answer);
  if (delivery.kind === "unrouted") {
    log.warn(
      { toolCallId: answer.toolCallId, channel: route.channel, thread: route.threadRoot },
      "slack: answer to a question card with no run — ignored",
    );
    return;
  }
  assertIdempotencyKey(delivery.idempotencyKey);
  await DBOS.send<AutomationInbox>(
    delivery.runId,
    delivery.message,
    AUTOMATION_TOPIC,
    delivery.idempotencyKey,
  );
}

async function defaultOpenModal(triggerId: string, view: ModalView): Promise<void> {
  const client = await getSlackClient();
  await client.views.open({ trigger_id: triggerId, view });
}

const log = rootLog.child({ component: "slack" });

export function makeSlackInteractivityRoute(deps: SlackInteractivityDeps = {}): Hono {
  const signingSecret = deps.signingSecret ?? getSlackSigningSecret;
  const deliverAnswer = deps.deliverAnswer ?? defaultDeliverAnswer;
  const openModal = deps.openModal ?? defaultOpenModal;
  const app = new Hono();

  app.post("/api/v1/integrations/slack/interactivity", async (c) => {
    const rawBody = await c.req.text();
    const valid = isValidSlackRequest({
      signingSecret: await signingSecret(),
      body: rawBody,
      headers: {
        "x-slack-signature": c.req.header("x-slack-signature") ?? "",
        "x-slack-request-timestamp": Number(c.req.header("x-slack-request-timestamp") ?? 0),
      },
    });
    if (!valid) return c.json({ error: "invalid signature" }, 401);

    const payload = new URLSearchParams(rawBody).get("payload") ?? "";
    const action = parseInteractivity(payload);
    switch (action.kind) {
      case "answer":
        log.info({ toolCallId: action.answer.toolCallId }, "slack: answer received");
        await deliverAnswer(action.route, action.answer);
        return c.body(null, 200);
      case "open_modal":
        log.info("slack: opening answer modal");
        await openModal(action.triggerId, action.view);
        return c.body(null, 200);
      case "ignore":
        return c.body(null, 200);
    }
  });

  return app;
}

export default makeSlackInteractivityRoute();
