/** GitLab webhook deliveries. The event route ledgers each one and
 * dispatches it to automations. */

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";

/** `MergeRequest` to `merge_request`. */
function snakeCase(name: string): string {
  return name.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
}

/** The event key: the hook's `object_kind`, with the action of a
 * merge-request hook (`merge_request.open`) or the thing a comment is on
 * (`note.merge_request`). */
function eventKey(payload: Record<string, unknown>): string | null {
  const kind = payload["object_kind"];
  if (typeof kind !== "string" || kind === "") return null;
  if (kind !== "note" && kind !== "merge_request") return kind;
  const detail = ownPath(
    payload,
    kind === "note" ? "object_attributes.noteable_type" : "object_attributes.action",
  );
  return typeof detail === "string" && detail !== "" ? `${kind}.${snakeCase(detail)}` : null;
}

export const gitlabEventSource: IntegrationEventSource = {
  provider: "gitlab",
  extract: (headers, payload) => {
    const key = eventKey(payload);
    if (key === null) return { kind: "skip", reason: "no usable object_kind" };
    // The same value on every retry of one delivery. `webhook-id` is its
    // name from GitLab 19.0; `idempotency-key` from 17.4.
    const deliveryId = headers.get("webhook-id") ?? headers.get("idempotency-key");
    if (!deliveryId) return { kind: "skip", reason: "missing webhook-id and idempotency-key" };
    const project = ownPath(payload, "project.path_with_namespace");
    return {
      kind: "event",
      event: {
        eventKey: key,
        deliveryId,
        ...(typeof project === "string" ? { scopeValue: project.toLowerCase() } : {}),
      },
    };
  },
};
