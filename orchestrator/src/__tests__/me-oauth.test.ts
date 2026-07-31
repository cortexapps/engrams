import { describe, expect, test } from "bun:test";
import { makeMeRoute, type OAuthCredentialClient } from "../routes/me.ts";
import type { UserSecretStore } from "../db/user-secrets.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";

const secrets: UserSecretStore = {
  put: async () => {},
  getAll: async () => ({}),
  get: async () => null,
  has: async () => false,
  delete: async () => {},
};

function appWith(oauth: OAuthCredentialClient) {
  return makeMeRoute({
    secrets,
    oauth,
    getSession: async () => ({ user: { id: "user-42", role: "user" } }),
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
    };
    const app = appWith(oauth);

    const listed = await app.request("/api/v1/me/credentials");
    expect(listed.status).toBe(200);
    expect(await listed.json()).toMatchObject({
      credentials: [{ kind: "oauth", provider: "openai-codex", connected: false }],
    });
    const begun = await app.request(
      "/api/v1/me/credentials/openai-codex/connect",
      { method: "POST" },
    );
    expect(begun.status).toBe(200);
    expect(await begun.json()).toMatchObject({ userCode: "ABCD-EFGH" });
    expect(subjects).toEqual([
      { kind: OauthSubjectKind.USER, id: "user-42" },
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
    };
    const response = await appWith(oauth).request(
      "/api/v1/me/credentials/other/connect",
      { method: "POST" },
    );
    expect(response.status).toBe(404);
  });
});
