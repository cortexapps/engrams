import { sql, type SQL } from "drizzle-orm";

import { getDb } from "../db/client.ts";

export interface LeaseStore {
  tryAcquire(sessionId: string, owner: string, ttlMs: number): Promise<boolean>;
  renew(sessionId: string, owner: string, ttlMs: number): Promise<boolean>;
  release(sessionId: string, owner: string): Promise<void>;
  markTerminal(sessionId: string): Promise<void>;
  listDesired(): Promise<string[]>;
  ensureRow(sessionId: string): Promise<void>;
}

export interface LeaseQueryResult {
  rowCount: number | null;
  rows: Record<string, unknown>[];
}

/** Narrow SQL executor seam used by the production store and recording tests. */
export interface LeaseExecutor {
  execute(statement: SQL): Promise<LeaseQueryResult>;
}

function productionExecutor(): LeaseExecutor {
  return {
    async execute(statement) {
      const result = await getDb().execute(statement);
      return { rowCount: result.rowCount, rows: result.rows };
    },
  };
}

export function makeLeaseStore(executor: LeaseExecutor = productionExecutor()): LeaseStore {
  return {
    async tryAcquire(sessionId, owner, ttlMs) {
      const result = await executor.execute(sql`
        update "session_listeners"
        set "owner" = ${owner},
            "lease_expires_at" = now() + (${ttlMs} * interval '1 millisecond')
        where "session_id" = ${sessionId}
          and "terminal_at" is null
          and ("owner" is null or "lease_expires_at" < now())
        returning "session_id"
      `);
      return (result.rowCount ?? result.rows.length) > 0;
    },

    async renew(sessionId, owner, ttlMs) {
      const result = await executor.execute(sql`
        update "session_listeners"
        set "lease_expires_at" = now() + (${ttlMs} * interval '1 millisecond')
        where "session_id" = ${sessionId}
          and "owner" = ${owner}
          and "terminal_at" is null
        returning "session_id"
      `);
      return (result.rowCount ?? result.rows.length) > 0;
    },

    async release(sessionId, owner) {
      await executor.execute(sql`
        update "session_listeners"
        set "owner" = null, "lease_expires_at" = null
        where "session_id" = ${sessionId} and "owner" = ${owner}
      `);
    },

    async markTerminal(sessionId) {
      await executor.execute(sql`
        update "session_listeners"
        set "terminal_at" = coalesce("terminal_at", now())
        where "session_id" = ${sessionId}
      `);
    },

    async listDesired() {
      const result = await executor.execute(sql`
        select "session_id"
        from "session_listeners"
        where "terminal_at" is null
        order by "session_id"
      `);
      return result.rows.flatMap((row) =>
        typeof row.session_id === "string" ? [row.session_id] : []
      );
    },

    async ensureRow(sessionId) {
      await executor.execute(sql`
        insert into "session_listeners" ("session_id")
        values (${sessionId})
        on conflict ("session_id") do nothing
      `);
    },
  };
}

interface MemoryLease {
  owner: string | null;
  leaseExpiresAt: Date | null;
  terminalAt: Date | null;
}

/** Deterministic in-memory implementation shared by unit tests. */
export function makeInMemoryLeaseStore(
  now: () => Date = () => new Date(),
): LeaseStore {
  const rows = new Map<string, MemoryLease>();
  return {
    async tryAcquire(sessionId, owner, ttlMs) {
      const row = rows.get(sessionId);
      if (!row || row.terminalAt !== null) return false;
      const current = now();
      if (
        row.owner !== null &&
        (row.leaseExpiresAt === null || row.leaseExpiresAt.getTime() >= current.getTime())
      ) {
        return false;
      }
      row.owner = owner;
      row.leaseExpiresAt = new Date(current.getTime() + ttlMs);
      return true;
    },

    async renew(sessionId, owner, ttlMs) {
      const row = rows.get(sessionId);
      if (!row || row.terminalAt !== null || row.owner !== owner) return false;
      row.leaseExpiresAt = new Date(now().getTime() + ttlMs);
      return true;
    },

    async release(sessionId, owner) {
      const row = rows.get(sessionId);
      if (row?.owner !== owner) return;
      row.owner = null;
      row.leaseExpiresAt = null;
    },

    async markTerminal(sessionId) {
      const row = rows.get(sessionId);
      if (row && row.terminalAt === null) row.terminalAt = now();
    },

    async listDesired() {
      return [...rows.entries()]
        .filter(([, row]) => row.terminalAt === null)
        .map(([sessionId]) => sessionId)
        .sort();
    },

    async ensureRow(sessionId) {
      if (!rows.has(sessionId)) {
        rows.set(sessionId, { owner: null, leaseExpiresAt: null, terminalAt: null });
      }
    },
  };
}
