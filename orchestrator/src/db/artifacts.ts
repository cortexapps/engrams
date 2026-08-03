/** Artifact registry data-access seam. Drizzle-backed by default and
 * injectable in tests.
 *
 * An artifact row is the stable identity (ownership, visibility, title);
 * artifact_version rows are the append-only history, each pointing at a
 * coordinator `artifacts` row (`coordArtifactId`) that holds the bytes.
 */

import { and, count, desc, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  artifact as artifactTable,
  artifactVersion as artifactVersionTable,
} from "./schema.ts";

export type ArtifactVisibility = "private" | "org";

export interface ArtifactVersionInput {
  sessionId: string;
  taskId: string | null;
  coordArtifactId: string;
  mediaType: string;
  sizeBytes: number;
}

export interface ArtifactVersionRow extends ArtifactVersionInput {
  artifactId: string;
  version: number;
  createdAt: Date;
}

export interface ArtifactRow {
  id: string;
  ownerUserId: string | null;
  title: string;
  fileName: string;
  visibility: ArtifactVisibility;
  currentVersion: number;
  createdAt: Date;
  updatedAt: Date;
}

/** An artifact joined with its current version's media facts. */
export interface ArtifactListRow extends ArtifactRow {
  mediaType: string;
  sizeBytes: number;
}

export interface ArtifactWithVersions extends ArtifactListRow {
  versions: ArtifactVersionRow[]; // newest first
}

export interface ArtifactListOpts {
  /** Restrict to one owner (the "mine" scope). */
  ownerUserId?: string;
  /** Restrict to one visibility (the "shared" scope). */
  visibility?: ArtifactVisibility;
  /** 1-based page; 0/absent with pageSize 0 = unpaginated. */
  page: number;
  pageSize: number;
}

export interface ArtifactStore {
  /** Insert the artifact row and its version 1 in one transaction. */
  create(
    row: {
      id: string;
      ownerUserId: string | null;
      title: string;
      fileName: string;
    },
    version: ArtifactVersionInput,
  ): Promise<ArtifactWithVersions>;
  /** Append version currentVersion+1 and bump the artifact row. Returns
   * the updated artifact, or null when the artifact does not exist. */
  appendVersion(
    artifactId: string,
    version: ArtifactVersionInput,
    refresh: { fileName?: string; title?: string },
  ): Promise<ArtifactWithVersions | null>;
  get(id: string): Promise<ArtifactWithVersions | null>;
  list(opts: ArtifactListOpts): Promise<{ rows: ArtifactListRow[]; totalCount: number }>;
  setVisibility(id: string, visibility: ArtifactVisibility): Promise<ArtifactRow | null>;
  /** Hard delete (versions cascade). Returns whether a row existed. */
  delete(id: string): Promise<boolean>;
}

function toArtifactRow(row: typeof artifactTable.$inferSelect): ArtifactRow {
  return {
    id: row.id,
    ownerUserId: row.ownerUserId ?? null,
    title: row.title,
    fileName: row.fileName,
    visibility: row.visibility === "org" ? "org" : "private",
    currentVersion: row.currentVersion,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

function toVersionRow(
  row: typeof artifactVersionTable.$inferSelect,
): ArtifactVersionRow {
  return {
    artifactId: row.artifactId,
    version: row.version,
    sessionId: row.sessionId,
    taskId: row.taskId ?? null,
    coordArtifactId: row.coordArtifactId,
    mediaType: row.mediaType,
    sizeBytes: Number(row.sizeBytes),
    createdAt: row.createdAt,
  };
}

export function makeArtifactStore(db = getDb()): ArtifactStore {
  async function getWithVersions(id: string): Promise<ArtifactWithVersions | null> {
    const rows = await db
      .select()
      .from(artifactTable)
      .where(eq(artifactTable.id, id))
      .limit(1);
    const row = rows[0];
    if (!row) return null;
    const versions = (
      await db
        .select()
        .from(artifactVersionTable)
        .where(eq(artifactVersionTable.artifactId, id))
        .orderBy(desc(artifactVersionTable.version))
    ).map(toVersionRow);
    const current = versions.find((v) => v.version === row.currentVersion);
    return {
      ...toArtifactRow(row),
      mediaType: current?.mediaType ?? "application/octet-stream",
      sizeBytes: current?.sizeBytes ?? 0,
      versions,
    };
  }

  return {
    async create(row, version) {
      await db.transaction(async (tx) => {
        await tx.insert(artifactTable).values({
          id: row.id,
          ownerUserId: row.ownerUserId,
          title: row.title,
          fileName: row.fileName,
          visibility: "private",
          currentVersion: 1,
        });
        await tx.insert(artifactVersionTable).values({
          artifactId: row.id,
          version: 1,
          sessionId: version.sessionId,
          taskId: version.taskId,
          coordArtifactId: version.coordArtifactId,
          mediaType: version.mediaType,
          sizeBytes: version.sizeBytes,
        });
      });
      const created = await getWithVersions(row.id);
      if (!created) throw new Error("artifact vanished after create");
      return created;
    },

    async appendVersion(artifactId, version, refresh) {
      const appended = await db.transaction(async (tx) => {
        // Serialize concurrent appends on the artifact row.
        const rows = await tx
          .select()
          .from(artifactTable)
          .where(eq(artifactTable.id, artifactId))
          .for("update")
          .limit(1);
        const row = rows[0];
        if (!row) return false;
        const next = row.currentVersion + 1;
        await tx.insert(artifactVersionTable).values({
          artifactId,
          version: next,
          sessionId: version.sessionId,
          taskId: version.taskId,
          coordArtifactId: version.coordArtifactId,
          mediaType: version.mediaType,
          sizeBytes: version.sizeBytes,
        });
        await tx
          .update(artifactTable)
          .set({
            currentVersion: next,
            updatedAt: new Date(),
            ...(refresh.fileName !== undefined ? { fileName: refresh.fileName } : {}),
            ...(refresh.title !== undefined ? { title: refresh.title } : {}),
          })
          .where(eq(artifactTable.id, artifactId));
        return true;
      });
      if (!appended) return null;
      return getWithVersions(artifactId);
    },

    get: getWithVersions,

    async list(opts) {
      const conditions = [
        ...(opts.ownerUserId !== undefined
          ? [eq(artifactTable.ownerUserId, opts.ownerUserId)]
          : []),
        ...(opts.visibility !== undefined
          ? [eq(artifactTable.visibility, opts.visibility)]
          : []),
      ];
      const where = conditions.length > 0 ? and(...conditions) : undefined;

      const totalRows = await (where
        ? db.select({ n: count() }).from(artifactTable).where(where)
        : db.select({ n: count() }).from(artifactTable));
      const totalCount = Number(totalRows[0]?.n ?? 0);

      let query = db
        .select({
          artifact: artifactTable,
          version: artifactVersionTable,
        })
        .from(artifactTable)
        .innerJoin(
          artifactVersionTable,
          and(
            eq(artifactVersionTable.artifactId, artifactTable.id),
            eq(artifactVersionTable.version, artifactTable.currentVersion),
          ),
        )
        .orderBy(desc(artifactTable.updatedAt))
        .$dynamic();
      if (where) query = query.where(where);
      if (opts.pageSize > 0) {
        query = query
          .limit(opts.pageSize)
          .offset(Math.max(0, opts.page - 1) * opts.pageSize);
      }
      const rows = await query;
      return {
        rows: rows.map((r) => ({
          ...toArtifactRow(r.artifact),
          mediaType: r.version.mediaType,
          sizeBytes: Number(r.version.sizeBytes),
        })),
        totalCount,
      };
    },

    async setVisibility(id, visibility) {
      const rows = await db
        .update(artifactTable)
        .set({ visibility, updatedAt: new Date() })
        .where(eq(artifactTable.id, id))
        .returning();
      const row = rows[0];
      return row ? toArtifactRow(row) : null;
    },

    async delete(id) {
      const rows = await db
        .delete(artifactTable)
        .where(eq(artifactTable.id, id))
        .returning({ id: artifactTable.id });
      return rows.length > 0;
    },
  };
}
