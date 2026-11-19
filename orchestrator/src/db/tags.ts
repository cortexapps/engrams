/** The tag store: installation-wide tags and the links from automations to
 * them. Callers pass values the tag rules already normalized. */

import { and, eq, inArray, isNull, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import { isForeignKeyViolation, isUniqueViolation } from "./pg-errors.ts";
import {
  automation as automationTable,
  automationTag as automationTagTable,
  tag as tagTable,
} from "./schema.ts";

export interface TagRow {
  id: string;
  name: string;
  color: string;
  description: string;
  createdAt: Date;
  updatedAt: Date;
}

export interface TagSummaryRow {
  tag: TagRow;
  /** Non-archived automations that have the tag. */
  automationCount: number;
}

export type AddToAutomationOutcome = "added" | "full" | "automation_missing" | "tag_missing";

export interface TagStore {
  /** Every tag, ordered by name without regard to case. */
  list(): Promise<TagSummaryRow[]>;
  create(input: {
    name: string;
    color: string;
    description: string;
  }): Promise<{ tag: TagRow } | { refused: "name_taken" }>;
  /** An empty patch returns the tag unchanged. */
  update(
    id: string,
    patch: { name?: string; color?: string; description?: string },
  ): Promise<{ tag: TagRow } | { refused: "name_taken" | "not_found" }>;
  /** Also removes the tag from every automation. False when no such tag. */
  delete(id: string): Promise<boolean>;
  /** Each automation's tags, ordered by name without regard to case.
   * Automations without tags are absent from the map. */
  tagsForAutomations(automationIds: readonly string[]): Promise<Map<string, TagRow[]>>;
  /** "added" also when the automation already has the tag, even at the limit.
   * "automation_missing" covers an archived automation. */
  addToAutomation(automationId: string, tagId: string, maxTags: number): Promise<AddToAutomationOutcome>;
  /** "removed" also when the automation does not have the tag. */
  removeFromAutomation(automationId: string, tagId: string): Promise<"removed" | "automation_missing">;
}

function tagRow(row: typeof tagTable.$inferSelect): TagRow {
  return {
    id: row.id,
    name: row.name,
    color: row.color,
    description: row.description,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

const byName = sql`lower(${tagTable.name})`;

/** Tag links change only on an automation that exists and is not archived. */
function isActive(row: { archivedAt: Date | null } | undefined): boolean {
  return row !== undefined && row.archivedAt === null;
}

export function makeTagStore(db: ReturnType<typeof getDb> = getDb()): TagStore {
  async function get(id: string): Promise<TagRow | null> {
    const [row] = await db.select().from(tagTable).where(eq(tagTable.id, id)).limit(1);
    return row ? tagRow(row) : null;
  }

  return {
    async list() {
      const rows = await db
        .select({ tag: tagTable, automationCount: sql<number>`count(${automationTable.id})::int` })
        .from(tagTable)
        .leftJoin(automationTagTable, eq(automationTagTable.tagId, tagTable.id))
        .leftJoin(
          automationTable,
          and(eq(automationTable.id, automationTagTable.automationId), isNull(automationTable.archivedAt)),
        )
        .groupBy(tagTable.id)
        .orderBy(byName);
      return rows.map((row) => ({ tag: tagRow(row.tag), automationCount: row.automationCount }));
    },

    async create(input) {
      try {
        const [row] = await db
          .insert(tagTable)
          .values({ id: crypto.randomUUID(), ...input })
          .returning();
        if (!row) throw new Error("tag insert returned no row");
        return { tag: tagRow(row) };
      } catch (error) {
        if (isUniqueViolation(error)) return { refused: "name_taken" };
        throw error;
      }
    },

    async update(id, patch) {
      const set = {
        ...(patch.name !== undefined ? { name: patch.name } : {}),
        ...(patch.color !== undefined ? { color: patch.color } : {}),
        ...(patch.description !== undefined ? { description: patch.description } : {}),
      };
      if (Object.keys(set).length === 0) {
        const current = await get(id);
        return current ? { tag: current } : { refused: "not_found" };
      }
      try {
        const [row] = await db
          .update(tagTable)
          .set({ ...set, updatedAt: new Date() })
          .where(eq(tagTable.id, id))
          .returning();
        return row ? { tag: tagRow(row) } : { refused: "not_found" };
      } catch (error) {
        if (isUniqueViolation(error)) return { refused: "name_taken" };
        throw error;
      }
    },

    async delete(id) {
      const rows = await db.delete(tagTable).where(eq(tagTable.id, id)).returning({ id: tagTable.id });
      return rows.length > 0;
    },

    async tagsForAutomations(automationIds) {
      const result = new Map<string, TagRow[]>();
      if (automationIds.length === 0) return result;
      const rows = await db
        .select({ automationId: automationTagTable.automationId, tag: tagTable })
        .from(automationTagTable)
        .innerJoin(tagTable, eq(tagTable.id, automationTagTable.tagId))
        .where(inArray(automationTagTable.automationId, [...automationIds]))
        .orderBy(byName);
      for (const row of rows) {
        const tags = result.get(row.automationId);
        if (tags) tags.push(tagRow(row.tag));
        else result.set(row.automationId, [tagRow(row.tag)]);
      }
      return result;
    },

    async addToAutomation(automationId, tagId, maxTags) {
      try {
        return await db.transaction(async (tx): Promise<AddToAutomationOutcome> => {
          // The row lock serializes adds to one automation, so the count
          // below cannot go stale before the insert. NO KEY UPDATE does not
          // conflict with the key-share lock a foreign-key check takes, so
          // inserts that reference the automation (runs) do not wait.
          const [target] = await tx
            .select({ archivedAt: automationTable.archivedAt })
            .from(automationTable)
            .where(eq(automationTable.id, automationId))
            .for("no key update");
          if (!isActive(target)) return "automation_missing";
          const linked = await tx
            .select({ tagId: automationTagTable.tagId })
            .from(automationTagTable)
            .where(eq(automationTagTable.automationId, automationId));
          if (linked.some((link) => link.tagId === tagId)) return "added";
          if (linked.length >= maxTags) return "full";
          await tx.insert(automationTagTable).values({ automationId, tagId }).onConflictDoNothing();
          return "added";
        });
      } catch (error) {
        if (isForeignKeyViolation(error)) return "tag_missing";
        throw error;
      }
    },

    async removeFromAutomation(automationId, tagId) {
      const [target] = await db
        .select({ archivedAt: automationTable.archivedAt })
        .from(automationTable)
        .where(eq(automationTable.id, automationId))
        .limit(1);
      if (!isActive(target)) return "automation_missing";
      await db
        .delete(automationTagTable)
        .where(and(eq(automationTagTable.automationId, automationId), eq(automationTagTable.tagId, tagId)));
      return "removed";
    },
  };
}
