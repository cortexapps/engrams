/**
 * Slack interactivity endpoint (ADR 0060 P2.9).
 *
 *   POST /api/v1/integrations/slack/interactivity
 *
 * The single inbound surface for AskUserQuestion answers: an inline option
 * click, an "Answer…" click (→ open the answer modal), or the modal submission.
 * Verifies every request with the SDK's `isValidSlackRequest` over the raw form
 * body, classifies the `payload` field via the Block Kit contract, and:
 *   - answer    → `DBOS.send(trigger_answer)` to the thread workflow (idempotent
 *                 on tool_call_id, so a double-click delivers once);
 *   - open_modal → `views.open` with the trigger id (must be within ~3s).
 *
 * Routing never reads `payload.channel`/`payload.message` — the thread route
 * rides the Block Kit value (and the modal's private_metadata), so a
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
import { threadHash, selectThreadWorkflowId } from "../workflows/thread-workflow-id.ts";
import {
  THREAD_TOPIC,
  type ThreadInbox,
  type SourceAnswer,
  type SourceProfileChoice,
} from "../workflows/thread-inbox.ts";

/** DBOS statuses a workflow can't re-run from → that epoch is done (Invariant 4). */
const TERMINAL_WF = new Set(["SUCCESS", "ERROR", "MAX_RECOVERY_ATTEMPTS_EXCEEDED", "CANCELLED"]);

export interface SlackInteractivityDeps {
  signingSecret?: () => Promise<string>;
  /** Deliver an answer to the live thread workflow. Default = workflow-id
   *  selection + DBOS.send (idempotent on tool_call_id). */
  deliverAnswer?: (route: ThreadRoute, answer: SourceAnswer) => Promise<void>;
  /** Deliver a profile-dropdown pick. Default = workflow-id selection +
   *  DBOS.send (idempotent on the ask's nonce — first selection wins). */
  deliverProfileChoice?: (
    route: ThreadRoute,
    choice: SourceProfileChoice,
    nonce: string,
  ) => Promise<void>;
  /** Open the answer modal. Default = Slack `views.open`. */
  openModal?: (triggerId: string, view: ModalView) => Promise<void>;
}

/** Default delivery: route the answer to the thread's live (non-terminal) epoch
 *  and send it once (idempotency key = the tool_call_id). */
async function defaultDeliverAnswer(route: ThreadRoute, answer: SourceAnswer): Promise<void> {
  const workflowId = await selectThreadWorkflowId(
    threadHash(route.team, route.channel, route.threadRoot),
    async (id) => {
      const status = await DBOS.getWorkflowStatus(id);
      return status != null && TERMINAL_WF.has(status.status);
    },
  );
  await DBOS.send<ThreadInbox>(
    workflowId,
    { kind: "trigger_answer", answer },
    THREAD_TOPIC,
    `slack-answer:${answer.toolCallId}`,
  );
}

/** Default profile-choice delivery: same routing as an answer; the idempotency
 *  key is the ask's nonce, so only the FIRST selection reaches the workflow. */
async function defaultDeliverProfileChoice(
  route: ThreadRoute,
  choice: SourceProfileChoice,
  nonce: string,
): Promise<void> {
  const workflowId = await selectThreadWorkflowId(
    threadHash(route.team, route.channel, route.threadRoot),
    async (id) => {
      const status = await DBOS.getWorkflowStatus(id);
      return status != null && TERMINAL_WF.has(status.status);
    },
  );
  await DBOS.send<ThreadInbox>(
    workflowId,
    { kind: "trigger_profile_choice", choice },
    THREAD_TOPIC,
    `slack-profile:${nonce}`,
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
  const deliverProfileChoice = deps.deliverProfileChoice ?? defaultDeliverProfileChoice;
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
      case "profile_choice":
        // Only the mentioning user decides which profile serves their request.
        if (action.clicker !== action.expectedUser) {
          log.info(
            { clicker: action.clicker, expected: action.expectedUser },
            "slack: profile pick from a non-requesting user — ignored",
          );
          return c.body(null, 200);
        }
        log.info({ profileId: action.choice.profileId }, "slack: profile pick received");
        await deliverProfileChoice(action.route, action.choice, action.nonce);
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
