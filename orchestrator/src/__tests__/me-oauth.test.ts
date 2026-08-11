import { beforeEach, describe, expect, test } from "bun:test";
import { makeMeRoute, type OAuthCredentialClient } from "../routes/me.ts";
import type { UserSecretStore } from "../db/user-secrets.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import { invalidateRegistry } from "../connectors/registry.ts";

const secrets: UserSecretStore = {
  put: async () => {},
  getAll: async () => ({}),
  get: async () => null,
  has: async () => false,
  delete: async () => {},
};

// ADR 0115: a custom connector declaring user-scoped support, so the /me
// connector-credential surface has something to serve.
const acmeConnector = {
  provider: "acme",
  protocol: "http",
  credential: {
    source: "inject",
    injects: [{ header: "Authorization", secretRef: "acme.token", template: "Bearer {}" }],
  },
  hosts: ["api.acme.test"],
  operations: [{ grants: ["issues:write"], match: { method: "POST", path: "/issues*" } }],
  userCredential: { token: { hint: "Create a PAT." } },
};

function appWith(oauth: OAuthCredentialClient) {
  return makeMeRoute({
    secrets,
    oauth,
    getSession: async () => ({ user: { id: "user-42", role: "user" } }),
    connectors: { list: async () => [{ provider: "acme", config: acmeConnector }] },
    harnessCatalog: {
      listHarnesses: async () => ({
        harnesses: [{
          name: "codex",
          descriptor: {
            label: "Codex",
            auth: { userOauth: { provider: "openai-codex" } },
          },
        }],
      }),
    },
  });
}

describe("/me OAuth credentials", () => {
  beforeEach(() => invalidateRegistry());

  test("uses only the authenticated user's subject for list and begin", async () => {
    const subjects: Array<{ kind: OauthSubjectKind; id: string }> = [];
    const oauth: OAuthCredentialClient = {
      listCredentials: async ({ subject }) => {
        subjects.push(subject);
        return { credentials: [] };
      },
      beginFlow: async ({ subject, provider }) => {
        subjects.push(subject);
        return {
          flow: {
            id: "flow-1",
            provider,
            status: "pending",
            expiresAt: "2026-08-01T00:00:00Z",
          },
          verificationUrl: "https://auth.openai.test/device",
          userCode: "ABCD-EFGH",
        };
      },
      getFlow: async () => ({ flow: undefined }),
      cancelFlow: async () => ({ flow: undefined }),
      disconnect: async () => {},
      putCredential: async () => {},
    };
    const app = appWith(oauth);

    const listed = await app.request("/api/v1/me/credentials");
    expect(listed.status).toBe(200);
    expect(await listed.json()).toMatchObject({
      credentials: [
        { kind: "oauth", provider: "openai-codex", connected: false },
        {
          kind: "connector",
          provider: "acme",
          modes: { oauth: false, token: true },
          tokenHint: "Create a PAT.",
          connected: false,
        },
      ],
    });
    const begun = await app.request(
      "/api/v1/me/credentials/openai-codex/connect",
      { method: "POST" },
    );
    expect(begun.status).toBe(200);
    expect(await begun.json()).toMatchObject({ userCode: "ABCD-EFGH" });
    // The harness list/begin use the `user` subject; the connector list uses
    // `user_connector` — the two planes never share a subject.
    expect(subjects).toEqual([
      { kind: OauthSubjectKind.USER, id: "user-42" },
      { kind: OauthSubjectKind.USER_CONNECTOR, id: "user-42" },
      { kind: OauthSubjectKind.USER, id: "user-42" },
    ]);
  });

  test("rejects providers that no harness declares", async () => {
    const oauth: OAuthCredentialClient = {
      listCredentials: async () => ({ credentials: [] }),
      beginFlow: async () => {
        throw new Error("must not be called");
      },
      getFlow: async () => ({ flow: undefined }),
      cancelFlow: async () => ({ flow: undefined }),
      disconnect: async () => {},
      putCredential: async () => {},
    };
    const response = await appWith(oauth).request(
      "/api/v1/me/credentials/other/connect",
      { method: "POST" },
    );
    expect(response.status).toBe(404);
  });
});

describe("/me connector credentials (ADR 0115)", () => {
  beforeEach(() => invalidateRegistry());

  function fakeOauth() {
    const calls: {
      put: Array<{ subject: { kind: OauthSubjectKind; id: string }; provider: string; secret: string }>;
      disconnect: Array<{ subject: { kind: OauthSubjectKind; id: string }; provider: string }>;
    } = { put: [], disconnect: [] };
    const oauth: OAuthCredentialClient = {
      listCredentials: async ({ subject }) => ({
        credentials:
          subject.kind === OauthSubjectKind.USER_CONNECTOR
            ? [{
                provider: "acme",
                version: 3n,
                connected: true,
                status: "connected",
                createdAt: "2026-08-01T00:00:00Z",
                updatedAt: "2026-08-01T00:00:00Z",
              }]
            : [],
      }),
      beginFlow: async () => {
        throw new Error("unused");
      },
      getFlow: async () => ({ flow: undefined }),
      cancelFlow: async () => ({ flow: undefined }),
      disconnect: async (req) => void calls.disconnect.push(req),
      putCredential: async (req) => void calls.put.push(req),
    };
    return { oauth, calls };
  }

  test("PUT seals a PAT under the user_connector subject", async () => {
    const { oauth, calls } = fakeOauth();
    const response = await appWith(oauth).request("/api/v1/me/connector-credentials/acme", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ value: "  pat-1  " }),
    });
    expect(response.status).toBe(204);
    expect(calls.put).toEqual([
      {
        subject: { kind: OauthSubjectKind.USER_CONNECTOR, id: "user-42" },
        provider: "acme",
        secret: "pat-1",
      },
    ]);
  });

  test("PUT 404s for a provider without token mode and 400s an empty value", async () => {
    const { oauth, calls } = fakeOauth();
    const app = appWith(oauth);
    const unknown = await app.request("/api/v1/me/connector-credentials/github", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ value: "pat" }),
    });
    expect(unknown.status).toBe(404);
    const empty = await app.request("/api/v1/me/connector-credentials/acme", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ value: "   " }),
    });
    expect(empty.status).toBe(400);
    expect(calls.put).toEqual([]);
  });

  test("DELETE disconnects with the listed version", async () => {
    const { oauth, calls } = fakeOauth();
    const response = await appWith(oauth).request("/api/v1/me/connector-credentials/acme", {
      method: "DELETE",
    });
    expect(response.status).toBe(204);
    expect(calls.disconnect).toMatchObject([
      { subject: { kind: OauthSubjectKind.USER_CONNECTOR, id: "user-42" }, provider: "acme" },
    ]);
  });

  test("the credentials list joins the connector row (status + account ride along)", async () => {
    const { oauth } = fakeOauth();
    const listed = await appWith(oauth).request("/api/v1/me/credentials");
    const body = (await listed.json()) as { credentials: Array<Record<string, unknown>> };
    const connector = body.credentials.find((entry) => entry.kind === "connector");
    expect(connector).toMatchObject({
      provider: "acme",
      connected: true,
      status: "connected",
      version: 3,
    });
  });
});
