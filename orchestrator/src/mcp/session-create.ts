import { mcpSessionCapabilities } from "./session-capabilities.ts";
import type { IntegrationConnectionSnapshot } from "../db/schema.ts";
import type { SessionCreateInput } from "../rpc/task-create.ts";
import { bindingSnapshotColumns } from "./binding-store.ts";
import { compileSessionBindings } from "./session-bindings.ts";
import type { McpSessionCapabilities } from "./session-capabilities.ts";

/** Candidate preparation is separate from activation. It cannot put tools into guest env. */
export function compileBoundSessionCandidate(
  input: SessionCreateInput,
  authority: Parameters<typeof compileSessionBindings>[0],
): SessionCreateInput {
  const snapshot = compileSessionBindings(authority);
  const connections: IntegrationConnectionSnapshot[] = authority.liveAuthority.connections.map(
    (connection) => ({
      id: connection.id,
      alias: connection.alias,
      displayName: connection.alias,
      provider: "mcp",
      config: structuredClone(
        snapshot.immutableBindings.find((binding) => binding.connectionId === connection.id)
          ?.config ?? {},
      ),
    }),
  );
  const direct = (input.integrationConnections ?? []).filter(
    (connection) => connection.provider !== "mcp",
  );
  const grants = snapshot.immutableBindings.flatMap((binding) =>
    binding.tools.map((bound) => ({
      connectionId: binding.connectionId,
      operation: "invoke",
      credentialScope: "org" as const,
      resourceConstraints: [],
      mcp: structuredClone(bound.grant),
    })),
  );
  return {
    ...input,
    ...bindingSnapshotColumns(snapshot, [...direct, ...connections]),
    integrationGrants: [
      ...(input.integrationGrants ?? []).filter((grant) => !grant.mcp),
      ...grants,
    ],
  };
}
export interface TrustedCreationInput {
  actorId: string;
  inputId: string;
}
/** Retain the actual created session so callers can compensate a rejected generic response. */
export class McpSessionCreationMismatchError extends Error {
  constructor(readonly createdSessionId: string) {
    super("coordinator returned a different reserved session ID");
    this.name = "McpSessionCreationMismatchError";
  }
}

/** The normal task path cannot route MCP authority through legacy/generic creation. */
export async function createReservedTaskSession(
  deps: {
    sessions: { createSession(input: SessionCreateInput): Promise<{ sessionId: string }> };
    boundSessionCreator?: Pick<McpSessionCapabilities, "create">;
  },
  input: SessionCreateInput,
  source: TrustedCreationInput,
) {
  const hasMcp =
    input.mcpBindingEnvelope ||
    input.integrationGrants?.some((grant) => grant.mcp) ||
    input.integrationConnections?.some((connection) => connection.provider === "mcp");
  if (!hasMcp) {
    const created = await deps.sessions.createSession(input);
    if (input.requestedSessionId && created.sessionId !== input.requestedSessionId)
      throw new McpSessionCreationMismatchError(created.sessionId);
    return created;
  }
  const creator = deps.boundSessionCreator ?? mcpSessionCapabilities();
  if (!input.mcpBindingEnvelope || !creator)
    throw new Error("external MCP session activation is not configured");
  const created = await creator.create(input, source);
  if (created.sessionId !== input.requestedSessionId)
    throw new McpSessionCreationMismatchError(created.sessionId);
  return created;
}
