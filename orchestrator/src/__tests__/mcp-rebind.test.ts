import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { taskSession } from "../db/schema.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeMcpRebind } from "../mcp/rebind.ts";

test("explicit reviewed rebind installs exact deny acknowledgement before atomic exposure", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        expect(previous).toEqual(hostState);
        const [row] = await f.db
          .select()
          .from(taskSession)
          .where(eq(taskSession.sessionId, f.sessionId));
        expect(row!.mcpBindingEnvelope!.bindingHash).toBe(f.snapshot.bindingHash);
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    const result = await service.commit(
      f.sessionId,
      f.snapshot.bindingHash,
      candidate.bindingHash,
      f.actorId,
    );
    expect(result.bindingHash).toBe(candidate.bindingHash);
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(row!.mcpBindingEnvelope!.bindingHash).toBe(candidate.bindingHash);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("typed rebind requires the exact reviewed candidate", async () => {
  const { createClient } = await import("@connectrpc/connect");
  const { createConnectTransport } = await import("@connectrpc/connect-node");
  const { Hono } = await import("hono");
  const { buildServer } = await import("../server.ts");
  const { IntegrationService } = await import("../gen/engram/app/v1/integration_pb.ts");
  const { McpRebindDrainReason, McpRebindDrainSchema } =
    await import("../gen/engram/app/v1/integration_pb.ts");
  const { McpRebindDraining } = await import("../mcp/rebind.ts");
  const { ConnectError } = await import("@connectrpc/connect");
  const { mcpHandlers } = await import("../mcp/contracts.ts");
  const f = await effectFixture({ classification: "read" });
  try {
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        expect(previous).toEqual(hostState);
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const server = buildServer(new Hono(), (router) =>
      router.service(
        IntegrationService,
        mcpHandlers({
          services: {
            enabled: true,
            journal: f.journal,
            repository: f.repository,
            runtime: {
              cancel: async (id) => f.driver.cancel(id),
              prepareRebind: service.prepare,
              rebind: async (session, old, candidate, actor) => {
                if (candidate === "drain")
                  throw new McpRebindDraining({
                    reason: "unknown_intent",
                    retryAfter: "2030-01-01T00:00:00.000Z",
                  });
                return service.commit(session, old, candidate, actor);
              },
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
    );
    const url = await new Promise<string>((resolve) =>
      server.listen(0, "127.0.0.1", () =>
        resolve(
          `http://127.0.0.1:${(server.address() as import("node:net").AddressInfo).port}/rpc`,
        ),
      ),
    );
    const client = createClient(
      IntegrationService,
      createConnectTransport({ baseUrl: url, httpVersion: "1.1" }),
    );
    try {
      const candidate = await client.prepareMcpRebind({
        sessionId: f.sessionId,
        expectedBindingHash: f.snapshot.bindingHash,
      });
      expect(candidate.bindingHash).toBeTruthy();
      expect(candidate.bindingTools.length).toBeGreaterThan(0);
      try {
        await client.rebindMcp({
          sessionId: f.sessionId,
          expectedBindingHash: f.snapshot.bindingHash,
          expectedCandidateHash: "drain",
        });
        throw Error("expected drain");
      } catch (error) {
        expect(error).toBeInstanceOf(ConnectError);
        const detail = (error as InstanceType<typeof ConnectError>).findDetails(
          McpRebindDrainSchema,
        )[0];
        expect(detail?.reason).toBe(McpRebindDrainReason.UNKNOWN_INTENT);
        expect(detail?.retryAfter).toBe("2030-01-01T00:00:00.000Z");
      }
      await expect(
        client.rebindMcp({
          sessionId: f.sessionId,
          expectedBindingHash: f.snapshot.bindingHash,
          expectedCandidateHash: "changed",
        }),
      ).rejects.toThrow();
      const committed = await client.rebindMcp({
        sessionId: f.sessionId,
        expectedBindingHash: f.snapshot.bindingHash,
        expectedCandidateHash: candidate.bindingHash,
      });
      expect(committed.bindingHash).toBe(candidate.bindingHash);
    } finally {
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    }
  } finally {
    await f.cleanup();
  }
}, 20000);

test("lost host acknowledgement leaves old snapshot and preserves installed denies on retry", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    let lose = true;
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        expect(previous).toEqual(hostState);
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        if (lose) {
          lose = false;
          throw Error("lost acknowledgement");
        }
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const first = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, first.bindingHash, f.actorId),
    ).rejects.toThrow("lost acknowledgement");
    const [unchanged] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(unchanged!.mcpBindingEnvelope!.bindingHash).toBe(f.snapshot.bindingHash);
    const retry = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    expect(retry.managedDenyPolicy.revision).toBeGreaterThan(first.managedDenyPolicy.revision);
    expect(
      first.managedDenyPolicy.hosts.every((name) => retry.managedDenyPolicy.hosts.includes(name)),
    ).toBe(true);
    await service.commit(f.sessionId, f.snapshot.bindingHash, retry.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(retry.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("stale hashes, lost owner and changed connection cannot apply a reviewed candidate", async () => {
  const { user, integrationConnection } = await import("../db/schema.ts");
  const f = await effectFixture({ classification: "read" });
  try {
    const hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    let applied = 0;
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
      ) {
        applied++;
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await expect(
      service.commit(f.sessionId, "stale", candidate.bindingHash, f.actorId),
    ).rejects.toThrow();
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, "stale", f.actorId),
    ).rejects.toThrow();
    await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow();
    await f.db.update(user).set({ banned: false }).where(eq(user.id, f.actorId));
    await f.db
      .update(integrationConnection)
      .set({ alias: "changed" })
      .where(eq(integrationConnection.id, f.connection.id));
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow();
    expect(applied).toBe(0);
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(row!.mcpBindingEnvelope!.bindingHash).toBe(f.snapshot.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("concurrent commits expose only one reviewed binding", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    let applied = 0;
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        expect(previous).toEqual(hostState);
        applied++;
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    const results = await Promise.allSettled([
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ]);
    expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(1);
    expect(applied).toBe(1);
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(row!.mcpBindingEnvelope!.bindingHash).toBe(candidate.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("physical placement change after host acknowledgement prevents exposure", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    let epoch = f.snapshot.bindingEpoch;
    const hostState = {
      sessionId: f.sessionId,
      bindingEpoch: epoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
      ) {
        epoch++;
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => epoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("placement changed");
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(row!.mcpBindingEnvelope!.bindingHash).toBe(f.snapshot.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("changed grant publication rejects commit before host apply", async () => {
  const { profile } = await import("../db/schema.ts");
  const f = await effectFixture({ classification: "read" });
  try {
    const hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    let applied = 0;
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
      ) {
        applied++;
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await f.db
      .update(profile)
      .set({ mcpGrantPublication: null })
      .where(eq(profile.id, f.profileId));
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("publication");
    expect(applied).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("child rebind retains the exact current parent intersection", async () => {
  const { task } = await import("../db/schema.ts");
  const { readLiveBindingAuthority, bindingSnapshotColumns } =
    await import("../mcp/binding-store.ts");
  const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
  const f = await effectFixture({ classification: "write" });
  const childTask = crypto.randomUUID(),
    childId = crypto.randomUUID();
  try {
    const live = await readLiveBindingAuthority(f.profileId, f.operatorPolicies, f.db);
    expect(live.profileGrants.length).toBeGreaterThan(1);
    const firstGrant = live.profileGrants[0]!;
    const parentGrant = {
      ...firstGrant,
      mcp: {
        ...firstGrant.mcp!,
        unrestricted: false,
        automationWrite: false,
        argumentConstraintDocuments: [{ type: "object", properties: { x: { maximum: 5 } } }],
      },
    };
    const parent = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: f.snapshot.bindingEpoch,
      requestedGrants: [parentGrant],
    });
    const [original] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    const parentColumns = bindingSnapshotColumns(parent, original!.integrationConnections ?? []);
    await f.db.update(taskSession).set(parentColumns).where(eq(taskSession.sessionId, f.sessionId));
    const child = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: 0,
      requestedGrants: [live.profileGrants[0]!],
      parentAuthority: { sessionId: f.sessionId, snapshot: parent },
    });
    await f.db.insert(task).values({
      id: childTask,
      type: "chat",
      createdByUserId: f.actorId,
      spawningSessionId: f.sessionId,
    });
    await f.db.insert(taskSession).values({
      taskId: childTask,
      sessionId: childId,
      profileId: f.profileId,
      integrationPrincipalId: f.actorId,
      ...bindingSnapshotColumns(child, original!.integrationConnections ?? []),
    });
    let hostState = {
      sessionId: childId,
      bindingEpoch: 0,
      bindingHash: child.bindingHash,
      policy: child.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    let parentCurrent = true;
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => 0,
      readParentCurrent: async (id) => {
        if (!parentCurrent) throw Error("parent stale");
        return f.resolver.readCurrent(id);
      },
    });
    const candidate = await service.prepare(childId, child.bindingHash, f.actorId);
    expect(candidate.ancestorAuthorityRefs[0]!.bindingHash).toBe(parent.bindingHash);
    expect(candidate.immutableBindings.flatMap((binding) => binding.tools)).toHaveLength(1);
    const effective = candidate.immutableBindings[0]!.tools[0]!.grant;
    expect(effective.automationWrite).toBe(false);
    expect(effective.unrestricted).toBe(false);
    expect(effective.argumentConstraintDocuments).toContainEqual({
      type: "object",
      properties: { x: { maximum: 5 } },
    });
    expect(candidate.profileGrantsHash).toBe(parent.profileGrantsHash);
    parentCurrent = false;
    await expect(
      service.commit(childId, child.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("parent stale");
    parentCurrent = true;
    await service.commit(childId, child.bindingHash, candidate.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
  } finally {
    await f.db.delete(taskSession).where(eq(taskSession.sessionId, childId));
    await f.db.delete(task).where(eq(task.id, childTask));
    await f.cleanup();
  }
}, 20000);

test("new published leaf grants expand only through the exact reviewed rebind", async () => {
  const { readLiveBindingAuthority, bindingSnapshotColumns } =
    await import("../mcp/binding-store.ts");
  const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
  const { profile } = await import("../db/schema.ts");
  const f = await effectFixture({ classification: "write" });
  try {
    const live = await readLiveBindingAuthority(f.profileId, f.operatorPolicies, f.db);
    const old = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: f.snapshot.bindingEpoch,
      requestedGrants: [live.profileGrants[0]!],
    });
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    await f.db
      .update(taskSession)
      .set(bindingSnapshotColumns(old, row!.integrationConnections ?? []))
      .where(eq(taskSession.sessionId, f.sessionId));
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: old.bindingEpoch,
      bindingHash: old.bindingHash,
      policy: old.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => old.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, old.bindingHash, f.actorId);
    expect(old.immutableBindings.flatMap((binding) => binding.tools)).toHaveLength(1);
    expect(candidate.immutableBindings.flatMap((binding) => binding.tools)).toHaveLength(2);
    await expect(
      service.commit(f.sessionId, old.bindingHash, "unreviewed", f.actorId),
    ).rejects.toThrow();
    expect(hostState.bindingHash).toBe(old.bindingHash);
    await service.commit(f.sessionId, old.bindingHash, candidate.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
    await f.db
      .update(profile)
      .set({ mcpGrantPublication: null })
      .where(eq(profile.id, f.profileId));
    await expect(service.prepare(f.sessionId, candidate.bindingHash, f.actorId)).rejects.toThrow(
      "publication",
    );
  } finally {
    await f.cleanup();
  }
}, 20000);

test("a newly administrator-published leaf grant is adopted only after candidate review", async () => {
  const { user } = await import("../db/schema.ts");
  const { makeProfileStore } = await import("../db/profiles.ts");
  const { readLiveBindingAuthority, bindingSnapshotColumns } =
    await import("../mcp/binding-store.ts");
  const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
  const f = await effectFixture({ classification: "write" });
  try {
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const store = makeProfileStore(f.db);
    const full = (await store.get(f.profileId))!;
    const grants = full.integrationGrants;
    expect(grants.length).toBeGreaterThan(1);
    await store.update(
      f.profileId,
      { ...full, integrationGrants: [grants[0]!] },
      { actorId: f.actorId, expectedRevision: 1 },
    );
    const narrowLive = await readLiveBindingAuthority(f.profileId, f.operatorPolicies, f.db);
    const old = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: narrowLive.profileGrants },
      liveAuthority: narrowLive,
      bindingEpoch: f.snapshot.bindingEpoch,
    });
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    await f.db
      .update(taskSession)
      .set(bindingSnapshotColumns(old, row!.integrationConnections ?? []))
      .where(eq(taskSession.sessionId, f.sessionId));
    const narrowProfile = (await store.get(f.profileId))!;
    await store.update(
      f.profileId,
      { ...narrowProfile, integrationGrants: grants },
      { actorId: f.actorId, expectedRevision: 2 },
    );
    let hostState = {
      sessionId: f.sessionId,
      bindingEpoch: old.bindingEpoch,
      bindingHash: old.bindingHash,
      policy: old.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
        policy: typeof hostState.policy,
      ) {
        hostState = { ...hostState, bindingHash: expected.bindingHash, policy };
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => old.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, old.bindingHash, f.actorId);
    expect(candidate.profilePublication!.revision).toBe(3);
    expect(candidate.immutableBindings.flatMap((binding) => binding.tools)).toHaveLength(2);
    await expect(
      service.commit(f.sessionId, old.bindingHash, "wrong", f.actorId),
    ).rejects.toThrow();
    expect(hostState.bindingHash).toBe(old.bindingHash);
    await service.commit(f.sessionId, old.bindingHash, candidate.bindingHash, f.actorId);
    expect(hostState.bindingHash).toBe(candidate.bindingHash);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("rebind remains closed when the trusted host producer is absent", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    await expect(service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId)).rejects.toThrow(
      "unavailable",
    );
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, "candidate", f.actorId),
    ).rejects.toThrow("unavailable");
  } finally {
    await f.cleanup();
  }
}, 20000);

test("managed authority inserted during host acknowledgement prevents exposure", async () => {
  const { integrationConnection } = await import("../db/schema.ts");
  const f = await effectFixture({ classification: "read" });
  const insertedId = crypto.randomUUID();
  try {
    const hostState = {
      sessionId: f.sessionId,
      bindingEpoch: f.snapshot.bindingEpoch,
      bindingHash: f.snapshot.bindingHash,
      policy: f.snapshot.managedDenyPolicy,
    };
    const host = {
      async readApplied() {
        return structuredClone(hostState);
      },
      async compareAndApply(
        _previous: typeof hostState,
        expected: {
          bindingHash: string;
          sessionId: string;
          bindingEpoch: number;
          denyRevision: number;
          denyDigest: string;
        },
      ) {
        const [source] = await f.db
          .select()
          .from(integrationConnection)
          .where(eq(integrationConnection.id, f.connection.id));
        await f.db
          .insert(integrationConnection)
          .values({ ...source!, id: insertedId, alias: `new-managed-${insertedId}` });
        return expected;
      },
    };
    const service = makeMcpRebind({
      db: f.db,
      deploymentId: f.deploymentId,
      host,
      readOperatorPolicies: async () => f.operatorPolicies,
      readBindingEpoch: async () => f.snapshot.bindingEpoch,
      readParentCurrent: async () => {
        throw Error("unexpected parent");
      },
    });
    const candidate = await service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
    await expect(
      service.commit(f.sessionId, f.snapshot.bindingHash, candidate.bindingHash, f.actorId),
    ).rejects.toThrow("managed authority changed");
    const [row] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    expect(row!.mcpBindingEnvelope!.bindingHash).toBe(f.snapshot.bindingHash);
  } finally {
    await f.db.delete(integrationConnection).where(eq(integrationConnection.id, insertedId));
    await f.cleanup();
  }
}, 20000);
