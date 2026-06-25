/**
 * Slack integration (PR2): the OAuth facet parse boundary, the OAuth acquisition
 * route, and the @slack/web-api adapter on the generic SDK seam.
 */

import { expect, test, describe, beforeEach } from "bun:test";
import { create } from "@bufbuild/protobuf";

import { parseConnector, connectorRegistry, invalidateRegistry } from "../connectors/registry.ts";
import { makeIntegrationOauthRoute } from "../routes/integration-oauth.ts";
import { getSlackClient, SLACK_SIGNING_SECRET_REF } from "../integrations/slack.ts";
import { invalidateIntegrationClient } from "../integrations/clients.ts";
import {
  ResolveIntegrationCredentialResponseSchema,
  ResolvedCredentialSchema,
} from "../gen/engram/app/v1/integration_op_pb.ts";

const emptySource = { list: async () => [] };
const adminSession = async () => ({ user: { id: "u1", role: "admin" } });

// ── OAuth facet parsing (the admin-trust boundary) ──────────────────────────

describe("oauth facet parse", () => {
  const base = {
    provider: "slack",
    protocol: "http",
    credential: { source: "inject", injects: [{ header: "Authorization", template: "Bearer {}", secretRef: "slack.bot_token" }] },
    hosts: ["slack.com"],
    operations: [{ grants: ["chat:write"], match: { path: "/api/chat.*" } }],
  };
  const oauth = {
    authorizeUrl: "https://slack.com/oauth/v2/authorize",
    tokenUrl: "https://slack.com/api/oauth.v2.access",
    scopes: ["chat:write"],
    clientIdRef: "slack.client_id",
    clientSecretRef: "slack.client_secret",
    tokenSecretRef: "slack.bot_token",
    tokenResponsePath: "access_token",
  };

  test("the built-in slack seed loads with an oauth facet", () => {
    const slack = connectorRegistry().get("slack");
    expect(slack).toBeDefined();
    expect(slack?.oauth?.tokenSecretRef).toBe("slack.bot_token");
    expect(slack?.oauth?.scopes).toContain("chat:write");
  });

  test("accepts a well-formed oauth facet", () => {
    const c = parseConnector({ ...base, oauth }, "x");
    expect(c.oauth?.clientIdRef).toBe("slack.client_id");
  });

  test("rejects a non-https endpoint", () => {
    expect(() => parseConnector({ ...base, oauth: { ...oauth, tokenUrl: "http://slack.com/x" } }, "x")).toThrow(/https/);
  });

  test("rejects an endpoint host outside the connector's hosts (anti-exfil)", () => {
    expect(() =>
      parseConnector({ ...base, oauth: { ...oauth, tokenUrl: "https://evil.example.com/x" } }, "x"),
    ).toThrow(/must be one of the connector's hosts/);
  });

  test("rejects a missing secret ref", () => {
    const { clientSecretRef: _omit, ...partial } = oauth;
    expect(() => parseConnector({ ...base, oauth: partial }, "x")).toThrow(/clientSecretRef/);
  });

  test("the built-in slack seed declares the signing-secret ref (ADR 0060 triggers)", () => {
    // The webhook verifier (getSlackSigningSecret) reads this ref; the connect UI
    // seals it. Both MUST agree on the name — guard against drift here.
    const slack = connectorRegistry().get("slack");
    expect(slack?.oauth?.signingSecretRef).toBe(SLACK_SIGNING_SECRET_REF);
  });

  test("parses an optional signingSecretRef on the oauth facet", () => {
    const c = parseConnector({ ...base, oauth: { ...oauth, signingSecretRef: "slack.signing_secret" } }, "x");
    expect(c.oauth?.signingSecretRef).toBe("slack.signing_secret");
  });

  test("leaves signingSecretRef undefined when the facet omits it (optional)", () => {
    const c = parseConnector({ ...base, oauth }, "x");
    expect(c.oauth?.signingSecretRef).toBeUndefined();
  });
});

// ── The OAuth acquisition route ─────────────────────────────────────────────

function fakeOauthClient() {
  const calls: { begin: unknown[]; complete: unknown[] } = { begin: [], complete: [] };
  const client = {
    beginIntegrationOauth: async (req: unknown) => {
      calls.begin.push(req);
      return { authorizeUrl: "https://slack.com/oauth/v2/authorize?client_id=c&state=s1" };
    },
    completeIntegrationOauth: async (req: unknown) => {
      calls.complete.push(req);
      return { ok: true, message: "Connected slack" };
    },
  };
  return { client, calls };
}

describe("integration oauth route", () => {
  beforeEach(() => invalidateRegistry());

  test("authorize: admin → 302 to the IdP authorize URL", async () => {
    const { client, calls } = fakeOauthClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      integrationOp: client as never,
      getSession: adminSession,
      randomState: () => "s1",
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/authorize");
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toContain("slack.com/oauth/v2/authorize");
    expect(calls.begin).toHaveLength(1);
  });

  test("authorize: non-admin → 403", async () => {
    const { client } = fakeOauthClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      integrationOp: client as never,
      getSession: async () => ({ user: { id: "u2", role: "user" } }),
      randomState: () => "s1",
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/authorize");
    expect(res.status).toBe(403);
  });

  test("callback: valid state → exchanges + 302 connected", async () => {
    const { client, calls } = fakeOauthClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      integrationOp: client as never,
      getSession: adminSession,
      randomState: () => "s1",
    });
    // Register the state via authorize first.
    await app.request("/api/v1/integrations/slack/oauth/authorize");
    const res = await app.request("/api/v1/integrations/slack/oauth/callback?code=abc&state=s1");
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toBe("/settings/integrations/slack?connected=1");
    expect(calls.complete).toHaveLength(1);
  });

  test("callback: unknown state → 403, no exchange", async () => {
    const { client, calls } = fakeOauthClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      integrationOp: client as never,
      getSession: adminSession,
      randomState: () => "s1",
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/callback?code=abc&state=forged");
    expect(res.status).toBe(403);
    expect(calls.complete).toHaveLength(0);
  });
});

// ── The @slack/web-api adapter (Mode B) ─────────────────────────────────────

describe("slack SDK adapter", () => {
  beforeEach(() => {
    invalidateRegistry();
    invalidateIntegrationClient("slack");
  });

  test("getSlackClient resolves the bot token and builds a WebClient", async () => {
    const credential = create(ResolvedCredentialSchema, {
      cred: { case: "bearer", value: { token: "xoxb-TEST" } },
    });
    const integrationOp = {
      runIntegrationOp: async () => {
        throw new Error("unused");
      },
      resolveIntegrationCredential: async () =>
        create(ResolveIntegrationCredentialResponseSchema, { credential }),
    };
    const client = await getSlackClient({ connectors: emptySource, integrationOp: integrationOp as never });
    expect(client.token).toBe("xoxb-TEST");
    expect(typeof client.chat.postMessage).toBe("function");
  });
});
