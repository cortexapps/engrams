import { McpAuthorityStale } from "./authority-error.ts";
import { makeSessionBindingResolver } from "./session-resolver.ts";
import { makeSessionWritePolicy } from "./session-policy.ts";
import { and, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  task,
  taskSession,
  profile,
  integrationConnection,
  mcpCatalog,
  type IntegrationConnectionSnapshot,
} from "../db/schema.ts";
import { canonicalDigest } from "./hash.ts";
import { reviewCatalog } from "./catalog.ts";
import { validateMcpConfig } from "./config.ts";
import type { BoundSessionAuthority } from "./session-resolver.ts";
import type {
  McpSessionBinding,
  McpSessionBindingSnapshot,
  McpOperatorPolicy,
  McpLiveAuthority,
} from "./session-bindings.ts";

/** Bindings remain in integration_connections; this envelope contains only session provenance. */
export type McpBindingEnvelope = Omit<McpSessionBindingSnapshot, "immutableBindings">;
export function bindingSnapshotColumns(
  snapshot: McpSessionBindingSnapshot,
  connections: readonly IntegrationConnectionSnapshot[],
) {
  const { immutableBindings, ...mcpBindingEnvelope } = structuredClone(snapshot);
  const integrationConnections = connections
    .map((connection) => {
      const binding = immutableBindings.find((binding) => binding.connectionId === connection.id);
      if (connection.provider === "mcp" && !binding) return null;
      return { ...structuredClone(connection), ...(binding ? { mcp: binding } : {}) };
    })
    .filter((connection) => connection !== null);
  if (
    integrationConnections.filter((connection) => connection.mcp).length !==
    immutableBindings.length
  )
    throw new Error("MCP snapshot connection identity missing");
  return { mcpBindingEnvelope, integrationConnections };
}
export function restoreBindingSnapshot(
  envelope: McpBindingEnvelope,
  connections: readonly IntegrationConnectionSnapshot[],
): McpSessionBindingSnapshot {
  const immutableBindings: McpSessionBinding[] = [];
  for (const connection of connections) {
    if (connection.provider !== "mcp") continue;
    const binding = connection.mcp;
    if (
      !binding ||
      !("alias" in binding) ||
      !("operatorPolicy" in binding) ||
      binding.connectionId !== connection.id ||
      binding.alias !== connection.alias ||
      canonicalDigest(binding.config) !== canonicalDigest(connection.config)
    )
      throw new McpAuthorityStale("MCP stored binding identity missing");
    immutableBindings.push(binding);
  }
  immutableBindings.sort((a, b) =>
    a.connectionId < b.connectionId ? -1 : a.connectionId > b.connectionId ? 1 : 0,
  );
  const snapshot = {
    ...structuredClone(envelope),
    immutableBindings: structuredClone(immutableBindings),
  };
  const { bindingHash, ...body } = snapshot;
  if (canonicalDigest(body) !== bindingHash)
    throw new McpAuthorityStale("MCP stored binding hash mismatch");
  return snapshot;
}
export async function readStoredBindingSession(
  sessionId: string,
  db = getDb(),
): Promise<BoundSessionAuthority | null> {
  const rows = await db
    .select({
      envelope: taskSession.mcpBindingEnvelope,
      connections: taskSession.integrationConnections,
      parentSessionId: task.spawningSessionId,
      status: task.status,
    })
    .from(taskSession)
    .innerJoin(task, eq(task.id, taskSession.taskId))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(2);
  if (rows.length !== 1 || !rows[0]?.envelope) return null;
  const row = rows[0];
  return {
    snapshot: restoreBindingSnapshot(row.envelope!, row.connections ?? []),
    parentSessionId: row.parentSessionId,
    active: !["done", "failed", "cancelled"].includes(row.status),
  };
}

/** Reads existing connection/catalog/profile authority. Operator policy has no admin-write path. */
export async function readLiveBindingAuthority(
  profileId: string,
  operatorPolicies: readonly McpOperatorPolicy[],
  db: ReturnType<typeof getDb> | import("./journal-review.ts").JournalTx = getDb(),
): Promise<McpLiveAuthority> {
  const [currentProfile] = await db.select().from(profile).where(eq(profile.id, profileId));
  if (!currentProfile || currentProfile.deletedAt)
    throw new McpAuthorityStale("MCP profile unavailable");
  const rows = await db
    .select()
    .from(integrationConnection)
    .where(eq(integrationConnection.provider, "mcp"));
  const connections: McpLiveAuthority["connections"] = [];
  for (const row of rows) {
    const [catalog] = await db
      .select()
      .from(mcpCatalog)
      .where(
        and(
          eq(mcpCatalog.connectionId, row.id),
          eq(mcpCatalog.kind, "published"),
          eq(mcpCatalog.revision, row.catalogRevision),
        ),
      );
    const available =
      catalog &&
      catalog.configRevision === row.configRevision &&
      catalog.authGeneration === row.authGeneration &&
      catalog.credentialIdentity === row.credentialIdentity;
    const tools = available ? reviewCatalog(catalog.tools) : [];
    if (available && canonicalDigest(tools) !== canonicalDigest(catalog.tools))
      throw new McpAuthorityStale("MCP catalog fingerprint mismatch");
    connections.push({
      id: row.id,
      catalogId: catalog?.id ?? "",
      alias: row.alias,
      config: validateMcpConfig(row.config),
      enabled: row.enabled && Boolean(available) && !row.mcpCredential?.transition,
      state: {
        configRevision: row.configRevision,
        authGeneration: row.authGeneration,
        credentialIdentity: row.credentialIdentity,
        catalogRevision: row.catalogRevision,
        revoked: row.revoked,
      },
      tools,
    });
  }
  return {
    profileId,
    profileGrants: currentProfile.integrationGrants,
    profilePublication: currentProfile.mcpGrantPublication,
    connections,
    operatorPolicies: structuredClone([...operatorPolicies]),
  };
}

/** Production composition must supply both operator policy and the real host enforcement fence. */
export function makeStoredSessionBindingResolver(
  deps: {
    control: import("./control-client.ts").SessionPolicyControlClient;
    readOperatorPolicies(): Promise<McpOperatorPolicy[]>;
    enforceManagedDeny?: import("./session-resolver.ts").SessionBindingResolverDeps["enforceManagedDeny"];
  },
  db = getDb(),
) {
  const policy = makeSessionWritePolicy(deps.control);
  return makeSessionBindingResolver({
    readSession: (sessionId) => readStoredBindingSession(sessionId, db),
    readAuthority: async (profileId) =>
      readLiveBindingAuthority(profileId, await deps.readOperatorPolicies(), db),
    readBindingEpoch: async (sessionId) => {
      const current = await policy.readForRequest({ sessionId });
      const epoch = current.ref?.bindingEpoch;
      if (
        !current.current ||
        epoch === undefined ||
        epoch < 0n ||
        epoch > BigInt(Number.MAX_SAFE_INTEGER)
      )
        throw new Error("MCP current binding epoch unavailable");
      return Number(epoch);
    },
    ...(deps.enforceManagedDeny ? { enforceManagedDeny: deps.enforceManagedDeny } : {}),
  });
}
