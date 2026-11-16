/** GitLab webhook deliveries. The event route ledgers each one and
 * dispatches it to automations. */

import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";
import { timingSafeHeader, verifySvixHeaders } from "../../automations/verify.ts";

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
  // GitLab has two secrets. A signing token (`whsec_...`, GitLab 19.0 and
  // later) signs the delivery as Standard Webhooks does. A secret token
  // arrives as it is in a header. The stored secret says which one the
  // administrator set, so a delivery cannot choose the weaker check.
  async verify(delivery) {
    const secret = await delivery.secret();
    if (secret.startsWith("whsec_")) return verifySvixHeaders(delivery, secret, "webhook");
    return secret !== "" && timingSafeHeader(secret, delivery.headers.get("x-gitlab-token"));
  },
  extract: (headers, payload) => {
    const key = eventKey(payload);
    if (key === null) return { kind: "skip", reason: "no usable object_kind" };
    // The same value on every retry of one delivery. `webhook-id` is its
    // name from GitLab 19.0; `idempotency-key` from 17.4. An earlier GitLab
    // sends neither, and the refusal shows in its webhook log.
    const deliveryId = headers.get("webhook-id") ?? headers.get("idempotency-key");
    if (!deliveryId) {
      return {
        kind: "reject",
        status: 400,
        error: "missing webhook-id and idempotency-key; GitLab 17.4 or later is required",
      };
    }
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
