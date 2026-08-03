/** Public deployment OIDC discovery and JWKS endpoints for Google WIF (ADR 0109). */

import { Hono } from "hono";

import { config } from "../config.ts";
import {
  makeIntegrationOidcKeyStore,
  type IntegrationOidcKeyStore,
} from "../db/integration-oidc-keys.ts";

export function googleOidcIssuer(baseUrl = config.baseUrl): string {
  const issuer = new URL(baseUrl);
  issuer.pathname = "/api/v1/integrations/google-cloud/oidc";
  issuer.search = "";
  issuer.hash = "";
  return issuer.toString();
}

export function makeGoogleOidcRoute(deps: {
  keys?: IntegrationOidcKeyStore;
  now?: () => Date;
  baseUrl?: string;
} = {}): Hono {
  const app = new Hono();
  const keys = deps.keys ?? makeIntegrationOidcKeyStore();
  const now = deps.now ?? (() => new Date());
  const issuer = googleOidcIssuer(deps.baseUrl);

  app.get("/api/v1/integrations/google-cloud/oidc/.well-known/openid-configuration", (c) => {
    c.header("cache-control", "public, max-age=300");
    return c.json({
      issuer,
      jwks_uri: `${issuer}/jwks`,
      response_types_supported: ["id_token"],
      subject_types_supported: ["public"],
      id_token_signing_alg_values_supported: ["RS256"],
      claims_supported: [
        "sub",
        "aud",
        "iat",
        "exp",
        "jti",
        "engrams_organization",
        "engrams_connection",
        "engrams_user",
        "engrams_profile_snapshot",
      ],
    });
  });

  // Read-only by design: a public GET must never write. The startup key
  // rotation driver (index.ts) creates the active key, so the set is
  // non-empty before the server accepts traffic.
  app.get("/api/v1/integrations/google-cloud/oidc/jwks", async (c) => {
    const published = await keys.listPublished(now());
    c.header("cache-control", "public, max-age=300");
    return c.json({ keys: published.map((key) => key.publicJwk) });
  });

  return app;
}
