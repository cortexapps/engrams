import { appendMcpManifest } from "../tools/manifest.ts";
import type { IntegrationConnectionSnapshot } from "../db/schema.ts";
import type { SessionCreateInput } from "../rpc/task-create.ts";
import type { SessionPolicyControlClient } from "./control-client.ts";
import {
  bindingSnapshotColumns,
  restoreBindingSnapshot,
  type McpBindingEnvelope,
} from "./binding-store.ts";
import {
  checkBindingCurrent,
  compileSessionBindings,
  type McpLiveAuthority,
  type McpSessionBindingSnapshot,
  type ManagedDenyPolicy,
} from "./session-bindings.ts";
import {
  assertManagedDenyAck,
  expectedManagedDenyAck,
  type ManagedDenyAck,
} from "./session-resolver.ts";

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
export interface BoundSessionReservation {
  principalId: string | null;
  parentSessionId: string | null;
  envelope: McpBindingEnvelope;
  connections: IntegrationConnectionSnapshot[];
}
export interface TrustedCreationInput {
  actorId: string;
  inputId: string;
}
export interface BoundSessionCreatorDeps {
  control: Pick<SessionPolicyControlClient, "createPolicySession">;
  readReservation(sessionId: string): Promise<BoundSessionReservation | null>;
  /** Check the current authenticated source input, principal and permission; never guest metadata. */
  authorizeInput(input: TrustedCreationInput, sessionId: string): Promise<void>;
  readAuthority(profileId: string): Promise<McpLiveAuthority>;
  readParentCurrent(sessionId: string): Promise<McpSessionBindingSnapshot>;
  enforceManagedDeny?(expected: ManagedDenyAck, policy: ManagedDenyPolicy): Promise<ManagedDenyAck>;
}
/** Retain the actual created session so callers can compensate a rejected response. */
export class McpSessionCreationMismatchError extends Error {
  constructor(readonly createdSessionId: string) {
    super("MCP policy session response mismatch");
    this.name = "McpSessionCreationMismatchError";
  }
}
/** Closed adapter for Task 6 composition. Generic create never initializes write authority. */
export function makeBoundSessionCreator(deps: BoundSessionCreatorDeps) {
  return {
    async create(input: SessionCreateInput, source: TrustedCreationInput) {
      const sessionId = input.requestedSessionId;
      if (!sessionId || !source.actorId || !source.inputId || !input.mcpBindingEnvelope)
        throw new Error("trusted MCP creation context required");
      await deps.authorizeInput(source, sessionId);
      const reservation = await deps.readReservation(sessionId);
      if (!reservation || reservation.principalId !== source.actorId)
        throw new Error("MCP reservation principal changed");
      const snapshot = restoreBindingSnapshot(reservation.envelope, reservation.connections);
      const candidate = restoreBindingSnapshot(
        input.mcpBindingEnvelope,
        input.integrationConnections ?? [],
      );
      if (candidate.bindingHash !== snapshot.bindingHash || snapshot.bindingEpoch !== 0)
        throw new Error("MCP creation binding changed");
      if (
        !checkBindingCurrent({
          snapshot,
          liveAuthority: await deps.readAuthority(snapshot.profileId),
        }).current
      )
        throw new Error("MCP creation authority changed");
      const parentId = reservation.parentSessionId;
      if (parentId) {
        const parent = await deps.readParentCurrent(parentId);
        const expected = snapshot.ancestorAuthorityRefs[0];
        if (
          !expected ||
          expected.sessionId !== parentId ||
          expected.bindingHash !== parent.bindingHash ||
          expected.bindingEpoch !== parent.bindingEpoch
        )
          throw new Error("MCP creation parent changed");
      } else if (snapshot.ancestorAuthorityRefs.length)
        throw new Error("MCP creation parent missing");
      if (!deps.enforceManagedDeny) throw new Error("MCP managed deny enforcement unavailable");
      const expected = expectedManagedDenyAck(sessionId, snapshot);
      assertManagedDenyAck(
        expected,
        await deps.enforceManagedDeny(expected, snapshot.managedDenyPolicy),
      );
      const {
        mcpBindingEnvelope: _envelope,
        integrationConnections: _connections,
        integrationGrants: _grants,
        ...wire
      } = input;
      const response = await deps.control.createPolicySession({
        protocol: 1,
        actorId: source.actorId,
        inputId: source.inputId,
        ...(parentId ? { parentSessionId: parentId } : {}),
        create: {
          ...wire,
          harnessEnv: {
            ...wire.harnessEnv,
            ENGRAM_TOOLS: JSON.stringify(
              appendMcpManifest(wire.harnessEnv?.ENGRAM_TOOLS, snapshot),
            ),
          },
        },
      });
      if (!response.session?.sessionId) throw new Error("MCP policy session response mismatch");
      if (response.session.sessionId !== sessionId)
        throw new McpSessionCreationMismatchError(response.session.sessionId);
      return response.session;
    },
  };
}

/** The normal task path cannot route MCP authority through legacy/generic creation. */
export async function createReservedTaskSession(
  deps: {
    sessions: { createSession(input: SessionCreateInput): Promise<{ sessionId: string }> };
    boundSessionCreator?: ReturnType<typeof makeBoundSessionCreator>;
  },
  input: SessionCreateInput,
  source: TrustedCreationInput,
) {
  const hasMcp =
    input.mcpBindingEnvelope ||
    input.integrationGrants?.some((grant) => grant.mcp) ||
    input.integrationConnections?.some((connection) => connection.provider === "mcp");
  if (!hasMcp) return deps.sessions.createSession(input);
  if (!input.mcpBindingEnvelope || !deps.boundSessionCreator)
    throw new Error("external MCP session activation is not configured");
  return deps.boundSessionCreator.create(input, source);
}
