/** Dynamically registered, signature-verified automation webhook ingress. */

import { Hono } from "hono";

import { dispatchWebhookOccurrence, type DispatchWebhookInput } from "../automations/dispatch.ts";
import { makeVerificationSecretResolver, verifyWebhook, type VerificationSecretResolver } from "../automations/verify.ts";
import { extractWebhookEvent, parseWebhookPayload, redactWebhookPayload, WebhookEventError } from "../automations/webhook.ts";
import { makeAutomationStore, type AutomationStore } from "../db/automations.ts";
import { BodyTooLargeError, readBoundedBody } from "../http/bounded-body.ts";

const REGISTRATION_ID_RE = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

export interface HooksRouteDeps {
  store?: Pick<AutomationStore, "getRegistration">;
  secretResolver?: VerificationSecretResolver;
  dispatch?: (input: DispatchWebhookInput) => Promise<unknown>;
  now?: () => Date;
}

export function makeHooksRoute(deps: HooksRouteDeps = {}): Hono {
  let resolvedStore = deps.store;
  const store = () => (resolvedStore ??= makeAutomationStore());
  const secretResolver = deps.secretResolver ?? makeVerificationSecretResolver();
  const dispatch = deps.dispatch ?? dispatchWebhookOccurrence;
  const now = deps.now ?? (() => new Date());
  const app = new Hono();

  app.post("/api/v1/hooks/:registrationId", async (c) => {
    const registrationId = c.req.param("registrationId");
    if (!REGISTRATION_ID_RE.test(registrationId)) {
      return c.json({ error: "webhook registration not found" }, 404);
    }
    const registration = await store().getRegistration(registrationId);
    if (!registration) return c.json({ error: "webhook registration not found" }, 404);

    let rawBody: Uint8Array;
    try {
      rawBody = await readBoundedBody(c.req.raw);
    } catch (error) {
      if (error instanceof BodyTooLargeError) return c.body(null, 413);
      throw error;
    }

    const secret = await secretResolver.resolve({
      provider: registration.providerHint ?? "webhook",
      secretRef: registration.verification.secretRef,
    });
    if (!verifyWebhook({
      verification: registration.verification,
      secret,
      headers: c.req.raw.headers,
      rawBody,
    })) {
      return c.json({ error: "invalid signature" }, 401);
    }

    let occurrence;
    try {
      const payload = parseWebhookPayload(rawBody);
      occurrence = extractWebhookEvent({
        registration,
        headers: c.req.raw.headers,
        rawBody,
        payload,
      });
    } catch (error) {
      if (error instanceof WebhookEventError) {
        return c.json({ error: error.message }, 400);
      }
      throw error;
    }

    const receivedAt = now();
    await dispatch({
      registrationId,
      registration,
      eventKey: occurrence.eventKey,
      deliveryId: occurrence.deliveryId,
      payload: redactWebhookPayload(occurrence.payload),
      receivedAt,
    });
    return c.body(null, 200);
  });

  return app;
}

export default makeHooksRoute();
