/**
 * Connector catalog data-access seam (ADR 0057 C1).
 *
 * The injectable seam `loadRegistry` (connectors/registry.ts) reads custom
 * (admin-authored) connectors from. Drizzle-backed by default; faked in tests.
 * Built-ins (`github`/`datadog`) are file seeds, NOT rows here — this table
 * holds only what an admin added. `config` is the raw connector JSON, validated
 * by `parseConnector` at load (the admin-trust boundary), never trusted here.
 *
 * C1 needs only the read path (`list`). The write surface (upsert/delete) lands
 * with the IntegrationService RPC in C3.
 */

import { getDb } from "./client.ts";
import { connector as connectorTable } from "./schema.ts";

export interface ConnectorRow {
  provider: string;
  /** Raw connector JSON; re-validated by `parseConnector` at registry load. */
  config: unknown;
  createdAt: Date;
  updatedAt: Date;
}

/** Read seam over the custom-connector table. Satisfies `CustomConnectorSource`. */
export interface ConnectorStore {
  /** All admin-authored connectors (raw configs). Built-ins are not included. */
  list(): Promise<ConnectorRow[]>;
}

export function makeConnectorStore(db: ReturnType<typeof getDb> = getDb()): ConnectorStore {
  return {
    async list() {
      const rows = await db.select().from(connectorTable);
      return rows.map((r) => ({
        provider: r.provider,
        config: r.config,
        createdAt: r.createdAt,
        updatedAt: r.updatedAt,
      }));
    },
  };
}
