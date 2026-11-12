/** Provider event webhooks.
 *
 *   POST /api/v1/integrations/:provider/events
 *
 * One route for every provider. The connector's `webhook.ingress` declares
 * how a delivery is signed; the provider's event source says what a verified
 * delivery means. The route answers after the ledger write and the dispatch
 * commit, so a crash before the answer is covered by the provider's retry
 * and the delivery id dedupes it.
 */

import { Hono } from "hono";

import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventSource,
} from "../automations/integration-ingress.ts";
import {
  makeVerificationSecretResolver,
  verifyIngress,
  type VerificationSecretResolver,
} from "../automations/verify.ts";
import { declaredIngress } from "../connectors/registry.ts";
import { githubEventSource } from "../providers/github/events.ts";
import { gitlabEventSource } from "../providers/gitlab/events.ts";
import { linearEventSource } from "../providers/linear/events.ts";
import { slackEventSource } from "../providers/slack/events.ts";
import { vantaEventSource } from "../providers/vanta/events.ts";

export interface IntegrationEventsDeps {
  sources?: IntegrationEventSource[];
  secrets?: VerificationSecretResolver;
  /** Ingress-spine seams (ledger store, trigger dispatch, connection). */
  ingress?: Omit<HandleDeliveryDeps, "now">;
  /** The one clock for verification, extraction, and the ledger. */
  now?: () => Date;
}

/** One source per connector that declares a webhook ingress. */
export function defaultEventSources(): IntegrationEventSource[] {
  return [githubEventSource(), gitlabEventSource, linearEventSource, slackEventSource, vantaEventSource];
}

export function makeIntegrationEventsRoute(deps: IntegrationEventsDeps = {}): Hono {
  const secrets = deps.secrets ?? makeVerificationSecretResolver();
  const now = deps.now ?? (() => new Date());
  const ingressDeps: HandleDeliveryDeps = { ...deps.ingress, now };

  const sources = new Map(
    (deps.sources ?? defaultEventSources()).map((source) => {
      const declared = declaredIngress(source.provider);
      if (!declared) {
        throw new Error(`event source ${source.provider}: its connector declares no webhook ingress`);
      }
      return [source.provider, { source, ...declared }] as const;
    }),
  );

  const app = new Hono();

  app.post("/api/v1/integrations/:provider/events", async (c, next) => {
    const entry = sources.get(c.req.param("provider"));
    if (!entry) return next();
    const { source, displayName, ingress } = entry;
    const headers = c.req.raw.headers;

    const delivery = await handleIntegrationDelivery(
      {
        provider: source.provider,
        displayName: `${displayName} (default)`,
        verify: (h, rawBody) =>
          verifyIngress({ provider: source.provider, ingress, headers: h, rawBody, secrets, now: now() }),
        extract: (h, payload) => source.extract(h, payload, now()),
      },
      c.req.raw,
      ingressDeps,
    );
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    return (await source.afterDelivery?.(delivery, headers)) ?? c.body(null, 200);
  });

  return app;
}

export default makeIntegrationEventsRoute();
