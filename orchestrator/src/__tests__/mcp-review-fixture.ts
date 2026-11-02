import { create } from "@bufbuild/protobuf";
import { eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { user, task, taskSession, profile, mcpCatalog, mcpAdmissionScope } from "../db/schema.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { makeMcpRepository } from "../mcp/repository.ts";
import { makeMcpJournal } from "../mcp/journal.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { bindingSnapshotColumns } from "../mcp/binding-store.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import { McpWritePolicyRefSchema } from "../gen/engram/app/v1/session_pb.ts";

/** The stored binding and policy are test fixtures. No host ACK or VM enforcement is claimed. */
export async function reviewFixture(documents: Record<string, unknown>[] = [], toolCount = 1) {
  const profileId = crypto.randomUUID();
  const db = getDb(),
    actorId = crypto.randomUUID(),
    taskId = crypto.randomUUID(),
    sessionId = crypto.randomUUID(),
    deploymentId: string = crypto.randomUUID();
  const now = new Date("2026-01-01T00:00:00Z");
  const connections = makeIntegrationConnectionStore(db),
    repository = makeMcpRepository(db);
  await db.insert(user).values({
    id: actorId,
    name: "Review owner",
    email: `${actorId}@example.test`,
    role: "user",
    createdAt: now,
    updatedAt: now,
  });
  const config = {
    endpoint: "https://tools.example.test/mcp",
    zone: "public",
    auth: { kind: "api_key" as const, secretRef: "mcp.test.key" },
  };
  const connection = await connections.create({
    alias: `test-${crypto.randomUUID()}`,
    provider: "mcp",
    displayName: "Test tools",
    config,
  });
  await repository.setAuthorization(connection.id, 1, 1, "test-account", now);
  const current = (await connections.get(connection.id))!;
  const tool = {
    upstreamName: "Write.Record",
    description: "Write a record",
    inputSchema: { type: "object" },
  };
  const tools = Array.from({ length: toolCount }, (_, i) => ({
    ...tool,
    upstreamName: i === 0 ? tool.upstreamName : `Write.Record${i}`,
  }));
  const draft = await repository.saveDraft(connection.id, current.mcp!, tools, actorId, now);
  const catalog = await repository.publish(
    connection.id,
    draft.id,
    0,
    tools.map((tool) => ({ ...tool, classification: "write", enabled: true })),
    actorId,
    now,
  );
  await repository.setEnabled(connection.id, 1, true, now);
  const grant = {
    connectionId: connection.id,
    operation: "invoke",
    resourceConstraints: [],
    credentialScope: "org" as const,
    mcp: {
      upstreamName: tool.upstreamName,
      automationWrite: true,
      unrestricted: documents.length === 0,
      argumentConstraintDocuments: documents,
    },
  };
  const grants = tools.map((tool) => ({
    ...grant,
    mcp: { ...grant.mcp, upstreamName: tool.upstreamName },
  }));
  const authority = {
    profileId,
    profileGrants: grants,
    connections: [
      {
        id: connection.id,
        catalogId: catalog.id,
        alias: connection.alias,
        enabled: true,
        config,
        state: { ...current.mcp!, catalogRevision: 1 },
        tools: [catalog.tools[0]!],
      },
    ],
    operatorPolicies: [
      {
        zoneId: "public",
        policyRevision: "1",
        policyDigest: canonicalDigest({ policy: 1 }),
        authorities: ["tools.example.test"],
        aliases: [],
      },
    ],
  };
  // The repository published this catalog as reviewed, with explicit write classification.
  const { reviewCatalog } = await import("../mcp/catalog.ts");
  const snapshot = compileSessionBindings({
    launchPolicy: { profileId: authority.profileId, integrationGrants: grants },
    liveAuthority: {
      ...authority,
      connections: [{ ...authority.connections[0]!, tools: reviewCatalog(catalog.tools) }],
    },
    bindingEpoch: 0,
  });
  await db.insert(profile).values({
    id: profileId,
    name: "Review profile",
    imageId: "image",
    harness: "claude",
    integrationGrants: grants,
  });
  await db.insert(task).values({ id: taskId, type: "chat", createdByUserId: actorId });
  await db.insert(taskSession).values({
    taskId,
    sessionId,
    profileId,
    integrationPrincipalId: actorId,
    ...bindingSnapshotColumns(snapshot, [
      {
        id: connection.id,
        alias: connection.alias,
        provider: "mcp",
        displayName: connection.displayName,
        config: connection.config,
      },
    ]),
  });
  const binding = snapshot.immutableBindings[0]!,
    bound = binding.tools[0]!;
  const resolvedTool = {
    bindingHash: snapshot.bindingHash,
    profilePublication: snapshot.profilePublication,
    binding,
    modelToolName: bound.name,
    reviewedTool: bound.tool,
    effectiveGrant: bound.grant,
    ancestorAuthorityRefs: snapshot.ancestorAuthorityRefs,
    execution: "deferred" as const,
  };
  const requestPolicyContext = {
    sessionId,
    runId: "run-1",
    toolCallId: "call-1",
    acceptedPolicyRef: create(McpWritePolicyRefSchema, {
      sessionId,
      policyRevision: 1n,
      bindingEpoch: 0n,
    }),
    acceptedWriteEligibility: "write_eligible",
    ancestorPolicyRefs: [],
  };
  const input = {
    taskId,
    sessionId,
    runId: "run-1",
    toolCallId: "call-1",
    principalId: actorId,
    connectionId: connection.id,
    catalogId: catalog.id,
    bindingHash: snapshot.bindingHash,
    argumentHash: canonicalDigest({ x: 1 }),
    originalArguments: '{ "x": 1 }',
    resolvedTool,
    requestPolicyContext,
    upstreamName: tool.upstreamName,
    configRevision: 1,
    authGeneration: current.mcp!.authGeneration,
    classification: "write" as const,
    deadline: new Date(now.getTime() + 600_000),
  };
  return {
    db,
    authority: {
      ...authority,
      connections: [{ ...authority.connections[0]!, tools: reviewCatalog(catalog.tools) }],
    },
    profileId,
    actorId,
    draft,
    actor: actorId,
    taskId,
    sessionId,
    deploymentId,
    now,
    connections,
    repository,
    connection,
    catalog,
    snapshot,
    input,
    journal: makeMcpJournal(db),
    async cleanup() {
      await db.delete(task).where(eq(task.id, taskId));
      await db.delete(mcpAdmissionScope).where(eq(mcpAdmissionScope.id, deploymentId));
      await db.delete(mcpCatalog).where(eq(mcpCatalog.connectionId, connection.id));
      await connections.delete(connection.id);
      await db.delete(profile).where(eq(profile.id, profileId));
      await db.delete(user).where(eq(user.id, actorId));
    },
  };
}

/** Stage fixture for tests of review/journal behavior after a successful worker preflight.
 * It obtains and releases real durable leases; the boolean worker result is injected. */
let priorSchemaPreparation: Promise<unknown> = Promise.resolve();
export async function schemaVerifiedFixture(
  documents: Record<string, unknown>[] = [],
  toolCount = 1,
  deploymentId?: string,
) {
  const f = await reviewFixture(documents, toolCount);
  if (deploymentId) f.deploymentId = deploymentId;
  const base = f.journal;
  return {
    ...f,
    journal: {
      ...base,
      async create(...args: Parameters<typeof base.create>) {
        const call = await base.create(...args);
        const ready = priorSchemaPreparation.then(async () => {
          if (
            ["waiting_approval", "authorized"].includes(call.state) &&
            !(await base.getAdmission(call.id))?.schemaDigest
          ) {
            await base.admitSchema(call.id, f.deploymentId, args[2]);
            const lease = (await base.getAdmission(call.id))!;
            await base.finishSchema(
              call.id,
              lease.reservationId,
              lease.expiresAt,
              call.requestDigest,
              true,
              args[2],
            );
          }
        });
        priorSchemaPreparation = ready.catch(() => {});
        await ready;
        return call;
      },
    },
  };
}
