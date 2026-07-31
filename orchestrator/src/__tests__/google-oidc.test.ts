import { describe, expect, test } from "bun:test";

import type { IntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import { makeGoogleOidcRoute } from "../routes/google-oidc.ts";

describe("tenant Google OIDC issuer", () => {
  test("publishes exact discovery metadata and overlapping JWKS keys", async () => {
    let ensuredActiveKey = false;
    const keys: IntegrationOidcKeyStore = {
      async getOrCreateActive(now) {
        expect(now).toEqual(new Date("2026-07-31T12:00:00Z"));
        ensuredActiveKey = true;
        return {
          kid: "new",
          privateKeyPem: "sealed",
          publicJwk: { kty: "RSA", kid: "new" },
          state: "active",
          createdAt: new Date(1),
          publishUntil: null,
        };
      },
      async rotate() { throw new Error("unused"); },
      async listPublished(now) {
        expect(now).toEqual(new Date("2026-07-31T12:00:00Z"));
        return [
          { kid: "new", publicJwk: { kty: "RSA", kid: "new" }, state: "active", createdAt: new Date(1), publishUntil: null },
          { kid: "old", publicJwk: { kty: "RSA", kid: "old" }, state: "retiring", createdAt: new Date(0), publishUntil: new Date(2) },
        ];
      },
    };
    const app = makeGoogleOidcRoute({
      keys,
      now: () => new Date("2026-07-31T12:00:00Z"),
      baseUrl: "https://tenant.example/",
    });
    const issuer = "https://tenant.example/api/v1/integrations/google-cloud/oidc";

    const discovery = await app.request(
      "/api/v1/integrations/google-cloud/oidc/.well-known/openid-configuration",
    );
    expect(await discovery.json()).toMatchObject({
      issuer,
      jwks_uri: `${issuer}/jwks`,
      id_token_signing_alg_values_supported: ["RS256"],
    });
    expect(discovery.headers.get("cache-control")).toBe("public, max-age=300");

    const jwks = await app.request("/api/v1/integrations/google-cloud/oidc/jwks");
    expect(await jwks.json()).toEqual({
      keys: [{ kty: "RSA", kid: "new" }, { kty: "RSA", kid: "old" }],
    });
    expect(ensuredActiveKey).toBe(true);
  });
});
