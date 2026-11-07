import { expect, test } from "bun:test";
import { eq, inArray } from "drizzle-orm";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { task, taskSession, profile, mcpCatalog } from "../db/schema.ts";
import {
  bindingSnapshotColumns,
  readLiveBindingAuthority,
  readStoredBindingSession,
} from "../mcp/binding-store.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { validateMcpConfig } from "../mcp/config.ts";
import { makeSessionBindingResolver } from "../mcp/session-resolver.ts";

test("currentness reads a deep multi-connection graph within the one-second fence", async () => {
  await verifyMcpTestDatabases();
  const f = await reviewFixture();
  const connectionIds: string[] = [],
    taskIds: string[] = [];
  try {
    const grants = [...f.authority.profileGrants];
    for (let i = 0; i < 15; i++) {
      const connection = await f.connections.create({
        alias: `test-${crypto.randomUUID()}`,
        provider: "mcp",
        displayName: "Graph tools",
        config: f.connection.config,
      });
      connectionIds.push(connection.id);
      await f.repository.setAuthorization(connection.id, 1, 1, "test-account", f.now);
      const row = (await f.connections.get(connection.id))!;
      const tools = [
        {
          upstreamName: "Write.Record",
          description: "Graph tool",
          inputSchema: { type: "object" },
        },
      ];
      const draft = await f.repository.saveDraft(connection.id, row.mcp!, tools, f.actorId, f.now);
      await f.repository.publish(
        connection.id,
        draft.id,
        0,
        tools.map((t) => ({ ...t, classification: "write", enabled: true })),
        f.actorId,
        f.now,
      );
      await f.repository.setEnabled(connection.id, 1, true, f.now);
      grants.push({ ...grants[0]!, connectionId: connection.id });
    }
    await f.db
      .update(profile)
      .set({ integrationGrants: grants })
      .where(eq(profile.id, f.profileId));
    const live = await readLiveBindingAuthority(f.profileId, f.authority.operatorPolicies, f.db);
    const connections = live.connections.map((c) => ({
      id: c.id,
      alias: c.alias,
      displayName: c.alias,
      provider: "mcp",
      config: validateMcpConfig(c.config),
    }));
    let parent:
      | { sessionId: string; snapshot: ReturnType<typeof compileSessionBindings> }
      | undefined;
    for (let depth = 0; depth < 24; depth++) {
      const sessionId = crypto.randomUUID(),
        taskId = crypto.randomUUID();
      const snapshot = compileSessionBindings({
        launchPolicy: { profileId: f.profileId, integrationGrants: grants },
        liveAuthority: live,
        bindingEpoch: 1,
        ...(parent ? { parentAuthority: parent } : {}),
      });
      await f.db
        .insert(task)
        .values({
          id: taskId,
          type: "chat",
          createdByUserId: f.actorId,
          ...(parent ? { spawningSessionId: parent.sessionId } : {}),
        });
      taskIds.push(taskId);
      await f.db
        .insert(taskSession)
        .values({
          taskId,
          sessionId,
          profileId: f.profileId,
          integrationPrincipalId: f.actorId,
          ...bindingSnapshotColumns(snapshot, connections),
        });
      parent = { sessionId, snapshot };
    }
    let externalReads = 0;
    const resolver = makeSessionBindingResolver({
      readSession: (id) => readStoredBindingSession(id, f.db),
      readAuthority: (id) => readLiveBindingAuthority(id, f.authority.operatorPolicies, f.db),
      readBindingEpoch: async () => {
        externalReads++;
        return 1;
      },
      enforceManagedDeny: async (expected) => {
        externalReads++;
        return expected;
      },
    });
    const start = performance.now();
    expect((await resolver.readCurrent(parent!.sessionId)).bindingHash).toBe(
      parent!.snapshot.bindingHash,
    );
    const elapsed = performance.now() - start;
    console.info(
      `MCP graph: 24 sessions, 16 connections, ${externalReads} injected external reads, ${Math.round(elapsed)} ms`,
    );
    expect(externalReads).toBe(25);
    expect(elapsed).toBeLessThan(1000);
    await f.db.update(profile).set({ integrationGrants: [] }).where(eq(profile.id, f.profileId));
    await expect(resolver.readCurrent(parent!.sessionId)).rejects.toThrow("authority changed");
  } finally {
    for (const id of taskIds.reverse()) await f.db.delete(task).where(eq(task.id, id));
    if (connectionIds.length)
      await f.db.delete(mcpCatalog).where(inArray(mcpCatalog.connectionId, connectionIds));
    for (const id of connectionIds) await f.connections.delete(id);
    await f.cleanup();
  }
});
