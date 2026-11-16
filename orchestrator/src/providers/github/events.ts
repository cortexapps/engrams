/** GitHub App webhook deliveries. The event route ledgers each one and
 * dispatches it to automations. */

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";
import { verifyHmacSha256Header } from "../../automations/verify.ts";

export const githubEventSource: IntegrationEventSource = {
  provider: "github",
  verify: (delivery) => verifyHmacSha256Header(delivery, "x-hub-signature-256", "sha256="),
  extract: (headers, payload) => {
    const base = headers.get("x-github-event");
    if (!base) return { kind: "skip", reason: "missing x-github-event" };
    const action = payload["action"];
    if (action !== undefined && (typeof action !== "string" || action === "")) {
      return { kind: "skip", reason: "non-string payload action" };
    }
    // Run ids derive from the delivery id, and DBOS dedupes notifications on
    // the message id alone, so an invented id would swallow later deliveries.
    const deliveryId = headers.get("x-github-delivery");
    if (!deliveryId) return { kind: "reject", status: 400, error: "missing x-github-delivery" };
    const repo = ownPath(payload, "repository.full_name");
    return {
      kind: "event",
      event: {
        eventKey: action === undefined ? base : `${base}.${action}`,
        deliveryId,
        ...(typeof repo === "string" ? { scopeValue: repo.toLowerCase() } : {}),
      },
    };
  },
};
