import { createHmac } from "node:crypto";
import { create } from "@bufbuild/protobuf";
import { beforeEach, describe, expect, test } from "bun:test";

import type { WebhookVerificationScheme } from "../../db/schema.ts";
import {
  ResolvedCredentialSchema,
  ResolveIntegrationCredentialResponseSchema,
} from "../../gen/engram/app/v1/integration_op_pb.ts";
import {
  makeVerificationSecretResolver,
  resetVerificationSecretCache,
  verifyWebhook,
} from "../verify.ts";

const SECRET = "verification-secret";
const BODY_TEXT = JSON.stringify({ action: "opened" });
const BODY = new TextEncoder().encode(BODY_TEXT);

function verify(scheme: WebhookVerificationScheme, headers: Record<string, string>): boolean {
  return verifyWebhook({
    verification: { scheme, secretRef: "webhook.example.secret" },
    secret: SECRET,
    headers: new Headers(headers),
    rawBody: BODY,
  });
}

function digest(value: string | Uint8Array): string {
  return createHmac("sha256", SECRET).update(value).digest("hex");
}

describe("webhook verification strategies", () => {
  test("github_hmac_sha256 accepts a valid signature", () => {
    expect(verify("github_hmac_sha256", {
      "x-hub-signature-256": `sha256=${digest(BODY)}`,
    })).toBe(true);
  });

  test("github_hmac_sha256 rejects invalid and missing signatures", () => {
    expect(verify("github_hmac_sha256", { "x-hub-signature-256": "sha256=bad" })).toBe(false);
    expect(verify("github_hmac_sha256", {})).toBe(false);
  });

  test("generic_hmac_sha256 accepts a valid signature", () => {
    expect(verify("generic_hmac_sha256", {
      "x-engrams-signature-256": `sha256=${digest(BODY)}`,
    })).toBe(true);
  });

  test("generic_hmac_sha256 rejects invalid and missing signatures", () => {
    expect(verify("generic_hmac_sha256", {
      "x-engrams-signature-256": `sha256=${"0".repeat(64)}`,
    })).toBe(false);
    expect(verify("generic_hmac_sha256", {})).toBe(false);
  });

  test("slack_v0 accepts a valid fresh signature", () => {
    const timestamp = String(Math.floor(Date.now() / 1_000));
    expect(verify("slack_v0", {
      "x-slack-request-timestamp": timestamp,
      "x-slack-signature": `v0=${digest(`v0:${timestamp}:${BODY_TEXT}`)}`,
    })).toBe(true);
  });

  test("slack_v0 rejects invalid and missing signatures", () => {
    const timestamp = String(Math.floor(Date.now() / 1_000));
    expect(verify("slack_v0", {
      "x-slack-request-timestamp": timestamp,
      "x-slack-signature": `v0=${"0".repeat(64)}`,
    })).toBe(false);
    expect(verify("slack_v0", { "x-slack-request-timestamp": timestamp })).toBe(false);
  });

  test("slack_v0 rejects a correctly signed stale timestamp", () => {
    const timestamp = String(Math.floor(Date.now() / 1_000) - 301);
    expect(verify("slack_v0", {
      "x-slack-request-timestamp": timestamp,
      "x-slack-signature": `v0=${digest(`v0:${timestamp}:${BODY_TEXT}`)}`,
    })).toBe(false);
  });
});

describe("webhook verification secret resolution", () => {
  beforeEach(() => resetVerificationSecretCache());

  test("uses the integration credential seam and refreshes after five minutes", async () => {
    let calls = 0;
    let now = 1_000;
    const resolver = makeVerificationSecretResolver({
      async resolveIntegrationCredential(request) {
        calls++;
        expect(request.provider).toBe("webhook");
        expect(request.credential?.injects?.[0]?.secretRef).toBe("webhook.my-hook.secret");
        return create(ResolveIntegrationCredentialResponseSchema, {
          credential: create(ResolvedCredentialSchema, {
            cred: { case: "bearer", value: { token: `secret-${calls}` } },
          }),
        });
      },
    }, () => now);

    const input = { provider: "webhook", secretRef: "webhook.my-hook.secret" };
    expect(await resolver.resolve(input)).toBe("secret-1");
    expect(await resolver.resolve(input)).toBe("secret-1");
    expect(calls).toBe(1);
    now += 5 * 60_000;
    expect(await resolver.resolve(input)).toBe("secret-2");
    expect(calls).toBe(2);
  });
});
