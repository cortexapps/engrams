/** Webhook verification strategies and sealed-secret resolution (ADR 0102). */

import { createHmac, timingSafeEqual } from "node:crypto";

import { integrationOp as defaultIntegrationOp } from "../control-plane/client.ts";
import type { WebhookVerification } from "../db/schema.ts";
import { asBearer } from "../integrations/clients.ts";
import type { IntegrationOpClient } from "../integrations/run-op.ts";

const SECRET_TTL_MS = 5 * 60_000;
const MAX_CACHED_SECRETS = 256;
const secretCache = new Map<string, { value: string; expiresAt: number }>();

export interface VerificationSecretInput {
  provider: string;
  secretRef: string;
}

export interface VerificationSecretResolver {
  resolve(input: VerificationSecretInput): Promise<string>;
}

export function makeVerificationSecretResolver(
  client: Pick<IntegrationOpClient, "resolveIntegrationCredential"> = defaultIntegrationOp,
  now: () => number = Date.now,
): VerificationSecretResolver {
  return {
    async resolve({ provider, secretRef }) {
      const key = `${provider}\0${secretRef}`;
      const timestamp = now();
      const cached = secretCache.get(key);
      if (cached && cached.expiresAt > timestamp) return cached.value;
      secretCache.delete(key);

      const response = await client.resolveIntegrationCredential({
        provider,
        credential: {
          source: "inject",
          mintProvider: "",
          injects: [{
            header: "x-engrams-webhook-signing",
            template: "{}",
            secretRef,
          }],
        },
      });
      if (!response.credential) {
        throw new Error(`webhook verification secret ${JSON.stringify(secretRef)} is not configured`);
      }
      const value = asBearer(response.credential);
      // Keep the cache bounded even if an administrator creates many short-lived
      // registrations. Expired entries are preferred eviction candidates.
      for (const [cachedKey, entry] of secretCache) {
        if (entry.expiresAt <= timestamp) secretCache.delete(cachedKey);
      }
      if (secretCache.size >= MAX_CACHED_SECRETS) {
        const oldest = secretCache.keys().next().value;
        if (oldest !== undefined) secretCache.delete(oldest);
      }
      secretCache.set(key, { value, expiresAt: timestamp + SECRET_TTL_MS });
      return value;
    },
  };
}

/** Test seam and rotation hook: clear all cached registration secrets. */
export function resetVerificationSecretCache(): void {
  secretCache.clear();
}

export interface VerifyWebhookInput {
  verification: WebhookVerification;
  secret: string;
  headers: Headers;
  rawBody: Uint8Array;
}

function timingSafeHeader(expected: string, actual: string | null): boolean {
  if (actual === null) return false;
  const expectedBytes = Buffer.from(expected, "ascii");
  const actualBytes = Buffer.from(actual, "ascii");
  return actualBytes.length === expectedBytes.length
    && timingSafeEqual(actualBytes, expectedBytes);
}

function sha256Signature(secret: string, body: Uint8Array): string {
  return createHmac("sha256", secret).update(body).digest("hex");
}

/** Bare hex-encoded HMAC-SHA256 of the raw body (no scheme prefix) — the
 * Linear-Signature format (ADR 0119 D5). Timing-safe with a length pre-check. */
export function verifyHexHmacSha256(
  secret: string,
  rawBody: Uint8Array,
  actual: string | null,
): boolean {
  return timingSafeHeader(sha256Signature(secret, rawBody), actual);
}

export interface SvixSignatureInput {
  /** The endpoint secret as the dashboard shows it (`whsec_<base64>`); a bare
   * base64 secret is accepted too. */
  secret: string;
  rawBody: Uint8Array;
  id: string | null;
  /** Unix seconds, as the `svix-timestamp` header carries it. */
  timestamp: string | null;
  /** One or more space-separated `v1,<base64>` entries. */
  signature: string | null;
  /** Unix milliseconds. */
  now: number;
  toleranceMs: number;
}

/** The Svix webhook signature (Vanta): HMAC-SHA256 over
 * `${id}.${timestamp}.${body}` under the base64-decoded secret, base64
 * encoded, compared against every `v1,` entry of the header (a rotation
 * sends the old and the new). A timestamp outside the tolerance is a
 * replay. Timing-safe per entry. */
export function verifySvixSignature(input: SvixSignatureInput): boolean {
  if (!input.id || !input.timestamp || !input.signature) return false;
  if (!/^\d{1,12}$/.test(input.timestamp)) return false;
  const sentMs = Number(input.timestamp) * 1000;
  if (Math.abs(input.now - sentMs) > input.toleranceMs) return false;
  const encoded = input.secret.startsWith("whsec_") ? input.secret.slice("whsec_".length) : input.secret;
  const key = Buffer.from(encoded, "base64");
  if (key.length === 0) return false;
  const expected = createHmac("sha256", key)
    .update(`${input.id}.${input.timestamp}.`)
    .update(input.rawBody)
    .digest("base64");
  for (const entry of input.signature.split(" ")) {
    const [version, value] = entry.split(",", 2);
    if (version !== "v1" || value === undefined) continue;
    if (timingSafeHeader(expected, value)) return true;
  }
  return false;
}

/** Custom-registration verification: the generic scheme only. Provider
 * schemes retired with ADR 0119 D5 — GitHub/Slack/Linear deliveries arrive on
 * the integration ingress routes and verify with the connection's secret. */
export function verifyWebhook(input: VerifyWebhookInput): boolean {
  switch (input.verification.scheme) {
    case "generic_hmac_sha256":
      return timingSafeHeader(
        `sha256=${sha256Signature(input.secret, input.rawBody)}`,
        input.headers.get("x-engrams-signature-256"),
      );
  }
}
