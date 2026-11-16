/** The ingress spine every provider event delivery passes through: bounded
 * body → verify → parse → extract → redact → ledger → dispatch.
 *
 * The provider's event source verifies the delivery and says what it means.
 * The spine owns the ledger write and the dispatch hand-off.
 * `setIntegrationEventDispatch` installs the trigger dispatcher at boot.
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
import type { IngressDelivery, VerificationSecretResolver } from "./verify.ts";
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

/** A provider's part of the event route: whether a delivery is the
 * provider's, and what a verified one means. */
export interface IntegrationEventSource {
  provider: string;
  /** Whether the provider signed the delivery. It rejects when the signing
   * secret cannot be read. */
  verify(delivery: IngressDelivery): Promise<boolean>;
  extract(headers: Headers, payload: Record<string, unknown>, now: Date): IngressOutcome;
}

/** What the connector declares about a provider's deliveries. */
export interface DeclaredDelivery {
  /** Display name for the auto-provisioned default connection. */
  displayName: string;
  /** The org secret that signs the deliveries. */
  secretRef: string;
  secrets: VerificationSecretResolver;
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

/** Until the dispatcher is installed a verified delivery is only ledgered. */
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
  /** Verified but not ledgered. */
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
       * the dispatcher is installed at boot). */
      dispatch: DispatchIntegrationResult | undefined;
    };

function jsonResponse(status: number, error: string): Response {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** The raw body of a request that `source` verifies; else the refusal. The
 * body is bounded. A signing secret that cannot be read (not sealed yet, or
 * the resolver is down) is a 401 and not a 5xx: a 5xx makes the provider
 * retry an endpoint that is not set up to receive it. */
export async function readVerifiedBody(
  source: Pick<IntegrationEventSource, "provider" | "verify">,
  declared: Pick<DeclaredDelivery, "secretRef" | "secrets">,
  request: Request,
  now: Date,
): Promise<{ ok: true; rawBody: Uint8Array } | { ok: false; response: Response }> {
  let rawBody: Uint8Array;
  try {
    rawBody = await readBoundedBody(request);
  } catch (error) {
    if (error instanceof BodyTooLargeError) {
      return { ok: false, response: new Response(null, { status: 413 }) };
    }
    throw error;
  }
  let verified: boolean;
  try {
    verified = await source.verify({
      headers: request.headers,
      rawBody,
      now,
      secret: () =>
        declared.secrets.resolve({ provider: source.provider, secretRef: declared.secretRef }),
    });
  } catch (error) {
    log.warn(
      { provider: source.provider, error },
      "integration delivery could not be verified; rejecting it",
    );
    return { ok: false, response: jsonResponse(401, "signature not verifiable") };
  }
  if (!verified) return { ok: false, response: jsonResponse(401, "invalid signature") };
  return { ok: true, rawBody };
}

export async function handleIntegrationDelivery(
  source: IntegrationEventSource,
  declared: DeclaredDelivery,
  request: Request,
  deps: HandleDeliveryDeps = {},
): Promise<DeliveryResult> {
  const now = deps.now ?? (() => new Date());
  const body = await readVerifiedBody(source, declared, request, now());
  if (!body.ok) return { kind: "rejected", response: body.response };
  const { rawBody } = body;

  let payload: Record<string, unknown>;
  try {
    payload = parseWebhookPayload(rawBody);
  } catch (error) {
    if (!(error instanceof WebhookEventError)) throw error;
    // Signed but unparseable: the provider decides what a non-JSON body means.
    return { kind: "skipped", reason: `unparseable payload: ${error.message}`, rawBody };
  }

  const outcome = source.extract(request.headers, payload, now());
  if (outcome.kind === "respond") return { kind: "responded", response: outcome.response };
  if (outcome.kind === "reject") {
    return { kind: "rejected", response: jsonResponse(outcome.status, outcome.error) };
  }
  if (outcome.kind === "skip") {
    return { kind: "skipped", reason: outcome.reason, rawBody, payload };
  }

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

  const connectionId = await connectionIdFor(source.provider, declared.displayName);
  const redacted = redactWebhookPayload(payload);
  const receivedAt = now();
  const { recorded } = await store.record({
    provider: source.provider,
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
      provider: source.provider,
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
      { provider: source.provider, eventKey: outcome.event.eventKey, error },
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
