/** Named integration connection store (ADR 0109). */

import { and, asc, eq, inArray } from "drizzle-orm";

import { getDb } from "./client.ts";
import { integrationConnection as connectionTable } from "./schema.ts";

export interface GoogleCloudConnectionConfig {
  workloadIdentityProvider: string;
  serviceAccountEmail: string;
  endpoints: string[];
  /** Exact public-IP PostgreSQL instance exposed through the host tunnel. */
  cloudSqlPostgresInstance?: string;
}

export interface IntegrationConnectionRow {
  id: string;
  alias: string;
  provider: string;
  displayName: string;
  isDefault: boolean;
  config: Record<string, unknown>;
  enabled: boolean;
  testedAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface IntegrationConnectionInput {
  alias: string;
  provider: string;
  displayName: string;
  config: Record<string, unknown>;
}

export interface IntegrationConnectionStore {
  list(): Promise<IntegrationConnectionRow[]>;
  get(id: string): Promise<IntegrationConnectionRow | null>;
  /** Fetch a set of connections in ONE query. Missing ids are simply absent. */
  getMany(ids: readonly string[]): Promise<IntegrationConnectionRow[]>;
  getDefault(provider: string): Promise<IntegrationConnectionRow | null>;
  create(input: IntegrationConnectionInput): Promise<IntegrationConnectionRow>;
  update(id: string, input: Omit<IntegrationConnectionInput, "provider">): Promise<IntegrationConnectionRow | null>;
  delete(id: string): Promise<boolean>;
  markTested(id: string, testedAt: Date): Promise<IntegrationConnectionRow | null>;
  setEnabled(id: string, enabled: boolean): Promise<IntegrationConnectionRow | null>;
  ensureDefault(provider: string, displayName: string): Promise<IntegrationConnectionRow>;
}

function toRow(r: typeof connectionTable.$inferSelect): IntegrationConnectionRow {
  return {
    id: r.id,
    alias: r.alias,
    provider: r.provider,
    displayName: r.displayName,
    isDefault: r.isDefault,
    config: (r.config ?? {}) as Record<string, unknown>,
    enabled: r.enabled,
    testedAt: r.testedAt ?? null,
    createdAt: r.createdAt,
    updatedAt: r.updatedAt,
  };
}

export function makeIntegrationConnectionStore(
  db: ReturnType<typeof getDb> = getDb(),
): IntegrationConnectionStore {
  async function getDefault(provider: string): Promise<IntegrationConnectionRow | null> {
    const rows = await db
      .select()
      .from(connectionTable)
      .where(and(
        eq(connectionTable.provider, provider),
        eq(connectionTable.isDefault, true),
      ))
      .limit(1);
    return rows[0] ? toRow(rows[0]) : null;
  }

  return {
    async list() {
      return (await db.select().from(connectionTable).orderBy(asc(connectionTable.alias))).map(toRow);
    },

    async get(id) {
      const rows = await db
        .select()
        .from(connectionTable)
        .where(eq(connectionTable.id, id))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

    async getMany(ids) {
      if (ids.length === 0) return [];
      const rows = await db
        .select()
        .from(connectionTable)
        .where(inArray(connectionTable.id, [...ids]));
      return rows.map(toRow);
    },

    getDefault,

    async create(input) {
      const rows = await db
        .insert(connectionTable)
        .values({ id: crypto.randomUUID(), ...input, enabled: false })
        .returning();
      return toRow(rows[0]!);
    },

    async update(id, input) {
      const rows = await db
        .update(connectionTable)
        .set({ ...input, enabled: false, testedAt: null, updatedAt: new Date() })
        .where(eq(connectionTable.id, id))
        .returning();
      return rows[0] ? toRow(rows[0]) : null;
    },

    async delete(id) {
      const rows = await db
        .delete(connectionTable)
        .where(eq(connectionTable.id, id))
        .returning({ id: connectionTable.id });
      return rows.length > 0;
    },

    async markTested(id, testedAt) {
      const rows = await db
        .update(connectionTable)
        .set({ testedAt, updatedAt: testedAt })
        .where(eq(connectionTable.id, id))
        .returning();
      return rows[0] ? toRow(rows[0]) : null;
    },

    async setEnabled(id, enabled) {
      const rows = await db
        .update(connectionTable)
        .set({ enabled, updatedAt: new Date() })
        .where(eq(connectionTable.id, id))
        .returning();
      return rows[0] ? toRow(rows[0]) : null;
    },

    async ensureDefault(provider, displayName) {
      const existing = await getDefault(provider);
      if (existing) return existing;
      await db.insert(connectionTable).values({
        id: crypto.randomUUID(),
        alias: `${provider}-default`,
        provider,
        displayName,
        isDefault: true,
        config: {},
        enabled: true,
        testedAt: new Date(0),
      }).onConflictDoNothing();
      const created = await getDefault(provider);
      if (!created) {
        throw new Error(`failed to create default integration connection for "${provider}"`);
      }
      return created;
    },
  };
}
