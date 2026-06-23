/**
 * Connector-logo data-access seam (redesign).
 *
 * Orchestrator-owned overlay on the connector catalog: an optional uploaded
 * brand mark, keyed by provider. The coordinator never sees connectors, so logos
 * live here (not in `BlobStorage`); bytes are small (≤512 KB, enforced at the
 * upload RPC) so a `bytea` column is the right home — transactional with the
 * catalog, no blob bucket. Absence ⇒ the renderer falls back to the monogram.
 *
 * Drizzle-backed by default; faked in tests.
 */

import { eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { connectorLogo as connectorLogoTable } from "./schema.ts";

export interface ConnectorLogoRow {
  provider: string;
  mediaType: string;
  data: Buffer;
  updatedAt: Date;
}

/** Data-access seam over the connector-logo table. */
export interface ConnectorLogoStore {
  /** The logo for a provider, or null. */
  get(provider: string): Promise<ConnectorLogoRow | null>;
  /** Create or replace a provider's logo. */
  put(provider: string, mediaType: string, data: Buffer): Promise<void>;
  /** Delete a provider's logo; returns whether a row was removed (idempotent). */
  delete(provider: string): Promise<boolean>;
  /** Providers that currently have a logo (drives the catalog `icon.logo` overlay). */
  listProviders(): Promise<string[]>;
}

export function makeConnectorLogoStore(
  db: ReturnType<typeof getDb> = getDb(),
): ConnectorLogoStore {
  return {
    async get(provider) {
      const rows = await db
        .select()
        .from(connectorLogoTable)
        .where(eq(connectorLogoTable.provider, provider))
        .limit(1);
      const r = rows[0];
      return r ? { provider: r.provider, mediaType: r.mediaType, data: r.data, updatedAt: r.updatedAt } : null;
    },
    async put(provider, mediaType, data) {
      await db
        .insert(connectorLogoTable)
        .values({ provider, mediaType, data })
        .onConflictDoUpdate({
          target: connectorLogoTable.provider,
          set: { mediaType, data, updatedAt: new Date() },
        });
    },
    async delete(provider) {
      const rows = await db
        .delete(connectorLogoTable)
        .where(eq(connectorLogoTable.provider, provider))
        .returning({ provider: connectorLogoTable.provider });
      return rows.length > 0;
    },
    async listProviders() {
      const rows = await db.select({ provider: connectorLogoTable.provider }).from(connectorLogoTable);
      return rows.map((r) => r.provider);
    },
  };
}
