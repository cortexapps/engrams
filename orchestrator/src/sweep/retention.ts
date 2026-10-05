/** The retention collector (ADR 0104 amendment, 2026-10-05): under the
 * sweep leader's lease, once per cycle, prune what the org's retention
 * policy says is old — a batch at a time, so a tick stays bounded and a
 * backlog drains over the following cycles. The policy is the admin's
 * (Settings → Retention), read fresh every cycle. */

import type { Logger } from "pino";

import type { OrgSettingStore, RetentionPolicy } from "../db/org-settings.ts";
import { readRetentionPolicy } from "../db/org-settings.ts";
import type { RetentionStore } from "../db/retention.ts";

/** Runs and workflows pruned per cycle, each. A minute's cycle drains a
 * few thousand rows an hour without a long transaction. */
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
  runsPruned: number;
  workflowsPruned: number;
}

export async function runRetentionTick(deps: RetentionDeps): Promise<RetentionResult> {
  const policy = await readRetentionPolicy(deps.settings);
  const cutoff = new Date(deps.now().getTime() - policy.runDetailDays * 24 * 60 * 60 * 1_000);
  const batch = deps.batch ?? RETENTION_BATCH;
  const runsPruned = await deps.store.pruneRunDetails(cutoff, batch);
  const workflowsPruned = await deps.store.pruneDbosWorkflows(cutoff, batch);
  if (runsPruned > 0 || workflowsPruned > 0) {
    deps.log.info(
      { component: "dbos-sweep", runDetailDays: policy.runDetailDays, runsPruned, workflowsPruned },
      "retention: pruned run details and finished DBOS workflows past the policy",
    );
  }
  return { policy, cutoff: cutoff.toISOString(), runsPruned, workflowsPruned };
}
