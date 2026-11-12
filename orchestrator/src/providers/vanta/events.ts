/** Vanta webhook deliveries, sent through Svix.
 *
 * `svix-id` is the delivery id; deliveries are at-least-once and dedupe on
 * it. Vanta names an event `v1.<domain>.<entity>.<action>` in the body's
 * `type` (or `eventType`) field; the catalog key drops the `v1.` prefix.
 */

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";

/** The catalog key for a Vanta event name: `v1.questionnaire.created` →
 * `questionnaire.created`. Null when the name is not one Vanta would send. */
export function vantaEventKey(name: unknown): string | null {
  if (typeof name !== "string") return null;
  const key = name.replace(/^v1\./, "").toLowerCase();
  return /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/.test(key) ? key : null;
}

export const vantaEventSource: IntegrationEventSource = {
  provider: "vanta",
  extract: (headers, payload) => {
    const deliveryId = headers.get("svix-id");
    if (!deliveryId) {
      return { kind: "reject", status: 400, error: "missing svix-id" };
    }
    const eventKey = vantaEventKey(payload["type"] ?? payload["eventType"]);
    if (eventKey === null) {
      return {
        kind: "skip",
        reason: `delivery without an event type (keys: ${Object.keys(payload).join(", ")})`,
      };
    }
    return { kind: "event", event: { eventKey, deliveryId } };
  },
};
