import { expect, test } from "bun:test";
import { eq, inArray } from "drizzle-orm";
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
      await db.insert(profile).values({
        id: pid,
        name: "Binding test",
        imageId: "image",
        harness: "claude",
        integrationGrants: grants,
      });
      await db.insert(integrationConnection).values({
        id: cid,
        alias: `docs-${cid}`,
        provider: "mcp",
        displayName: "Docs",
        config: source.config,
        enabled: true,
        ...source.state,
      });
      await db.insert(mcpCatalog).values({
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

test.skipIf(!reachable)(
  "live authority reads all current catalogs in two queries and retains unavailable connections",
  async () => {
    const { drizzle } = await import("drizzle-orm/node-postgres");
    const { getPool } = await import("../db/client.ts");
    const schema = await import("../db/schema.ts");
    const { profile, integrationConnection, mcpCatalog } = schema;
    const { readLiveBindingAuthority } = await import("../mcp/binding-store.ts");
    const { canonicalDigest } = await import("../mcp/hash.ts");
    const { reviewCatalog } = await import("../mcp/catalog.ts");
    const { checkBindingCurrent } = await import("../mcp/session-bindings.ts");
    const queries: string[] = [];
    const db = drizzle(getPool(), {
      schema,
      logger: { logQuery: (query) => queries.push(query) },
    });
    const pid = crypto.randomUUID();
    const ids = {
      current: crypto.randomUUID(),
      stale: crypto.randomUUID(),
      draft: crypto.randomUUID(),
      credentials: crypto.randomUUID(),
      revoked: crypto.randomUUID(),
      missing: crypto.randomUUID(),
      other: crypto.randomUUID(),
    };
    const fixture = bindingAuthority(),
      source = fixture.connections[0]!;
    const grants = [ids.current, ids.revoked].map((connectionId) => ({
      ...fixture.profileGrants[0]!,
      connectionId,
    }));
    const updatedTools = reviewCatalog([
      { ...source.tools[0]!, description: "Search current documents" },
    ]);
    const catalogs = [
      { connectionId: ids.current, revision: 1, kind: "published" as const },
      { connectionId: ids.current, revision: 2, kind: "published" as const },
      { connectionId: ids.stale, revision: 1, kind: "published" as const },
      { connectionId: ids.draft, revision: 0, kind: "draft" as const },
      { connectionId: ids.credentials, revision: 1, kind: "published" as const },
      { connectionId: ids.revoked, revision: 1, kind: "published" as const },
      { connectionId: ids.current, revision: 3, kind: "published" as const },
    ].map((value) => {
      const tools =
        value.revision === 3
          ? [{ ...updatedTools[0]!, fingerprint: "invalid" }]
          : value.revision === 2
            ? updatedTools
            : source.tools;
      return {
        ...value,
        id: crypto.randomUUID(),
        configRevision: 1,
        authGeneration: value.connectionId === ids.credentials ? 2 : 1,
        credentialIdentity: source.state.credentialIdentity,
        tools,
        fingerprint: canonicalDigest(tools),
        actorId: "test-operator",
        createdAt: new Date(),
      };
    });
    async function read() {
      queries.length = 0;
      const authority = await readLiveBindingAuthority(pid, fixture.operatorPolicies, db);
      expect(queries).toHaveLength(2);
      return authority;
    }
    try {
      await db.insert(profile).values({
        id: pid,
        name: "Catalog join test",
        imageId: "image",
        harness: "claude",
        integrationGrants: grants,
      });
      await db.insert(integrationConnection).values(
        Object.entries(ids).map(([name, id]) => ({
          id,
          alias: `docs-${id}`,
          provider: name === "other" ? "github" : "mcp",
          displayName: "Docs",
          config: name === "other" ? {} : source.config,
          enabled: true,
          ...source.state,
          catalogRevision: name === "stale" ? 2 : name === "draft" ? 0 : 1,
        })),
      );
      await db.insert(mcpCatalog).values(catalogs);
      const authority = await read();
      const connections = new Map(
        authority.connections.map((connection) => [connection.id, connection]),
      );
      expect(connections.has(ids.other)).toBe(false);
      expect(connections.get(ids.current)).toMatchObject({
        catalogId: catalogs[0]!.id,
        enabled: true,
        tools: source.tools,
      });
      for (const id of [ids.stale, ids.draft, ids.missing]) {
        expect(connections.get(id)).toMatchObject({ catalogId: "", enabled: false, tools: [] });
      }
      expect(connections.get(ids.credentials)).toMatchObject({
        catalogId: catalogs[4]!.id,
        enabled: false,
        tools: [],
      });
      const snapshot = compileSessionBindings({
        launchPolicy: { profileId: pid, integrationGrants: grants },
        liveAuthority: authority,
        bindingEpoch: 0,
      });
      expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(true);
      await db
        .update(integrationConnection)
        .set({ catalogRevision: 2 })
        .where(eq(integrationConnection.id, ids.current));
      await db
        .update(integrationConnection)
        .set({ revoked: true })
        .where(eq(integrationConnection.id, ids.revoked));
      const current = await read();
      expect(current.connections.find((connection) => connection.id === ids.current)).toMatchObject(
        {
          catalogId: catalogs[1]!.id,
          enabled: true,
          tools: updatedTools,
          state: { catalogRevision: 2 },
        },
      );
      expect(current.connections.find((connection) => connection.id === ids.revoked)).toMatchObject(
        {
          catalogId: catalogs[5]!.id,
          state: { revoked: true },
        },
      );
      expect(checkBindingCurrent({ snapshot, liveAuthority: current })).toMatchObject({
        current: false,
        staleReasons: expect.arrayContaining(["catalogRevision", "connection_unavailable"]),
      });
      await db
        .update(integrationConnection)
        .set({ catalogRevision: 3 })
        .where(eq(integrationConnection.id, ids.current));
      await expect(readLiveBindingAuthority(pid, fixture.operatorPolicies, db)).rejects.toThrow(
        "MCP catalog fingerprint mismatch",
      );
    } finally {
      await db.delete(mcpCatalog).where(inArray(mcpCatalog.connectionId, Object.values(ids)));
      await db
        .delete(integrationConnection)
        .where(inArray(integrationConnection.id, Object.values(ids)));
      await db.delete(profile).where(eq(profile.id, pid));
    }
  },
);
