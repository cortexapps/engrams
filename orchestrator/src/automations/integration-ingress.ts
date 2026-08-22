/** Per-provider integration-event ingress spine (ADR 0119 D5).
 *
 * One shared pipeline for every provider event route: bounded body → verify →
 * parse → extract → redact → ledger → dispatch. Verification lives with the
 * integration (each route supplies its provider's verifier and secret); the
 * spine owns the ledger write and the dispatch hand-off. Dispatch to
 * automations is an injectable seam that stays a no-op until trigger matching
 * lands (stack item 2.C) — `setIntegrationEventDispatch` installs it.
 *
 * The spine never swallows a provider's legacy behavior: routes receive the
 * raw body and parsed payload back and keep running their existing paths
 * (PR-review classifier, Slack thread brain) unchanged.
 */

import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { getDb } from "../db/client.ts";
import {
  makeIntegrationEventStore,
  type IntegrationEventStore,
} from "../db/integration-events.ts";
import { BodyTooLargeError, readBoundedBody } from "../http/bounded-body.ts";
import type { DispatchIntegrationResult } from "./dispatch.ts";
import { log as rootLog } from "../log.ts";
import { parseWebhookPayload, redactWebhookPayload, WebhookEventError } from "./webhook.ts";

const log = rootLog.child({ component: "integration-ingress" });

export interface ExtractedIntegrationEvent {
  eventKey: string;
  deliveryId: string;
  scopeValue?: string;
}

/** What a provider's extract step decided about a verified delivery. */
export type IngressOutcome =
  | { kind: "event"; event: ExtractedIntegrationEvent }
  /** Terminal provider handshake (e.g. Slack url_verification). */
  | { kind: "respond"; response: Response }
  /** Signed but not automation-eligible (unknown inner type, bot subtype…). */
  | { kind: "skip"; reason: string }
  /** Malformed in a way the provider contract forbids (e.g. a missing
   * delivery id) — the route should refuse it. */
  | { kind: "reject"; status: number; error: string };

export interface IntegrationEventRoute {
  provider: string;
  /** Display name for the auto-provisioned default connection. */
  displayName: string;
  verify(headers: Headers, rawBody: Uint8Array): Promise<boolean>;
  extract(headers: Headers, payload: Record<string, unknown>): IngressOutcome;
}

export interface IntegrationEventDispatchInput {
  provider: string;
  connectionId: string;
  eventKey: string;
  deliveryId: string;
  scopeValue?: string;
  /** Redacted. */
  payload: Record<string, unknown>;
  receivedAt: Date;
}

export type IntegrationEventDispatch = (
  input: IntegrationEventDispatchInput,
) => Promise<DispatchIntegrationResult | undefined>;

/** The 2.C seam: trigger matching installs itself here at boot. Until then a
 * verified delivery is ledgered and goes nowhere else. */
let integrationEventDispatch: IntegrationEventDispatch = async () => undefined;

export function setIntegrationEventDispatch(dispatch: IntegrationEventDispatch): void {
  integrationEventDispatch = dispatch;
}

export function getIntegrationEventDispatch(): IntegrationEventDispatch {
  return integrationEventDispatch;
}

export interface HandleDeliveryDeps {
  store?: IntegrationEventStore;
  dispatch?: IntegrationEventDispatch;
  connectionIdFor?: (provider: string, displayName: string) => Promise<string>;
  now?: () => Date;
}

export type DeliveryResult =
  /** 413/401/400-class terminal answers — return the response as-is. */
  | { kind: "rejected"; response: Response }
  /** Provider handshake answered — return the response as-is. */
  | { kind: "responded"; response: Response }
  /** Verified but not ledgered; the route continues its legacy paths. */
  | { kind: "skipped"; reason: string; rawBody: Uint8Array; payload?: Record<string, unknown> }
  /** Verified, redacted, ledgered, dispatched. */
  | {
      kind: "recorded";
      /** False when the delivery unique already held a row (provider retry). */
      recorded: boolean;
      rawBody: Uint8Array;
      payload: Record<string, unknown>;
      event: ExtractedIntegrationEvent;
      connectionId: string;
      /** What the trigger dispatcher did with the delivery (undefined until
       * the dispatcher is installed at boot). A legacy route decides its
       * own fallback from THIS — the dispatcher's fresh read — so the two
       * brains can never disagree on who owns the delivery. */
      dispatch: DispatchIntegrationResult | undefined;
    };

function jsonResponse(status: number, error: string): Response {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export async function handleIntegrationDelivery(
  route: IntegrationEventRoute,
  request: Request,
  deps: HandleDeliveryDeps = {},
): Promise<DeliveryResult> {
  let rawBody: Uint8Array;
  try {
    rawBody = await readBoundedBody(request);
  } catch (error) {
    if (error instanceof BodyTooLargeError) {
      return { kind: "rejected", response: new Response(null, { status: 413 }) };
    }
    throw error;
  }

  if (!(await route.verify(request.headers, rawBody))) {
    return { kind: "rejected", response: jsonResponse(401, "invalid signature") };
  }

  let payload: Record<string, unknown>;
  try {
    payload = parseWebhookPayload(rawBody);
  } catch (error) {
    if (!(error instanceof WebhookEventError)) throw error;
    // Signed but unparseable: tolerate, exactly like the legacy GitHub path —
    // the route's own classifier decides what a non-JSON body means.
    return { kind: "skipped", reason: `unparseable payload: ${error.message}`, rawBody };
  }

  const outcome = route.extract(request.headers, payload);
  if (outcome.kind === "respond") return { kind: "responded", response: outcome.response };
  if (outcome.kind === "reject") {
    return { kind: "rejected", response: jsonResponse(outcome.status, outcome.error) };
  }
  if (outcome.kind === "skip") {
    return { kind: "skipped", reason: outcome.reason, rawBody, payload };
  }

  const now = deps.now ?? (() => new Date());
  const store = deps.store ?? makeIntegrationEventStore();
  const connectionIdFor =
    deps.connectionIdFor ??
    (async (provider: string, displayName: string) => {
      const connection = await makeIntegrationConnectionStore(getDb()).ensureDefault(
        provider,
        displayName,
      );
      return connection.id;
    });

  const connectionId = await connectionIdFor(route.provider, route.displayName);
  const redacted = redactWebhookPayload(payload);
  const receivedAt = now();
  const { recorded } = await store.record({
    provider: route.provider,
    connectionId,
    eventKey: outcome.event.eventKey,
    deliveryId: outcome.event.deliveryId,
    payload: redacted,
    ...(outcome.event.scopeValue !== undefined ? { scopeValue: outcome.event.scopeValue } : {}),
    receivedAt,
  });

  const dispatch = deps.dispatch ?? integrationEventDispatch;
  let dispatched: DispatchIntegrationResult | undefined;
  try {
    dispatched = await dispatch({
      provider: route.provider,
      connectionId,
      eventKey: outcome.event.eventKey,
      deliveryId: outcome.event.deliveryId,
      ...(outcome.event.scopeValue !== undefined ? { scopeValue: outcome.event.scopeValue } : {}),
      payload: redacted,
      receivedAt,
    });
  } catch (error) {
    // A dispatch fault must FAIL the delivery, not be swallowed behind a 200:
    // the provider only redelivers on a non-2xx, and nothing else re-drives
    // a ledgered row. The retry is safe — the ledger row is delivery-unique
    // and every run id is derived from the delivery id, so targets that
    // already admitted dedupe on the replay.
    log.error(
      { provider: route.provider, eventKey: outcome.event.eventKey, error },
      "integration event dispatch failed; failing the delivery so the provider retries",
    );
    return {
      kind: "rejected",
      response: jsonResponse(500, "dispatch failed; retry"),
    };
  }

  return {
    kind: "recorded",
    recorded,
    rawBody,
    payload,
    event: outcome.event,
    connectionId,
    dispatch: dispatched,
  };
}
