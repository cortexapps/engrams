import { describe, expect, test } from "bun:test";
import { generateKeyPairSync } from "node:crypto";

import type { IntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import {
  assertGoogleCloudConfig,
  googleOidcAudience,
  makeGoogleWifBroker,
} from "../integrations/google-wif.ts";

const PROVIDER =
  "//iam.googleapis.com/projects/123456/locations/global/" +
  "workloadIdentityPools/engrams/providers/engrams-oidc";

function signingKeys(): IntegrationOidcKeyStore {
  const pair = generateKeyPairSync("rsa", { modulusLength: 2048 });
  return {
    async getOrCreateActive() {
      return {
        kid: "key-1",
        publicJwk: pair.publicKey.export({ format: "jwk" }),
        privateKeyPem: pair.privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
        state: "active",
        createdAt: new Date(0),
        publishUntil: null,
      };
    },
    async listPublished() { return []; },
    async rotate() { throw new Error("unused"); },
  };
}

function decodePart(token: string, index: number): Record<string, unknown> {
  return JSON.parse(Buffer.from(token.split(".")[index]!, "base64url").toString("utf8"));
}

describe("Google WIF broker", () => {
  test("mints immutable, exact-audience claims with injected time and entropy", async () => {
    const broker = makeGoogleWifBroker({
      keys: signingKeys(),
      issuer: "https://tenant.example/oidc",
      now: () => new Date("2026-07-31T12:00:00.000Z"),
      randomId: () => "jti-1",
    });
    const token = await broker.mintSubjectToken({
      workloadIdentityProvider: PROVIDER,
      serviceAccountEmail: "engram-reader@customer.iam.gserviceaccount.com",
      endpoints: ["compute.googleapis.com"],
    }, {
      sessionId: "11111111-1111-4111-8111-111111111111",
      organizationId: "tenant-1",
      connectionId: "connection-1",
      userId: "user-1",
      profileSnapshotId: "profile-1:session-1",
    });

    expect(decodePart(token, 0)).toMatchObject({ alg: "RS256", typ: "JWT", kid: "key-1" });
    expect(decodePart(token, 1)).toEqual({
      iss: "https://tenant.example/oidc",
      sub: "11111111-1111-4111-8111-111111111111",
      aud: PROVIDER,
      iat: 1785499200,
      nbf: 1785499195,
      exp: 1785499500,
      jti: "jti-1",
      engrams_organization: "tenant-1",
      engrams_connection: "connection-1",
      engrams_user: "user-1",
      engrams_profile_snapshot: "profile-1:session-1",
    });
  });

  test("exchanges only through fixed Google STS and IAM Credentials endpoints", async () => {
    const calls: Array<{ url: string; init?: RequestInit }> = [];
    const broker = makeGoogleWifBroker({
      keys: signingKeys(),
      issuer: "https://tenant.example/oidc",
      now: () => new Date("2026-07-31T12:00:00.000Z"),
      randomId: () => "jti-1",
      fetch: (async (input, init) => {
        calls.push({ url: String(input), init });
        if (calls.length === 1) {
          return Response.json({ access_token: "federated-token" });
        }
        return Response.json({
          accessToken: "service-account-token",
          expireTime: "2026-07-31T12:05:00.000Z",
        });
      }) as typeof fetch,
    });
    const token = await broker.exchange({
      workloadIdentityProvider: PROVIDER,
      serviceAccountEmail: "engram-reader@customer.iam.gserviceaccount.com",
      endpoints: ["compute.googleapis.com"],
    }, {
      sessionId: "session-1",
      organizationId: "tenant-1",
      connectionId: "connection-1",
      userId: "user-1",
      profileSnapshotId: "profile-snapshot-1",
    });

    expect(calls.map((call) => call.url)).toEqual([
      "https://sts.googleapis.com/v1/token",
      "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/" +
        "engram-reader%40customer.iam.gserviceaccount.com:generateAccessToken",
    ]);
    const stsBody = new URLSearchParams(String(calls[0]!.init?.body));
    expect(stsBody.get("audience")).toBe(PROVIDER);
    expect(stsBody.get("subject_token_type")).toBe("urn:ietf:params:oauth:token-type:jwt");
    expect(new Headers(calls[1]!.init?.headers).get("authorization")).toBe("Bearer federated-token");
    expect(token).toEqual({
      accessToken: "service-account-token",
      expiresAt: new Date("2026-07-31T12:05:00.000Z"),
    });
  });

  test("accepts only non-secret WIF configuration", () => {
    expect(() => assertGoogleCloudConfig({
      workloadIdentityProvider: PROVIDER,
      serviceAccountEmail: "engram-reader@customer.iam.gserviceaccount.com",
      endpoints: ["compute.googleapis.com"],
      private_key: "must-not-be-accepted",
    })).toThrow(/unsupported fields/);
    expect(googleOidcAudience(PROVIDER)).toBe(PROVIDER);
  });
});
