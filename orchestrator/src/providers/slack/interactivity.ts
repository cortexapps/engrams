/** Slack's interactivity endpoint: an option click, an "Answer…" click that
 * opens the answer modal, or the modal's submission. The address of an
 * answer is in the card's value and in the modal's private metadata, so a
 * view submission, which has no channel, has one too. */

import { Hono } from "hono";
import type { ModalView } from "@slack/types";

import { readVerifiedBody } from "../../automations/integration-ingress.ts";
import {
  makeVerificationSecretResolver,
  type VerificationSecretResolver,
} from "../../automations/verify.ts";
import { deliverAnswer as defaultDeliverAnswer } from "../../chat/answers.ts";
import type { AnswerRoute } from "../../chat/chat.ts";
import { declaredIngress } from "../../connectors/registry.ts";
import { log as rootLog } from "../../log.ts";
import type { SourceAnswer } from "../../workflows/communication-policy.ts";
import { parseInteractivity } from "./blocks.ts";
import { getSlackClient, SLACK_PROVIDER } from "./client.ts";
import { verifySlackRequest } from "./events.ts";

export interface SlackInteractivityDeps {
  secrets?: VerificationSecretResolver;
  now?: () => Date;
  deliverAnswer?: (route: AnswerRoute, answer: SourceAnswer) => Promise<void>;
  /** Open the answer modal. Default = Slack `views.open`. */
  openModal?: (triggerId: string, view: ModalView) => Promise<void>;
}

async function defaultOpenModal(triggerId: string, view: ModalView): Promise<void> {
  const client = await getSlackClient();
  await client.views.open({ trigger_id: triggerId, view });
}

const log = rootLog.child({ component: "slack" });

export function makeSlackInteractivityRoute(deps: SlackInteractivityDeps = {}): Hono {
  const secrets = deps.secrets ?? makeVerificationSecretResolver();
  const declared = declaredIngress(SLACK_PROVIDER);
  if (!declared) throw new Error("the slack connector declares no webhook ingress");
  const { secretRef } = declared.ingress;
  const deliverAnswer = deps.deliverAnswer ?? defaultDeliverAnswer;
  const openModal = deps.openModal ?? defaultOpenModal;
  const now = deps.now ?? (() => new Date());
  const app = new Hono();

  app.post("/", async (c) => {
    const body = await readVerifiedBody(
      { provider: SLACK_PROVIDER, verify: verifySlackRequest },
      { secretRef, secrets },
      c.req.raw,
      now(),
    );
    if (!body.ok) return body.response;
    const { rawBody } = body;

    const payload = new URLSearchParams(new TextDecoder().decode(rawBody)).get("payload") ?? "";
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
