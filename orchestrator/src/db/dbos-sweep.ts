import { sql, type SQL } from "drizzle-orm";

import { getDb } from "./client.ts";

const SWEEP_LEASE_NAME = "dbos-orphan-sweep";

/** Drizzle's sql template JSON-stringifies a raw JS-array param (22P02 under a
 * ::text[] cast), so array params are expanded element-by-element instead. */
function textArray(values: string[]): SQL {
  if (values.length === 0) return sql`array[]::text[]`;
  return sql`array[${sql.join(
    values.map((value) => sql`${value}`),
    sql`, `,
  )}]::text[]`;
}

export interface HeartbeatStore {
  beat(appVersion: string, podName: string): Promise<void>;
  liveVersions(graceMs: number): Promise<string[]>;
}

export interface SweepLeaseStore {
  tryAcquire(owner: string, ttlMs: number): Promise<boolean>;
  release(owner: string): Promise<void>;
}

export interface SweepLedgerRow {
  workflowUuid: string;
  workflowName: string;
  sweepCount: number;
  firstSweptAt: Date | null;
  lastSweptAt: Date | null;
  suppressed: boolean;
  cleanupDoneAt: Date | null;
  cleanupFn: string | null;
  alertedAt: Date | null;
}

export interface SweepLedgerStore {
  recordSweep(workflowUuid: string, workflowName: string): Promise<SweepLedgerRow>;
  get(workflowUuid: string): Promise<SweepLedgerRow | null>;
  setSuppressed(workflowUuid: string, suppressed: boolean): Promise<void>;
  markCleanupDone(workflowUuid: string, fnName: string): Promise<void>;
  markAlerted(workflowUuid: string): Promise<void>;
  getWatermark(key: string): Promise<number | null>;
  setWatermark(key: string, epochMs: number): Promise<void>;
}

export interface DbosWorkflowRow {
  workflowUuid: string;
  name: string;
  status: string;
  applicationVersion: string | null;
  createdAtEpochMs: number;
  recoveryAttempts: number;
}

export interface FailedDbosWorkflowRow extends DbosWorkflowRow {
  updatedAtEpochMs: number;
}

export interface DbosStatusStore {
  listNonTerminalOnVersionsNotIn(
    liveVersions: string[],
    limit: number,
  ): Promise<DbosWorkflowRow[]>;
  adoptPending(workflowUuids: string[]): Promise<string[]>;
  clearVersionOnEnqueued(workflowUuids: string[]): Promise<string[]>;
  listNewlyTerminalFailed(
    sinceEpochMs: number,
    limit: number,
  ): Promise<FailedDbosWorkflowRow[]>;
}

function affectedRows(result: {
  rowCount: number | null;
  rows: Record<string, unknown>[];
}): number {
  return result.rowCount ?? result.rows.length;
}

function numberValue(value: unknown): number {
  if (typeof value === "number") return value;
  if (typeof value === "bigint" || typeof value === "string") return Number(value);
  throw new Error(`expected numeric database value, received ${String(value)}`);
}

function nullableDate(value: unknown): Date | null {
  if (value === null || value === undefined) return null;
  if (value instanceof Date) return value;
  return new Date(String(value));
}

function ledgerRow(row: Record<string, unknown>): SweepLedgerRow {
  return {
    workflowUuid: String(row.workflow_uuid),
    workflowName: String(row.workflow_name),
    sweepCount: numberValue(row.sweep_count),
    firstSweptAt: nullableDate(row.first_swept_at),
    lastSweptAt: nullableDate(row.last_swept_at),
    suppressed: row.suppressed === true,
    cleanupDoneAt: nullableDate(row.cleanup_done_at),
    cleanupFn: row.cleanup_fn === null ? null : String(row.cleanup_fn),
    alertedAt: nullableDate(row.alerted_at),
  };
}

function dbosWorkflowRow(row: Record<string, unknown>): DbosWorkflowRow {
  return {
    workflowUuid: String(row.workflow_uuid),
    name: String(row.name),
    status: String(row.status),
    applicationVersion:
      row.application_version === null ? null : String(row.application_version),
    createdAtEpochMs: numberValue(row.created_at),
    recoveryAttempts: numberValue(row.recovery_attempts),
  };
}

function failedDbosWorkflowRow(
  row: Record<string, unknown>,
): FailedDbosWorkflowRow {
  return {
    ...dbosWorkflowRow(row),
    updatedAtEpochMs: numberValue(row.updated_at),
  };
}

export function makeHeartbeatStore(
  db: ReturnType<typeof getDb> = getDb(),
): HeartbeatStore {
  return {
    async beat(appVersion, podName) {
      await db.execute(sql`
        insert into "dbos_version_heartbeats"
          ("application_version", "pod_name", "last_seen")
        values (${appVersion}, ${podName}, now())
        on conflict ("application_version", "pod_name") do update
        set "last_seen" = now()
      `);
    },

    async liveVersions(graceMs) {
      const result = await db.execute(sql`
        select "application_version"
        from "dbos_version_heartbeats"
        group by "application_version"
        having max("last_seen") > now() - (${graceMs} * interval '1 millisecond')
        order by "application_version"
      `);
      return result.rows.map((row) => String(row.application_version));
    },
  };
}

export function makeSweepLeaseStore(
  db: ReturnType<typeof getDb> = getDb(),
): SweepLeaseStore {
  return {
    async tryAcquire(owner, ttlMs) {
      const result = await db.execute(sql`
        insert into "dbos_sweep_lease" ("name", "owner", "expires_at")
        values (
          ${SWEEP_LEASE_NAME},
          ${owner},
          now() + (${ttlMs} * interval '1 millisecond')
        )
        on conflict ("name") do update
        set "owner" = excluded."owner",
            "expires_at" = excluded."expires_at"
        where "dbos_sweep_lease"."expires_at" < now()
        returning "name"
      `);
      return affectedRows(result) > 0;
    },

    async release(owner) {
      await db.execute(sql`
        delete from "dbos_sweep_lease"
        where "name" = ${SWEEP_LEASE_NAME} and "owner" = ${owner}
      `);
    },
  };
}

export function makeSweepLedgerStore(
  db: ReturnType<typeof getDb> = getDb(),
): SweepLedgerStore {
  return {
    async recordSweep(workflowUuid, workflowName) {
      const result = await db.execute(sql`
        insert into "dbos_sweep_ledger"
          ("workflow_uuid", "workflow_name", "sweep_count",
           "first_swept_at", "last_swept_at")
        values (${workflowUuid}, ${workflowName}, 1, now(), now())
        on conflict ("workflow_uuid") do update
        set "workflow_name" = excluded."workflow_name",
            "sweep_count" = "dbos_sweep_ledger"."sweep_count" + 1,
            "first_swept_at" = coalesce(
              "dbos_sweep_ledger"."first_swept_at",
              excluded."first_swept_at"
            ),
            "last_swept_at" = now()
        returning "workflow_uuid", "workflow_name", "sweep_count",
                  "first_swept_at", "last_swept_at", "suppressed",
                  "cleanup_done_at", "cleanup_fn", "alerted_at"
      `);
      const row = result.rows[0];
      if (!row) throw new Error(`sweep ledger upsert returned no row for ${workflowUuid}`);
      return ledgerRow(row);
    },

    async get(workflowUuid) {
      const result = await db.execute(sql`
        select "workflow_uuid", "workflow_name", "sweep_count",
               "first_swept_at", "last_swept_at", "suppressed",
               "cleanup_done_at", "cleanup_fn", "alerted_at"
        from "dbos_sweep_ledger"
        where "workflow_uuid" = ${workflowUuid}
        limit 1
      `);
      const row = result.rows[0];
      return row ? ledgerRow(row) : null;
    },

    async setSuppressed(workflowUuid, suppressed) {
      await db.execute(sql`
        update "dbos_sweep_ledger"
        set "suppressed" = ${suppressed}
        where "workflow_uuid" = ${workflowUuid}
      `);
    },

    async markCleanupDone(workflowUuid, fnName) {
      await db.execute(sql`
        update "dbos_sweep_ledger"
        set "cleanup_done_at" = now(), "cleanup_fn" = ${fnName}
        where "workflow_uuid" = ${workflowUuid}
      `);
    },

    async markAlerted(workflowUuid) {
      await db.execute(sql`
        update "dbos_sweep_ledger"
        set "alerted_at" = now()
        where "workflow_uuid" = ${workflowUuid}
      `);
    },

    async getWatermark(key) {
      const result = await db.execute(sql`
        select "epoch_ms"
        from "dbos_sweep_state"
        where "key" = ${key}
        limit 1
      `);
      const row = result.rows[0];
      return row ? numberValue(row.epoch_ms) : null;
    },

    async setWatermark(key, epochMs) {
      await db.execute(sql`
        insert into "dbos_sweep_state" ("key", "epoch_ms")
        values (${key}, ${epochMs})
        on conflict ("key") do update set "epoch_ms" = excluded."epoch_ms"
      `);
    },
  };
}

export function makeDbosStatusStore(
  db: ReturnType<typeof getDb> = getDb(),
): DbosStatusStore {
  return {
    async listNonTerminalOnVersionsNotIn(liveVersions, limit) {
      const result = await db.execute(sql`
        select "workflow_uuid", "name", "status", "application_version",
               "created_at", coalesce("recovery_attempts", 0) as "recovery_attempts"
        from "dbos"."workflow_status"
        where "status" in ('PENDING', 'ENQUEUED')
          and "application_version" is not null
          and "application_version" <> all(${textArray(liveVersions)})
        order by "created_at" asc
        limit ${limit}
      `);
      return result.rows.map(dbosWorkflowRow);
    },

    async adoptPending(workflowUuids) {
      if (workflowUuids.length === 0) return [];
      const result = await db.execute(sql`
        update "dbos"."workflow_status"
        set "status" = 'ENQUEUED',
            "queue_name" = '_dbos_internal_queue',
            "application_version" = null,
            "workflow_deadline_epoch_ms" = null,
            "deduplication_id" = null,
            "started_at_epoch_ms" = null,
            "completed_at" = null,
            "updated_at" = (extract(epoch from now()) * 1000)::bigint
        where "status" = 'PENDING'
          and "workflow_uuid" = any(${textArray(workflowUuids)})
        returning "workflow_uuid"
      `);
      return result.rows.map((row) => String(row.workflow_uuid));
    },

    async clearVersionOnEnqueued(workflowUuids) {
      if (workflowUuids.length === 0) return [];
      const result = await db.execute(sql`
        update "dbos"."workflow_status"
        set "application_version" = null
        where "status" = 'ENQUEUED'
          and "workflow_uuid" = any(${textArray(workflowUuids)})
        returning "workflow_uuid"
      `);
      return result.rows.map((row) => String(row.workflow_uuid));
    },

    async listNewlyTerminalFailed(sinceEpochMs, limit) {
      const result = await db.execute(sql`
        select "workflow_uuid", "name", "status", "application_version",
               "created_at", "updated_at",
               coalesce("recovery_attempts", 0) as "recovery_attempts"
        from "dbos"."workflow_status"
        where "status" in ('ERROR', 'MAX_RECOVERY_ATTEMPTS_EXCEEDED')
          and "updated_at" > ${sinceEpochMs}
        order by "updated_at" asc
        limit ${limit}
      `);
      return result.rows.map(failedDbosWorkflowRow);
    },
  };
}

/** Deterministic in-memory heartbeat store shared by sweep unit tests. */
export function makeInMemoryHeartbeatStore(
  now: () => Date = () => new Date(),
): HeartbeatStore {
  const rows = new Map<string, number>();
  return {
    async beat(appVersion, podName) {
      rows.set(`${appVersion}\0${podName}`, now().getTime());
    },

    async liveVersions(graceMs) {
      const cutoff = now().getTime() - graceMs;
      const latestByVersion = new Map<string, number>();
      for (const [key, lastSeen] of rows) {
        const appVersion = key.slice(0, key.indexOf("\0"));
        latestByVersion.set(
          appVersion,
          Math.max(latestByVersion.get(appVersion) ?? Number.NEGATIVE_INFINITY, lastSeen),
        );
      }
      return [...latestByVersion]
        .filter(([, lastSeen]) => lastSeen > cutoff)
        .map(([appVersion]) => appVersion)
        .sort();
    },
  };
}

/** Deterministic in-memory single-row lease with PG's strict expiry boundary. */
export function makeInMemorySweepLeaseStore(
  now: () => Date = () => new Date(),
): SweepLeaseStore {
  let lease: { owner: string; expiresAtMs: number } | null = null;
  return {
    async tryAcquire(owner, ttlMs) {
      const currentMs = now().getTime();
      if (lease !== null && lease.expiresAtMs >= currentMs) return false;
      lease = { owner, expiresAtMs: currentMs + ttlMs };
      return true;
    },

    async release(owner) {
      if (lease?.owner === owner) lease = null;
    },
  };
}

function cloneLedgerRow(row: SweepLedgerRow): SweepLedgerRow {
  return {
    ...row,
    firstSweptAt: row.firstSweptAt ? new Date(row.firstSweptAt) : null,
    lastSweptAt: row.lastSweptAt ? new Date(row.lastSweptAt) : null,
    cleanupDoneAt: row.cleanupDoneAt ? new Date(row.cleanupDoneAt) : null,
    alertedAt: row.alertedAt ? new Date(row.alertedAt) : null,
  };
}

/** Deterministic in-memory ledger and watermark KV shared by sweep unit tests. */
export function makeInMemorySweepLedgerStore(
  now: () => Date = () => new Date(),
): SweepLedgerStore {
  const rows = new Map<string, SweepLedgerRow>();
  const watermarks = new Map<string, number>();
  return {
    async recordSweep(workflowUuid, workflowName) {
      const sweptAt = now();
      const existing = rows.get(workflowUuid);
      const row: SweepLedgerRow = existing
        ? {
            ...existing,
            workflowName,
            sweepCount: existing.sweepCount + 1,
            firstSweptAt: existing.firstSweptAt ?? sweptAt,
            lastSweptAt: sweptAt,
          }
        : {
            workflowUuid,
            workflowName,
            sweepCount: 1,
            firstSweptAt: sweptAt,
            lastSweptAt: sweptAt,
            suppressed: false,
            cleanupDoneAt: null,
            cleanupFn: null,
            alertedAt: null,
          };
      rows.set(workflowUuid, row);
      return cloneLedgerRow(row);
    },

    async get(workflowUuid) {
      const row = rows.get(workflowUuid);
      return row ? cloneLedgerRow(row) : null;
    },

    async setSuppressed(workflowUuid, suppressed) {
      const row = rows.get(workflowUuid);
      if (row) row.suppressed = suppressed;
    },

    async markCleanupDone(workflowUuid, fnName) {
      const row = rows.get(workflowUuid);
      if (!row) return;
      row.cleanupDoneAt = now();
      row.cleanupFn = fnName;
    },

    async markAlerted(workflowUuid) {
      const row = rows.get(workflowUuid);
      if (row) row.alertedAt = now();
    },

    async getWatermark(key) {
      return watermarks.get(key) ?? null;
    },

    async setWatermark(key, epochMs) {
      watermarks.set(key, epochMs);
    },
  };
}

export interface InMemoryDbosStatusRow extends FailedDbosWorkflowRow {
  queueName: string | null;
  workflowDeadlineEpochMs: number | null;
  deduplicationId: string | null;
  startedAtEpochMs: number | null;
}

export type InMemoryDbosStatusSeed = FailedDbosWorkflowRow &
  Partial<
    Pick<
      InMemoryDbosStatusRow,
      | "queueName"
      | "workflowDeadlineEpochMs"
      | "deduplicationId"
      | "startedAtEpochMs"
    >
  >;

export interface InMemoryDbosStatusStore extends DbosStatusStore {
  inspect(workflowUuid: string): InMemoryDbosStatusRow | null;
}

/** Deterministic in-memory DBOS system-table projection for sweep unit tests. */
export function makeInMemoryDbosStatusStore(
  initialRows: InMemoryDbosStatusSeed[] = [],
  now: () => Date = () => new Date(),
): InMemoryDbosStatusStore {
  const rows = new Map<string, InMemoryDbosStatusRow>(
    initialRows.map((row) => [
      row.workflowUuid,
      {
        ...row,
        queueName: row.queueName ?? null,
        workflowDeadlineEpochMs: row.workflowDeadlineEpochMs ?? null,
        deduplicationId: row.deduplicationId ?? null,
        startedAtEpochMs: row.startedAtEpochMs ?? null,
      },
    ]),
  );

  const workflowProjection = (row: InMemoryDbosStatusRow): DbosWorkflowRow => ({
    workflowUuid: row.workflowUuid,
    name: row.name,
    status: row.status,
    applicationVersion: row.applicationVersion,
    createdAtEpochMs: row.createdAtEpochMs,
    recoveryAttempts: row.recoveryAttempts,
  });

  const failedProjection = (
    row: InMemoryDbosStatusRow,
  ): FailedDbosWorkflowRow => ({
    ...workflowProjection(row),
    updatedAtEpochMs: row.updatedAtEpochMs,
  });

  return {
    async listNonTerminalOnVersionsNotIn(liveVersions, limit) {
      return [...rows.values()]
        .filter(
          (row) =>
            (row.status === "PENDING" || row.status === "ENQUEUED") &&
            row.applicationVersion !== null &&
            !liveVersions.includes(row.applicationVersion),
        )
        .sort((left, right) => left.createdAtEpochMs - right.createdAtEpochMs)
        .slice(0, limit)
        .map(workflowProjection);
    },

    async adoptPending(workflowUuids) {
      const adopted: string[] = [];
      for (const workflowUuid of workflowUuids) {
        const row = rows.get(workflowUuid);
        if (!row || row.status !== "PENDING") continue;
        row.status = "ENQUEUED";
        row.queueName = "_dbos_internal_queue";
        row.applicationVersion = null;
        row.workflowDeadlineEpochMs = null;
        row.deduplicationId = null;
        row.startedAtEpochMs = null;
        row.updatedAtEpochMs = now().getTime();
        adopted.push(workflowUuid);
      }
      return adopted;
    },

    async clearVersionOnEnqueued(workflowUuids) {
      const cleared: string[] = [];
      for (const workflowUuid of workflowUuids) {
        const row = rows.get(workflowUuid);
        if (!row || row.status !== "ENQUEUED") continue;
        row.applicationVersion = null;
        cleared.push(workflowUuid);
      }
      return cleared;
    },

    async listNewlyTerminalFailed(sinceEpochMs, limit) {
      return [...rows.values()]
        .filter(
          (row) =>
            (row.status === "ERROR" ||
              row.status === "MAX_RECOVERY_ATTEMPTS_EXCEEDED") &&
            row.updatedAtEpochMs > sinceEpochMs,
        )
        .sort((left, right) => left.updatedAtEpochMs - right.updatedAtEpochMs)
        .slice(0, limit)
        .map(failedProjection);
    },

    inspect(workflowUuid) {
      const row = rows.get(workflowUuid);
      return row ? { ...row } : null;
    },
  };
}
