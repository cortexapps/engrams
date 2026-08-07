/** Internal host-only named-connection credential broker (ADR 0109). */

import { Hono } from "hono";
import { eq } from "drizzle-orm";
import { timingSafeEqual } from "node:crypto";
import { ConnectError, Code } from "@connectrpc/connect";

import { config } from "../config.ts";
import { getDb } from "../db/client.ts";
import { makeIntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import {
  task,
  taskSession,
  type IntegrationConnectionSnapshot,
  type ProfileIntegrationGrant,
} from "../db/schema.ts";
import { errorMessage, log as rootLog } from "../log.ts";
import type { makeGoogleWifBroker } from "../integrations/google-wif.ts";
import { makeGoogleProvider } from "../integrations/providers/google.ts";
import {
  connectionProviders,
  makeConnectionProviders,
  type ConnectionProvider,
  type CredentialPurpose,
  type ProviderConnection,
} from "../integrations/providers/index.ts";
import { integrationSnapshotHash } from "../integrations/grants.ts";
import { googleOidcIssuer } from "./google-oidc.ts";
import { sessions as controlPlaneSessions } from "../control-plane/client.ts";

/**
 * The identity a credential acts as, for the audit record. Provider-supplied:
 * the broker must not know that Google calls it a service account.
 */
function auditIdentity(
  providers: ReadonlyMap<string, ConnectionProvider>,
  connection: ProviderConnection | null | undefined,
): string | undefined {
  if (!connection) return undefined;
  return providers.get(connection.provider)?.auditIdentity(connection);
}

interface BrokerRequest {
  sessionId: string;
  connectionId: string;
  purpose?: CredentialPurpose;
}

const CREDENTIAL_PURPOSE_RE = /^[a-z][a-z0-9_.-]{0,63}$/;

interface CachedToken {
  accessToken: string;
  expiresAt: Date;
}

export interface CredentialBrokerSession {
  userId: string | null;
  principalId: string | null;
  profileId: string | null;
  integrationGrants: ProfileIntegrationGrant[] | null;
  integrationConnections: IntegrationConnectionSnapshot[] | null;
}

export interface CredentialBrokerSessionStore {
  get(sessionId: string): Promise<CredentialBrokerSession | null>;
}

/**
 * Live-session gate (O6): `task_session` rows outlive their session, so the
 * snapshot alone must NOT authorize minting forever. Returns the control-plane
 * session status, or null when the session no longer exists. A lookup FAILURE
 * must throw — the caller then refuses to mint (fail closed, retryable).
 */
export type SessionStatusProbe = (sessionId: string) => Promise<string | null>;

/** Control-plane statuses after which a session must never mint again. */
const ENDED_SESSION_STATUSES = new Set(["completed", "failed", "dead", "host_lost"]);

const log = rootLog.child({ component: "connection-credential-broker" });

/** Amortized expiry sweep period for the token cache. */
const CACHE_SWEEP_INTERVAL_MS = 60_000;

function bearerMatches(header: string | undefined, expected: string): boolean {
  const actual = header?.startsWith("Bearer ") ? header.slice(7) : "";
  const actualBytes = Buffer.from(actual);
  const expectedBytes = Buffer.from(expected);
  return actualBytes.length === expectedBytes.length && timingSafeEqual(actualBytes, expectedBytes);
}

function defaultSessionStatus(): SessionStatusProbe {
  return async (sessionId) => {
    try {
      const response = await controlPlaneSessions.getSession({ sessionId });
      return response.session?.status ?? null;
    } catch (error) {
      if (error instanceof ConnectError && error.code === Code.NotFound) return null;
      throw error;
    }
  };
}

export function makeConnectionCredentialBrokerRoute(deps: {
  db?: ReturnType<typeof getDb>;
  bearer?: string;
  now?: () => Date;
  issuer?: string;
  /** Deployment identity emitted as the `engrams_organization` claim. */
  organizationId?: string;
  exchange?: ReturnType<typeof makeGoogleWifBroker>["exchange"];
  /** Override the whole provider registry (a suite with its own providers). */
  providers?: ReadonlyMap<string, ConnectionProvider>;
  sessions?: CredentialBrokerSessionStore;
  sessionStatus?: SessionStatusProbe;
} = {}): Hono {
  const app = new Hono();
  const db = deps.db ?? getDb();
  // The broker's OWN bearer (CONNECTION_BROKER_BEARER) — the coordinator sends
  // it from ENGRAM_CONNECTION_BROKER_BEARER. Not the shared control-plane one.
  const bearer = deps.bearer ?? config.connectionBrokerBearer;
  const now = deps.now ?? (() => new Date());
  const issuer = deps.issuer ?? googleOidcIssuer();
  const organizationId = deps.organizationId ?? config.deploymentId;
  // Minting is the provider's job. The broker owns what is provider-NEUTRAL:
  // authorization, session lifetime, the token cache, and the audit record.
  const providers = deps.providers ??
    (deps.exchange
      ? makeConnectionProviders([makeGoogleProvider({ exchange: deps.exchange })])
      : connectionProviders());
  const sessions = deps.sessions ?? {
    async get(sessionId: string): Promise<CredentialBrokerSession | null> {
      const rows = await db.select({
        userId: task.createdByUserId,
        principalId: taskSession.integrationPrincipalId,
        profileId: taskSession.profileId,
        integrationGrants: taskSession.integrationGrants,
        integrationConnections: taskSession.integrationConnections,
      }).from(taskSession).innerJoin(task, eq(taskSession.taskId, task.id)).where(
        eq(taskSession.sessionId, sessionId),
      ).limit(1);
      return rows[0] ?? null;
    },
  };
  const sessionStatus = deps.sessionStatus ?? defaultSessionStatus();
  const cache = new Map<string, CachedToken>();
  let nextSweepAt = 0;

  function evictSession(sessionId: string): void {
    for (const key of cache.keys()) {
      if (key.startsWith(`${sessionId}:`)) cache.delete(key);
    }
  }

  app.post("/internal/v1/integrations/credentials/mint", async (c) => {
    if (!bearerMatches(c.req.header("authorization"), bearer)) {
      return c.json({ error: "unauthorized" }, 401);
    }
    let request: BrokerRequest;
    try {
      request = await c.req.json<BrokerRequest>();
    } catch {
      return c.json({ error: "invalid JSON" }, 400);
    }
    if (!request.sessionId || !request.connectionId) {
      return c.json({ error: "sessionId and connectionId are required" }, 400);
    }
    const purpose = request.purpose ?? "api";
    if (!CREDENTIAL_PURPOSE_RE.test(purpose)) {
      return c.json({ error: "unsupported credential purpose" }, 400);
    }

    const session = await sessions.get(request.sessionId);
    const grants = (session?.integrationGrants ?? []) as ProfileIntegrationGrant[];
    const connections = (session?.integrationConnections ?? []) as IntegrationConnectionSnapshot[];
    const connection = connections.find((candidate) => candidate.id === request.connectionId);
    const principalId = session?.principalId ?? session?.userId ?? "automation";
    const profileSnapshotId = session === null ? "none" : integrationSnapshotHash({
      profileId: session.profileId,
      integrationGrants: grants,
      integrationConnections: connections,
    });

    let outcome = "denied";
    try {
      if (!session || !connection) {
        return c.json({ error: "forbidden" }, 403);
      }
      const provider = providers.get(connection.provider);
      if (!provider) {
        return c.json({ error: "connection provider does not support remote minting" }, 400);
      }
      const authorizingOperations = purpose === "api"
        ? null
        : provider.credentialPurposes?.[purpose];
      if (purpose !== "api" && !authorizingOperations) {
        return c.json({ error: "connection provider does not support credential purpose" }, 400);
      }
      const granted = grants.some((grant) =>
        grant.connectionId === request.connectionId &&
        (purpose === "api" || authorizingOperations!.includes(grant.operation))
      );
      if (!granted) {
        return c.json({ error: "forbidden" }, 403);
      }
      // O6: the snapshot row outlives the session. Authorization is bounded by
      // the session's LIFETIME — a session the control plane no longer knows,
      // or one in a terminal state, never mints again.
      const status = await sessionStatus(request.sessionId);
      if (status === null || ENDED_SESSION_STATUSES.has(status)) {
        evictSession(request.sessionId);
        return c.json({ error: "forbidden" }, 403);
      }
      const cacheKey = `${request.sessionId}:${request.connectionId}:${purpose}`;
      const requestTime = now();
      // Amortized upkeep: expire the requested key inline; sweep the rest at
      // most once per interval (session end evicts eagerly above).
      if (requestTime.getTime() >= nextSweepAt) {
        nextSweepAt = requestTime.getTime() + CACHE_SWEEP_INTERVAL_MS;
        cache.forEach((cached, key) => {
          if (cached.expiresAt <= requestTime) cache.delete(key);
        });
      }
      let token = cache.get(cacheKey);
      if (token && token.expiresAt <= requestTime) {
        cache.delete(cacheKey);
        token = undefined;
      }
      if (!token || token.expiresAt.getTime() - requestTime.getTime() < 60_000) {
        const minted = await provider.mint(connection, {
          sessionId: request.sessionId,
          organizationId,
          connectionId: request.connectionId,
          userId: principalId,
          profileSnapshotId,
        }, purpose);
        token = { accessToken: minted.token, expiresAt: minted.expiresAt };
        cache.set(cacheKey, token);
      }
      outcome = "allowed";
      return c.json({
        kind: "bearer",
        token: token.accessToken,
        expiresAt: token.expiresAt.toISOString(),
      });
    } catch (error) {
      outcome = "error";
      log.warn({
        userId: principalId,
        sessionId: request.sessionId,
        profileSnapshotId,
        connectionId: request.connectionId,
        identity: auditIdentity(providers, connection),
        purpose,
        outcome,
        error: errorMessage(error),
      }, "connection credential mint failed");
      return c.json({ error: "connection credential mint failed" }, 502);
    } finally {
      if (outcome !== "error") {
        log.info({
          userId: principalId,
          sessionId: request.sessionId,
          profileSnapshotId,
          connectionId: request.connectionId,
          identity: auditIdentity(providers, connection),
          purpose,
          outcome,
        }, "connection credential mint");
      }
    }
  });

  return app;
}
