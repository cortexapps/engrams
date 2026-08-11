/**
 * Slack integration: the OAuth facet parse boundary (ADR 0106 addendum:
 * OAuth-only connectors, sealed-store tokens), the OAuth acquisition route
 * over the coordinator's redirect-flow RPCs, and the @slack/web-api adapter
 * on the generic SDK seam.
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
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import type { BeginRedirectFlowRequest, CompleteRedirectFlowRequest } from "../gen/engram/app/v1/oauth_pb.ts";
import { ConnectError, Code } from "@connectrpc/connect";

const emptySource = { list: async () => [] };
const adminSession = async () => ({ user: { id: "u1", role: "admin" } });

// ── OAuth facet parsing (the admin-trust boundary) ──────────────────────────

describe("oauth facet parse", () => {
  const base = {
    provider: "slack",
    protocol: "http",
    // OAuth-only: the single inject carries NO secretRef — the value is the
    // store-resolved access token.
    credential: { source: "inject", injects: [{ header: "Authorization", template: "Bearer {}" }] },
    hosts: ["slack.com"],
    operations: [{ grants: ["chat:write"], match: { path: "/api/chat.*" } }],
  };
  const oauth = {
    authorizeUrl: "https://slack.com/oauth/v2/authorize",
    tokenUrl: "https://slack.com/api/oauth.v2.access",
    scopes: ["chat:write"],
    clientIdRef: "slack.client_id",
    clientSecretRef: "slack.client_secret",
  };

  test("the built-in slack seed is OAuth-only with sealed-store metadata mapping", () => {
    const slack = connectorRegistry().get("slack");
    expect(slack).toBeDefined();
    expect(slack?.oauth?.scopes).toContain("chat:write");
    // Tokens live in the credential store: no secretRef on the inject, and
    // account identity maps declaratively off the token response.
    expect(slack?.credential.source).toBe("inject");
    if (slack?.credential.source === "inject") {
      expect(slack.credential.injects[0]?.secretRef).toBeUndefined();
    }
    expect(slack?.oauth?.metadata?.fromTokenResponse?.accountId).toBe("team.id");
    expect(slack?.oauth?.metadata?.fromTokenResponse?.workspaceName).toBe("team.name");
  });

  test("accepts a well-formed oauth facet", () => {
    const c = parseConnector({ ...base, oauth }, "x");
    expect(c.oauth?.clientIdRef).toBe("slack.client_id");
  });

  test("rejects a non-https endpoint", () => {
    expect(() => parseConnector({ ...base, oauth: { ...oauth, tokenUrl: "http://slack.com/x" } }, "x")).toThrow(/https/);
  });

  test("rejects a token endpoint outside the connector's hosts (anti-exfil)", () => {
    expect(() =>
      parseConnector({ ...base, oauth: { ...oauth, tokenUrl: "https://evil.example.com/x" } }, "x"),
    ).toThrow(/must be one of/);
  });

  test("acquisitionHosts admit the authorize URL only — never the token URL", () => {
    const c = parseConnector(
      {
        ...base,
        oauth: {
          ...oauth,
          acquisitionHosts: ["auth.slack.com"],
          authorizeUrl: "https://auth.slack.com/authorize",
        },
      },
      "x",
    );
    expect(c.oauth?.acquisitionHosts).toEqual(["auth.slack.com"]);
    expect(() =>
      parseConnector(
        {
          ...base,
          oauth: {
            ...oauth,
            acquisitionHosts: ["auth.slack.com"],
            tokenUrl: "https://auth.slack.com/token",
          },
        },
        "x",
      ),
    ).toThrow(/must be one of/);
  });

  test("rejects the retired tokenSecretRef/tokenResponsePath fields", () => {
    expect(() =>
      parseConnector({ ...base, oauth: { ...oauth, tokenSecretRef: "slack.bot_token" } }, "x"),
    ).toThrow(/retired/);
    expect(() =>
      parseConnector({ ...base, oauth: { ...oauth, tokenResponsePath: "access_token" } }, "x"),
    ).toThrow(/retired/);
  });

  test("an oauth connector's inject must not carry a secretRef (OAuth-only)", () => {
    const withRef = {
      ...base,
      credential: {
        source: "inject",
        injects: [{ header: "Authorization", template: "Bearer {}", secretRef: "slack.bot_token" }],
      },
    };
    expect(() => parseConnector({ ...withRef, oauth }, "x")).toThrow(/must not carry/);
  });

  test("a connector WITHOUT an oauth facet still requires secretRef", () => {
    const { credential: _omit, ...rest } = base;
    expect(() =>
      parseConnector(
        {
          ...rest,
          credential: { source: "inject", injects: [{ header: "Authorization" }] },
        },
        "x",
      ),
    ).toThrow(/"secretRef" is required/);
  });

  test("rejects reserved extraAuthorizeParams and oversized maps", () => {
    expect(() =>
      parseConnector(
        { ...base, oauth: { ...oauth, extraAuthorizeParams: { redirect_uri: "https://evil" } } },
        "x",
      ),
    ).toThrow(/reserved/);
    const c = parseConnector(
      { ...base, oauth: { ...oauth, extraAuthorizeParams: { actor: "app" } } },
      "x",
    );
    expect(c.oauth?.extraAuthorizeParams).toEqual({ actor: "app" });
  });

  test("metadata dot-paths are bounded and field-checked", () => {
    expect(() =>
      parseConnector(
        { ...base, oauth: { ...oauth, metadata: { fromTokenResponse: { nonsense: "team.id" } } } },
        "x",
      ),
    ).toThrow(/not a metadata field/);
    expect(() =>
      parseConnector(
        { ...base, oauth: { ...oauth, metadata: { fromTokenResponse: { accountId: "__proto__.x" } } } },
        "x",
      ),
    ).toThrow(/not allowed/);
    const c = parseConnector(
      {
        ...base,
        oauth: {
          ...oauth,
          metadata: {
            probe: { method: "GET", path: "/api/auth.test", map: { accountId: "team_id" } },
          },
        },
      },
      "x",
    );
    expect(c.oauth?.metadata?.probe?.map.accountId).toBe("team_id");
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

// ── The OAuth acquisition route (durable-flow CSRF, coordinator-side) ───────

function fakeRedirectClient(opts?: {
  completeError?: ConnectError;
  lookupError?: ConnectError;
  flowSubject?: { kind: OauthSubjectKind; id: string };
}) {
  const calls: {
    begin: BeginRedirectFlowRequest[];
    complete: CompleteRedirectFlowRequest[];
    lookup: Array<{ flowId: string }>;
  } = {
    begin: [],
    complete: [],
    lookup: [],
  };
  const client = {
    beginRedirectFlow: async (req: BeginRedirectFlowRequest) => {
      calls.begin.push(req);
      return {
        flow: { id: "10600000-0000-4000-8000-0000000000aa" },
        authorizeUrl:
          "https://slack.com/oauth/v2/authorize?client_id=c&state=10600000-0000-4000-8000-0000000000aa",
      };
    },
    completeRedirectFlow: async (req: CompleteRedirectFlowRequest) => {
      calls.complete.push(req);
      if (opts?.completeError) throw opts.completeError;
      return { credential: { provider: "slack", connected: true } };
    },
    // ADR 0115: the callback resolves the flow's subject before completing.
    lookupRedirectFlow: async (req: { flowId: string }) => {
      calls.lookup.push(req);
      if (opts?.lookupError) throw opts.lookupError;
      return {
        subject: opts?.flowSubject ?? {
          kind: OauthSubjectKind.CONNECTOR,
          id: "conn-slack-default",
        },
        provider: "slack",
        status: "pending",
      };
    },
  };
  return { client, calls };
}

const connectionIdFor = async () => "conn-slack-default";

describe("integration oauth route", () => {
  beforeEach(() => invalidateRegistry());

  test("authorize: admin → 302 to the coordinator-assembled authorize URL", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/authorize");
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toContain("slack.com/oauth/v2/authorize");
    expect(calls.begin).toHaveLength(1);
    const begin = calls.begin[0]!;
    expect(begin.provider).toBe("slack");
    expect(begin.subject?.kind).toBe(OauthSubjectKind.CONNECTOR);
    expect(begin.subject?.id).toBe("conn-slack-default");
    expect(begin.redirectUri).toContain("/api/v1/integrations/slack/oauth/callback");
    expect(begin.spec?.tokenUrl).toBe("https://slack.com/api/oauth.v2.access");
    expect(begin.spec?.clientSecretRef).toBe("slack.client_secret");
    // The seed's metadata mapping rides the wire in snake_case.
    expect(begin.spec?.metadata?.fromTokenResponse?.account_id).toBe("team.id");
  });

  test("authorize: force=1 appends prompt=consent (reconnect)", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    await app.request("/api/v1/integrations/slack/oauth/authorize?force=1");
    expect(calls.begin[0]!.spec?.extraAuthorizeParams?.prompt).toBe("consent");
  });

  test("authorize: non-admin → 403", async () => {
    const { client } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: async () => ({ user: { id: "u2", role: "user" } }),
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/authorize");
    expect(res.status).toBe(403);
  });

  test("callback: completes the flow (state = flow id) → 302 connected", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request(
      "/api/v1/integrations/slack/oauth/callback?code=abc&state=10600000-0000-4000-8000-0000000000aa",
    );
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toBe("/settings/integrations/slack?connected=1");
    expect(calls.complete).toHaveLength(1);
    const complete = calls.complete[0]!;
    expect(complete.flowId).toBe("10600000-0000-4000-8000-0000000000aa");
    expect(complete.code).toBe("abc");
    expect(complete.subject?.id).toBe("conn-slack-default");
  });

  test("callback: a coordinator rejection (forged/expired state) → error redirect", async () => {
    const { client } = fakeRedirectClient({
      completeError: new ConnectError("OAuth resource not found", Code.NotFound),
    });
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/callback?code=abc&state=forged");
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toContain("error=");
  });

  test("callback: missing code/state → 400, no coordinator call", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/callback");
    expect(res.status).toBe(400);
    expect(calls.complete).toHaveLength(0);
  });

  // ── ADR 0115: user-subject flows over the SAME registered callback ────────

  const userOauthConnector = {
    provider: "acmeoauth",
    protocol: "http",
    credential: {
      source: "inject",
      injects: [{ header: "Authorization", template: "Bearer {}" }],
    },
    hosts: ["api.acmeoauth.test"],
    operations: [{ grants: ["issues:write"], match: { method: "POST", path: "/issues*" } }],
    oauth: {
      authorizeUrl: "https://acmeoauth.test/oauth/authorize",
      acquisitionHosts: ["acmeoauth.test"],
      tokenUrl: "https://api.acmeoauth.test/oauth/token",
      scopes: ["read"],
      // The ORG identity pin (Linear's actor=app) — the user flow must NOT
      // inherit it, or personal tokens act as the application.
      extraAuthorizeParams: { actor: "app" },
      clientIdRef: "acmeoauth.client_id",
      clientSecretRef: "acmeoauth.client_secret",
    },
    userCredential: {
      oauth: {
        scopesParam: "user_scope",
        grantPath: "authed_user",
        metadata: { fromTokenResponse: { accountId: "authed_user.id" } },
      },
    },
  };
  const userSource = {
    list: async () => [{ provider: "acmeoauth", config: userOauthConnector }],
  };
  const memberSession = async () => ({ user: { id: "u2", role: "user" } });

  test("user authorize: any member → 302 with a user_connector subject", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: userSource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: memberSession,
    });
    const res = await app.request("/api/v1/me/connector-credentials/acmeoauth/oauth/authorize");
    expect(res.status).toBe(302);
    const begin = calls.begin[0]!;
    expect(begin.subject?.kind).toBe(OauthSubjectKind.USER_CONNECTOR);
    expect(begin.subject?.id).toBe("u2");
    // The provider requires an exact-match redirect URI — the user flow
    // shares the org callback.
    expect(begin.redirectUri).toContain("/api/v1/integrations/acmeoauth/oauth/callback");
    // ADR 0115 amendment: the org identity pin (actor=app) is NOT inherited,
    // and the connector's user overrides ride the wire spec.
    expect(begin.spec?.extraAuthorizeParams).toEqual({});
    expect(begin.spec?.scopesParam).toBe("user_scope");
    expect(begin.spec?.grantPath).toBe("authed_user");
    expect(begin.spec?.metadata?.fromTokenResponse).toEqual({ account_id: "authed_user.id" });
  });

  test("org authorize keeps its identity params (actor=app untouched)", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: userSource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request("/api/v1/integrations/acmeoauth/oauth/authorize");
    expect(res.status).toBe(302);
    expect(calls.begin[0]!.spec?.extraAuthorizeParams).toEqual({ actor: "app" });
    expect(calls.begin[0]!.spec?.scopesParam ?? "").toBe("");
    expect(calls.begin[0]!.spec?.grantPath ?? "").toBe("");
  });

  test("user authorize: 404 without userCredential.oauth", async () => {
    const { client } = fakeRedirectClient();
    // An oauth-facet connector WITHOUT user-scoped support: org connections
    // only, so the member authorize route must not serve it.
    const { userCredential: _none, ...orgOnly } = userOauthConnector;
    const app = makeIntegrationOauthRoute({
      connectors: { list: async () => [{ provider: "acmeoauth", config: orgOnly }] },
      oauthCredential: client as never,
      connectionIdFor,
      getSession: memberSession,
    });
    const res = await app.request("/api/v1/me/connector-credentials/acmeoauth/oauth/authorize");
    expect(res.status).toBe(404);
  });

  test("callback: a user-subject flow completes for its owner → credentials page", async () => {
    const { client, calls } = fakeRedirectClient({
      flowSubject: { kind: OauthSubjectKind.USER_CONNECTOR, id: "u2" },
    });
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: memberSession,
    });
    const res = await app.request(
      "/api/v1/integrations/slack/oauth/callback?code=abc&state=10600000-0000-4000-8000-0000000000aa",
    );
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toBe("/settings/credentials?connected=slack");
    expect(calls.complete[0]!.subject).toMatchObject({
      kind: OauthSubjectKind.USER_CONNECTOR,
      id: "u2",
    });
  });

  test("callback: a user-subject flow is fenced to its owner", async () => {
    const { client, calls } = fakeRedirectClient({
      flowSubject: { kind: OauthSubjectKind.USER_CONNECTOR, id: "someone-else" },
    });
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: memberSession,
    });
    const res = await app.request(
      "/api/v1/integrations/slack/oauth/callback?code=abc&state=10600000-0000-4000-8000-0000000000aa",
    );
    expect(res.status).toBe(403);
    expect(calls.complete).toHaveLength(0);
  });

  test("callback: an org-subject flow still requires an admin", async () => {
    const { client, calls } = fakeRedirectClient();
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: memberSession,
    });
    const res = await app.request(
      "/api/v1/integrations/slack/oauth/callback?code=abc&state=10600000-0000-4000-8000-0000000000aa",
    );
    expect(res.status).toBe(403);
    expect(calls.complete).toHaveLength(0);
  });

  test("callback: an unknown flow id (forged state) → error redirect at lookup", async () => {
    const { client, calls } = fakeRedirectClient({
      lookupError: new ConnectError("OAuth resource not found", Code.NotFound),
    });
    const app = makeIntegrationOauthRoute({
      connectors: emptySource,
      oauthCredential: client as never,
      connectionIdFor,
      getSession: adminSession,
    });
    const res = await app.request("/api/v1/integrations/slack/oauth/callback?code=abc&state=forged");
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toContain("error=");
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
    const client = await getSlackClient({
      connectors: emptySource,
      integrationOp: integrationOp as never,
      connectionIdFor: async () => "conn-slack-default",
    });
    expect(client.token).toBe("xoxb-TEST");
    expect(typeof client.chat.postMessage).toBe("function");
  });
});
