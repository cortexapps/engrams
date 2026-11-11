/** Linear webhook deliveries.
 *
 * Event keys are `<type-lowercase>.<action>` (issue.create, comment.create),
 * matching the connector catalog. Linear's docs say to reject a delivery
 * whose `webhookTimestamp` (unix ms) is more than a minute old.
 */

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";

const TIMESTAMP_SKEW_MS = 60_000;

export const linearEventSource: IntegrationEventSource = {
  provider: "linear",
  extract: (headers, payload, now) => {
    const timestamp = payload["webhookTimestamp"];
    if (typeof timestamp !== "number" || !Number.isFinite(timestamp)) {
      return { kind: "reject", status: 401, error: "missing webhookTimestamp" };
    }
    if (Math.abs(now.getTime() - timestamp) > TIMESTAMP_SKEW_MS) {
      return { kind: "reject", status: 401, error: "stale webhookTimestamp" };
    }
    // Run ids derive from the delivery id, and DBOS dedupes notifications on
    // the message id alone, so an invented id would swallow later deliveries.
    const deliveryId = headers.get("linear-delivery");
    if (!deliveryId) {
      return { kind: "reject", status: 400, error: "missing Linear-Delivery" };
    }
    const type = payload["type"];
    const action = payload["action"];
    if (typeof type !== "string" || type === "" || typeof action !== "string" || action === "") {
      return { kind: "skip", reason: "payload without type/action" };
    }
    // An issue carries its team at `data.team`. A child entity that embeds
    // the issue (a comment) carries it at `data.issue.team`. `attachment.*`
    // has only an `issueId` and `issuelabel.*` has no team, so both stay
    // unscoped and a team-scoped trigger declines them.
    const team =
      ownPath(payload, "data.team.key") ?? ownPath(payload, "data.issue.team.key");
    return {
      kind: "event",
      event: {
        eventKey: `${type.toLowerCase()}.${action}`,
        deliveryId,
        ...(typeof team === "string" ? { scopeValue: team } : {}),
      },
    };
  },
};
