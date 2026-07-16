/** Papercut data-access seam. Drizzle-backed by default and injectable in tests. */

import { and, desc, eq, isNull } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  papercut as papercutTable,
  profile as profileTable,
} from "./schema.ts";

export interface PapercutInput {
  summary: string;
  description: string;
  category: string;
  severity: string | null;
  tags: string[];
  sessionId: string;
  toolCallId: string | null;
  taskId: string | null;
  profileId: string | null;
  userId: string | null;
}

export interface PapercutRow extends PapercutInput {
  id: string;
  archivedAt: Date | null;
  createdAt: Date;
}

export interface PapercutListRow extends PapercutRow {
  profileName: string | null;
  profileIcon: string | null;
}

export interface PapercutStore {
  insert(row: PapercutInput): Promise<string>;
  list(opts: { includeArchived: boolean; limit: number }): Promise<PapercutListRow[]>;
  get(id: string): Promise<PapercutRow | null>;
  setArchived(id: string, archived: boolean): Promise<void>;
}

function toRow(row: typeof papercutTable.$inferSelect): PapercutRow {
  return {
    id: row.id,
    summary: row.summary,
    description: row.description,
    category: row.category,
    severity: row.severity ?? null,
    tags: row.tags ?? [],
    sessionId: row.sessionId,
    toolCallId: row.toolCallId ?? null,
    taskId: row.taskId ?? null,
    profileId: row.profileId ?? null,
    userId: row.userId ?? null,
    archivedAt: row.archivedAt ?? null,
    createdAt: row.createdAt,
  };
}

const listSelection = {
  id: papercutTable.id,
  summary: papercutTable.summary,
  description: papercutTable.description,
  category: papercutTable.category,
  severity: papercutTable.severity,
  tags: papercutTable.tags,
  sessionId: papercutTable.sessionId,
  toolCallId: papercutTable.toolCallId,
  taskId: papercutTable.taskId,
  profileId: papercutTable.profileId,
  userId: papercutTable.userId,
  archivedAt: papercutTable.archivedAt,
  createdAt: papercutTable.createdAt,
  profileName: profileTable.name,
  profileIcon: profileTable.icon,
};

export function makePapercutStore(
  db: ReturnType<typeof getDb> = getDb(),
): PapercutStore {
  return {
    async insert(row) {
      const id = crypto.randomUUID();
      const inserted = await db
        .insert(papercutTable)
        .values({ id, ...row })
        .onConflictDoNothing({
          target: [papercutTable.sessionId, papercutTable.toolCallId],
        })
        .returning({ id: papercutTable.id });
      if (inserted[0]) return inserted[0].id;

      if (row.toolCallId == null) {
        throw new Error("papercut insert unexpectedly conflicted without a tool call id");
      }
      const existing = await db
        .select({ id: papercutTable.id })
        .from(papercutTable)
        .where(
          and(
            eq(papercutTable.sessionId, row.sessionId),
            eq(papercutTable.toolCallId, row.toolCallId),
          ),
        )
        .limit(1);
      if (!existing[0]) {
        throw new Error("conflicting papercut row was not found after insert replay");
      }
      return existing[0].id;
    },

    async list({ includeArchived, limit }) {
      const rows = includeArchived
        ? await db
            .select(listSelection)
            .from(papercutTable)
            .leftJoin(profileTable, eq(papercutTable.profileId, profileTable.id))
            .orderBy(desc(papercutTable.createdAt))
            .limit(limit)
        : await db
            .select(listSelection)
            .from(papercutTable)
            .leftJoin(profileTable, eq(papercutTable.profileId, profileTable.id))
            .where(isNull(papercutTable.archivedAt))
            .orderBy(desc(papercutTable.createdAt))
            .limit(limit);
      return rows.map((row) => ({
        ...toRow(row),
        profileName: row.profileName ?? null,
        profileIcon: row.profileIcon ?? null,
      }));
    },

    async get(id) {
      const rows = await db
        .select()
        .from(papercutTable)
        .where(eq(papercutTable.id, id))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

    async setArchived(id, archived) {
      await db
        .update(papercutTable)
        .set({ archivedAt: archived ? new Date() : null })
        .where(eq(papercutTable.id, id));
    },
  };
}
