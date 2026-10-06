/** The retention collector's writes (ADR 0104 amendment, 2026-10-05).
 *
 * Two prunes, both batch-capped so a sweep tick stays bounded and a backlog
 * drains over cycles:
 *  - run details: the step ledger (`automation_step_run`) of runs that
 *    ended before the cutoff, at most `batch` step ROWS per call (a run with
 *    a loop holds a row per iteration per attempt, so a run count would not
 *    bound the transaction). The run row stays, stamped `details_pruned_at`
 *    once its rows are gone, so the frontier (a partial index on ended runs
 *    not yet pruned) shrinks as it drains instead of being rescanned.
 *  - DBOS records: the engine's `workflow_status` rows (the SDK schema
 *    cascades their step outputs, notifications, events and streams) for
 *    workflows created before the cutoff that are not PENDING/ENQUEUED/
 *    DELAYED and whose parent, if any, is not live either. A finished
 *    workflow is never recovered or replayed, so nothing reads these rows
 *    again.
 */

import { sql } from "drizzle-orm";

import { getDb } from "./client.ts";

export interface RunDetailPrune {
  /** Step rows deleted this call. */
  rows: number;
  /** Runs stamped as pruned this call (their last rows went, or they never
   * had any). */
  runs: number;
}

export interface RetentionStore {
  /** Delete the step records of the oldest runs that ended before `cutoff`,
   * at most `batch` step rows per call, and stamp the runs left empty. */
  pruneRunDetails(cutoff: Date, batch: number): Promise<RunDetailPrune>;
  /** Delete the DBOS records of terminal workflows created before `cutoff`;
   * at most `batch` workflows per call. Returns the number deleted. */
  pruneDbosWorkflows(cutoff: Date, batch: number): Promise<number>;
}

const DBOS_LIVE_STATUSES = ["PENDING", "ENQUEUED", "DELAYED"];

export function makeRetentionStore(db: ReturnType<typeof getDb> = getDb()): RetentionStore {
  return {
    async pruneRunDetails(cutoff, batch) {
      return db.transaction(async (tx) => {
        // The frontier: the oldest ended runs not yet stamped. Walked
        // through the partial index, bounded by `batch` either way.
        const frontier = sql`
          select "id"
          from "automation_run"
          where "ended_at" is not null
            and "details_pruned_at" is null
            and "ended_at" < ${cutoff}
          order by "ended_at" asc
          limit ${batch}
        `;
        const deleted = await tx.execute(sql`
          delete from "automation_step_run"
          where "ctid" in (
            select "s"."ctid"
            from (${frontier}) as "r"
            join "automation_step_run" as "s" on "s"."run_id" = "r"."id"
            limit ${batch}
          )
        `);
        // Stamp the frontier runs left without rows: the ones just emptied,
        // and the ones that never had any (a dry run, a run that failed
        // before its first step).
        const stamped = await tx.execute(sql`
          update "automation_run"
          set "details_pruned_at" = now()
          where "id" in (${frontier})
            and not exists (
              select 1 from "automation_step_run" as "s" where "s"."run_id" = "automation_run"."id"
            )
        `);
        return { rows: deleted.rowCount ?? 0, runs: stamped.rowCount ?? 0 };
      });
    },

    async pruneDbosWorkflows(cutoff, batch) {
      // One delete: the SDK's schema cascades every child table (step
      // outputs, inputs, notifications, events, streams, the queue row) from
      // workflow_status. A terminal workflow whose parent is still live is
      // kept — the parent may still read the child's result.
      const deleted = await db.execute(sql`
        delete from "dbos"."workflow_status" as "w"
        where "w"."workflow_uuid" in (
          select "c"."workflow_uuid"
          from "dbos"."workflow_status" as "c"
          where "c"."created_at" < ${cutoff.getTime()}
            and "c"."status" <> all(${textArray(DBOS_LIVE_STATUSES)})
            and not exists (
              select 1 from "dbos"."workflow_status" as "p"
              where "p"."workflow_uuid" = "c"."parent_workflow_id"
                and "p"."status" = any(${textArray(DBOS_LIVE_STATUSES)})
            )
          order by "c"."created_at" asc
          limit ${batch}
        )
      `);
      return deleted.rowCount ?? 0;
    },
  };
}

function textArray(values: string[]) {
  return sql`array[${sql.join(values.map((value) => sql`${value}`), sql`, `)}]::text[]`;
}

/** Deterministic in-memory projection for the sweep unit tests: runs with
 * their end time and step count, DBOS workflows with status and creation. */
export function makeInMemoryRetentionStore(seed: {
  runs?: Array<{ id: string; endedAt: Date | null; steps: number; pruned?: boolean }>;
  workflows?: Array<{ id: string; status: string; createdAt: Date; parentId?: string }>;
} = {}): RetentionStore & {
  runs: Map<string, { endedAt: Date | null; steps: number; pruned: boolean }>;
  workflows: Map<string, { status: string; createdAt: Date; parentId?: string }>;
} {
  const runs = new Map(
    (seed.runs ?? []).map((r) => [r.id, { endedAt: r.endedAt, steps: r.steps, pruned: r.pruned ?? false }]),
  );
  const workflows = new Map(
    (seed.workflows ?? []).map((w) => [
      w.id,
      { status: w.status, createdAt: w.createdAt, ...(w.parentId !== undefined ? { parentId: w.parentId } : {}) },
    ]),
  );
  const parentLive = (parentId: string | undefined) => {
    if (parentId === undefined) return false;
    const parent = workflows.get(parentId);
    return parent !== undefined && DBOS_LIVE_STATUSES.includes(parent.status);
  };
  return {
    runs,
    workflows,
    async pruneRunDetails(cutoff, batch) {
      const frontier = [...runs.values()]
        .filter((r) => r.endedAt !== null && r.endedAt < cutoff && !r.pruned)
        .sort((a, b) => a.endedAt!.getTime() - b.endedAt!.getTime())
        .slice(0, batch);
      let rows = 0;
      for (const r of frontier) {
        const take = Math.min(r.steps, batch - rows);
        r.steps -= take;
        rows += take;
      }
      let stamped = 0;
      for (const r of frontier) {
        if (r.steps === 0) {
          r.pruned = true;
          stamped++;
        }
      }
      return { rows, runs: stamped };
    },
    async pruneDbosWorkflows(cutoff, batch) {
      const doomed = [...workflows.entries()]
        .filter(
          ([, w]) => w.createdAt < cutoff && !DBOS_LIVE_STATUSES.includes(w.status) && !parentLive(w.parentId),
        )
        .sort((a, b) => a[1].createdAt.getTime() - b[1].createdAt.getTime())
        .slice(0, batch);
      for (const [id] of doomed) workflows.delete(id);
      return doomed.length;
    },
  };
}
