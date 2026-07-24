import type { Logger } from "pino";

import type {
  DbosStatusStore,
  SweepLedgerStore,
} from "../db/dbos-sweep.ts";
import type {
  FailedWorkflow,
  ResolvedPolicy,
  SweepContext,
  SweepPolicy,
} from "./policy.ts";
import type { SweepDecision } from "./sweeper.ts";

const TERMINAL_FAILURE_WATERMARK = "terminal_failures";
const TERMINAL_FAILURE_LOOKBACK_MS = 24 * 60 * 60 * 1_000;
const TERMINAL_FAILURE_BATCH_SIZE = 50;
// The DBOS SDK stamps workflow_status.updated_at in JS before the write
// commits, so a row can become visible AFTER a scan already advanced the
// watermark past its timestamp (narrowest case: a same-millisecond tie whose
// commit lands late). The watermark therefore never advances into the most
// recent lag window; once a timestamp is older than the lag, no new commits
// can appear behind it and advancing to it exactly is safe.
export const TERMINAL_FAILURE_VISIBILITY_LAG_MS = 10_000;
const ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "cancelled_stale",
  "cancelled_capped",
  "error",
]);
// alert_only and error decisions recur every cycle for a row that stays in
// the scan set, so they dedup on the ledger's alertedAt. A successful cancel
// is intrinsically once-per-workflow — the row turns CANCELLED and leaves the
// scan set — so cancel alerts always post, even after an earlier alert_only
// alert set alertedAt (the cancellation is the transition operators must see).
const DEDUPED_ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "error",
]);

export interface FailureScanResult {
  scanned: number;
  alerted: number;
  cleanupsRun: number;
  cleanupsFailed: number;
  watermark: number;
}

export interface SweepAlerter {
  alertDecisions(decisions: SweepDecision[]): Promise<void>;
  scanTerminalFailures(): Promise<FailureScanResult>;
}

export interface SweepAlerterDeps {
  ledger: SweepLedgerStore;
  status: DbosStatusStore;
  post: (text: string) => Promise<void>;
  policies: (name: string) => ResolvedPolicy;
  cleanupCtx: SweepContext;
  now: () => Date;
  log: Logger;
  maxCleanupAttempts: number;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function callbackName(
  callback: NonNullable<SweepPolicy["onTerminalFailure"]>,
): string {
  return callback.name || "onTerminalFailure";
}

function decisionMessage(decision: SweepDecision): string {
  const reason = decision.reason ? ` — ${decision.reason}` : "";
  return (
    `DBOS orphan sweep: ${decision.name} ${decision.workflowUuid} ` +
    `${decision.action}${reason}`
  );
}

function terminalFailureMessage(workflow: FailedWorkflow): string {
  return (
    `DBOS workflow terminal failure: ${workflow.name} ` +
    `${workflow.workflowUuid} — ${workflow.status}`
  );
}

/** Handle one terminal-failed row: unconditional alert first, then the policy
 * cleanup callback. Returns true when the row is fully processed (the
 * watermark may move past it), false when it must be re-scanned next cycle. */
async function processRow(
  deps: SweepAlerterDeps,
  cleanupFailures: Map<string, number>,
  row: { workflowUuid: string; name: string; status: string; updatedAtEpochMs: number },
  result: FailureScanResult,
): Promise<boolean> {
  const workflow: FailedWorkflow = {
    workflowUuid: row.workflowUuid,
    name: row.name,
    status: row.status,
    updatedAtEpochMs: row.updatedAtEpochMs,
  };

  // Generic ops alert FIRST, outside the callback, unconditionally — a buggy
  // cleanup can never suppress the alarm. Dedup on the terminal-failure
  // marker, never the sweep-decision alertedAt: a decision alert (e.g. an
  // adoption-race error) must not swallow the later terminal-failure alarm.
  const ledgerBeforeAlert = await deps.ledger.get(row.workflowUuid);
  if (!ledgerBeforeAlert?.terminalAlertedAt) {
    try {
      await deps.post(terminalFailureMessage(workflow));
      await deps.ledger.markTerminalAlerted(row.workflowUuid, row.name);
      result.alerted++;
    } catch (error) {
      deps.log.error(
        { error, workflowUuid: row.workflowUuid },
        "failed to post DBOS terminal failure alert",
      );
      return false;
    }
  }

  const policy = deps.policies(row.name);
  const callback =
    "onTerminalFailure" in policy ? policy.onTerminalFailure : undefined;
  const ledgerBeforeCleanup = await deps.ledger.get(row.workflowUuid);
  if (!callback || ledgerBeforeCleanup?.cleanupDoneAt) return true;

  const fnName = callbackName(callback);
  result.cleanupsRun++;
  try {
    await callback(deps.cleanupCtx, workflow);
    await deps.ledger.markCleanupDone(row.workflowUuid, row.name, fnName);
    cleanupFailures.delete(row.workflowUuid);
    return true;
  } catch (error) {
    result.cleanupsFailed++;
    const failures = (cleanupFailures.get(row.workflowUuid) ?? 0) + 1;
    cleanupFailures.set(row.workflowUuid, failures);
    deps.log.error(
      {
        error,
        workflowUuid: row.workflowUuid,
        cleanup: fnName,
        failures,
        maxCleanupAttempts: deps.maxCleanupAttempts,
      },
      "DBOS terminal failure cleanup failed",
    );

    if (failures < deps.maxCleanupAttempts) return false;

    // Attempt cap hit (per process lifetime): the abandonment alert must land
    // before the durable gave-up mark allows the watermark to move on.
    const gaveUpName = `gave-up:${fnName}`;
    try {
      await deps.post(
        `DBOS cleanup abandoned: ${row.name} ${row.workflowUuid} — ` +
          `${fnName} failed ${failures} consecutive times`,
      );
    } catch (postError) {
      deps.log.error(
        { error: postError, workflowUuid: row.workflowUuid, cleanup: fnName },
        "failed to post abandoned DBOS cleanup alert",
      );
      return false;
    }
    try {
      await deps.ledger.markCleanupDone(row.workflowUuid, row.name, gaveUpName);
      cleanupFailures.delete(row.workflowUuid);
    } catch (markError) {
      deps.log.error(
        { error: markError, workflowUuid: row.workflowUuid, cleanup: fnName },
        "failed to record abandoned DBOS cleanup",
      );
      return false;
    }
    return true;
  }
}

/**
 * Failure cleanup callbacks are at-least-once. A process can crash after a
 * callback's side effect and before markCleanupDone commits, so callbacks must
 * tolerate a repeat. The in-memory attempt cap resets across pods/restarts,
 * which permits extra idempotent cleanup attempts but never a lost alarm. Once
 * the cap is reached, every retry remains capped: the abandonment alert posts
 * first, and only its success permits the durable gave-up cleanup mark and
 * watermark advance.
 */
export function makeSweepAlerter(deps: SweepAlerterDeps): SweepAlerter {
  const cleanupFailures = new Map<string, number>();
  let expandedRescan:
    | { watermark: number; queryLimit: number }
    | undefined;

  return {
    async alertDecisions(decisions) {
      for (const decision of decisions) {
        if (!ALERT_ACTIONS.has(decision.action)) continue;
        try {
          if (DEDUPED_ALERT_ACTIONS.has(decision.action)) {
            const ledger = await deps.ledger.get(decision.workflowUuid);
            if (ledger?.alertedAt) continue;
          }
          await deps.post(decisionMessage(decision));
          await deps.ledger.markAlerted(decision.workflowUuid, decision.name);
        } catch (error) {
          deps.log.error(
            {
              error,
              workflowUuid: decision.workflowUuid,
              action: decision.action,
            },
            "failed to post DBOS sweep decision alert",
          );
        }
      }
    },

    async scanTerminalFailures() {
      const since =
        (await deps.ledger.getWatermark(TERMINAL_FAILURE_WATERMARK)) ??
        deps.now().getTime() - TERMINAL_FAILURE_LOOKBACK_MS;
      const queryLimit =
        expandedRescan?.watermark === since
          ? expandedRescan.queryLimit
          : TERMINAL_FAILURE_BATCH_SIZE;
      expandedRescan = undefined;
      const rows = await deps.status.listNewlyTerminalFailed(
        since,
        queryLimit,
      );
      const result: FailureScanResult = {
        scanned: rows.length,
        alerted: 0,
        cleanupsRun: 0,
        cleanupsFailed: 0,
        watermark: since,
      };

      // Rewind one millisecond before the first blocked row, or before the
      // final timestamp of a full batch. This re-includes every row tied at
      // that timestamp; ledger dedup keeps completed work quiet. A full batch
      // also expands the next query enough to get past a tied prefix that
      // would otherwise consume the same LIMIT forever.
      let firstBlockedAt: number | undefined;

      for (const row of rows) {
        const processed = await processRow(deps, cleanupFailures, row, result);
        if (!processed && firstBlockedAt === undefined) {
          firstBlockedAt = row.updatedAtEpochMs;
        }
      }

      if (firstBlockedAt !== undefined) {
        result.watermark = firstBlockedAt - 1;
      } else if (rows.length > 0) {
        const lastUpdatedAt = rows[rows.length - 1]!.updatedAtEpochMs;
        if (rows.length < queryLimit) {
          result.watermark = lastUpdatedAt;
        } else {
          result.watermark = lastUpdatedAt - 1;
          const tiedAtBoundary = rows.filter(
            (row) => row.updatedAtEpochMs === lastUpdatedAt,
          ).length;
          expandedRescan = {
            watermark: result.watermark,
            queryLimit: TERMINAL_FAILURE_BATCH_SIZE + tiedAtBoundary,
          };
        }
      }

      // Trail wall clock by the visibility lag (see the constant above) so a
      // late-committing row can never land behind the watermark. The clamped
      // window is re-scanned next cycle; ledger dedup keeps it quiet.
      result.watermark = Math.max(
        since,
        Math.min(
          result.watermark,
          deps.now().getTime() - TERMINAL_FAILURE_VISIBILITY_LAG_MS,
        ),
      );
      if (expandedRescan) expandedRescan.watermark = result.watermark;

      if (result.watermark !== since) {
        await deps.ledger.setWatermark(
          TERMINAL_FAILURE_WATERMARK,
          result.watermark,
        );
      }

      return result;
    },
  };
}

export function makeDisabledSweepAlerter(log: Logger): SweepAlerter {
  let logged = false;
  const logDisabled = () => {
    if (logged) return;
    logged = true;
    log.warn("alerts disabled: no ops channel configured");
  };
  return {
    async alertDecisions() {
      logDisabled();
    },
    async scanTerminalFailures() {
      logDisabled();
      return {
        scanned: 0,
        alerted: 0,
        cleanupsRun: 0,
        cleanupsFailed: 0,
        watermark: 0,
      };
    },
  };
}
