/** Pull-request reference data-access seam (ADR 0100). */

import { desc, eq, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import { prRef as prRefTable } from "./schema.ts";

export interface PrRefInput {
  repo: string;
  prNumber: number;
  authoringTaskId: string | null;
  sessionId: string;
  title: string;
  url: string;
  headBranch: string;
  baseBranch: string;
  observedAt: Date;
}

export interface PrRefRow extends PrRefInput {
  id: string;
}

export interface PrRefStore {
  upsert(row: PrRefInput): Promise<string>;
  listByTaskId(taskId: string): Promise<PrRefRow[]>;
  listBySessionId(sessionId: string): Promise<PrRefRow[]>;
}

function toRow(row: typeof prRefTable.$inferSelect): PrRefRow {
  return {
    id: row.id,
    repo: row.repo,
    prNumber: row.prNumber,
    authoringTaskId: row.authoringTaskId ?? null,
    sessionId: row.sessionId,
    title: row.title,
    url: row.url,
    headBranch: row.headBranch,
    baseBranch: row.baseBranch,
    observedAt: row.observedAt,
  };
}

export function makePrRefStore(
  db: ReturnType<typeof getDb> = getDb(),
): PrRefStore {
  const listWhere = async (
    column: typeof prRefTable.authoringTaskId | typeof prRefTable.sessionId,
    value: string,
  ): Promise<PrRefRow[]> => {
    const rows = await db
      .select()
      .from(prRefTable)
      .where(eq(column, value))
      .orderBy(desc(prRefTable.observedAt));
    return rows.map(toRow);
  };

  return {
    async upsert(row) {
      const id = crypto.randomUUID();
      const rows = await db
        .insert(prRefTable)
        .values({ id, ...row })
        .onConflictDoUpdate({
          target: [prRefTable.repo, prRefTable.prNumber],
          set: {
            title: row.title,
            url: row.url,
            headBranch: row.headBranch,
            baseBranch: row.baseBranch,
            observedAt: row.observedAt,
            authoringTaskId: sql`coalesce(${prRefTable.authoringTaskId}, excluded.authoring_task_id)`,
          },
        })
        .returning({ id: prRefTable.id });
      const stored = rows[0];
      if (!stored) throw new Error("PR reference upsert returned no row");
      return stored.id;
    },

    listByTaskId(taskId) {
      return listWhere(prRefTable.authoringTaskId, taskId);
    },

    listBySessionId(sessionId) {
      return listWhere(prRefTable.sessionId, sessionId);
    },
  };
}
