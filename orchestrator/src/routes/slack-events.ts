/**
 * Slack Events API endpoint (ADR 0060 P2.8; ingress spine ADR 0119 D5).
 *
 *   POST /api/v1/integrations/slack/events
 *
 * Verifies every request with the Slack SDK's standalone `isValidSlackRequest`
 * (HMAC + staleness — no hand-rolled crypto). The ingress spine answers the
 * url_verification challenge, ledgers eligible events (app_mention, plain
 * message, reaction_added), and hands them to the trigger dispatch seam — the
 * Slack thread built-in (and any automation on a Slack trigger) takes it from
 * there. The route acks 200 once the ledger write and the dispatch commit
 * (Invariant 3); a crash before the ack is covered by Slack's retry, whose
 * event_id makes the ledger write a no-op — X-Slack-Retry-Num needs no
 * special handling. No Slack API calls here, so the handler is trivially
 * under the 3 s ack budget.
 */

import { Hono } from "hono";
import { isValidSlackRequest } from "@slack/bolt";

import { log as rootLog } from "../log.ts";
import { getSlackSigningSecret } from "../integrations/slack.ts";
import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import { ownPath } from "../automations/paths.ts";

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
      log.debug({ retryNum }, "slack: redelivery (event_id dedupes the ledger)");
    }
    const delivery = await handleIntegrationDelivery(ingressRoute, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    return c.body(null, 200);
  });

  return app;
}

export default makeSlackEventsRoute();
