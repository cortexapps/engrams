import { createHmac } from "node:crypto";
import { create } from "@bufbuild/protobuf";
import { beforeEach, describe, expect, test } from "bun:test";

import type { WebhookVerificationScheme } from "../../db/schema.ts";
import {
  ResolvedCredentialSchema,
  ResolveIntegrationCredentialResponseSchema,
} from "../../gen/engram/app/v1/integration_op_pb.ts";
import type { IntegrationEventSource } from "../integration-ingress.ts";
import { githubEventSource } from "../../providers/github/events.ts";
import { gitlabEventSource } from "../../providers/gitlab/events.ts";
import { linearEventSource } from "../../providers/linear/events.ts";
import { slackEventSource } from "../../providers/slack/events.ts";
import { vantaEventSource } from "../../providers/vanta/events.ts";
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

describe("provider event source verification", () => {
  const NOW = new Date("2026-08-21T12:00:00Z");
  const seconds = String(Math.floor(NOW.getTime() / 1000));

  function ingress(
    source: IntegrationEventSource,
    headers: Record<string, string>,
    secret = SECRET,
  ): Promise<boolean> {
    return source.verify({
      headers: new Headers(headers),
      rawBody: BODY,
      now: NOW,
      secret: async () => secret,
    });
  }

  test("a secret that cannot be read rejects the verify", async () => {
    const unreadable = githubEventSource.verify({
      headers: new Headers({ "x-hub-signature-256": `sha256=${digest(BODY)}` }),
      rawBody: BODY,
      now: NOW,
      secret: async () => {
        throw new Error("not sealed");
      },
    });
    await expect(unreadable).rejects.toThrow("not sealed");
  });

  test("GitHub reads the prefixed X-Hub-Signature-256", async () => {
    const header = "x-hub-signature-256";
    expect(await ingress(githubEventSource, { [header]: `sha256=${digest(BODY)}` })).toBe(true);
    expect(await ingress(githubEventSource, { [header]: `sha256=${digest("other")}` })).toBe(false);
    expect(await ingress(githubEventSource, { [header]: "sha256=short" })).toBe(false);
    expect(await ingress(githubEventSource, { [header]: encodeURI("💥") })).toBe(false);
    expect(await ingress(githubEventSource, {})).toBe(false);
  });

  test("Linear reads the bare hex Linear-Signature", async () => {
    expect(await ingress(linearEventSource, { "linear-signature": digest(BODY) })).toBe(true);
    expect(await ingress(linearEventSource, { "linear-signature": `sha256=${digest(BODY)}` }))
      .toBe(false);
    expect(await ingress(linearEventSource, {})).toBe(false);
  });

  test("Slack signs the timestamp with the body and refuses a stale one", async () => {
    const sign = (ts: string) => ({
      "x-slack-request-timestamp": ts,
      "x-slack-signature": `v0=${digest(Buffer.concat([Buffer.from(`v0:${ts}:`), BODY]))}`,
    });
    expect(await ingress(slackEventSource, sign(seconds))).toBe(true);
    expect(await ingress(slackEventSource, sign(String(Number(seconds) - 3600)))).toBe(false);
    expect(await ingress(slackEventSource, {})).toBe(false);
  });

  test("Vanta (Svix) signs id, timestamp, and body under the decoded secret", async () => {
    const key = Buffer.from("svix-test-key");
    const secret = `whsec_${key.toString("base64")}`;
    const sign = (ts: string) => ({
      "svix-id": "msg_1",
      "svix-timestamp": ts,
      "svix-signature": `v1,${createHmac("sha256", key)
        .update(`msg_1.${ts}.`)
        .update(BODY)
        .digest("base64")}`,
    });
    expect(await ingress(vantaEventSource, sign(seconds), secret)).toBe(true);
    expect(await ingress(vantaEventSource, sign(String(Number(seconds) - 3600)), secret))
      .toBe(false);
    expect(await ingress(vantaEventSource, {}, secret)).toBe(false);
  });

  test("GitLab compares the secret token header", async () => {
    expect(await ingress(gitlabEventSource, { "x-gitlab-token": SECRET })).toBe(true);
    expect(await ingress(gitlabEventSource, { "x-gitlab-token": "other" })).toBe(false);
    expect(await ingress(gitlabEventSource, {})).toBe(false);
    expect(await ingress(gitlabEventSource, { "x-gitlab-token": "" }, "")).toBe(false);
  });

  test("GitLab with a signing token takes only a signature", async () => {
    const key = Buffer.from("gitlab-test-key");
    const secret = `whsec_${key.toString("base64")}`;
    const signed = (ts: string) => ({
      "webhook-id": "msg_1",
      "webhook-timestamp": ts,
      "webhook-signature": `v1,${createHmac("sha256", key)
        .update(`msg_1.${ts}.`)
        .update(BODY)
        .digest("base64")}`,
    });
    expect(await ingress(gitlabEventSource, signed(seconds), secret)).toBe(true);
    expect(await ingress(gitlabEventSource, signed(String(Number(seconds) - 3600)), secret))
      .toBe(false);
    // The secret in the token header does not pass in place of a signature.
    expect(await ingress(gitlabEventSource, { "x-gitlab-token": secret }, secret)).toBe(false);
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
