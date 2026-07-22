/** Pure webhook event extraction, filtering, and persistence redaction. */

import { createHash } from "node:crypto";

import type { WebhookRegistrationRow } from "../db/automations.ts";

export const EVENT_KEY_RE = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/;
export const SYSTEM_GITHUB_REGISTRATION_ID = "github-app";
const DELIVERY_ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$/;
const SAFE_PATH_RE = /^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$/;
const UNSAFE_PATH_SEGMENTS = new Set(["__proto__", "constructor", "prototype"]);
export const WEBHOOK_REDACTION_MAX_DEPTH = 12;

export class WebhookEventError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "WebhookEventError";
  }
}

function bodyText(rawBody: Uint8Array): string {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(rawBody);
  } catch {
    throw new WebhookEventError("webhook body must be valid UTF-8 JSON");
  }
}

export function parseWebhookPayload(rawBody: Uint8Array): Record<string, unknown> {
  let value: unknown;
  try {
    value = JSON.parse(bodyText(rawBody));
  } catch (error) {
    if (error instanceof WebhookEventError) throw error;
    throw new WebhookEventError("webhook body must be a JSON object");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new WebhookEventError("webhook body must be a JSON object");
  }
  return value as Record<string, unknown>;
}

function requiredEventKey(value: string | null, source: string): string {
  if (value === null || !EVENT_KEY_RE.test(value)) {
    throw new WebhookEventError(`${source} must be a lowercase dot-delimited event key`);
  }
  return value;
}

function requiredDeliveryId(value: unknown, source: string): string {
  if (typeof value !== "string" || !DELIVERY_ID_RE.test(value)) {
    throw new WebhookEventError(`${source} must be a non-empty delivery identifier`);
  }
  return value;
}

export interface ExtractedWebhookEvent {
  eventKey: string;
  deliveryId: string;
  payload: Record<string, unknown>;
}

export function extractWebhookEvent(input: {
  registration: Pick<WebhookRegistrationRow, "verification" | "providerHint">;
  headers: Headers;
  rawBody: Uint8Array;
  payload?: Record<string, unknown>;
}): ExtractedWebhookEvent {
  const payload = input.payload ?? parseWebhookPayload(input.rawBody);
  const provider = input.registration.providerHint;
  const scheme = input.registration.verification.scheme;

  if (provider === "github" || scheme === "github_hmac_sha256") {
    const base = requiredEventKey(input.headers.get("x-github-event"), "X-GitHub-Event");
    const action = payload["action"];
    const eventKey = action === undefined
      ? base
      : `${base}.${requiredEventKey(
          typeof action === "string" ? action : null,
          "GitHub payload action",
        )}`;
    return {
      eventKey,
      deliveryId: requiredDeliveryId(
        input.headers.get("x-github-delivery"),
        "X-GitHub-Delivery",
      ),
      payload,
    };
  }

  if (provider === "slack" || scheme === "slack_v0") {
    if (payload["type"] !== "event_callback") {
      throw new WebhookEventError('Slack payload type must be "event_callback"');
    }
    const event = payload["event"];
    if (typeof event !== "object" || event === null || Array.isArray(event)) {
      throw new WebhookEventError("Slack event_callback must contain an event object");
    }
    return {
      eventKey: requiredEventKey(
        typeof (event as Record<string, unknown>)["type"] === "string"
          ? (event as Record<string, unknown>)["type"] as string
          : null,
        "Slack inner event type",
      ),
      deliveryId: requiredDeliveryId(payload["event_id"], "Slack event_id"),
      payload,
    };
  }

  const declaredDelivery = input.headers.get("x-engrams-delivery");
  return {
    eventKey: requiredEventKey(
      input.headers.get("x-engrams-event"),
      "X-Engrams-Event",
    ),
    deliveryId: declaredDelivery === null
      ? createHash("sha256").update(input.rawBody).digest("hex")
      : requiredDeliveryId(declaredDelivery, "X-Engrams-Delivery"),
    payload,
  };
}

function isSecretKey(key: string): boolean {
  const lower = key.toLowerCase();
  return ["token", "secret", "password", "authorization"].some(
    (name) => lower === name || lower.endsWith(`_${name}`) || lower.endsWith(`-${name}`)
      || key.endsWith(name[0]!.toUpperCase() + name.slice(1)),
  );
}

/**
 * Remove common credential-bearing fields before persistence. Once the depth
 * bound is reached the entire subtree is replaced, rather than left unscanned.
 */
export function redactWebhookPayload(
  value: Record<string, unknown>,
  maxDepth = WEBHOOK_REDACTION_MAX_DEPTH,
): Record<string, unknown> {
  const visit = (current: unknown, depth: number): unknown => {
    if (typeof current !== "object" || current === null) return current;
    if (depth >= maxDepth) return "[REDACTED: depth limit]";
    if (Array.isArray(current)) return current.map((item) => visit(item, depth + 1));
    // Plain (prototype-full) objects: the redacted payload is persisted via
    // Drizzle, whose entity check dereferences Object.getPrototypeOf(value)
    // and throws on null-prototype maps. Pollution-vector keys are dropped
    // outright instead, so no attacker key can ever become a prototype.
    const output: Record<string, unknown> = {};
    for (const [key, child] of Object.entries(current)) {
      if (UNSAFE_PATH_SEGMENTS.has(key)) continue;
      if (!isSecretKey(key)) output[key] = visit(child, depth + 1);
    }
    return output;
  };
  return visit(value, 0) as Record<string, unknown>;
}

function ownPath(payload: Record<string, unknown>, path: string): unknown {
  if (
    !SAFE_PATH_RE.test(path)
    || path.split(".").some((segment) => UNSAFE_PATH_SEGMENTS.has(segment))
  ) {
    return undefined;
  }
  let current: unknown = payload;
  for (const segment of path.split(".")) {
    if (
      typeof current !== "object"
      || current === null
      || !Object.prototype.hasOwnProperty.call(current, segment)
    ) {
      return undefined;
    }
    current = (current as Record<string, unknown>)[segment];
  }
  return current;
}

function jsonEqual(actual: unknown, expected: unknown): boolean {
  if (Object.is(actual, expected)) return true;
  if (Array.isArray(actual) || Array.isArray(expected)) {
    return Array.isArray(actual)
      && Array.isArray(expected)
      && actual.length === expected.length
      && actual.every((value, index) => jsonEqual(value, expected[index]));
  }
  if (
    typeof actual !== "object" || actual === null
    || typeof expected !== "object" || expected === null
  ) {
    return false;
  }
  const actualEntries = Object.entries(actual);
  const expectedEntries = Object.entries(expected);
  return actualEntries.length === expectedEntries.length
    && expectedEntries.every(([key, value]) =>
      Object.prototype.hasOwnProperty.call(actual, key)
      && jsonEqual((actual as Record<string, unknown>)[key], value));
}

/**
 * A webhook filter is an AND of dotted own-property payload paths to expected
 * JSON values. Every path must exist and match by structural JSON equality.
 */
export function matchesWebhookFilter(
  payload: Record<string, unknown>,
  filter: Record<string, unknown> | undefined,
): boolean {
  if (filter === undefined) return true;
  return Object.entries(filter).every(([path, expected]) =>
    jsonEqual(ownPath(payload, path), expected));
}
