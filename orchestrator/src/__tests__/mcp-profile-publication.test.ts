import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { user, profile } from "../db/schema.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { canonicalDigest } from "../mcp/hash.ts";

test("MCP publication requires current admin and exact explicit revision; unrelated edits preserve provenance", async () => {
  const f = await reviewFixture();
  try {
    const store = makeProfileStore(f.db);
    const input = (await store.get(f.profileId))!;
    await expect(
      store.update(f.profileId, input, { actorId: f.actorId, expectedRevision: 0 }),
    ).rejects.toThrow("administrator");
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const published = await store.update(f.profileId, input, {
      actorId: f.actorId,
      expectedRevision: 0,
    });
    expect(published!.mcpGrantPublication).toEqual({
      revision: 1,
      actorId: f.actorId,
      grantsHash: canonicalDigest(input.integrationGrants),
    });
    await expect(
      store.update(f.profileId, input, { actorId: f.actorId, expectedRevision: 0 }),
    ).rejects.toThrow("revision");
    const unchanged = await store.update(f.profileId, { ...input, name: "Changed label" });
    expect(unchanged!.mcpGrantPublication).toEqual(published!.mcpGrantPublication);
    await f.db
      .update(profile)
      .set({ mcpGrantPublication: null })
      .where(eq(profile.id, f.profileId));
    expect(
      (await store.update(f.profileId, { ...input, name: "Legacy label" }))!.mcpGrantPublication,
    ).toBeNull();
    await expect(store.update(f.profileId, { ...input, integrationGrants: [] })).rejects.toThrow(
      "publication",
    );
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    await expect(
      store.update(f.profileId, input, { actorId: f.actorId, expectedRevision: 0 }),
    ).rejects.toThrow("administrator");
  } finally {
    await f.cleanup();
  }
});

test("publication waits for a current-role write and concurrent revision CAS has one winner", async () => {
  const f = await reviewFixture();
  const { sql } = await import("drizzle-orm");
  try {
    const store = makeProfileStore(f.db),
      input = (await store.get(f.profileId))!;
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    let unlock!: () => void, locked!: (pid: number) => void;
    const release = new Promise<void>((resolve) => {
      unlock = resolve;
    });
    const ready = new Promise<number>((resolve) => {
      locked = resolve;
    });
    const mutation = f.db.transaction(async (tx) => {
      await tx.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
      const rows = await tx.execute<{ pid: number }>(sql`SELECT pg_backend_pid() AS pid`);
      locked(Number(rows.rows[0]!.pid));
      await release;
    });
    const blocker = await ready;
    const attempt = store
      .update(f.profileId, input, { actorId: f.actorId, expectedRevision: 0 })
      .then(
        () => "allowed",
        () => "denied",
      );
    try {
      const deadline = Date.now() + 2000;
      let blocked = false;
      while (Date.now() < deadline) {
        const rows = await f.db.execute<{ waiting: boolean }>(
          sql`SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE ${blocker} = ANY(pg_blocking_pids(pid))) AS waiting`,
        );
        if (rows.rows[0]?.waiting) {
          blocked = true;
          break;
        }
        await Bun.sleep(5);
      }
      expect(blocked).toBe(true);
    } finally {
      unlock();
    }
    await mutation;
    expect(await attempt).toBe("denied");
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const results = await Promise.allSettled(
      [1, 2].map(() =>
        store.update(f.profileId, input, { actorId: f.actorId, expectedRevision: 0 }),
      ),
    );
    expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
    expect((await store.get(f.profileId))!.mcpGrantPublication?.revision).toBe(1);
  } finally {
    await f.cleanup();
  }
});

test("stored resolver permits current unknown write policy but rejects terminal identity", async () => {
  const f = await reviewFixture();
  const { createClient, createRouterTransport } = await import("@connectrpc/connect");
  const { SessionPolicyService } = await import("../gen/engram/mcp/v1/session_policy_pb.ts");
  const { makeStoredSessionBindingResolver } = await import("../mcp/binding-store.ts");
  let current = true;
  const control = createClient(
    SessionPolicyService,
    createRouterTransport((router) =>
      router.service(SessionPolicyService, {
        readWritePolicy: async () => ({
          state: {
            policy: {
              ref: f.input.requestPolicyContext.acceptedPolicyRef,
              current,
              initialized: false,
              writeEligibility: "unknown",
            },
          },
        }),
      }),
    ),
  );
  try {
    const resolver = makeStoredSessionBindingResolver(
      {
        control,
        readOperatorPolicies: async () => f.authority.operatorPolicies,
        enforceManagedDeny: async (expected) => expected,
      },
      f.db,
    );
    const { readLiveBindingAuthority, bindingSnapshotColumns } =
      await import("../mcp/binding-store.ts");
    const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
    const { taskSession } = await import("../db/schema.ts");
    const live = await readLiveBindingAuthority(f.profileId, f.authority.operatorPolicies, f.db);
    const snapshot = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: 0,
    });
    await f.db
      .update(taskSession)
      .set(
        bindingSnapshotColumns(snapshot, [
          {
            id: f.connection.id,
            alias: f.connection.alias,
            provider: "mcp",
            displayName: f.connection.displayName,
            config: f.connection.config,
          },
        ]),
      )
      .where(eq(taskSession.sessionId, f.sessionId));
    await resolver.readCurrent(f.sessionId);
    expect(
      await resolver.resolveBoundTool({
        sessionId: f.sessionId,
        modelToolName: f.input.resolvedTool.modelToolName,
      }),
    ).toBeDefined();
    current = false;
    expect(
      await resolver.resolveBoundTool({
        sessionId: f.sessionId,
        modelToolName: f.input.resolvedTool.modelToolName,
      }),
    ).toBeUndefined();
  } finally {
    await f.cleanup();
  }
});
