/** GitHub App webhook credential resolution and signature verification. */

import { createHmac, timingSafeEqual } from "node:crypto";

import { integrationOp as defaultIntegrationOp } from "../control-plane/client.ts";
import { asBearer } from "./clients.ts";
import type { RunOpDeps } from "./run-op.ts";

export const GITHUB_PROVIDER = "github";
export const GITHUB_WEBHOOK_SECRET_REF = "github.webhook_secret";

const SIGNING_TTL_MS = 5 * 60_000;
let signingCache: { value: string; expiresAt: number } | undefined;

export async function getGithubWebhookSecret(deps?: RunOpDeps): Promise<string> {
  const now = Date.now();
  if (signingCache && signingCache.expiresAt > now) return signingCache.value;
  const client = deps?.integrationOp ?? defaultIntegrationOp;
  const resp = await client.resolveIntegrationCredential({
    provider: GITHUB_PROVIDER,
    credential: {
      source: "inject",
      mintProvider: "",
      injects: [{
        header: "x-github-webhook-signing",
        template: "{}",
        secretRef: GITHUB_WEBHOOK_SECRET_REF,
      }],
    },
  });
  if (!resp.credential) {
    throw new Error("github.webhook_secret is not configured");
  }
  const value = asBearer(resp.credential);
  signingCache = { value, expiresAt: now + SIGNING_TTL_MS };
  return value;
}

/** Test seam: drop the cached webhook secret. */
export function __resetGithubWebhookSecretCache(): void {
  signingCache = undefined;
}

/** GitHub's documented X-Hub-Signature-256 HMAC verification scheme. */
export function verifyGithubSignature(
  secret: string,
  rawBody: string | Uint8Array,
  signatureHeader: string | undefined,
): boolean {
  if (signatureHeader == null) return false;
  const expected = Buffer.from(
    `sha256=${createHmac("sha256", secret).update(rawBody).digest("hex")}`,
  );
  const actual = Buffer.from(signatureHeader);
  if (actual.length !== expected.length) return false;
  return timingSafeEqual(actual, expected);
}
