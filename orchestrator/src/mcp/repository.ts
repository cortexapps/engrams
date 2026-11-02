import { notifyMcpAuthorityChanged } from "./authority-notification.ts";
import { and, desc, eq, sql } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { integrationConnection as connection, mcpCatalog as catalog } from "../db/schema.ts";
import { assertSchemaShape, reviewCatalog } from "./catalog.ts";
import { validateMcpConfig } from "./config.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import type { McpConnectionState, McpDiscoveredTool } from "./types.ts";

export function makeMcpRepository(db = getDb()) {
  return {
    async updateConfig(
      id: string,
      expectedRevision: number,
      input: { alias: string; displayName: string; config: unknown },
      now: Date,
    ) {
      const config = validateMcpConfig(input.config);
      const rows = await db
        .update(connection)
        .set({
          alias: input.alias,
          displayName: input.displayName,
          config,
          configRevision: sql`${connection.configRevision} + 1`,
          enabled: false,
          testedAt: null,
          updatedAt: now,
        })
        .where(
          and(
            eq(connection.id, id),
            eq(connection.provider, "mcp"),
            sql`${connection.mcpCredential}->'transition' IS NULL`,
            eq(connection.configRevision, expectedRevision),
          ),
        )
        .returning();
      if (!rows[0]) throw new Error("stale connection revision");
      notifyMcpAuthorityChanged();
      return rows[0];
    },
    /** Only a verified credential exchange may call this method. Refresh does not call it. */
    async setAuthorization(
      id: string,
      expectedConfigRevision: number,
      expectedGeneration: number,
      credentialIdentity: string,
      now: Date,
    ) {
      if (!credentialIdentity || credentialIdentity.length > 512)
        throw new Error("credential identity is required");
      const rows = await db
        .update(connection)
        .set({
          credentialIdentity,
          authGeneration: sql`${connection.authGeneration} + 1`,
          revoked: false,
          enabled: false,
          testedAt: null,
          updatedAt: now,
        })
        .where(
          and(
            eq(connection.id, id),
            eq(connection.provider, "mcp"),
            sql`${connection.mcpCredential}->'transition' IS NULL`,
            eq(connection.authGeneration, expectedGeneration),
            eq(connection.configRevision, expectedConfigRevision),
          ),
        )
        .returning();
      if (!rows[0]) throw new Error("stale authorization generation");
      notifyMcpAuthorityChanged();
      return rows[0];
    },
    async disconnect(id: string, expectedGeneration: number, now: Date) {
      const rows = await db
        .update(connection)
        .set({
          revoked: true,
          enabled: false,
          testedAt: null,
          authGeneration: sql`${connection.authGeneration} + 1`,
          updatedAt: now,
        })
        .where(
          and(
            eq(connection.id, id),
            eq(connection.provider, "mcp"),
            sql`${connection.mcpCredential}->'transition' IS NULL`,
            eq(connection.authGeneration, expectedGeneration),
          ),
        )
        .returning();
      if (!rows[0]) throw new Error("stale authorization generation");
      notifyMcpAuthorityChanged();
      return rows[0];
    },
    async setEnabled(id: string, expectedRevision: number, enabled: boolean, now: Date) {
      const result = await db.transaction(async (tx) => {
        const [row] = await tx
          .select()
          .from(connection)
          .where(and(eq(connection.id, id), eq(connection.provider, "mcp")))
          .for("update");
        if (!row || row.configRevision !== expectedRevision)
          throw new Error("stale connection revision");
        if (enabled) {
          const [active] = await tx
            .select()
            .from(catalog)
            .where(
              and(
                eq(catalog.connectionId, id),
                eq(catalog.kind, "published"),
                eq(catalog.revision, row.catalogRevision),
              ),
            );
          if (
            row.revoked ||
            !row.credentialIdentity ||
            !active ||
            active.configRevision !== row.configRevision ||
            active.authGeneration !== row.authGeneration ||
            active.credentialIdentity !== row.credentialIdentity
          )
            throw new Error("current reviewed catalog and authorization required");
        }
        const [updated] = await tx
          .update(connection)
          .set({ enabled, updatedAt: now })
          .where(eq(connection.id, id))
          .returning();
        return updated!;
      });
      notifyMcpAuthorityChanged();
      return result;
    },
    async saveDraft(
      id: string,
      expected: McpConnectionState,
      tools: McpDiscoveredTool[],
      actorId: string,
      now: Date,
    ) {
      expected = structuredClone(expected);
      tools = structuredClone(tools);
      if (
        tools.length > MCP_LIMITS.toolsPerSession ||
        Buffer.byteLength(canonicalJson(tools)) > MCP_LIMITS.manifestBytes
      )
        throw new Error("catalog is too large");
      const names = new Set<string>();
      for (const t of tools) {
        if (
          !t.upstreamName ||
          t.upstreamName.length > 256 ||
          names.has(t.upstreamName) ||
          typeof t.description !== "string"
        )
          throw new Error("invalid discovery");
        names.add(t.upstreamName);
        assertSchemaShape(t.inputSchema);
        if (t.outputSchema !== undefined) assertSchemaShape(t.outputSchema);
      }
      // Pick only untrusted metadata. Discovery never grants read or write authority.
      const discovered = tools.map((t) => ({
        upstreamName: t.upstreamName,
        description: t.description,
        inputSchema: t.inputSchema,
        ...(t.outputSchema === undefined ? {} : { outputSchema: t.outputSchema }),
      }));
      return db.transaction(async (tx) => {
        const [row] = await tx
          .select()
          .from(connection)
          .where(and(eq(connection.id, id), eq(connection.provider, "mcp")))
          .for("update");
        if (
          !row ||
          row.revoked ||
          row.configRevision !== expected.configRevision ||
          row.authGeneration !== expected.authGeneration ||
          row.credentialIdentity !== expected.credentialIdentity
        )
          throw new Error("stale discovery");
        const [draft] = await tx
          .insert(catalog)
          .values({
            id: crypto.randomUUID(),
            connectionId: id,
            kind: "draft",
            revision: 0,
            configRevision: row.configRevision,
            authGeneration: row.authGeneration,
            credentialIdentity: row.credentialIdentity,
            tools: discovered,
            fingerprint: canonicalDigest(discovered),
            actorId,
            createdAt: now,
          })
          .returning();
        return draft!;
      });
    },
    async publish(
      id: string,
      draftId: string,
      expectedRevision: number,
      input: readonly unknown[],
      actorId: string,
      now: Date,
    ) {
      const tools = reviewCatalog(input);
      const result = await db.transaction(async (tx) => {
        const [row] = await tx
          .select()
          .from(connection)
          .where(and(eq(connection.id, id), eq(connection.provider, "mcp")))
          .for("update");
        const [draft] = await tx
          .select()
          .from(catalog)
          .where(
            and(eq(catalog.id, draftId), eq(catalog.connectionId, id), eq(catalog.kind, "draft")),
          );
        if (
          !row ||
          !draft ||
          row.revoked ||
          !row.credentialIdentity ||
          row.catalogRevision !== expectedRevision ||
          draft.configRevision !== row.configRevision ||
          draft.authGeneration !== row.authGeneration ||
          draft.credentialIdentity !== row.credentialIdentity
        )
          throw new Error("stale catalog revision or discovery");
        const originals = new Map(draft.tools.map((t) => [t.upstreamName, t]));
        for (const t of tools) {
          const original = originals.get(t.upstreamName);
          const metadata = {
            upstreamName: t.upstreamName,
            description: t.description,
            inputSchema: t.inputSchema,
            ...(t.outputSchema === undefined ? {} : { outputSchema: t.outputSchema }),
          };
          if (!original || canonicalDigest(original) !== canonicalDigest(metadata))
            throw new Error("review cannot change discovered metadata");
        }
        const revision = expectedRevision + 1;
        const [published] = await tx
          .insert(catalog)
          .values({
            id: crypto.randomUUID(),
            connectionId: id,
            kind: "published",
            revision,
            configRevision: row.configRevision,
            authGeneration: row.authGeneration,
            credentialIdentity: row.credentialIdentity,
            tools,
            fingerprint: canonicalDigest(tools),
            actorId,
            createdAt: now,
          })
          .returning();
        await tx
          .update(connection)
          .set({ catalogRevision: revision, enabled: false, testedAt: now, updatedAt: now })
          .where(eq(connection.id, id));
        return published!;
      });
      notifyMcpAuthorityChanged();
      return result;
    },
    async getCatalog(id: string, revision: number) {
      return (
        (
          await db
            .select()
            .from(catalog)
            .where(
              and(
                eq(catalog.connectionId, id),
                eq(catalog.kind, "published"),
                eq(catalog.revision, revision),
              ),
            )
        )[0] ?? null
      );
    },
    async listCatalogs(id: string) {
      return db
        .select()
        .from(catalog)
        .where(eq(catalog.connectionId, id))
        .orderBy(desc(catalog.createdAt))
        .limit(100);
    },
  };
}
export type McpRepository = ReturnType<typeof makeMcpRepository>;
