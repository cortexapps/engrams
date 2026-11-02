import { and, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { taskSession, type ProfileIntegrationGrant } from "../db/schema.ts";
import { compileSessionBindings, type McpSessionBindingSnapshot } from "./session-bindings.ts";

/** The caller supplies the actual tool-call session ID, never a task launch snapshot. */
export async function compileChildSessionBindings(
  input: Omit<Parameters<typeof compileSessionBindings>[0], "parentAuthority"> & {
    spawningSessionId: string;
    readParentCurrent(sessionId: string): Promise<McpSessionBindingSnapshot>;
  },
) {
  const snapshot = await input.readParentCurrent(input.spawningSessionId);
  return compileSessionBindings({
    ...input,
    requestedGrants: input.requestedGrants ?? [],
    parentAuthority: { sessionId: input.spawningSessionId, snapshot },
  });
}
/** Retain the unconditional activation guard until the host can enforce child denies. */
export async function assertChildMcpCreationBlocked(
  taskId: string,
  spawningSessionId: string,
  launchGrants: readonly ProfileIntegrationGrant[],
  db = getDb(),
) {
  const [parent] = await db
    .select({
      grants: taskSession.integrationGrants,
      connections: taskSession.integrationConnections,
      envelope: taskSession.mcpBindingEnvelope,
    })
    .from(taskSession)
    .where(and(eq(taskSession.taskId, taskId), eq(taskSession.sessionId, spawningSessionId)));
  if (!parent) throw new Error("spawning session authority unavailable");
  if (
    parent.envelope ||
    [...(parent.grants ?? []), ...launchGrants].some((grant) => grant.mcp) ||
    parent.connections?.some((connection) => connection.provider === "mcp")
  )
    throw new Error("external MCP session activation is not configured");
}
