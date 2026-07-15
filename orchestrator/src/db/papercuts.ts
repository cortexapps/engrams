/** Papercut data-access seam. Drizzle-backed by default and injectable in tests. */

import { desc, eq, isNull } from "drizzle-orm";

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
  taskId: string | null;
  profileId: string | null;
  userId: string | null;
}

export interface PapercutRow extends PapercutInput {
  id: string;
  fixTaskId: string | null;
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
  setFixTask(id: string, taskId: string): Promise<void>;
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
    taskId: row.taskId ?? null,
    profileId: row.profileId ?? null,
    userId: row.userId ?? null,
    fixTaskId: row.fixTaskId ?? null,
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
  taskId: papercutTable.taskId,
  profileId: papercutTable.profileId,
  userId: papercutTable.userId,
  fixTaskId: papercutTable.fixTaskId,
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
      await db.insert(papercutTable).values({ id, ...row });
      return id;
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

    async setFixTask(id, taskId) {
      await db
        .update(papercutTable)
        .set({ fixTaskId: taskId })
        .where(eq(papercutTable.id, id));
    },
  };
}
