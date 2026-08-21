/**
 * Slack Events API endpoint (ADR 0060 P2.8; ingress spine ADR 0119 D5).
 *
 *   POST /api/v1/integrations/slack/events
 *
 * Verifies every request with the Slack SDK's standalone `isValidSlackRequest`
 * (HMAC + staleness — no hand-rolled crypto). The ingress spine answers the
 * url_verification challenge, ledgers eligible events (app_mention, plain
 * message, reaction_added), and hands them to the trigger dispatch seam. The
 * legacy thread-brain path is unchanged below it: an `app_mention` becomes an
 * idempotent `startWorkflow(SlackThreadWorkflow) + send(mention)` — acking 200
 * only after BOTH durable writes commit (Invariant 3); a crash before the ack
 * is covered by Slack's retry (the `event_id` idempotency key dedupes the
 * send, and the same event_id makes the ledger write a no-op — X-Slack-Retry-Num
 * needs no special handling). No Slack API calls here, so the handler is
 * trivially under the 3s ack budget.
 */

import { Hono } from "hono";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { isValidSlackRequest } from "@slack/bolt";

import { log as rootLog } from "../log.ts";
import { getSlackSigningSecret } from "../integrations/slack.ts";
import { classifySlackEvent } from "../integrations/slack-webhook.ts";
import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import { ownPath } from "../automations/paths.ts";
import { threadHash, selectThreadWorkflowId } from "../workflows/thread-workflow-id.ts";
import { slackThreadWorkflow } from "../workflows/slack-thread.ts";
import { THREAD_TOPIC, type ThreadInbox } from "../workflows/thread-inbox.ts";

/** DBOS statuses a workflow can't re-run from → the thread is done (Invariant 4). */
const TERMINAL_WF = new Set(["SUCCESS", "ERROR", "MAX_RECOVERY_ATTEMPTS_EXCEEDED", "CANCELLED"]);

/** Inner event types the ledger accepts (the declared catalog keys). */
const LEDGERED_EVENT_TYPES = new Set(["app_mention", "message", "reaction_added"]);

export interface SlackEventsDeps {
  /** The Slack signing secret resolver (default = the org-secret resolver). */
  signingSecret?: () => Promise<string>;
  /** Ingress-spine seams (ledger store, new-trigger dispatch, connection). */
  ingress?: HandleDeliveryDeps;
}

const log = rootLog.child({ component: "slack" });

export function makeSlackEventsRoute(deps: SlackEventsDeps = {}): Hono {
  const signingSecret = deps.signingSecret ?? getSlackSigningSecret;
  const ingressDeps: HandleDeliveryDeps = deps.ingress ?? {};

  const ingressRoute: IntegrationEventRoute = {
    provider: "slack",
    displayName: "Slack (default)",
    verify: async (headers, rawBody) =>
      isValidSlackRequest({
        signingSecret: await signingSecret(),
        body: new TextDecoder().decode(rawBody),
        headers: {
          "x-slack-signature": headers.get("x-slack-signature") ?? "",
          "x-slack-request-timestamp": Number(headers.get("x-slack-request-timestamp") ?? 0),
        },
      }),
    extract: (_headers, payload) => {
      if (payload["type"] === "url_verification") {
        const challenge = payload["challenge"];
        if (typeof challenge !== "string") {
          return { kind: "reject", status: 400, error: "url_verification without challenge" };
        }
        return {
          kind: "respond",
          response: new Response(challenge, {
            status: 200,
            headers: { "content-type": "text/plain" },
          }),
        };
      }
      if (payload["type"] !== "event_callback") {
        return { kind: "skip", reason: "not an event_callback" };
      }
      const event = payload["event"];
      if (typeof event !== "object" || event === null || Array.isArray(event)) {
        return { kind: "skip", reason: "event_callback without event object" };
      }
      const inner = event as Record<string, unknown>;
      const type = inner["type"];
      if (typeof type !== "string" || !LEDGERED_EVENT_TYPES.has(type)) {
        return { kind: "skip", reason: `inner type ${String(type)} not ledgered` };
      }
      // Edited/bot/system message subtypes are noise for triggers; the plain
      // user message has no subtype and no bot_id.
      if (type === "message" && (inner["subtype"] !== undefined || inner["bot_id"] !== undefined)) {
        return { kind: "skip", reason: "message subtype/bot" };
      }
      const deliveryId = payload["event_id"];
      if (typeof deliveryId !== "string" || deliveryId === "") {
        return { kind: "skip", reason: "missing event_id" };
      }
      const scope = ownPath(payload, "event.channel") ?? ownPath(payload, "event.item.channel");
      return {
        kind: "event",
        event: {
          eventKey: type,
          deliveryId,
          ...(typeof scope === "string" ? { scopeValue: scope } : {}),
        },
      };
    },
  };

  const app = new Hono();

  app.post("/api/v1/integrations/slack/events", async (c) => {
    const retryNum = c.req.header("x-slack-retry-num");
    if (retryNum !== undefined) {
      log.debug({ retryNum }, "slack: redelivery (event_id dedupes ledger and sends)");
    }
    const delivery = await handleIntegrationDelivery(ingressRoute, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    const rawBody = new TextDecoder().decode(delivery.rawBody);

    // Legacy thread-brain path (retires with the Slack built-in): unchanged.
    const evt = classifySlackEvent(rawBody);
    if (evt.kind === "challenge") return c.text(evt.challenge);
    if (evt.kind === "ignore") {
      log.debug("slack: non-mention event ignored");
      return c.body(null, 200);
    }

    const m = evt.mention;
    const workflowId = await selectThreadWorkflowId(
      threadHash(m.team, m.channel, m.threadRoot),
      async (id) => {
        const status = await DBOS.getWorkflowStatus(id);
        return status != null && TERMINAL_WF.has(status.status);
      },
    );
    log.info(
      { channel: m.channel, user: m.user, thread: m.threadRoot, workflowId },
      "slack: app_mention → thread workflow",
    );
    // 1st mention creates the thread workflow; later ones are a no-op start and
    // the send delivers. Ack only after both commit (Invariant 3); the event_id
    // idempotency key makes a Slack retry a no-op.
    await DBOS.startWorkflow(slackThreadWorkflow, { workflowID: workflowId })();
    await DBOS.send<ThreadInbox>(workflowId, { kind: "trigger_mention", mention: m }, THREAD_TOPIC, m.eventId);
    return c.body(null, 200);
  });

  return app;
}

export default makeSlackEventsRoute();
