/** The retention collector (ADR 0104 amendment, 2026-10-05): under the
 * sweep leader's lease, once per cycle, prune what the org's retention
 * policy says is old — a batch at a time, so a tick stays bounded and a
 * backlog drains over the following cycles. The policy is the admin's
 * (Settings → Retention), read fresh every cycle. */

import type { Logger } from "pino";

import type { OrgSettingStore, RetentionPolicy } from "../db/org-settings.ts";
import { readRetentionPolicy } from "../db/org-settings.ts";
import type { RetentionStore } from "../db/retention.ts";

/** Rows per prune per cycle (step rows; DBOS workflows). A minute's cycle
 * drains thousands of rows an hour without a long transaction. */
export const RETENTION_BATCH = 500;

export interface RetentionDeps {
  settings: Pick<OrgSettingStore, "get">;
  store: RetentionStore;
  now: () => Date;
  batch?: number;
  log: Logger;
}

export interface RetentionResult {
  policy: RetentionPolicy;
  cutoff: string;
  /** Step rows deleted. */
  runDetailRowsPruned: number;
  /** Runs stamped as pruned. */
  runsPruned: number;
  workflowsPruned: number;
}

export async function runRetentionTick(deps: RetentionDeps): Promise<RetentionResult> {
  const policy = await readRetentionPolicy(deps.settings);
  const cutoff = new Date(deps.now().getTime() - policy.runDetailDays * 24 * 60 * 60 * 1_000);
  const batch = deps.batch ?? RETENTION_BATCH;
  const details = await deps.store.pruneRunDetails(cutoff, batch);
  const workflowsPruned = await deps.store.pruneDbosWorkflows(cutoff, batch);
  if (details.rows > 0 || details.runs > 0 || workflowsPruned > 0) {
    deps.log.info(
      {
        component: "dbos-sweep",
        runDetailDays: policy.runDetailDays,
        runDetailRowsPruned: details.rows,
        runsPruned: details.runs,
        workflowsPruned,
      },
      "retention: pruned run details and finished DBOS workflows past the policy",
    );
  }
  return {
    policy,
    cutoff: cutoff.toISOString(),
    runDetailRowsPruned: details.rows,
    runsPruned: details.runs,
    workflowsPruned,
  };
}
