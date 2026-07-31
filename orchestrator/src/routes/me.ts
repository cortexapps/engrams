/**
 * /api/v1/me/harness-env routes (ADR 0063 B3).
 *
 * The orchestrator OWNS the user's harness identity secrets in its OWN Postgres
 * (the `user_session_secrets` table, keyed by `(userId, envVarName)`, KEK-envelope
 * sealed at rest — ADR 0051 Drip A). These are the *human* credentials a harness
 * needs, declared by each registered harness's descriptor as `auth.user_env`
 * (e.g. Claude Code → `CLAUDE_CODE_OAUTH_TOKEN`). There is no longer any
 * Claude-specific "claude token" concept — the settings page is just the list of
 * env vars the registered harnesses ask for, and the user fills them in.
 *
 * At session-create the selected harness's `user_env` value is opened and passed
 * to the control plane via `CreateSession.harness_env` (see rpc/task-create.ts).
 *
 * Routes:
 *   GET    /api/v1/me/harness-env          — the catalog's `user_env` union, each
 *                                            with which harnesses ask for it +
 *                                            whether the caller has set it.
 *   PUT    /api/v1/me/harness-env/:envVar  — { value } → store.put → 204.
 *   DELETE /api/v1/me/harness-env/:envVar  — store.delete → 204.
 *
 * The key is ALWAYS the session's user id — never client-supplied. `:envVar` must
 * be a `user_env` some registered harness declares (else 404) so a client can't
 * seal arbitrary names. The secret plaintext is NEVER logged.
 *
 * Injectable deps for tests: see makeMeRoute(deps).
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import { makeUserSecretStore, type UserSecretStore } from "../db/user-secrets.ts";
import { harnessCatalog as defaultHarnessCatalog } from "../control-plane/client.ts";
import { oauthCredential as defaultOAuthCredential } from "../control-plane/client.ts";
import { OAuthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import type { GetSession } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** The catalog read surface the route needs: each harness's `user_env` (the
 *  human credential env-var name) + a display label. Structurally satisfied by
 *  the generated HarnessCatalogService connect client. */
export interface HarnessCatalogReader {
  listHarnesses(req: Record<string, never>): Promise<{
    harnesses: Array<{
      name: string;
      descriptor?: {
        label?: string;
        auth?: {
          userEnv?: string;
          userEnvHint?: string;
          userOauth?: { provider: string };
        };
      };
    }>;
  }>;
}

/** One harness that asks for a given env var (for the settings-page list). */
interface HarnessRef {
  name: string;
  label: string;
}

/** The union entry for one env var: who asks for it + the first setup hint
 *  a declaring harness provides. */
interface EnvVarEntry {
  harnesses: HarnessRef[];
  hint?: string;
}

/** Injectable deps for the /me route. */
export interface MeDeps {
  secrets?: UserSecretStore;
  harnessCatalog?: HarnessCatalogReader;
  getSession?: GetSession;
  oauth?: OAuthCredentialClient;
}

export interface OAuthCredentialClient {
  beginFlow(req: {
    subject: { kind: OAuthSubjectKind; id: string };
    provider: string;
  }): Promise<{
    flow?: { id: string; provider: string; status: string; expiresAt: string; errorCode?: string };
    verificationUrl: string;
    userCode: string;
  }>;
  getFlow(req: { subject: { kind: OAuthSubjectKind; id: string }; flowId: string }): Promise<{
    flow?: { id: string; provider: string; status: string; expiresAt: string; errorCode?: string };
  }>;
  cancelFlow(req: { subject: { kind: OAuthSubjectKind; id: string }; flowId: string }): Promise<{
    flow?: { id: string; provider: string; status: string; expiresAt: string; errorCode?: string };
  }>;
  listCredentials(req: { subject: { kind: OAuthSubjectKind; id: string } }): Promise<{
    credentials: Array<{
      provider: string;
      version: bigint;
      connected: boolean;
      account?: {
        displayName?: string;
        planType?: string;
        workspaceId?: string;
        workspaceName?: string;
      };
      createdAt: string;
      updatedAt: string;
    }>;
  }>;
  disconnect(req: {
    subject: { kind: OAuthSubjectKind; id: string };
    provider: string;
    expectedVersion: bigint;
  }): Promise<unknown>;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export function makeMeRoute(deps?: MeDeps): Hono {
  const app = new Hono();
  // The store + catalog are resolved lazily by default so importing this module
  // does not require ORCHESTRATOR_DATABASE_URL / a live control plane at import
  // time (tests inject fakes).
  const resolveStore = (): UserSecretStore => deps?.secrets ?? makeUserSecretStore();
  const resolveCatalog = (): HarnessCatalogReader =>
    deps?.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogReader);
  const resolveOAuth = (): OAuthCredentialClient =>
    deps?.oauth ?? defaultOAuthCredential;

  const resolveSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;

  /** Resolve the authenticated user or throw 401. */
  async function requireUser(headers: Headers): Promise<{ id: string; role: string }> {
    const session = await resolveSession(headers);
    if (!session) {
      throw new HTTPException(401, { message: "unauthenticated" });
    }
    return { id: session.user.id, role: session.user.role ?? "user" };
  }

  /** The `user_env` union across the registered harnesses → who asks for each,
   *  in catalog order. The map's keys are the only env-var names the PUT/DELETE
   *  routes will seal (defends against sealing arbitrary names). */
  async function userEnvUnion(): Promise<Map<string, EnvVarEntry>> {
    const { harnesses } = await resolveCatalog().listHarnesses({});
    const union = new Map<string, EnvVarEntry>();
    for (const h of harnesses) {
      const envVar = h.descriptor?.auth?.userEnv;
      if (!envVar) continue;
      const entry = union.get(envVar) ?? { harnesses: [] };
      entry.harnesses.push({ name: h.name, label: h.descriptor?.label || h.name });
      // First non-empty hint wins (harnesses share an env var by convention).
      if (!entry.hint && h.descriptor?.auth?.userEnvHint) entry.hint = h.descriptor.auth.userEnvHint;
      union.set(envVar, entry);
    }
    return union;
  }

  async function oauthUnion(): Promise<Map<string, EnvVarEntry>> {
    const { harnesses } = await resolveCatalog().listHarnesses({});
    const union = new Map<string, EnvVarEntry>();
    for (const h of harnesses) {
      const provider = h.descriptor?.auth?.userOauth?.provider;
      if (!provider) continue;
      const entry = union.get(provider) ?? { harnesses: [] };
      entry.harnesses.push({ name: h.name, label: h.descriptor?.label || h.name });
      if (!entry.hint && h.descriptor?.auth?.userEnvHint) entry.hint = h.descriptor.auth.userEnvHint;
      union.set(provider, entry);
    }
    return union;
  }

  const oauthSubject = (userId: string) => ({
    kind: OAuthSubjectKind.OAUTH_SUBJECT_KIND_USER,
    id: userId,
  });

  // GET /api/v1/me/harness-env — the env vars the registered harnesses ask for,
  // each with whether the caller has set it. This is the settings-page list.
  app.get("/api/v1/me/harness-env", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const store = resolveStore();
    const union = await userEnvUnion();
    const vars = await Promise.all(
      [...union.entries()].map(async ([envVar, entry]) => ({
        envVar,
        harnesses: entry.harnesses,
        ...(entry.hint ? { hint: entry.hint } : {}),
        present: await store.has(user.id, envVar),
      })),
    );
    return c.json({ vars });
  });

  // PUT /api/v1/me/harness-env/:envVar — seal the value under (userId, envVar).
  // body: { value: string }. 404 if no registered harness declares :envVar.
  app.put("/api/v1/me/harness-env/:envVar", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const envVar = c.req.param("envVar");
    if (!(await userEnvUnion()).has(envVar)) {
      throw new HTTPException(404, { message: `no registered harness declares ${envVar}` });
    }

    let body: { value?: unknown };
    try {
      body = (await c.req.json()) as { value?: unknown };
    } catch {
      throw new HTTPException(400, { message: "invalid JSON body" });
    }
    const value = typeof body.value === "string" ? body.value.trim() : undefined;
    if (!value) {
      throw new HTTPException(400, { message: "value must not be empty" });
    }

    // NEVER log the value. Stored KEK-envelope sealed under the env-var name.
    await resolveStore().put(user.id, envVar, value);
    return new Response(null, { status: 204 });
  });

  // DELETE /api/v1/me/harness-env/:envVar — clear it (idempotent).
  app.delete("/api/v1/me/harness-env/:envVar", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const envVar = c.req.param("envVar");
    if (!(await userEnvUnion()).has(envVar)) {
      throw new HTTPException(404, { message: `no registered harness declares ${envVar}` });
    }
    await resolveStore().delete(user.id, envVar);
    return new Response(null, { status: 204 });
  });

  // ADR 0106: unified credential read surface. Secret-env entries retain the
  // existing sealed-value flow; OAuth entries expose metadata and lifecycle.
  app.get("/api/v1/me/credentials", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const [envVars, providers, oauthRows] = await Promise.all([
      userEnvUnion(),
      oauthUnion(),
      resolveOAuth().listCredentials({ subject: oauthSubject(user.id) }),
    ]);
    const byProvider = new Map(oauthRows.credentials.map((row) => [row.provider, row]));
    const secretEntries = await Promise.all(
      [...envVars.entries()].map(async ([envVar, entry]) => ({
        kind: "secret_env" as const,
        envVar,
        harnesses: entry.harnesses,
        ...(entry.hint ? { hint: entry.hint } : {}),
        connected: await resolveStore().has(user.id, envVar),
      })),
    );
    const oauthEntries = [...providers.entries()].map(([provider, entry]) => {
      const row = byProvider.get(provider);
      return {
        kind: "oauth" as const,
        provider,
        harnesses: entry.harnesses,
        ...(entry.hint ? { hint: entry.hint } : {}),
        connected: row?.connected ?? false,
        ...(row
          ? {
              version: Number(row.version),
              account: row.account,
              createdAt: row.createdAt,
              updatedAt: row.updatedAt,
            }
          : {}),
      };
    });
    return c.json({ credentials: [...oauthEntries, ...secretEntries] });
  });

  app.post("/api/v1/me/credentials/:provider/connect", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const provider = c.req.param("provider");
    if (!(await oauthUnion()).has(provider)) {
      throw new HTTPException(404, { message: `no registered harness declares ${provider}` });
    }
    const response = await resolveOAuth().beginFlow({
      subject: oauthSubject(user.id),
      provider,
    });
    if (!response.flow) throw new HTTPException(502, { message: "OAuth provider returned no flow" });
    return c.json({
      flow: response.flow,
      verificationUrl: response.verificationUrl,
      userCode: response.userCode,
    });
  });

  app.get("/api/v1/me/credentials/flows/:flowId", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const response = await resolveOAuth().getFlow({
      subject: oauthSubject(user.id),
      flowId: c.req.param("flowId"),
    });
    if (!response.flow) throw new HTTPException(404, { message: "OAuth flow not found" });
    return c.json({ flow: response.flow });
  });

  app.post("/api/v1/me/credentials/flows/:flowId/cancel", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const response = await resolveOAuth().cancelFlow({
      subject: oauthSubject(user.id),
      flowId: c.req.param("flowId"),
    });
    if (!response.flow) throw new HTTPException(404, { message: "OAuth flow not found" });
    return c.json({ flow: response.flow });
  });

  app.delete("/api/v1/me/credentials/:provider", async (c) => {
    const user = await requireUser(c.req.raw.headers);
    const provider = c.req.param("provider");
    if (!(await oauthUnion()).has(provider)) {
      throw new HTTPException(404, { message: `no registered harness declares ${provider}` });
    }
    const rows = await resolveOAuth().listCredentials({ subject: oauthSubject(user.id) });
    const row = rows.credentials.find((credential) => credential.provider === provider);
    if (!row || !row.connected) return new Response(null, { status: 204 });
    await resolveOAuth().disconnect({
      subject: oauthSubject(user.id),
      provider,
      expectedVersion: row.version,
    });
    return new Response(null, { status: 204 });
  });

  return app;
}

export default makeMeRoute();
