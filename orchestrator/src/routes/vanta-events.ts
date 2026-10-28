/**
 * Vanta webhook endpoint.
 *
 *   POST /api/v1/integrations/vanta/events
 *
 * Vanta delivers webhooks through Svix. Each delivery carries `svix-id` (the
 * delivery id; deliveries are at-least-once and dedupe on it),
 * `svix-timestamp` (unix seconds), and `svix-signature` (one or more
 * space-separated `v1,<base64>` entries): an HMAC-SHA256 over
 * `${id}.${timestamp}.${rawBody}` under the endpoint's signing secret, which
 * the admin copies from Vanta's webhook dashboard into the org secret
 * `vanta.webhook_secret`. Deliveries older than five minutes are replays and
 * refused. An endpoint must answer 2xx within 15 seconds or Vanta retries.
 *
 * Vanta names an event `v1.<domain>.<entity>.<action>`; the catalog key is
 * that name without the `v1.` prefix (`questionnaire.status-changed`). The
 * name is read from the delivery body's `type` (or `eventType`) field; a
 * delivery that carries none cannot be routed and is skipped with its
 * top-level keys in the reason so the gap shows in the log.
 */

import { Hono } from "hono";

import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import {
  makeVerificationSecretResolver,
  verifySvixSignature,
  type VerificationSecretResolver,
} from "../automations/verify.ts";

export const VANTA_WEBHOOK_SECRET_REF = "vanta.webhook_secret";
export const VANTA_TIMESTAMP_SKEW_MS = 5 * 60_000;

export interface VantaEventsDeps {
  /** The webhook signing secret resolver (default = the sealed org-secret path). */
  secrets?: VerificationSecretResolver;
  /** Ingress-spine seams (ledger store, new-trigger dispatch, connection). */
  ingress?: HandleDeliveryDeps;
  now?: () => Date;
}

/** The catalog key for a Vanta event name: `v1.questionnaire.created` →
 * `questionnaire.created`. Null when the name is not one Vanta would send. */
export function vantaEventKey(name: unknown): string | null {
  if (typeof name !== "string") return null;
  const key = name.replace(/^v1\./, "").toLowerCase();
  return /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/.test(key) ? key : null;
}

export function makeVantaEventsRoute(deps: VantaEventsDeps = {}): Hono {
  const secrets = deps.secrets ?? makeVerificationSecretResolver();
  const now = deps.now ?? (() => new Date());
  const ingressDeps: HandleDeliveryDeps = { ...(deps.ingress ?? {}), now };

  const ingressRoute: IntegrationEventRoute = {
    provider: "vanta",
    displayName: "Vanta (default)",
    verify: async (headers, rawBody) => {
      const secret = await secrets.resolve({
        provider: "vanta",
        secretRef: VANTA_WEBHOOK_SECRET_REF,
      });
      return verifySvixSignature({
        secret,
        rawBody,
        id: headers.get("svix-id"),
        timestamp: headers.get("svix-timestamp"),
        signature: headers.get("svix-signature"),
        now: now().getTime(),
        toleranceMs: VANTA_TIMESTAMP_SKEW_MS,
      });
    },
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

  const app = new Hono();

  app.post("/api/v1/integrations/vanta/events", async (c) => {
    const delivery = await handleIntegrationDelivery(ingressRoute, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    return c.body(null, 200);
  });

  return app;
}

export default makeVantaEventsRoute();
