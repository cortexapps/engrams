/**
 * OAuth authorization-code acquisition for connectors with an `oauth` facet
 * ("Add to Slack", "Connect Linear"). The orchestrator owns the browser
 * redirect; the coordinator (the only tier that can read org secrets) builds
 * the authorize URL, runs the code→token exchange, and seals the obtained
 * tokens into its credential store (ADR 0106 addendum) — org secrets never
 * hold an acquired token.
 *
 * CSRF state is the coordinator's durable flow row: `state` = the flow id,
 * begun on any replica and completed on any other. No in-memory state.
 *
 *   GET /api/v1/integrations/:provider/oauth/authorize  (admin) → 302 to the IdP
 *   GET /api/v1/integrations/:provider/oauth/callback           → exchange + 302 back
 *
 * Generic over any oauth-facet connector — Slack and Linear are the first.
 */

import { Hono } from "hono";
import type { Context } from "hono";
import { HTTPException } from "hono/http-exception";
import { ConnectError } from "@connectrpc/connect";
import { getSessionFromHeaders } from "../auth/session.ts";
import { abilityFor } from "../authz/ability.ts";
import { config } from "../config.ts";
import { loadRegistry } from "../connectors/registry.ts";
import type { Connector } from "../connectors/registry.ts";
import { redirectOauthSpec } from "../connectors/oauth-spec.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { getDb } from "../db/client.ts";
import { oauthCredential as defaultOauthCredential } from "../control-plane/client.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";

type OauthCredentialClient = Pick<
  typeof defaultOauthCredential,
  "beginRedirectFlow" | "completeRedirectFlow"
>;

type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface IntegrationOauthDeps {
  connectors?: { list(): Promise<ReadonlyArray<{ provider: string; config: unknown }>> };
  oauthCredential?: OauthCredentialClient;
  /** Resolve the provider's default connection row (the credential subject). */
  connectionIdFor?: (provider: string, displayName: string) => Promise<string>;
  /** Override the admin-session lookup (tests). */
  getSession?: GetSession;
}

export function makeIntegrationOauthRoute(deps?: IntegrationOauthDeps): Hono {
  const app = new Hono();
  const oauthCredential = deps?.oauthCredential ?? defaultOauthCredential;
  // Resolved lazily (per request) so constructing the default route doesn't touch
  // the DB at import time.
  const connectorSource = () => deps?.connectors ?? makeConnectorStore(getDb());
  const connectionIdFor =
    deps?.connectionIdFor ??
    (async (provider: string, displayName: string) => {
      const row = await makeIntegrationConnectionStore(getDb()).ensureDefault(
        provider,
        displayName,
      );
      return row.id;
    });
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;

  function redirectUri(provider: string): string {
    return `${config.baseUrl.replace(/\/$/, "")}/api/v1/integrations/${encodeURIComponent(provider)}/oauth/callback`;
  }

  async function requireAdmin(headers: Headers): Promise<{ id: string }> {
    const session = await getSession(headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    const ability = abilityFor({ id: session.user.id, role: session.user.role ?? "user" });
    if (!ability.can("manage", "all")) throw new HTTPException(403, { message: "admin required" });
    return { id: session.user.id };
  }

  async function oauthConnector(provider: string): Promise<Connector> {
    const registry = await loadRegistry(connectorSource());
    const conn = registry.get(provider);
    if (!conn?.oauth) {
      throw new HTTPException(404, { message: `connector "${provider}" has no OAuth flow` });
    }
    return conn;
  }

  async function subjectFor(connector: Connector) {
    const id = await connectionIdFor(
      connector.provider,
      `${connector.display.name} (default)`,
    );
    return { kind: OauthSubjectKind.CONNECTOR, id };
  }

  // The browser lands back on the settings page with a connected/error flag.
  function finish(provider: string, ok: boolean, message: string, c: Context) {
    const base = `/settings/integrations/${encodeURIComponent(provider)}`;
    const qs = ok ? "connected=1" : `error=${encodeURIComponent(message)}`;
    return c.redirect(`${base}?${qs}`);
  }

  app.get("/api/v1/integrations/:provider/oauth/authorize", async (c) => {
    await requireAdmin(c.req.raw.headers);
    const provider = c.req.param("provider");
    const connector = await oauthConnector(provider);
    // Reconnect after revocation: force the provider's consent screen so a
    // fresh grant is issued (providers ignore unknown params otherwise).
    const force = c.req.query("force") === "1";
    const { authorizeUrl } = await oauthCredential.beginRedirectFlow({
      subject: await subjectFor(connector),
      provider,
      spec: redirectOauthSpec(connector, force ? { prompt: "consent" } : undefined),
      redirectUri: redirectUri(provider),
    });
    return c.redirect(authorizeUrl);
  });

  app.get("/api/v1/integrations/:provider/oauth/callback", async (c) => {
    const provider = c.req.param("provider");
    const idpError = c.req.query("error");
    if (idpError) return finish(provider, false, idpError, c);

    const code = c.req.query("code");
    const state = c.req.query("state");
    if (!code || !state) throw new HTTPException(400, { message: "missing code or state" });
    // The callback carries the admin's session cookie; the durable flow row
    // (looked up by `state` coordinator-side) is the CSRF check.
    await requireAdmin(c.req.raw.headers);

    const connector = await oauthConnector(provider);
    try {
      await oauthCredential.completeRedirectFlow({
        subject: await subjectFor(connector),
        provider,
        flowId: state,
        code,
        redirectUri: redirectUri(provider),
        spec: redirectOauthSpec(connector),
      });
      return finish(provider, true, "connected", c);
    } catch (err) {
      const message = err instanceof ConnectError ? err.rawMessage : "OAuth exchange failed";
      return finish(provider, false, message, c);
    }
  });

  return app;
}

export default makeIntegrationOauthRoute();
