/** Slack Events API deliveries.
 *
 * Slack retries a delivery that is not acknowledged within 3 seconds; the
 * retry carries the same `event_id`, so the ledger write dedupes it.
 */

import { isValidSlackRequest } from "@slack/bolt";

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";
import type { IngressDelivery } from "../../automations/verify.ts";

/** Inner event types the ledger accepts (the declared catalog keys). */
const LEDGERED_EVENT_TYPES = new Set(["app_mention", "message", "reaction_added"]);

/** Slack's `v0` request signature, with its five-minute replay window.
 * Events and card clicks are signed the same way. */
export async function verifySlackRequest(delivery: IngressDelivery): Promise<boolean> {
  return isValidSlackRequest({
    signingSecret: await delivery.secret(),
    body: new TextDecoder().decode(delivery.rawBody),
    headers: {
      "x-slack-signature": delivery.headers.get("x-slack-signature") ?? "",
      "x-slack-request-timestamp": Number(delivery.headers.get("x-slack-request-timestamp") ?? 0),
    },
    nowMilliseconds: delivery.now.getTime(),
  });
}

export const slackEventSource: IntegrationEventSource = {
  provider: "slack",
  verify: verifySlackRequest,
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
    // A plain user message has no subtype and no bot_id; edits, bot posts,
    // and system messages do, and no trigger wants them.
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
