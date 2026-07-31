/** Internal host-only Google access-token broker (ADR 0107). */

import { Hono } from "hono";
import { eq } from "drizzle-orm";
import { timingSafeEqual } from "node:crypto";

import { config } from "../config.ts";
import { getDb } from "../db/client.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { makeIntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import { task, taskSession, type ProfileIntegrationGrant } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import { assertGoogleCloudConfig, makeGoogleWifBroker } from "../integrations/google-wif.ts";
import { googleOidcIssuer } from "./google-oidc.ts";

interface BrokerRequest {
  sessionId: string;
  connectionId: string;
  operation: string;
  target: string;
}

interface CachedToken {
  accessToken: string;
  expiresAt: Date;
}

export interface GoogleBrokerSession {
  userId: string | null;
  principalId: string | null;
  profileId: string | null;
  integrationGrants: ProfileIntegrationGrant[] | null;
}

export interface GoogleBrokerSessionStore {
  get(sessionId: string): Promise<GoogleBrokerSession | null>;
}

const log = rootLog.child({ component: "google-token-broker" });

function bearerMatches(header: string | undefined, expected: string): boolean {
  const actual = header?.startsWith("Bearer ") ? header.slice(7) : "";
  const actualBytes = Buffer.from(actual);
  const expectedBytes = Buffer.from(expected);
  return actualBytes.length === expectedBytes.length && timingSafeEqual(actualBytes, expectedBytes);
}

export function makeGoogleTokenBrokerRoute(deps: {
  db?: ReturnType<typeof getDb>;
  connections?: IntegrationConnectionStore;
  bearer?: string;
  now?: () => Date;
  exchange?: ReturnType<typeof makeGoogleWifBroker>["exchange"];
  sessions?: GoogleBrokerSessionStore;
} = {}): Hono {
  const app = new Hono();
  const db = deps.db ?? getDb();
  const connections = deps.connections ?? makeIntegrationConnectionStore(db);
  const bearer = deps.bearer ?? config.controlPlaneBearer;
  const now = deps.now ?? (() => new Date());
  const exchange = deps.exchange ?? makeGoogleWifBroker({
    keys: makeIntegrationOidcKeyStore(db),
    issuer: googleOidcIssuer(),
    now,
  }).exchange;
  const sessions = deps.sessions ?? {
    async get(sessionId: string): Promise<GoogleBrokerSession | null> {
      const rows = await db.select({
        userId: task.createdByUserId,
        principalId: taskSession.integrationPrincipalId,
        profileId: taskSession.profileId,
        integrationGrants: taskSession.integrationGrants,
      }).from(taskSession).innerJoin(task, eq(taskSession.taskId, task.id)).where(
        eq(taskSession.sessionId, sessionId),
      ).limit(1);
      return rows[0] ?? null;
    },
  };
  const cache = new Map<string, CachedToken>();

  app.post("/internal/v1/integrations/google-cloud/token", async (c) => {
    if (!bearerMatches(c.req.header("authorization"), bearer)) {
      return c.json({ error: "unauthorized" }, 401);
    }
    let request: BrokerRequest;
    try {
      request = await c.req.json<BrokerRequest>();
    } catch {
      return c.json({ error: "invalid JSON" }, 400);
    }
    if (!request.sessionId || !request.connectionId || !request.operation || !request.target) {
      return c.json({ error: "sessionId, connectionId, operation, and target are required" }, 400);
    }

    const session = await sessions.get(request.sessionId);
    const connection = await connections.get(request.connectionId);
    const target = request.target.toLowerCase();
    const grants = (session?.integrationGrants ?? []) as ProfileIntegrationGrant[];
    const principalId = session?.principalId ?? session?.userId ?? "automation";
    const granted = grants.some(
      (grant) => grant.connectionId === request.connectionId && grant.operation === request.operation,
    );

    let outcome = "denied";
    try {
      if (!session || !connection || connection.provider !== "gcp" || !connection.enabled || !granted) {
        return c.json({ error: "forbidden" }, 403);
      }
      const google = assertGoogleCloudConfig(connection.config);
      if (!google.endpoints.includes(target)) {
        return c.json({ error: "target is not enabled for this connection" }, 403);
      }

      // Updating a connection disables it and changes updatedAt. If it is
      // tested and enabled again, no access token for its former service
      // account can be reused.
      const cacheKey =
        `${request.sessionId}:${request.connectionId}:${connection.updatedAt.toISOString()}`;
      const requestTime = now();
      cache.forEach((cached, key) => {
        if (cached.expiresAt <= requestTime) cache.delete(key);
      });
      let token = cache.get(cacheKey);
      if (!token || token.expiresAt.getTime() - requestTime.getTime() < 60_000) {
        token = await exchange(google, {
          sessionId: request.sessionId,
          organizationId: googleOidcIssuer(),
          connectionId: request.connectionId,
          userId: principalId,
          profileSnapshotId: `${session.profileId ?? "none"}:${request.sessionId}`,
        });
        cache.set(cacheKey, token);
      }
      outcome = "allowed";
      return c.json({ accessToken: token.accessToken, expiresAt: token.expiresAt.toISOString() });
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
        operation: request.operation,
        target,
        outcome,
        error: error instanceof Error ? error.message : String(error),
      }, "Google Cloud broker request failed");
      return c.json({ error: "Google Cloud token exchange failed" }, 502);
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
          operation: request.operation,
          target,
          outcome,
        }, "Google Cloud broker request");
      }
    }
  });

  return app;
}
