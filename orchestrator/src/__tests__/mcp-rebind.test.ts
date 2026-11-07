import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { profile, task, taskSession, user } from "../db/schema.ts";
import { bindingSnapshotColumns, readLiveBindingAuthority } from "../mcp/binding-store.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { McpRebindDraining } from "../mcp/rebind.ts";
import { coldFixture } from "./mcp-cold-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
await verifyMcpTestDatabases();
import { reviewFixture } from "./mcp-review-fixture.ts";

async function finish(f: Awaited<ReturnType<typeof coldFixture>>, hash: string) {
  await expect(
    f.service.commit(f.sessionId, f.snapshot.bindingHash, hash, f.actorId),
  ).rejects.toBeInstanceOf(McpRebindDraining);
  f.start();
  return f.service.commit(f.sessionId, f.snapshot.bindingHash, hash, f.actorId);
}

test("concurrent cold commits reserve one recovery and expose only its finalized binding", async () => {
  const f = await coldFixture();
  try {
    const candidate = await f.service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    const results = await Promise.allSettled(
      Array.from({ length: 2 }, () =>
        f.service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
      ),
    );
    expect(results.every((r) => r.status === "rejected")).toBe(true);
    expect(f.stats().recoveryIds).toBe(1);
    f.start();
    const result = await f.service.commit(
      f.sessionId,
      f.snapshot.bindingHash,
      candidate.bindingHash,
      f.actorId,
    );
    expect(result.bindingHash).toBe(
      (await f.store.read(f.sessionId)).session.mcpBindingEnvelope!.bindingHash,
    );
  } finally {
    await f.cleanup();
  }
});

test("child cold rebind uses the current parent intersection and rejects a changed parent", async () => {
  const base = await reviewFixture([], 2);
  const parent = await coldFixture(base);
  const childTask = crypto.randomUUID(),
    childId = crypto.randomUUID();
  try {
    const live = await readLiveBindingAuthority(
      base.profileId,
      base.authority.operatorPolicies,
      base.db,
    );
    const first = live.profileGrants[0]!;
    const grant = {
      ...first,
      mcp: {
        ...first.mcp!,
        unrestricted: false,
        automationWrite: false,
        argumentConstraintDocuments: [{ type: "object", properties: { x: { maximum: 5 } } }],
      },
    };
    const connections =
      (await parent.store.read(base.sessionId)).session.integrationConnections ?? [];
    const narrow = compileSessionBindings({
      launchPolicy: { profileId: base.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: 1,
      requestedGrants: [grant],
    });
    await base.db
      .update(taskSession)
      .set(bindingSnapshotColumns(narrow, connections))
      .where(eq(taskSession.sessionId, base.sessionId));
    const child = compileSessionBindings({
      launchPolicy: { profileId: base.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: 1,
      parentAuthority: { sessionId: base.sessionId, snapshot: narrow },
    });
    await base.db.insert(task).values({
      id: childTask,
      type: "chat",
      createdByUserId: base.actorId,
      spawningSessionId: base.sessionId,
    });
    await base.db.insert(taskSession).values({
      taskId: childTask,
      sessionId: childId,
      profileId: base.profileId,
      integrationPrincipalId: base.actorId,
      ...bindingSnapshotColumns(child, connections),
    });
    const f = await coldFixture(
      { ...base, sessionId: childId, taskId: childTask },
      undefined,
      child,
    );
    let current = true;
    f.parentReader(async (id) => {
      expect(id).toBe(base.sessionId);
      if (!current) throw Error("parent stale");
      return narrow;
    });
    const candidate = await f.service.prepare(childId, child.bindingHash, f.actorId);
    const effective = candidate.immutableBindings.flatMap((b) => b.tools);
    expect(effective).toHaveLength(1);
    expect(effective[0]!.grant).toMatchObject({
      unrestricted: false,
      automationWrite: false,
      argumentConstraintDocuments: grant.mcp.argumentConstraintDocuments,
    });
    expect(candidate.ancestorAuthorityRefs[0]!.bindingHash).toBe(narrow.bindingHash);
    current = false;
    await expect(
      f.service.commit(childId, child.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("parent stale");
    expect(f.stats().recoverCalls).toBe(0);
    current = true;
    await finish(f, candidate.bindingHash);
  } finally {
    await base.db.delete(taskSession).where(eq(taskSession.sessionId, childId));
    await base.db.delete(task).where(eq(task.id, childTask));
    await parent.cleanup();
  }
});

test("new administrator-published leaf grants require a fresh reviewed cold candidate", async () => {
  const base = await reviewFixture([], 2);
  const f = await coldFixture(base);
  try {
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const full = (await f.profiles.get(f.profileId))!;
    await f.profiles.update(
      f.profileId,
      { ...full, integrationGrants: [full.integrationGrants[0]!] },
      { actorId: f.actorId, expectedRevision: 1 },
    );
    const candidate = await f.service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    expect(candidate.immutableBindings.flatMap((b) => b.tools)).toHaveLength(1);
    const narrow = (await f.profiles.get(f.profileId))!;
    await f.profiles.update(
      f.profileId,
      { ...narrow, integrationGrants: full.integrationGrants },
      { actorId: f.actorId, expectedRevision: 2 },
    );
    await expect(
      f.service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("stale");
    expect(f.stats().recoverCalls).toBe(0);
    const reviewed = await f.service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    expect(reviewed.profilePublication?.revision).toBe(3);
    expect(reviewed.immutableBindings.flatMap((b) => b.tools)).toHaveLength(2);
    await finish(f, reviewed.bindingHash);
  } finally {
    await f.cleanup();
  }
});

test("typed rebind returns drain details and requires the exact reviewed candidate", async () => {
  const { createClient, createRouterTransport, ConnectError } = await import("@connectrpc/connect");
  const { IntegrationService, McpRebindDrainSchema, McpRebindDrainReason } =
    await import("../gen/engram/app/v1/integration_pb.ts");
  const { mcpHandlers } = await import("../mcp/contracts.ts");
  const f = await coldFixture();
  try {
    const client = createClient(
      IntegrationService,
      createRouterTransport((router) =>
        router.service(
          IntegrationService,
          mcpHandlers({
            services: {
              enabled: true,
              journal: f.journal,
              repository: f.repository,
              runtime: {
                cancel: async () => {},
                prepareRebind: f.service.prepare,
                rebind: f.service.commit,
              },
            },
            connections: f.connections,
            getSession: async () => ({
              user: { id: f.actorId, role: "user", email: "user@example.test" },
            }),
            now: () => new Date(),
            connectionToProto: (row) => ({ id: row.id }),
          }),
        ),
      ),
    );
    const candidate = await client.prepareMcpRebind({
      sessionId: f.sessionId,
      expectedBindingHash: f.snapshot.bindingHash,
    });
    expect(candidate.bindingTools).toHaveLength(1);
    await expect(
      client.rebindMcp({
        sessionId: f.sessionId,
        expectedBindingHash: f.snapshot.bindingHash,
        expectedCandidateHash: "wrong",
      }),
    ).rejects.toThrow();
    try {
      await client.rebindMcp({
        sessionId: f.sessionId,
        expectedBindingHash: f.snapshot.bindingHash,
        expectedCandidateHash: candidate.bindingHash,
      });
      throw Error("expected drain");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      if (!(error instanceof ConnectError)) throw error;
      expect(error.findDetails(McpRebindDrainSchema)[0]?.reason).toBe(
        McpRebindDrainReason.DISPATCH_IN_PROGRESS,
      );
    }
    f.start();
    const result = await client.rebindMcp({
      sessionId: f.sessionId,
      expectedBindingHash: f.snapshot.bindingHash,
      expectedCandidateHash: candidate.bindingHash,
    });
    expect(result.bindingHash).toBe(
      (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!.bindingHash,
    );
    await f.db
      .update(profile)
      .set({ mcpGrantPublication: null })
      .where(eq(profile.id, f.profileId));
    await expect(
      client.prepareMcpRebind({ sessionId: f.sessionId, expectedBindingHash: result.bindingHash }),
    ).rejects.toThrow("administrator must publish");
  } finally {
    await f.cleanup();
  }
});
