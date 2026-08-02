/** Internal host-only named-connection credential broker (ADR 0109). */

import { Hono } from "hono";
import { eq } from "drizzle-orm";
import { timingSafeEqual } from "node:crypto";

import { config } from "../config.ts";
import { getDb } from "../db/client.ts";
import { makeIntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import {
  task,
  taskSession,
  type IntegrationConnectionSnapshot,
  type ProfileIntegrationGrant,
} from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import { assertGoogleCloudConfig, makeGoogleWifBroker } from "../integrations/google-wif.ts";
import { googleOidcIssuer } from "./google-oidc.ts";

interface BrokerRequest {
  sessionId: string;
  connectionId: string;
}

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

const log = rootLog.child({ component: "connection-credential-broker" });

function bearerMatches(header: string | undefined, expected: string): boolean {
  const actual = header?.startsWith("Bearer ") ? header.slice(7) : "";
  const actualBytes = Buffer.from(actual);
  const expectedBytes = Buffer.from(expected);
  return actualBytes.length === expectedBytes.length && timingSafeEqual(actualBytes, expectedBytes);
}

export function makeConnectionCredentialBrokerRoute(deps: {
  db?: ReturnType<typeof getDb>;
  bearer?: string;
  now?: () => Date;
  issuer?: string;
  exchange?: ReturnType<typeof makeGoogleWifBroker>["exchange"];
  sessions?: CredentialBrokerSessionStore;
} = {}): Hono {
  const app = new Hono();
  const db = deps.db ?? getDb();
  const bearer = deps.bearer ?? config.controlPlaneBearer;
  const now = deps.now ?? (() => new Date());
  const issuer = deps.issuer ?? googleOidcIssuer();
  const exchange = deps.exchange ?? makeGoogleWifBroker({
    keys: makeIntegrationOidcKeyStore(db),
    issuer,
    now,
  }).exchange;
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
  const cache = new Map<string, CachedToken>();

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

    const session = await sessions.get(request.sessionId);
    const grants = (session?.integrationGrants ?? []) as ProfileIntegrationGrant[];
    const connections = (session?.integrationConnections ?? []) as IntegrationConnectionSnapshot[];
    const connection = connections.find((candidate) => candidate.id === request.connectionId);
    const principalId = session?.principalId ?? session?.userId ?? "automation";
    const granted = grants.some((grant) => grant.connectionId === request.connectionId);

    let outcome = "denied";
    try {
      if (!session || !connection || !granted) {
        return c.json({ error: "forbidden" }, 403);
      }
      if (connection.provider !== "gcp") {
        return c.json({ error: "connection provider does not support remote minting" }, 400);
      }
      const google = assertGoogleCloudConfig(connection.config);

      const cacheKey = `${request.sessionId}:${request.connectionId}`;
      const requestTime = now();
      cache.forEach((cached, key) => {
        if (cached.expiresAt <= requestTime) cache.delete(key);
      });
      let token = cache.get(cacheKey);
      if (!token || token.expiresAt.getTime() - requestTime.getTime() < 60_000) {
        token = await exchange(google, {
          sessionId: request.sessionId,
          organizationId: issuer,
          connectionId: request.connectionId,
          userId: principalId,
          profileSnapshotId: `${session.profileId ?? "none"}:${request.sessionId}`,
        });
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
        profileSnapshotId: `${session?.profileId ?? "none"}:${request.sessionId}`,
        connectionId: request.connectionId,
        serviceAccount: connection?.provider === "gcp"
          ? (connection.config.serviceAccountEmail as string | undefined)
          : undefined,
        outcome,
        error: error instanceof Error ? error.message : String(error),
      }, "connection credential mint failed");
      return c.json({ error: "connection credential mint failed" }, 502);
    } finally {
      if (outcome !== "error") {
        log.info({
          userId: principalId,
          sessionId: request.sessionId,
          profileSnapshotId: `${session?.profileId ?? "none"}:${request.sessionId}`,
          connectionId: request.connectionId,
          serviceAccount: connection?.provider === "gcp"
            ? (connection.config.serviceAccountEmail as string | undefined)
            : undefined,
          outcome,
        }, "connection credential mint");
      }
    }
  });

  return app;
}
