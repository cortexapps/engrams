/** The integration-event ledger (ADR 0119 D5).
 *
 * One row per verified, redacted provider delivery. The delivery unique makes
 * provider retries no-ops. The write path is ONE idempotent insert — no
 * transaction, no lock — because it sits on every webhook's ack-critical
 * path (Slack's 3 s budget) and every delivery of one event type across the
 * deployment would otherwise serialize. Retention is enforced by the hourly
 * sweep instead: 20 newest per (connection, event key) plus a 7-day window —
 * the ledger feeds trigger dispatch and editor samples, not an archive, and
 * readers already take the newest N by index.
 */

import { and, desc, eq, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import { integrationEvent as integrationEventTable, type IntegrationEventRow } from "./schema.ts";

export const INTEGRATION_EVENT_RETENTION_PER_KEY = 20;
export const INTEGRATION_EVENT_RETENTION_MS = 7 * 24 * 3600 * 1000;

export interface RecordIntegrationEventInput {
  provider: string;
  connectionId: string;
  eventKey: string;
  deliveryId: string;
  /** Redacted before it reaches this store. */
  payload: Record<string, unknown>;
  scopeValue?: string;
  receivedAt: Date;
}

export interface RecordIntegrationEventResult {
  /** False when the delivery unique already held a row (provider retry). */
  recorded: boolean;
}

export interface IntegrationEventStore {
  record(input: RecordIntegrationEventInput): Promise<RecordIntegrationEventResult>;
  /** Enforces both retention bounds; runs hourly (scheduler.ts). */
  sweep(now: Date): Promise<number>;
  list(
    connectionId: string,
    eventKey: string | undefined,
    limit: number,
  ): Promise<IntegrationEventRow[]>;
  getLatest(connectionId: string, eventKey: string): Promise<IntegrationEventRow | null>;
  listObservedEventKeys(connectionId: string): Promise<string[]>;
  getById(id: string): Promise<IntegrationEventRow | null>;
  /** Distinct provider-noun values seen on this connection (repositories,
   * channels, teams) — the input-key picker's fallback source. */
  listObservedScopeValues(connectionId: string): Promise<string[]>;
}

export function makeIntegrationEventStore(
  db: ReturnType<typeof getDb> = getDb(),
): IntegrationEventStore {
  return {
    async record(input) {
      const inserted = await db
        .insert(integrationEventTable)
        .values({
          provider: input.provider,
          connectionId: input.connectionId,
          eventKey: input.eventKey,
          deliveryId: input.deliveryId,
          payload: input.payload,
          scopeValue: input.scopeValue ?? null,
          receivedAt: input.receivedAt,
        })
        .onConflictDoNothing()
        .returning({ id: integrationEventTable.id });
      return { recorded: inserted.length > 0 };
    },

    async sweep(now) {
      const cutoff = new Date(now.getTime() - INTEGRATION_EVENT_RETENTION_MS);
      // Two concurrent sweeps (one per pod) may select overlapping victims;
      // a DELETE on a row the other already removed is a no-op, so no lock.
      const result = await db.execute(sql`
        delete from ${integrationEventTable}
        where received_at < ${cutoff}
           or id in (
             select id from (
               select id,
                      row_number() over (
                        partition by connection_id, event_key
                        order by received_at desc, id desc
                      ) as rank
               from ${integrationEventTable}
             ) ranked
             where rank > ${INTEGRATION_EVENT_RETENTION_PER_KEY}
           )
      `);
      return Number(result.rowCount ?? 0);
    },

    async list(connectionId, eventKey, limit) {
      const condition = eventKey
        ? and(
            eq(integrationEventTable.connectionId, connectionId),
            eq(integrationEventTable.eventKey, eventKey),
          )
        : eq(integrationEventTable.connectionId, connectionId);
      return db
        .select()
        .from(integrationEventTable)
        .where(condition)
        .orderBy(desc(integrationEventTable.receivedAt), desc(integrationEventTable.id))
        .limit(limit);
    },

    async getLatest(connectionId, eventKey) {
      const [row] = await db
        .select()
        .from(integrationEventTable)
        .where(
          and(
            eq(integrationEventTable.connectionId, connectionId),
            eq(integrationEventTable.eventKey, eventKey),
          ),
        )
        .orderBy(desc(integrationEventTable.receivedAt), desc(integrationEventTable.id))
        .limit(1);
      return row ?? null;
    },

    async listObservedEventKeys(connectionId) {
      const rows = await db
        .selectDistinct({ eventKey: integrationEventTable.eventKey })
        .from(integrationEventTable)
        .where(eq(integrationEventTable.connectionId, connectionId))
        .orderBy(integrationEventTable.eventKey);
      return rows.map((r) => r.eventKey);
    },

    async getById(id) {
      const [row] = await db
        .select()
        .from(integrationEventTable)
        .where(eq(integrationEventTable.id, id))
        .limit(1);
      return row ?? null;
    },

    async listObservedScopeValues(connectionId) {
      const rows = await db
        .selectDistinct({ scopeValue: integrationEventTable.scopeValue })
        .from(integrationEventTable)
        .where(eq(integrationEventTable.connectionId, connectionId))
        .orderBy(integrationEventTable.scopeValue);
      return rows.map((r) => r.scopeValue).filter((v): v is string => v !== null);
    },
  };
}
