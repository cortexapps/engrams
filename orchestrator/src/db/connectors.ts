/**
 * Connector catalog data-access seam (ADR 0057 C1).
 *
 * The injectable seam `loadRegistry` (connectors/registry.ts) reads custom
 * (admin-authored) connectors from. Drizzle-backed by default; faked in tests.
 * Built-ins (`github`/`datadog`) are file seeds, NOT rows here — this table
 * holds only what an admin added. `config` is the raw connector JSON, validated
 * by `parseConnector` at load (the admin-trust boundary), never trusted here.
 *
 * C1 added the read path (`list`); C3's IntegrationService adds the write surface
 * (`get`/`upsert`/`delete`). `config` is the raw connector JSON — `parseConnector`
 * (the admin-trust boundary) validates it at the RPC before it ever reaches here.
 */

import { eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { connector as connectorTable } from "./schema.ts";

export interface ConnectorRow {
  provider: string;
  /** Raw connector JSON; re-validated by `parseConnector` at registry load. */
  config: unknown;
  createdAt: Date;
  updatedAt: Date;
}

/** Data-access seam over the custom-connector table. Built-ins are file seeds,
 * not rows here. The read side (`list`) satisfies `CustomConnectorSource`. */
export interface ConnectorStore {
  /** All admin-authored connectors (raw configs). Built-ins are not included. */
  list(): Promise<ConnectorRow[]>;
  /** One admin-authored connector by provider, or null. */
  get(provider: string): Promise<ConnectorRow | null>;
  /** Create or replace a custom connector (validated upstream). Returns the row. */
  upsert(provider: string, config: unknown): Promise<ConnectorRow>;
  /** Delete a custom connector; returns whether a row was removed (idempotent). */
  delete(provider: string): Promise<boolean>;
}

function toRow(r: typeof connectorTable.$inferSelect): ConnectorRow {
  return {
    provider: r.provider,
    config: r.config,
    createdAt: r.createdAt,
    updatedAt: r.updatedAt,
  };
}

export function makeConnectorStore(db: ReturnType<typeof getDb> = getDb()): ConnectorStore {
  return {
    async list() {
      const rows = await db.select().from(connectorTable);
      return rows.map(toRow);
    },
    async get(provider) {
      const rows = await db
        .select()
        .from(connectorTable)
        .where(eq(connectorTable.provider, provider))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async upsert(provider, config) {
      const rows = await db
        .insert(connectorTable)
        .values({ provider, config })
        .onConflictDoUpdate({
          target: connectorTable.provider,
          set: { config, updatedAt: new Date() },
        })
        .returning();
      return toRow(rows[0]!);
    },
    async delete(provider) {
      const rows = await db
        .delete(connectorTable)
        .where(eq(connectorTable.provider, provider))
        .returning({ provider: connectorTable.provider });
      return rows.length > 0;
    },
  };
}
