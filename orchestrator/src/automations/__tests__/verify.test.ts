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
const BODY = new TextEncoder().encode(JSON.stringify({ action: "opened" }));

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
  // Only the generic scheme survives ADR 0119 D5; the provider schemes
  // (github_hmac_sha256, slack_v0) are verified on the integration ingress
  // routes and can no longer be stored on a registration.
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

  test("a provider header never satisfies the generic scheme", () => {
    expect(verify("generic_hmac_sha256", {
      "x-hub-signature-256": `sha256=${digest(BODY)}`,
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
