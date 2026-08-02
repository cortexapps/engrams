import { describe, expect, test } from "bun:test";

import type { GoogleAccessToken, WifIdentity } from "../integrations/google-wif.ts";
import {
  makeConnectionCredentialBrokerRoute,
  type CredentialBrokerSessionStore,
} from "../routes/connection-credential-broker.ts";

const connection = {
  id: "connection-1",
  alias: "prod-readonly",
  provider: "gcp",
  displayName: "Production read only",
  config: {
    workloadIdentityProvider:
      "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
    serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
    endpoints: ["compute.googleapis.com"],
  },
};

function sessionStore(): CredentialBrokerSessionStore {
  return {
    async get(sessionId) {
      if (sessionId !== "session-1") return null;
      return {
        userId: "user-1",
        principalId: "user-1",
        profileId: "profile-1",
        integrationGrants: [{
          connectionId: "connection-1",
          operation: "compute.instances.get",
          resourceConstraints: [],
        }],
        integrationConnections: [structuredClone(connection)],
      };
    },
  };
}

function post(body: Record<string, string>, bearer = "broker-secret"): RequestInit {
  return {
    method: "POST",
    headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
    body: JSON.stringify(body),
  };
}

const body = { sessionId: "session-1", connectionId: "connection-1" };
const path = "/internal/v1/integrations/credentials/mint";

describe("named-connection credential broker", () => {
  test("mints from the immutable session snapshot and caches by session and connection", async () => {
    let exchanges = 0;
    const exchangedPrincipals: string[] = [];
    const exchangedOrganizations: string[] = [];
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      now: () => new Date("2026-07-31T12:00:00Z"),
      issuer: "https://tenant.example/api/v1/integrations/google-cloud/oidc",
      exchange: async (_config, identity: WifIdentity): Promise<GoogleAccessToken> => {
        exchanges += 1;
        exchangedPrincipals.push(identity.userId);
        exchangedOrganizations.push(identity.organizationId);
        return {
          accessToken: `host-token-${exchanges}`,
          expiresAt: new Date("2026-07-31T12:05:00Z"),
        };
      },
    });

    const first = await app.request(path, post(body));
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({
      kind: "bearer",
      token: "host-token-1",
      expiresAt: "2026-07-31T12:05:00.000Z",
    });
    expect((await app.request(path, post(body))).status).toBe(200);
    expect(exchanges).toBe(1);
    expect(exchangedPrincipals).toEqual(["user-1"]);
    expect(exchangedOrganizations).toEqual([
      "https://tenant.example/api/v1/integrations/google-cloud/oidc",
    ]);
  });

  test("rejects a connection that is absent from either session snapshot", async () => {
    const sessions = sessionStore();
    const originalGet = sessions.get.bind(sessions);
    sessions.get = async (sessionId) => {
      const session = await originalGet(sessionId);
      if (session) session.integrationConnections = [];
      return session;
    };
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions,
      exchange: async () => { throw new Error("must not run"); },
    });
    expect((await app.request(path, post(body))).status).toBe(403);
  });

  test("rejects callers without the host control-plane bearer", async () => {
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      exchange: async () => { throw new Error("must not run"); },
    });
    expect((await app.request(path, post(body, "wrong"))).status).toBe(401);
  });
});
