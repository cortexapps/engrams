/**
 * OAuth authorization-code acquisition for connectors with an `oauth` facet (e.g.
 * Slack "Add to Slack"). The orchestrator owns the browser redirect + CSRF state;
 * the coordinator (the only tier that can read/write org secrets) builds the
 * authorize URL and runs the code→token exchange, writing the access token.
 *
 *   GET /api/v1/integrations/:provider/oauth/authorize  (admin) → 302 to the IdP
 *   GET /api/v1/integrations/:provider/oauth/callback           → exchange + 302 back
 *
 * Generic over any oauth-facet connector — Slack is the first.
 */

import { Hono } from "hono";
import type { Context } from "hono";
import { HTTPException } from "hono/http-exception";
import { auth } from "../auth/better-auth.ts";
import { abilityFor } from "../authz/ability.ts";
import { config } from "../config.ts";
import { loadRegistry } from "../connectors/registry.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { getDb } from "../db/client.ts";
import { integrationOp as defaultIntegrationOp } from "../control-plane/client.ts";

type IntegrationOauthClient = Pick<
  typeof defaultIntegrationOp,
  "beginIntegrationOauth" | "completeIntegrationOauth"
>;

interface PendingState {
  provider: string;
  userId: string;
  /** epoch ms expiry. */
  exp: number;
}

type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface IntegrationOauthDeps {
  connectors?: { list(): Promise<ReadonlyArray<{ provider: string; config: unknown }>> };
  integrationOp?: IntegrationOauthClient;
  /** Override the admin-session lookup (tests). */
  getSession?: GetSession;
  /** Override the CSRF state generator (tests). */
  randomState?: () => string;
}

const STATE_TTL_MS = 10 * 60_000;

export function makeIntegrationOauthRoute(deps?: IntegrationOauthDeps): Hono {
  const app = new Hono();
  const integrationOp = deps?.integrationOp ?? defaultIntegrationOp;
  // Resolved lazily (per request) so constructing the default route doesn't touch
  // the DB at import time.
  const connectorSource = () => deps?.connectors ?? makeConnectorStore(getDb());
  const randomState = deps?.randomState ?? (() => crypto.randomUUID());
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));

  // In-memory CSRF state (single-instance). The callback also re-checks the admin
  // session cookie, so this guards against cross-site request forgery, not auth.
  const pending = new Map<string, PendingState>();

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

  async function oauthConnector(provider: string) {
    const registry = await loadRegistry(connectorSource());
    const conn = registry.get(provider);
    if (!conn?.oauth) {
      throw new HTTPException(404, { message: `connector "${provider}" has no OAuth flow` });
    }
    return conn.oauth;
  }

  // The browser lands back on the settings page with a connected/error flag.
  function finish(provider: string, ok: boolean, message: string, c: Context) {
    const base = `/settings/integrations/${encodeURIComponent(provider)}`;
    const qs = ok ? "connected=1" : `error=${encodeURIComponent(message)}`;
    return c.redirect(`${base}?${qs}`);
  }

  app.get("/api/v1/integrations/:provider/oauth/authorize", async (c) => {
    const user = await requireAdmin(c.req.raw.headers);
    const provider = c.req.param("provider");
    const oauth = await oauthConnector(provider);

    // Prune expired states, then mint a fresh one.
    const now = Date.now();
    for (const [k, v] of pending) if (v.exp < now) pending.delete(k);
    const state = randomState();
    pending.set(state, { provider, userId: user.id, exp: now + STATE_TTL_MS });

    const { authorizeUrl } = await integrationOp.beginIntegrationOauth({
      provider,
      authorizeUrl: oauth.authorizeUrl,
      scopes: oauth.scopes,
      clientIdRef: oauth.clientIdRef,
      redirectUri: redirectUri(provider),
      state,
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

    const entry = pending.get(state);
    pending.delete(state);
    if (!entry || entry.provider !== provider || entry.exp < Date.now()) {
      throw new HTTPException(403, { message: "invalid or expired OAuth state" });
    }
    // Defence in depth: the callback carries the admin's session cookie.
    await requireAdmin(c.req.raw.headers);

    const oauth = await oauthConnector(provider);
    const res = await integrationOp.completeIntegrationOauth({
      provider,
      tokenUrl: oauth.tokenUrl,
      clientIdRef: oauth.clientIdRef,
      clientSecretRef: oauth.clientSecretRef,
      redirectUri: redirectUri(provider),
      code,
      tokenSecretRef: oauth.tokenSecretRef,
      tokenResponsePath: oauth.tokenResponsePath,
    });
    return finish(provider, res.ok, res.message, c);
  });

  return app;
}

export default makeIntegrationOauthRoute();
