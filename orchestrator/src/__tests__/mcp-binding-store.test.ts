import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { checkDb, getDb } from "../db/client.ts";
import { task, taskSession } from "../db/schema.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { bindingSnapshotColumns, readStoredBindingSession } from "../mcp/binding-store.ts";
import { bindingAuthority } from "./mcp-binding-fixture.ts";
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
test.skipIf(!reachable)(
  "binding envelope and existing connection snapshots round-trip atomically",
  async () => {
    const db = getDb(),
      taskId = crypto.randomUUID(),
      sessionId = crypto.randomUUID();
    const authority = bindingAuthority();
    const snapshot = compileSessionBindings({
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    });
    const columns = bindingSnapshotColumns(
      snapshot,
      authority.connections.map((c) => ({
        id: c.id,
        alias: c.alias,
        provider: "mcp",
        displayName: "Docs",
        config: c.config,
      })),
    );
    try {
      await db.insert(task).values({ id: taskId, type: "chat" });
      await expect(
        db.transaction(async (tx) => {
          await tx.insert(taskSession).values({ taskId, sessionId, ...columns });
          throw new Error("reservation rollback");
        }),
      ).rejects.toThrow("reservation rollback");
      expect(await readStoredBindingSession(sessionId, db)).toBeNull();
      await db.insert(taskSession).values({ taskId, sessionId, ...columns });
      expect((await readStoredBindingSession(sessionId, db))?.snapshot).toEqual(snapshot);
      expect(columns.mcpBindingEnvelope).not.toHaveProperty("immutableBindings");
      await db.update(task).set({ status: "cancelled" }).where(eq(task.id, taskId));
      expect((await readStoredBindingSession(sessionId, db))?.active).toBe(false);
      await db
        .update(taskSession)
        .set({ integrationConnections: [] })
        .where(eq(taskSession.sessionId, sessionId));
      await expect(readStoredBindingSession(sessionId, db)).rejects.toThrow();
    } finally {
      await db.delete(task).where(eq(task.id, taskId));
    }
  },
);
test.skipIf(!reachable)(
  "resolver reads current stored profile and immutable published catalog authority",
  async () => {
    const { profile, integrationConnection, mcpCatalog } = await import("../db/schema.ts");
    const { readLiveBindingAuthority } = await import("../mcp/binding-store.ts");
    const { canonicalDigest } = await import("../mcp/hash.ts");
    const { checkBindingCurrent } = await import("../mcp/session-bindings.ts");
    const { integrationSnapshotHash } = await import("../integrations/grants.ts");
    const db = getDb(),
      pid = crypto.randomUUID(),
      cid = crypto.randomUUID(),
      catalogId = crypto.randomUUID();
    const fixture = bindingAuthority(),
      source = fixture.connections[0]!;
    const grants = fixture.profileGrants.map((grant) => ({ ...grant, connectionId: cid }));
    try {
      await db
        .insert(profile)
        .values({
          id: pid,
          name: "Binding test",
          imageId: "image",
          harness: "claude",
          integrationGrants: grants,
        });
      await db
        .insert(integrationConnection)
        .values({
          id: cid,
          alias: `docs-${cid}`,
          provider: "mcp",
          displayName: "Docs",
          config: source.config,
          enabled: true,
          ...source.state,
        });
      await db
        .insert(mcpCatalog)
        .values({
          id: catalogId,
          connectionId: cid,
          kind: "published",
          revision: 1,
          configRevision: 1,
          authGeneration: 1,
          credentialIdentity: source.state.credentialIdentity,
          tools: source.tools,
          fingerprint: canonicalDigest(source.tools),
          actorId: "test-operator",
          createdAt: new Date(),
        });
      const authority = await readLiveBindingAuthority(pid, fixture.operatorPolicies, db);
      const snapshot = compileSessionBindings({
        launchPolicy: { profileId: pid, integrationGrants: grants },
        liveAuthority: authority,
        bindingEpoch: 0,
      });
      expect(snapshot.immutableBindings[0]!.catalogId).toBe(catalogId);
      const columns = bindingSnapshotColumns(snapshot, [
        {
          id: cid,
          alias: `docs-${cid}`,
          provider: "mcp",
          displayName: "Docs",
          config: source.config,
        },
      ]);
      const fullHash = integrationSnapshotHash({
        profileId: pid,
        integrationGrants: grants,
        ...columns,
      });
      expect(fullHash).not.toBe(
        integrationSnapshotHash({
          profileId: pid,
          integrationGrants: grants,
          integrationConnections: columns.integrationConnections,
        }),
      );
      expect(fullHash).toBe(
        integrationSnapshotHash(
          JSON.parse(JSON.stringify({ profileId: pid, integrationGrants: grants, ...columns })),
        ),
      );
      // Credential refresh metadata changes alone do not alter the authority tuple.
      await db
        .update(integrationConnection)
        .set({ testedAt: new Date(), updatedAt: new Date() })
        .where(eq(integrationConnection.id, cid));
      expect(
        checkBindingCurrent({
          snapshot,
          liveAuthority: await readLiveBindingAuthority(pid, fixture.operatorPolicies, db),
        }).current,
      ).toBe(true);
      await db.update(profile).set({ integrationGrants: [] }).where(eq(profile.id, pid));
      expect(
        checkBindingCurrent({
          snapshot,
          liveAuthority: await readLiveBindingAuthority(pid, fixture.operatorPolicies, db),
        }).current,
      ).toBe(false);
      await db.update(profile).set({ integrationGrants: grants }).where(eq(profile.id, pid));
      await db
        .update(integrationConnection)
        .set({ authGeneration: 2 })
        .where(eq(integrationConnection.id, cid));
      expect(
        checkBindingCurrent({
          snapshot,
          liveAuthority: await readLiveBindingAuthority(pid, fixture.operatorPolicies, db),
        }).current,
      ).toBe(false);
    } finally {
      await db.delete(mcpCatalog).where(eq(mcpCatalog.id, catalogId));
      await db.delete(integrationConnection).where(eq(integrationConnection.id, cid));
      await db.delete(profile).where(eq(profile.id, pid));
    }
  },
);
