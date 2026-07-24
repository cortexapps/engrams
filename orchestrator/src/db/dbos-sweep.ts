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
  /**
   * Delete rows whose last_seen is older than retentionMs. Rows older than
   * the grace window are already dead for liveness purposes (liveVersions
   * takes max(last_seen) per version; the flip fence only checks fresh
   * rows), so any retention >= the grace window cannot change behavior —
   * this only bounds table growth across deploys. Returns the deleted count.
   */
  prune(retentionMs: number): Promise<number>;
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
  alertedAt: Date | null;
  terminalAlertedAt: Date | null;
}

export interface SweepLedgerStore {
  recordSweep(workflowUuid: string, workflowName: string): Promise<SweepLedgerRow>;
  get(workflowUuid: string): Promise<SweepLedgerRow | null>;
  setSuppressed(workflowUuid: string, suppressed: boolean): Promise<void>;
  /** Upserts: an alert-only or naturally-failed workflow has no prior sweep
   * row, and alert dedup must still stick for it. */
  markAlerted(workflowUuid: string, workflowName: string): Promise<void>;
  /** Terminal-failure alerts dedup separately from sweep-decision alerts —
   * a decision alert must never suppress the terminal-failure alarm. */
  markTerminalAlerted(
    workflowUuid: string,
    workflowName: string,
  ): Promise<void>;
}

export interface DbosWorkflowRow {
  workflowUuid: string;
  name: string;
  status: string;
  applicationVersion: string | null;
  createdAtEpochMs: number;
  recoveryAttempts: number;
}

export interface DbosSweepTransitionInput {
  workflowUuid: string;
  expectedVersion: string;
  workflowName: string;
}

export type DbosSweepTransitionResult =
  | { flipped: false }
  | { flipped: true; sweepCount: number };

export interface FailedDbosWorkflowRow extends DbosWorkflowRow {
  updatedAtEpochMs: number;
}

export interface DbosStatusStore {
  listNonTerminalOnVersionsNotIn(
    liveVersions: string[],
    limit: number,
    after?: { createdAtEpochMs: number; workflowUuid: string },
  ): Promise<DbosWorkflowRow[]>;
  adoptPendingRecording(
    input: DbosSweepTransitionInput,
    graceMs: number,
  ): Promise<DbosSweepTransitionResult>;
  clearVersionOnEnqueuedRecording(
    input: DbosSweepTransitionInput,
    graceMs: number,
  ): Promise<DbosSweepTransitionResult>;
  /**
   * Terminal failures inside the lookback window that have not been logged
   * yet: an anti-join on the ledger excludes rows whose terminal_alerted_at
   * is recorded. The ledger is the single source of truth for progress —
   * there is no watermark to advance, so a row whose commit becomes visible
   * late, or that ties another row's timestamp, is simply still in the set
   * next cycle.
   */
  listUnhandledTerminalFailures(
    lookbackMs: number,
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
    alertedAt: nullableDate(row.alerted_at),
    terminalAlertedAt: nullableDate(row.terminal_alerted_at),
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

function recordSweepQuery(
  workflowUuid: string,
  workflowName: string,
): SQL {
  return sql`
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
              "alerted_at", "terminal_alerted_at"
  `;
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

    async prune(retentionMs) {
      const result = await db.execute(sql`
        delete from "dbos_version_heartbeats"
        where "last_seen" < now() - (${retentionMs} * interval '1 millisecond')
        returning "application_version"
      `);
      return affectedRows(result);
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
      const result = await db.execute(
        recordSweepQuery(workflowUuid, workflowName),
      );
      const row = result.rows[0];
      if (!row) throw new Error(`sweep ledger upsert returned no row for ${workflowUuid}`);
      return ledgerRow(row);
    },

    async get(workflowUuid) {
      const result = await db.execute(sql`
        select "workflow_uuid", "workflow_name", "sweep_count",
               "first_swept_at", "last_swept_at", "suppressed",
               "alerted_at", "terminal_alerted_at"
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

    async markAlerted(workflowUuid, workflowName) {
      await db.execute(sql`
        insert into "dbos_sweep_ledger"
          ("workflow_uuid", "workflow_name", "alerted_at")
        values (${workflowUuid}, ${workflowName}, now())
        on conflict ("workflow_uuid") do update
        set "alerted_at" = now()
      `);
    },

    async markTerminalAlerted(workflowUuid, workflowName) {
      await db.execute(sql`
        insert into "dbos_sweep_ledger"
          ("workflow_uuid", "workflow_name", "terminal_alerted_at")
        values (${workflowUuid}, ${workflowName}, now())
        on conflict ("workflow_uuid") do update
        set "terminal_alerted_at" = now()
      `);
    },

  };
}

export function makeDbosStatusStore(
  db: ReturnType<typeof getDb> = getDb(),
): DbosStatusStore {
  return {
    async listNonTerminalOnVersionsNotIn(liveVersions, limit, after) {
      const afterClause = after
        ? sql`
          and ("created_at", "workflow_uuid") >
              (${after.createdAtEpochMs}, ${after.workflowUuid})
        `
        : sql``;
      const result = await db.execute(sql`
        select "workflow_uuid", "name", "status", "application_version",
               "created_at", coalesce("recovery_attempts", 0) as "recovery_attempts"
        from "dbos"."workflow_status"
        where "status" in ('PENDING', 'ENQUEUED')
          and "application_version" is not null
          and "application_version" <> all(${textArray(liveVersions)})
          ${afterClause}
        order by "created_at" asc, "workflow_uuid" asc
        limit ${limit}
      `);
      return result.rows.map(dbosWorkflowRow);
    },

    async adoptPendingRecording(input, graceMs) {
      return db.transaction(async (tx) => {
        const flipped = await tx.execute(sql`
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
            and "workflow_uuid" = ${input.workflowUuid}
            and "application_version" = ${input.expectedVersion}
            and not exists (
              select 1
              from "dbos_version_heartbeats" as "heartbeat"
              where "heartbeat"."application_version" =
                    ${input.expectedVersion}
                and "heartbeat"."last_seen" >
                    now() - (${graceMs} * interval '1 millisecond')
            )
          returning "workflow_uuid"
        `);
        if (affectedRows(flipped) === 0) return { flipped: false };

        const recorded = await tx.execute(
          recordSweepQuery(input.workflowUuid, input.workflowName),
        );
        const row = recorded.rows[0];
        if (!row) {
          throw new Error(
            `sweep ledger upsert returned no row for ${input.workflowUuid}`,
          );
        }
        return {
          flipped: true,
          sweepCount: numberValue(row.sweep_count),
        };
      });
    },

    async clearVersionOnEnqueuedRecording(input, graceMs) {
      return db.transaction(async (tx) => {
        const flipped = await tx.execute(sql`
          update "dbos"."workflow_status"
          set "application_version" = null
          where "status" = 'ENQUEUED'
            and "workflow_uuid" = ${input.workflowUuid}
            and "application_version" = ${input.expectedVersion}
            and not exists (
              select 1
              from "dbos_version_heartbeats" as "heartbeat"
              where "heartbeat"."application_version" =
                    ${input.expectedVersion}
                and "heartbeat"."last_seen" >
                    now() - (${graceMs} * interval '1 millisecond')
            )
          returning "workflow_uuid"
        `);
        if (affectedRows(flipped) === 0) return { flipped: false };

        const recorded = await tx.execute(
          recordSweepQuery(input.workflowUuid, input.workflowName),
        );
        const row = recorded.rows[0];
        if (!row) {
          throw new Error(
            `sweep ledger upsert returned no row for ${input.workflowUuid}`,
          );
        }
        return {
          flipped: true,
          sweepCount: numberValue(row.sweep_count),
        };
      });
    },

    async listUnhandledTerminalFailures(lookbackMs, limit) {
      const result = await db.execute(sql`
        select "workflow_uuid", "name", "status", "application_version",
               "created_at", "updated_at",
               coalesce("recovery_attempts", 0) as "recovery_attempts"
        from "dbos"."workflow_status" as "ws"
        where "ws"."status" in ('ERROR', 'MAX_RECOVERY_ATTEMPTS_EXCEEDED')
          and "ws"."updated_at" >
              (extract(epoch from now()) * 1000)::bigint - ${lookbackMs}
          and not exists (
            select 1
            from "dbos_sweep_ledger" as "ledger"
            where "ledger"."workflow_uuid" = "ws"."workflow_uuid"
              and "ledger"."terminal_alerted_at" is not null
          )
        order by "ws"."updated_at" asc
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

    async prune(retentionMs) {
      const cutoff = now().getTime() - retentionMs;
      let deleted = 0;
      for (const [key, lastSeen] of rows) {
        if (lastSeen < cutoff) {
          rows.delete(key);
          deleted++;
        }
      }
      return deleted;
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

function emptyLedgerRow(
  workflowUuid: string,
  workflowName: string,
): SweepLedgerRow {
  return {
    workflowUuid,
    workflowName,
    sweepCount: 0,
    firstSweptAt: null,
    lastSweptAt: null,
    suppressed: false,
    alertedAt: null,
    terminalAlertedAt: null,
  };
}

function cloneLedgerRow(row: SweepLedgerRow): SweepLedgerRow {
  return {
    ...row,
    firstSweptAt: row.firstSweptAt ? new Date(row.firstSweptAt) : null,
    lastSweptAt: row.lastSweptAt ? new Date(row.lastSweptAt) : null,
    alertedAt: row.alertedAt ? new Date(row.alertedAt) : null,
    terminalAlertedAt: row.terminalAlertedAt
      ? new Date(row.terminalAlertedAt)
      : null,
  };
}

/** Deterministic in-memory ledger shared by sweep unit tests. */
export function makeInMemorySweepLedgerStore(
  now: () => Date = () => new Date(),
): SweepLedgerStore {
  const rows = new Map<string, SweepLedgerRow>();
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
            alertedAt: null,
            terminalAlertedAt: null,
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

    async markAlerted(workflowUuid, workflowName) {
      const row = rows.get(workflowUuid) ?? emptyLedgerRow(workflowUuid, workflowName);
      rows.set(workflowUuid, row);
      row.alertedAt = now();
    },

    async markTerminalAlerted(workflowUuid, workflowName) {
      const row = rows.get(workflowUuid) ?? emptyLedgerRow(workflowUuid, workflowName);
      rows.set(workflowUuid, row);
      row.terminalAlertedAt = now();
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

export interface InMemoryDbosStatusOptions {
  isVersionLive?: (
    applicationVersion: string,
    graceMs: number,
  ) => boolean | Promise<boolean>;
  recordSweep?: (
    workflowUuid: string,
    workflowName: string,
  ) => Promise<SweepLedgerRow>;
  /** Mirrors the PG anti-join: excluded once the terminal alert is recorded.
   * Tests wire this to the in-memory ledger. */
  isTerminalFailureHandled?: (
    workflowUuid: string,
  ) => boolean | Promise<boolean>;
}

/** Deterministic in-memory DBOS system-table projection for sweep unit tests. */
export function makeInMemoryDbosStatusStore(
  initialRows: InMemoryDbosStatusSeed[] = [],
  now: () => Date = () => new Date(),
  options: InMemoryDbosStatusOptions = {},
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

  const recordTransition = async (
    row: InMemoryDbosStatusRow,
    input: DbosSweepTransitionInput,
    mutate: () => void,
  ): Promise<DbosSweepTransitionResult> => {
    const before = { ...row };
    mutate();
    try {
      const recorded = await options.recordSweep?.(
        input.workflowUuid,
        input.workflowName,
      );
      return {
        flipped: true,
        sweepCount: recorded?.sweepCount ?? 1,
      };
    } catch (error) {
      rows.set(input.workflowUuid, before);
      throw error;
    }
  };

  return {
    async listNonTerminalOnVersionsNotIn(liveVersions, limit, after) {
      return [...rows.values()]
        .filter(
          (row) =>
            (row.status === "PENDING" || row.status === "ENQUEUED") &&
            row.applicationVersion !== null &&
            !liveVersions.includes(row.applicationVersion) &&
            (after === undefined ||
              row.createdAtEpochMs > after.createdAtEpochMs ||
              (row.createdAtEpochMs === after.createdAtEpochMs &&
                row.workflowUuid > after.workflowUuid)),
        )
        .sort(
          (left, right) =>
            left.createdAtEpochMs - right.createdAtEpochMs ||
            (left.workflowUuid < right.workflowUuid
              ? -1
              : left.workflowUuid > right.workflowUuid
                ? 1
                : 0),
        )
        .slice(0, limit)
        .map(workflowProjection);
    },

    async adoptPendingRecording(input, graceMs) {
      const row = rows.get(input.workflowUuid);
      if (
        !row ||
        row.status !== "PENDING" ||
        row.applicationVersion !== input.expectedVersion ||
        (await options.isVersionLive?.(
          input.expectedVersion,
          graceMs,
        )) === true
      ) {
        return { flipped: false };
      }
      return recordTransition(row, input, () => {
        row.status = "ENQUEUED";
        row.queueName = "_dbos_internal_queue";
        row.applicationVersion = null;
        row.workflowDeadlineEpochMs = null;
        row.deduplicationId = null;
        row.startedAtEpochMs = null;
        row.updatedAtEpochMs = now().getTime();
      });
    },

    async clearVersionOnEnqueuedRecording(input, graceMs) {
      const row = rows.get(input.workflowUuid);
      if (
        !row ||
        row.status !== "ENQUEUED" ||
        row.applicationVersion !== input.expectedVersion ||
        (await options.isVersionLive?.(
          input.expectedVersion,
          graceMs,
        )) === true
      ) {
        return { flipped: false };
      }
      return recordTransition(row, input, () => {
        row.applicationVersion = null;
      });
    },

    async listUnhandledTerminalFailures(lookbackMs, limit) {
      const cutoff = now().getTime() - lookbackMs;
      const candidates = [...rows.values()]
        .filter(
          (row) =>
            (row.status === "ERROR" ||
              row.status === "MAX_RECOVERY_ATTEMPTS_EXCEEDED") &&
            row.updatedAtEpochMs > cutoff,
        )
        .sort(
          (left, right) => left.updatedAtEpochMs - right.updatedAtEpochMs,
        );
      const unhandled: InMemoryDbosStatusRow[] = [];
      for (const row of candidates) {
        if (unhandled.length >= limit) break;
        const handled = await options.isTerminalFailureHandled?.(
          row.workflowUuid,
        );
        if (!handled) unhandled.push(row);
      }
      return unhandled.map(failedProjection);
    },

    inspect(workflowUuid) {
      const row = rows.get(workflowUuid);
      return row ? { ...row } : null;
    },
  };
}
