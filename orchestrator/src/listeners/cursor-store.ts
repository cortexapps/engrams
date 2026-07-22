import { sql, type SQL } from "drizzle-orm";

import { getDb } from "../db/client.ts";

export interface CursorStore {
  get(sessionId: string, consumer: string): Promise<bigint>;
  set(sessionId: string, consumer: string, idx: bigint): Promise<void>;
}

interface CursorQueryResult {
  rows: Record<string, unknown>[];
}

export interface CursorExecutor {
  execute(statement: SQL): Promise<CursorQueryResult>;
}

function productionExecutor(): CursorExecutor {
  return {
    async execute(statement) {
      const result = await getDb().execute(statement);
      return { rows: result.rows };
    },
  };
}

function parseIdx(value: unknown): bigint | undefined {
  if (typeof value === "bigint") return value;
  if (typeof value === "string" && /^-?\d+$/.test(value)) return BigInt(value);
  if (typeof value === "number" && Number.isSafeInteger(value)) return BigInt(value);
  return undefined;
}

export function makeCursorStore(executor: CursorExecutor = productionExecutor()): CursorStore {
  return {
    async get(sessionId, consumer) {
      const result = await executor.execute(sql`
        select "last_idx"
        from "consumer_cursors"
        where "session_id" = ${sessionId} and "consumer" = ${consumer}
        limit 1
      `);
      return parseIdx(result.rows[0]?.last_idx) ?? -1n;
    },

    async set(sessionId, consumer, idx) {
      // Monotonic: a stale writer (e.g. a zombie listener whose lease was
      // stolen, issue #704) must never rewind a successor's cursor.
      await executor.execute(sql`
        insert into "consumer_cursors" ("session_id", "consumer", "last_idx", "updated_at")
        values (${sessionId}, ${consumer}, ${idx}, now())
        on conflict ("session_id", "consumer") do update
        set "last_idx" = excluded."last_idx", "updated_at" = excluded."updated_at"
        where "consumer_cursors"."last_idx" < excluded."last_idx"
      `);
    },
  };
}

export function makeInMemoryCursorStore(): CursorStore {
  const cursors = new Map<string, bigint>();
  const key = (sessionId: string, consumer: string) => `${sessionId}\u0000${consumer}`;
  return {
    async get(sessionId, consumer) {
      return cursors.get(key(sessionId, consumer)) ?? -1n;
    },
    async set(sessionId, consumer, idx) {
      const existing = cursors.get(key(sessionId, consumer));
      if (existing !== undefined && existing >= idx) return;
      cursors.set(key(sessionId, consumer), idx);
    },
  };
}
