import type { Logger } from "pino";

import type {
  DbosStatusStore,
  SweepLedgerStore,
} from "../db/dbos-sweep.ts";
import type { SweepDecision } from "./sweeper.ts";

// Progress through terminal failures is tracked by the ledger's
// terminal_alerted_at mark via a store-level anti-join, never by a timestamp
// watermark: a row whose commit becomes visible late, or that ties another
// row's timestamp, is simply still in the set next cycle. The lookback only
// bounds the query; a week tolerates long outages.
const TERMINAL_FAILURE_LOOKBACK_MS = 7 * 24 * 60 * 60 * 1_000;
const TERMINAL_FAILURE_BATCH_SIZE = 50;

const ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "cancelled_stale",
  "cancelled_capped",
  "error",
]);
// alert_only and error decisions recur every cycle for a row that stays in
// the scan set, so they dedup on the ledger's alertedAt. A successful cancel
// is intrinsically once-per-workflow — the row turns CANCELLED and leaves the
// scan set — so cancel alerts always log, even after an earlier alert_only
// alert set alertedAt (the cancellation is the transition operators must see).
const DEDUPED_ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "error",
]);

export interface FailureScanResult {
  scanned: number;
}

export interface SweepAlerter {
  alertDecisions(decisions: SweepDecision[]): Promise<void>;
  scanTerminalFailures(): Promise<FailureScanResult>;
}

/**
 * Alerts are error-level log lines (component=dbos-sweep); the operator wires
 * log-based alerting on top. Internal hiccups (a failed ledger write) log at
 * warn so the error level stays a clean alert signal, and the un-marked row
 * is simply re-logged next cycle.
 */
export interface SweepAlerterDeps {
  ledger: SweepLedgerStore;
  status: DbosStatusStore;
  log: Logger;
}

export function makeSweepAlerter(deps: SweepAlerterDeps): SweepAlerter {
  return {
    async alertDecisions(decisions) {
      for (const decision of decisions) {
        if (!ALERT_ACTIONS.has(decision.action)) continue;
        try {
          if (DEDUPED_ALERT_ACTIONS.has(decision.action)) {
            const ledger = await deps.ledger.get(decision.workflowUuid);
            if (ledger?.alertedAt) continue;
          }
          deps.log.error(
            {
              component: "dbos-sweep",
              workflowUuid: decision.workflowUuid,
              name: decision.name,
              action: decision.action,
              reason: decision.reason,
            },
            "DBOS orphan sweep alert",
          );
          await deps.ledger.markAlerted(decision.workflowUuid, decision.name);
        } catch (error) {
          deps.log.warn(
            {
              error,
              workflowUuid: decision.workflowUuid,
              action: decision.action,
            },
            "failed to record DBOS sweep decision alert",
          );
        }
      }
    },

    async scanTerminalFailures() {
      const rows = await deps.status.listUnhandledTerminalFailures(
        TERMINAL_FAILURE_LOOKBACK_MS,
        TERMINAL_FAILURE_BATCH_SIZE,
      );
      for (const row of rows) {
        // The anti-join returned it, so it has not been logged yet. Log
        // first: if the mark fails the row re-logs next cycle, never the
        // other way around.
        deps.log.error(
          {
            component: "dbos-sweep",
            workflowUuid: row.workflowUuid,
            name: row.name,
            status: row.status,
          },
          "DBOS workflow terminal failure",
        );
        try {
          await deps.ledger.markTerminalAlerted(row.workflowUuid, row.name);
        } catch (error) {
          deps.log.warn(
            { error, workflowUuid: row.workflowUuid },
            "failed to record DBOS terminal failure alert",
          );
        }
      }
      return { scanned: rows.length };
    },
  };
}
