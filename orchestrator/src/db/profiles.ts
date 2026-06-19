/**
 * Profile data-access seam (ADR 0052).
 *
 * The injectable seam that ProfileService (rpc/profiles.ts) and
 * TaskService.createTask (rpc/tasks.ts) depend on, and that the seam tests
 * fake. Drizzle-backed by default. Soft delete only (deleted_at).
 */

import { and, eq, inArray, isNull } from "drizzle-orm";

import { getDb } from "./client.ts";
import { profile as profileTable } from "./schema.ts";

export interface ProfileRow {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  // ADR 0055: dynamic skill bundle names this profile's sessions mount.
  skills: string[];
  createdAt: Date;
  updatedAt: Date;
  deletedAt: Date | null;
}

export interface ProfileInput {
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  skills: string[];
}

/** The seam injected into ProfileService and TaskService. */
export interface ProfileStore {
  /** Active profiles by default; includeArchived adds soft-deleted ones. Ordered by name. */
  list(opts: { includeArchived: boolean }): Promise<ProfileRow[]>;
  /** Any profile (active or archived), or null. */
  get(id: string): Promise<ProfileRow | null>;
  /** Active (deleted_at IS NULL) only, or null. Used by createTask. */
  getActive(id: string): Promise<ProfileRow | null>;
  /** Rows for the given ids (active or archived) — for snapshot enrichment. */
  getByIds(ids: string[]): Promise<ProfileRow[]>;
  create(input: ProfileInput): Promise<ProfileRow>;
  /** Returns the updated row, or null if the id is absent / archived. */
  update(id: string, input: ProfileInput): Promise<ProfileRow | null>;
  /** Idempotent soft delete (sets deleted_at). */
  softDelete(id: string): Promise<void>;
}

function toRow(r: typeof profileTable.$inferSelect): ProfileRow {
  return {
    id: r.id,
    name: r.name,
    description: r.description,
    icon: r.icon,
    imageId: r.imageId,
    includeUserTokens: r.includeUserTokens,
    envVars: (r.envVars ?? {}) as Record<string, string>,
    skills: (r.skills ?? []) as string[],
    createdAt: r.createdAt,
    updatedAt: r.updatedAt,
    deletedAt: r.deletedAt,
  };
}

export function makeProfileStore(db: ReturnType<typeof getDb> = getDb()): ProfileStore {
  return {
    async list({ includeArchived }) {
      const rows = includeArchived
        ? await db.select().from(profileTable)
        : await db.select().from(profileTable).where(isNull(profileTable.deletedAt));
      return rows.map(toRow).sort((a, b) => a.name.localeCompare(b.name));
    },
    async get(id) {
      const rows = await db.select().from(profileTable).where(eq(profileTable.id, id)).limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async getActive(id) {
      const rows = await db
        .select()
        .from(profileTable)
        .where(and(eq(profileTable.id, id), isNull(profileTable.deletedAt)))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async getByIds(ids) {
      if (ids.length === 0) return [];
      const rows = await db.select().from(profileTable).where(inArray(profileTable.id, ids));
      return rows.map(toRow);
    },
    async create(input) {
      const id = crypto.randomUUID();
      await db.insert(profileTable).values({ id, ...input });
      const row = await this.get(id);
      return row!;
    },
    async update(id, input) {
      const existing = await this.getActive(id);
      if (!existing) return null;
      await db
        .update(profileTable)
        .set({ ...input, updatedAt: new Date() })
        .where(eq(profileTable.id, id));
      return this.get(id);
    },
    async softDelete(id) {
      await db
        .update(profileTable)
        .set({ deletedAt: new Date() })
        .where(and(eq(profileTable.id, id), isNull(profileTable.deletedAt)));
    },
  };
}
