/** Webhook verification strategies and sealed-secret resolution (ADR 0102). */

import { isValidSlackRequest } from "@slack/bolt";
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

export function verifyWebhook(input: VerifyWebhookInput): boolean {
  switch (input.verification.scheme) {
    case "github_hmac_sha256":
      return timingSafeHeader(
        `sha256=${sha256Signature(input.secret, input.rawBody)}`,
        input.headers.get("x-hub-signature-256"),
      );
    case "generic_hmac_sha256":
      return timingSafeHeader(
        `sha256=${sha256Signature(input.secret, input.rawBody)}`,
        input.headers.get("x-engrams-signature-256"),
      );
    case "slack_v0": {
      const timestamp = input.headers.get("x-slack-request-timestamp");
      const signature = input.headers.get("x-slack-signature");
      if (!timestamp || !/^\d+$/.test(timestamp) || !signature) return false;
      const timestampSeconds = Number(timestamp);
      if (!Number.isSafeInteger(timestampSeconds) || timestampSeconds <= 0) return false;
      let body: string;
      try {
        body = new TextDecoder("utf-8", { fatal: true }).decode(input.rawBody);
      } catch {
        return false;
      }
      try {
        // Bolt implements Slack's v0 base string, timing-safe signature check,
        // and five-minute timestamp freshness rule.
        return isValidSlackRequest({
          signingSecret: input.secret,
          body,
          headers: {
            "x-slack-signature": signature,
            "x-slack-request-timestamp": timestampSeconds,
          },
        });
      } catch {
        return false;
      }
    }
  }
}
