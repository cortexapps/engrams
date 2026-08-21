/** The integration-event ledger (ADR 0119 D5).
 *
 * One row per verified, redacted provider delivery. The delivery unique makes
 * provider retries no-ops; retention is bounded twice — 20 newest per
 * (connection, event key) at write time, plus a 7-day sweep — because the
 * ledger feeds trigger dispatch and editor samples, not an archive.
 */

import { and, desc, eq, lt, sql } from "drizzle-orm";

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
  sweepExpired(now: Date): Promise<number>;
  list(
    connectionId: string,
    eventKey: string | undefined,
    limit: number,
  ): Promise<IntegrationEventRow[]>;
  getLatest(connectionId: string, eventKey: string): Promise<IntegrationEventRow | null>;
  listObservedEventKeys(connectionId: string): Promise<string[]>;
}

export function makeIntegrationEventStore(
  db: ReturnType<typeof getDb> = getDb(),
): IntegrationEventStore {
  return {
    async record(input) {
      return db.transaction(async (tx) => {
        // Serialize writers per (connection, event key) before insert+prune.
        // An advisory xact lock instead of a row lock: the connection row is
        // shared by every event key of the provider, and locking it would
        // serialize unrelated deliveries.
        await tx.execute(sql`
          select pg_advisory_xact_lock(hashtext(${input.connectionId} || ':' || ${input.eventKey}))
        `);
        const inserted = await tx
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
        if (inserted.length === 0) return { recorded: false };
        await tx.execute(sql`
          delete from ${integrationEventTable}
          where connection_id = ${input.connectionId}
            and event_key = ${input.eventKey}
            and id not in (
              select id from ${integrationEventTable}
              where connection_id = ${input.connectionId}
                and event_key = ${input.eventKey}
              order by received_at desc, id desc
              limit ${INTEGRATION_EVENT_RETENTION_PER_KEY}
            )
        `);
        return { recorded: true };
      });
    },

    async sweepExpired(now) {
      const cutoff = new Date(now.getTime() - INTEGRATION_EVENT_RETENTION_MS);
      const deleted = await db
        .delete(integrationEventTable)
        .where(lt(integrationEventTable.receivedAt, cutoff))
        .returning({ id: integrationEventTable.id });
      return deleted.length;
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
  };
}
