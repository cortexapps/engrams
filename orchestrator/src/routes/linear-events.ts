/**
 * Linear webhook endpoint (ADR 0119 D5).
 *
 *   POST /api/v1/integrations/linear/events
 *
 * Linear signs each delivery with `Linear-Signature`: a bare hex HMAC-SHA256
 * of the raw body under the webhook signing secret (org secret
 * `linear.webhook_secret` — the admin seals it when creating the webhook in
 * Linear's UI). The payload carries `webhookTimestamp` (unix ms) and Linear's
 * docs say to reject deliveries more than a minute old — replay guard.
 * `Linear-Delivery` (a UUID) is the documented delivery id; a request without
 * one is malformed, and an invented key would be actively destructive
 * downstream (DBOS notifications dedupe on the message id alone), so it is
 * refused — the same reasoning as the GitHub route's X-GitHub-Delivery check.
 *
 * Event keys are `<type-lowercase>.<action>` (issue.create, issue.update,
 * comment.create), matching the connector catalog. There is no legacy path
 * below the spine — Linear ingress is new with this route.
 */

import { Hono } from "hono";

import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import { ownPath } from "../automations/paths.ts";
import {
  makeVerificationSecretResolver,
  verifyHexHmacSha256,
  type VerificationSecretResolver,
} from "../automations/verify.ts";

export const LINEAR_WEBHOOK_SECRET_REF = "linear.webhook_secret";
export const LINEAR_TIMESTAMP_SKEW_MS = 60_000;

export interface LinearEventsDeps {
  /** The webhook signing secret resolver (default = the sealed org-secret path). */
  secrets?: VerificationSecretResolver;
  /** Ingress-spine seams (ledger store, new-trigger dispatch, connection). */
  ingress?: HandleDeliveryDeps;
  now?: () => Date;
}

export function makeLinearEventsRoute(deps: LinearEventsDeps = {}): Hono {
  const secrets = deps.secrets ?? makeVerificationSecretResolver();
  const now = deps.now ?? (() => new Date());
  const ingressDeps: HandleDeliveryDeps = { ...(deps.ingress ?? {}), now };

  const ingressRoute: IntegrationEventRoute = {
    provider: "linear",
    displayName: "Linear (default)",
    verify: async (headers, rawBody) => {
      const secret = await secrets.resolve({
        provider: "linear",
        secretRef: LINEAR_WEBHOOK_SECRET_REF,
      });
      return verifyHexHmacSha256(secret, rawBody, headers.get("linear-signature"));
    },
    extract: (headers, payload) => {
      // Replay guard per Linear's docs: webhookTimestamp is unix ms at the
      // payload root; reject anything more than a minute from now.
      const timestamp = payload["webhookTimestamp"];
      if (typeof timestamp !== "number" || !Number.isFinite(timestamp)) {
        return { kind: "reject", status: 401, error: "missing webhookTimestamp" };
      }
      if (Math.abs(now().getTime() - timestamp) > LINEAR_TIMESTAMP_SKEW_MS) {
        return { kind: "reject", status: 401, error: "stale webhookTimestamp" };
      }
      const deliveryId = headers.get("linear-delivery");
      if (!deliveryId) {
        return { kind: "reject", status: 400, error: "missing Linear-Delivery" };
      }
      const type = payload["type"];
      const action = payload["action"];
      if (typeof type !== "string" || type === "" || typeof action !== "string" || action === "") {
        return { kind: "skip", reason: "payload without type/action" };
      }
      // Where the team lives depends on the entity. An issue payload carries
      // it at `data.team`; a CHILD entity (a comment, and the other
      // issue-scoped types) has no team on `data` at all and nests it under
      // the issue instead. Reading only `data.team.key` left every comment
      // delivery with NO scope, so a team-scoped trigger could never match one
      // — it is dropped by the `event.scopeValue === undefined` arm of
      // triggerMatches (dispatch.ts). Prod 2026-08-31: a CD-495 comment
      // ledgered with an empty scope_value and started no run.
      //
      // Deliberate boundary: this covers the child entities that EMBED the
      // issue (comments do). It does not cover `attachment.*`, which carries
      // only an `issueId`, nor `issuelabel.*`, which is organization-level and
      // has no team at all. Scoping an attachment would need an API lookup
      // inside the 3s ack budget; those stay unscoped, so only a SCOPED
      // trigger declines them.
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

  const app = new Hono();

  app.post("/api/v1/integrations/linear/events", async (c) => {
    const delivery = await handleIntegrationDelivery(ingressRoute, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    return c.body(null, 200);
  });

  return app;
}

export default makeLinearEventsRoute();
