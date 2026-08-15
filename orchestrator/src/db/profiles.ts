/**
 * Profile data-access seam (ADR 0053).
 *
 * The injectable seam that ProfileService (rpc/profiles.ts) and
 * TaskService.createTask (rpc/tasks.ts) depend on, and that the seam tests
 * fake. Drizzle-backed by default. Soft delete only (deleted_at).
 */

import { and, eq, inArray, isNull, ne } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  profile as profileTable,
  DEFAULT_PROFILE_NETWORK,
  type ProfileApp,
  type ProfileNetwork,
  type ProfileRepo,
  type ProfileSecret,
  type ProfileIntegrationGrant,
} from "./schema.ts";

export interface ProfileRow {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  // ADR 0062/0063: default harness (catalog name) — REQUIRED (a profile always
  // names a concrete harness) + default model/effort (catalog option ids; null =
  // the harness descriptor's default).
  harness: string;
  modelRouter?: string | null;
  model: string | null;
  effort: string | null;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  // ADR 0055: dynamic skill bundle names this profile's sessions mount.
  skills: string[];
  // ADR 0109: structured named-connection authority.
  integrationGrants: ProfileIntegrationGrant[];
  // ADR 0057: profile-defined egress allow-list + injected secrets.
  network: ProfileNetwork;
  secrets: ProfileSecret[];
  // Git checkouts inside the image (user-managed; picker card input).
  repos: ProfileRepo[];
  // ADR 0118: the services every session from this profile hosts.
  apps: ProfileApp[];
  designation: string | null;
  createdAt: Date;
  updatedAt: Date;
  deletedAt: Date | null;
}

// `designation` is deliberately absent: update() spreads only ProfileInput into
// its SET clause, so admin edits cannot clobber a system marker.
export interface ProfileInput {
  name: string;
  description: string;
  icon: string;
  imageId: string;
  harness: string;
  modelRouter?: string | null;
  model: string | null;
  effort: string | null;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  skills: string[];
  integrationGrants: ProfileIntegrationGrant[];
  network: ProfileNetwork;
  secrets: ProfileSecret[];
  repos: ProfileRepo[];
  apps: ProfileApp[];
}

/** The seam injected into ProfileService and TaskService. */
export interface ProfileStore {
  /** Active profiles by default; includeArchived adds soft-deleted ones. Ordered by name. */
  list(opts: { includeArchived: boolean }): Promise<ProfileRow[]>;
  /** Any profile (active or archived), or null. */
  get(id: string): Promise<ProfileRow | null>;
  /** Active (deleted_at IS NULL) only, or null. Used by createTask. */
  getActive(id: string): Promise<ProfileRow | null>;
  /** The active profile carrying this system designation, or null. */
  getByDesignation(designation: string): Promise<ProfileRow | null>;
  /** Rows for the given ids (active or archived) — for snapshot enrichment. */
  getByIds(ids: string[]): Promise<ProfileRow[]>;
  create(input: ProfileInput, designation?: string | null): Promise<ProfileRow>;
  /** Assign or clear a system designation, keeping each value on at most one profile. */
  setDesignation(id: string, designation: string | null): Promise<void>;
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
    harness: r.harness,
    modelRouter: r.modelRouter ?? null,
    model: r.model ?? null,
    effort: r.effort ?? null,
    includeUserTokens: r.includeUserTokens,
    envVars: (r.envVars ?? {}) as Record<string, string>,
    skills: (r.skills ?? []) as string[],
    integrationGrants: (r.integrationGrants ?? []) as ProfileIntegrationGrant[],
    network: (r.network ?? DEFAULT_PROFILE_NETWORK) as ProfileNetwork,
    secrets: (r.secrets ?? []) as ProfileSecret[],
    repos: (r.repos ?? []) as ProfileRepo[],
    apps: (r.apps ?? []) as ProfileApp[],
    designation: r.designation ?? null,
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
    async getByDesignation(designation) {
      const rows = await db
        .select()
        .from(profileTable)
        .where(and(eq(profileTable.designation, designation), isNull(profileTable.deletedAt)))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async getByIds(ids) {
      if (ids.length === 0) return [];
      const rows = await db.select().from(profileTable).where(inArray(profileTable.id, ids));
      return rows.map(toRow);
    },
    async create(input, designation) {
      const id = crypto.randomUUID();
      await db.insert(profileTable).values({ id, ...input, designation: designation ?? null });
      const row = await this.get(id);
      return row!;
    },
    async setDesignation(id, designation) {
      const updatedAt = new Date();
      await db.transaction(async (tx) => {
        if (designation !== null) {
          await tx
            .update(profileTable)
            .set({ designation: null, updatedAt })
            .where(and(eq(profileTable.designation, designation), ne(profileTable.id, id)));
        }
        await tx
          .update(profileTable)
          .set({ designation, updatedAt })
          .where(eq(profileTable.id, id));
      });
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
