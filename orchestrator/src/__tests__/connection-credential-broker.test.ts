import { describe, expect, test } from "bun:test";

import type { GoogleAccessToken, WifIdentity } from "../integrations/google-wif.ts";
import { integrationSnapshotHash } from "../integrations/grants.ts";
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

const sessionGrants = [{
  connectionId: "connection-1",
  operation: "compute.instances.get",
  resourceConstraints: [],
}];

function sessionStore(): CredentialBrokerSessionStore {
  return {
    async get(sessionId) {
      if (sessionId !== "session-1") return null;
      return {
        userId: "user-1",
        principalId: "user-1",
        profileId: "profile-1",
        integrationGrants: structuredClone(sessionGrants),
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
    const exchangedSnapshots: string[] = [];
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      sessionStatus: async () => "active",
      now: () => new Date("2026-07-31T12:00:00Z"),
      issuer: "https://tenant.example/api/v1/integrations/google-cloud/oidc",
      organizationId: "tenant.example",
      exchange: async (_config, identity: WifIdentity): Promise<GoogleAccessToken> => {
        exchanges += 1;
        exchangedPrincipals.push(identity.userId);
        exchangedOrganizations.push(identity.organizationId);
        exchangedSnapshots.push(identity.profileSnapshotId);
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
    // A-claims: the organization claim is the deployment id, and the profile
    // snapshot claim is the content-hash of the immutable snapshot.
    expect(exchangedOrganizations).toEqual(["tenant.example"]);
    expect(exchangedSnapshots).toEqual([integrationSnapshotHash({
      profileId: "profile-1",
      integrationGrants: sessionGrants,
      integrationConnections: [connection],
    })]);
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
      sessionStatus: async () => "active",
      exchange: async () => { throw new Error("must not run"); },
    });
    expect((await app.request(path, post(body))).status).toBe(403);
  });

  test("rejects callers without the broker bearer", async () => {
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      sessionStatus: async () => "active",
      exchange: async () => { throw new Error("must not run"); },
    });
    expect((await app.request(path, post(body, "wrong"))).status).toBe(401);
  });

  // O6: `task_session` rows outlive the session; authorization is bounded by
  // the session's LIFETIME.
  test("refuses to mint for an ended or deleted session and evicts its cache", async () => {
    let status: string | null = "active";
    let exchanges = 0;
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      sessionStatus: async () => status,
      now: () => new Date("2026-07-31T12:00:00Z"),
      issuer: "https://tenant.example/api/v1/integrations/google-cloud/oidc",
      organizationId: "tenant.example",
      exchange: async (): Promise<GoogleAccessToken> => {
        exchanges += 1;
        return {
          accessToken: `host-token-${exchanges}`,
          expiresAt: new Date("2026-07-31T13:00:00Z"),
        };
      },
    });

    // Live: mints and caches.
    expect((await app.request(path, post(body))).status).toBe(200);
    expect(exchanges).toBe(1);

    // Ended: 403 even though a fresh cached token exists, and the cache entry
    // is evicted.
    for (const ended of ["completed", "failed", "dead", "host_lost", null]) {
      status = ended;
      expect((await app.request(path, post(body))).status).toBe(403);
    }

    // Back alive (a fresh probe result): the evicted cache forces a new
    // exchange rather than serving the ended-session token.
    status = "active";
    expect((await app.request(path, post(body))).status).toBe(200);
    expect(exchanges).toBe(2);
  });

  test("fails closed when the session status probe fails", async () => {
    let exchanges = 0;
    const app = makeConnectionCredentialBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      sessions: sessionStore(),
      sessionStatus: async () => { throw new Error("control plane unreachable"); },
      exchange: async (): Promise<GoogleAccessToken> => {
        exchanges += 1;
        return { accessToken: "must-not-mint", expiresAt: new Date("2026-07-31T13:00:00Z") };
      },
    });
    // 502 (retryable), never a mint on unverified session state.
    expect((await app.request(path, post(body))).status).toBe(502);
    expect(exchanges).toBe(0);
  });
});
