/** Provider event webhooks.
 *
 *   POST /api/v1/integrations/:provider/events
 *
 * One route for every provider. The connector's `webhook.ingress` names the
 * secret that signs a delivery; the provider's event source verifies the
 * delivery and says what it means. The route answers after the ledger write
 * and the dispatch commit, so a crash before the answer is covered by the
 * provider's retry and the delivery id dedupes it. A git forge's delivery
 * also goes to the review's own handler.
 */

import { Hono } from "hono";

import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventSource,
} from "../automations/integration-ingress.ts";
import {
  makeVerificationSecretResolver,
  type VerificationSecretResolver,
} from "../automations/verify.ts";
import { declaredIngress } from "../connectors/registry.ts";
import type { GitForge } from "../forges/forge.ts";
import { forgeFor } from "../forges/registry.ts";
import { eventSources } from "../providers/index.ts";
import { makeForgeDeliveryHandler } from "../reviews/forge-deliveries.ts";

export interface IntegrationEventsDeps {
  sources?: IntegrationEventSource[];
  secrets?: VerificationSecretResolver;
  /** Ingress-spine seams (ledger store, trigger dispatch, connection). */
  ingress?: Omit<HandleDeliveryDeps, "now">;
  /** The one clock for verification, extraction, and the ledger. */
  now?: () => Date;
  forgeDelivery?: (forge: GitForge, eventKey: string, payload: Record<string, unknown>) => Promise<void>;
}

export function makeIntegrationEventsRoute(deps: IntegrationEventsDeps = {}): Hono {
  const secrets = deps.secrets ?? makeVerificationSecretResolver();
  const now = deps.now ?? (() => new Date());
  const ingressDeps: HandleDeliveryDeps = { ...deps.ingress, now };
  const forgeDelivery = deps.forgeDelivery ?? makeForgeDeliveryHandler();

  const sources = new Map(
    (deps.sources ?? eventSources()).map((source) => {
      const declared = declaredIngress(source.provider);
      if (!declared) {
        throw new Error(`event source ${source.provider}: its connector declares no webhook ingress`);
      }
      return [
        source.provider,
        {
          source,
          declared: {
            displayName: `${declared.displayName} (default)`,
            secretRef: declared.ingress.secretRef,
            secrets,
          },
        },
      ] as const;
    }),
  );

  const app = new Hono();

  app.post("/api/v1/integrations/:provider/events", async (c, next) => {
    const entry = sources.get(c.req.param("provider"));
    if (!entry) return next();
    const { source, declared } = entry;
    const delivery = await handleIntegrationDelivery(source, declared, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    const forge = delivery.kind === "recorded" ? forgeFor(source.provider) : null;
    if (forge && delivery.kind === "recorded") {
      await forgeDelivery(forge, delivery.event.eventKey, delivery.payload);
    }
    return c.body(null, 200);
  });

  return app;
}
