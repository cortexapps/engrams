import { describe, expect, test } from "bun:test";

import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  makeGoogleTokenBrokerRoute,
  type GoogleBrokerSessionStore,
} from "../routes/google-token-broker.ts";

function connectionStore(row: IntegrationConnectionRow): IntegrationConnectionStore {
  return {
    async list() { return [row]; },
    async get(id) { return id === row.id ? row : null; },
    async create() { return row; },
    async update() { return row; },
    async delete() { return true; },
    async markTested() { return row; },
    async setEnabled(_id, enabled) { row.enabled = enabled; return row; },
    async ensureLegacy() {},
  };
}

function sessionStore(operation = "compute.instances.get"): GoogleBrokerSessionStore {
  return {
    async get(sessionId) {
      if (sessionId !== "session-1") return null;
      return {
        userId: "user-1",
        principalId: "user-1",
        profileId: "profile-1",
        integrationGrants: [{
          connectionId: "connection-1",
          operation,
          resourceConstraints: [],
        }],
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

const body = {
  sessionId: "session-1",
  connectionId: "connection-1",
  operation: "compute.instances.get",
  target: "compute.googleapis.com",
};

describe("Google host token broker", () => {
  test("checks the session grant, connection state, and exact endpoint on every request", async () => {
    const row: IntegrationConnectionRow = {
      id: "connection-1",
      alias: "prod-readonly",
      provider: "gcp",
      displayName: "Production read only",
      config: {
        workloadIdentityProvider: "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
        endpoints: ["compute.googleapis.com"],
      },
      enabled: true,
      testedAt: new Date(0),
      createdAt: new Date(0),
      updatedAt: new Date(0),
    };
    let exchanges = 0;
    const exchangedPrincipals: string[] = [];
    const app = makeGoogleTokenBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      connections: connectionStore(row),
      sessions: sessionStore(),
      now: () => new Date("2026-07-31T12:00:00Z"),
      exchange: async (_config, identity) => {
        exchanges += 1;
        exchangedPrincipals.push(identity.userId);
        return { accessToken: `host-token-${exchanges}`, expiresAt: new Date("2026-07-31T12:05:00Z") };
      },
    });
    const path = "/internal/v1/integrations/google-cloud/token";

    const first = await app.request(path, post(body));
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({
      accessToken: "host-token-1",
      expiresAt: "2026-07-31T12:05:00.000Z",
    });

    row.enabled = false;
    expect((await app.request(path, post(body))).status).toBe(403);
    row.enabled = true;
    row.updatedAt = new Date(1);
    expect((await app.request(path, post(body))).status).toBe(200);
    expect(exchanges).toBe(2);
    expect(exchangedPrincipals).toEqual(["user-1", "user-1"]);

    expect((await app.request(path, post({ ...body, target: "logging.googleapis.com" }))).status)
      .toBe(403);
    expect((await app.request(path, post({ ...body, operation: "compute.instances.stop" }))).status)
      .toBe(403);
  });

  test("rejects callers without the host control-plane bearer", async () => {
    const app = makeGoogleTokenBrokerRoute({
      db: {} as never,
      bearer: "broker-secret",
      connections: connectionStore({
        id: "connection-1", alias: "prod", provider: "gcp", displayName: "Prod",
        config: {}, enabled: true, testedAt: new Date(0), createdAt: new Date(0), updatedAt: new Date(0),
      }),
      sessions: sessionStore(),
      exchange: async () => { throw new Error("must not run"); },
    });
    const response = await app.request(
      "/internal/v1/integrations/google-cloud/token",
      post(body, "wrong"),
    );
    expect(response.status).toBe(401);
  });
});
