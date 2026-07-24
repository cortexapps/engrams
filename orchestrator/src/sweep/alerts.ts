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

// Progress through terminal failures is tracked by the ledger's completion
// marks (terminal_alerted_at + cleanup_done_at) via a store-level anti-join,
// never by a timestamp watermark: a row whose commit becomes visible late, or
// that ties another row's timestamp, is simply still in the set next cycle.
// The lookback only bounds the query; a week tolerates long outages.
const TERMINAL_FAILURE_LOOKBACK_MS = 7 * 24 * 60 * 60 * 1_000;
const TERMINAL_FAILURE_BATCH_SIZE = 50;
const ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "cancelled_stale",
  "cancelled_capped",
  "cancelled_policy",
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
 * cleanup callback. Progress is recorded as ledger completion marks; a row
 * that returns without both marks simply stays in the anti-join's result set
 * and is retried next cycle. */
async function processRow(
  deps: SweepAlerterDeps,
  cleanupFailures: Map<string, number>,
  row: { workflowUuid: string; name: string; status: string; updatedAtEpochMs: number },
  result: FailureScanResult,
): Promise<void> {
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
      return;
    }
  }

  const policy = deps.policies(row.name);
  const callback =
    "onTerminalFailure" in policy ? policy.onTerminalFailure : undefined;
  const ledgerBeforeCleanup = await deps.ledger.get(row.workflowUuid);
  if (ledgerBeforeCleanup?.cleanupDoneAt) return;
  if (!callback) {
    // Record "nothing to clean up" so the anti-join stops returning the row.
    try {
      await deps.ledger.markCleanupDone(row.workflowUuid, row.name, "none");
    } catch (error) {
      deps.log.error(
        { error, workflowUuid: row.workflowUuid },
        "failed to record no-op DBOS cleanup",
      );
    }
    return;
  }

  const fnName = callbackName(callback);
  result.cleanupsRun++;
  try {
    await callback(deps.cleanupCtx, workflow);
    await deps.ledger.markCleanupDone(row.workflowUuid, row.name, fnName);
    cleanupFailures.delete(row.workflowUuid);
    return;
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

    if (failures < deps.maxCleanupAttempts) return;

    // Attempt cap hit (per process lifetime): the abandonment alert must land
    // before the durable gave-up mark removes the row from the unhandled set.
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
      return;
    }
    try {
      await deps.ledger.markCleanupDone(row.workflowUuid, row.name, gaveUpName);
      cleanupFailures.delete(row.workflowUuid);
    } catch (markError) {
      deps.log.error(
        { error: markError, workflowUuid: row.workflowUuid, cleanup: fnName },
        "failed to record abandoned DBOS cleanup",
      );
    }
  }
}

/**
 * Failure cleanup callbacks are at-least-once. A process can crash after a
 * callback's side effect and before markCleanupDone commits, so callbacks must
 * tolerate a repeat. The in-memory attempt cap resets across pods/restarts,
 * which permits extra idempotent cleanup attempts but never a lost alarm. Once
 * the cap is reached, every retry remains capped: the abandonment alert posts
 * first, and only its success permits the durable gave-up cleanup mark that
 * removes the row from the unhandled set.
 */
export function makeSweepAlerter(deps: SweepAlerterDeps): SweepAlerter {
  const cleanupFailures = new Map<string, number>();

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
      const rows = await deps.status.listUnhandledTerminalFailures(
        TERMINAL_FAILURE_LOOKBACK_MS,
        TERMINAL_FAILURE_BATCH_SIZE,
      );
      const result: FailureScanResult = {
        scanned: rows.length,
        alerted: 0,
        cleanupsRun: 0,
        cleanupsFailed: 0,
      };

      for (const row of rows) {
        await processRow(deps, cleanupFailures, row, result);
      }

      return result;
    },
  };
}

