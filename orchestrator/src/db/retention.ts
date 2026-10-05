/** The retention collector's writes (ADR 0104 amendment, 2026-10-05).
 *
 * Two prunes, both batch-capped so a sweep tick stays bounded and a backlog
 * drains over cycles:
 *  - run details: the step ledger (`automation_step_run`) of runs that
 *    ended before the cutoff. The run row stays, stamped `details_pruned_at`
 *    so the frontier (a partial index on ended runs not yet pruned) shrinks
 *    as it drains instead of being rescanned every cycle.
 *  - DBOS records: the engine's `workflow_status` rows (the SDK schema
 *    cascades their step outputs, notifications, events and streams) for
 *    workflows created before the cutoff that are not PENDING/ENQUEUED/
 *    DELAYED and whose parent, if any, is not live either. A finished
 *    workflow is never recovered or replayed, so nothing reads these rows
 *    again.
 */

import { sql } from "drizzle-orm";

import { getDb } from "./client.ts";

export interface RetentionStore {
  /** Delete the step records of runs that ended before `cutoff`; at most
   * `batch` runs per call. Returns the number of runs pruned. */
  pruneRunDetails(cutoff: Date, batch: number): Promise<number>;
  /** Delete the DBOS records of terminal workflows created before `cutoff`;
   * at most `batch` workflows per call. Returns the number deleted. */
  pruneDbosWorkflows(cutoff: Date, batch: number): Promise<number>;
}

const DBOS_LIVE_STATUSES = ["PENDING", "ENQUEUED", "DELAYED"];

export function makeRetentionStore(db: ReturnType<typeof getDb> = getDb()): RetentionStore {
  return {
    async pruneRunDetails(cutoff, batch) {
      return db.transaction(async (tx) => {
        const doomed = await tx.execute(sql`
          select "id"
          from "automation_run"
          where "ended_at" is not null
            and "details_pruned_at" is null
            and "ended_at" < ${cutoff}
          order by "ended_at" asc
          limit ${batch}
        `);
        const ids = doomed.rows.map((row) => String(row.id));
        if (ids.length === 0) return 0;
        const list = textArray(ids);
        await tx.execute(sql`delete from "automation_step_run" where "run_id" = any(${list})`);
        await tx.execute(sql`
          update "automation_run" set "details_pruned_at" = now() where "id" = any(${list})
        `);
        return ids.length;
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
      const doomed = [...runs.entries()]
        .filter(([, r]) => r.endedAt !== null && r.endedAt < cutoff && !r.pruned)
        .sort((a, b) => a[1].endedAt!.getTime() - b[1].endedAt!.getTime())
        .slice(0, batch);
      for (const [, r] of doomed) {
        r.steps = 0;
        r.pruned = true;
      }
      return doomed.length;
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
