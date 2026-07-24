import type { Logger } from "pino";

import type {
  DbosStatusStore,
  HeartbeatStore,
  SweepLeaseStore,
  SweepLedgerStore,
} from "../db/dbos-sweep.ts";
import type { FailureScanResult, SweepAlerter } from "./alerts.ts";
import { resolvePolicy, type ResolvedPolicy } from "./policy.ts";

export const HEARTBEAT_INTERVAL_MS = 30_000;
export const SWEEP_INTERVAL_MS = 60_000;
export const SWEEP_GRACE_MS = 600_000;
export const SWEEP_BATCH_CAP = 5;
export const MAX_SWEEPS = 3;
// An unregistered workflow name means no live binary carries its code, so it
// can never execute again on any current version. Alerting gives operators
// this window to suppress before the sweep cancels it to a terminal state.
export const ALERT_ONLY_STALE_AFTER_HOURS = 48;
// dbos_version_heartbeats accrues one row per (version, pod) across deploys
// and nothing else deletes them. Rows older than the grace window are already
// dead for liveness, so pruning at a comfortable multiple only bounds growth;
// a week keeps recent deploy history visible for operators.
export const HEARTBEAT_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;

export interface SweepConfig {
  heartbeatIntervalMs: number;
  sweepIntervalMs: number;
  graceMs: number;
  batchCap: number;
  maxSweeps: number;
}

export const DEFAULT_SWEEP_CONFIG = {
  heartbeatIntervalMs: HEARTBEAT_INTERVAL_MS,
  sweepIntervalMs: SWEEP_INTERVAL_MS,
  graceMs: SWEEP_GRACE_MS,
  batchCap: SWEEP_BATCH_CAP,
  maxSweeps: MAX_SWEEPS,
} as const satisfies SweepConfig;

export interface ScanCursor {
  createdAtEpochMs: number;
  workflowUuid: string;
}

export interface SweepTickDeps {
  owner: string;
  appVersion: () => string;
  config: SweepConfig;
  heartbeats: HeartbeatStore;
  lease: SweepLeaseStore;
  ledger: SweepLedgerStore;
  status: DbosStatusStore;
  cancelWorkflow: (workflowUuid: string) => Promise<void>;
  alerter?: SweepAlerter;
  resolvePolicy?: (name: string) => ResolvedPolicy;
  /**
   * Keyset position shared across ticks. Non-actionable rows (alert-only,
   * ignored, suppressed) stay in the scan set, so a scan that always restarted
   * at the oldest row would re-examine that prefix forever and starve newer
   * adoptable work once the prefix outgrows the scan budget. Persisting the
   * cursor makes consecutive ticks rotate through the whole backlog; it resets
   * once a pass reaches the end. The Sweeper class injects one automatically.
   */
  scanCursor?: { value: ScanCursor | undefined };
  log: Logger;
  now?: () => Date;
}

export type SweepDecision = {
  workflowUuid: string;
  name: string;
  action:
    | "adopted"
    | "enqueued_cleared"
    | "cancelled_stale"
    | "cancelled_capped"
    | "alert_only"
    | "suppressed"
    // The flip's fence no-opped: the owner version regained liveness or the
    // row changed underneath us. A healthy lost race, never alert-worthy.
    | "raced"
    | "error";
  reason?: string;
};

export interface SweepTickResult {
  leaseHeld: boolean;
  aborted?: "self-version-not-live";
  liveVersions: string[];
  scanned: number;
  decisions: SweepDecision[];
  alerted?: number;
  failureScan?: FailureScanResult;
}

const MUTATING_ACTIONS = new Set<SweepDecision["action"]>([
  "adopted",
  "enqueued_cleared",
  "cancelled_stale",
  "cancelled_capped",
]);

const ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "cancelled_stale",
  "cancelled_capped",
  "error",
]);

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function logCycle(log: Logger, result: SweepTickResult): void {
  const counts: Partial<Record<SweepDecision["action"], number>> = {};
  for (const decision of result.decisions) {
    counts[decision.action] = (counts[decision.action] ?? 0) + 1;
  }
  log.info(
    {
      component: "dbos-sweep",
      leaseHeld: result.leaseHeld,
      aborted: result.aborted,
      liveVersions: result.liveVersions,
      scanned: result.scanned,
      counts,
    },
    "DBOS orphan sweep cycle",
  );
}

export async function runSweepTick(
  deps: SweepTickDeps,
): Promise<SweepTickResult> {
  const result: SweepTickResult = {
    leaseHeld: false,
    liveVersions: [],
    scanned: 0,
    decisions: [],
  };
  const leaseHeld = await deps.lease.tryAcquire(
    deps.owner,
    2 * deps.config.sweepIntervalMs,
  );
  if (!leaseHeld) {
    logCycle(deps.log, result);
    return result;
  }

  result.leaseHeld = true;
  try {
    const liveVersions = await deps.heartbeats.liveVersions(
      deps.config.graceMs,
    );
    result.liveVersions = liveVersions;
    const selfVersion = deps.appVersion();
    if (!liveVersions.includes(selfVersion)) {
      result.aborted = "self-version-not-live";
      deps.log.warn(
        {
          component: "dbos-sweep",
          selfVersion,
          liveVersions,
        },
        "aborting DBOS orphan sweep because this version is not live",
      );
      return result;
    }

    // Contained: a failed prune must never abort the sweep it rides on. The
    // grace multiple keeps the floor safe even for absurdly long grace configs.
    try {
      const pruned = await deps.heartbeats.prune(
        Math.max(HEARTBEAT_RETENTION_MS, 2 * deps.config.graceMs),
      );
      if (pruned > 0) {
        deps.log.info(
          { component: "dbos-sweep", pruned },
          "pruned stale version-heartbeat rows",
        );
      }
    } catch (error) {
      deps.log.warn(
        { error },
        "failed to prune stale version-heartbeat rows",
      );
    }

    const pageSize = deps.config.batchCap * 4;
    const scanBudget = deps.config.batchCap * 40;
    let actions = 0;
    const cursor = deps.scanCursor;
    let after = cursor?.value;
    let exhausted = false;
    const nowMs = (deps.now ?? (() => new Date()))().getTime();

    scan: while (
      actions < deps.config.batchCap &&
      result.scanned < scanBudget
    ) {
      const rows = await deps.status.listNonTerminalOnVersionsNotIn(
        liveVersions,
        pageSize,
        after,
      );

      for (const row of rows) {
        if (
          actions >= deps.config.batchCap ||
          result.scanned >= scanBudget
        ) {
          break scan;
        }
        result.scanned++;
        after = {
          createdAtEpochMs: row.createdAtEpochMs,
          workflowUuid: row.workflowUuid,
        };
        let decision: SweepDecision;
        try {
          const policy = (deps.resolvePolicy ?? resolvePolicy)(row.name);
          const prior = await deps.ledger.get(row.workflowUuid);
          if (prior?.suppressed) {
            decision = {
              workflowUuid: row.workflowUuid,
              name: row.name,
              action: "suppressed",
            };
          } else if (
            // The cap dominates every repeated intent: once a workflow has
            // consumed maxSweeps recorded attempts (adoptions or failing
            // cancels), further ticks cancel WITHOUT recordSweep, so a
            // persistently-failing cancelWorkflow retries once per rotation
            // but can no longer inflate sweep_count without bound.
            prior !== null &&
            prior.sweepCount >= deps.config.maxSweeps
          ) {
            await deps.cancelWorkflow(row.workflowUuid);
            decision = {
              workflowUuid: row.workflowUuid,
              name: row.name,
              action: "cancelled_capped",
            };
          } else {
            const ageHours =
              (nowMs - row.createdAtEpochMs) / (60 * 60 * 1_000);
            const staleAfterHours =
              policy.mode === "alert-only"
                ? ALERT_ONLY_STALE_AFTER_HOURS
                : policy.staleAfterHours;
            if (ageHours > staleAfterHours) {
              await deps.ledger.recordSweep(row.workflowUuid, row.name);
              await deps.cancelWorkflow(row.workflowUuid);
              decision = {
                workflowUuid: row.workflowUuid,
                name: row.name,
                action: "cancelled_stale",
                ...(policy.mode === "alert-only"
                  ? {
                      reason:
                        "unregistered workflow name past the stale window",
                    }
                  : {}),
              };
            } else if (policy.mode === "alert-only") {
              decision = {
                workflowUuid: row.workflowUuid,
                name: row.name,
                action: "alert_only",
              };
            } else {
              if (row.status === "PENDING") {
                const adopted = await deps.status.adoptPendingRecording(
                  {
                    workflowUuid: row.workflowUuid,
                    expectedVersion: row.applicationVersion!,
                    workflowName: row.name,
                  },
                  deps.config.graceMs,
                );
                decision = adopted.flipped
                  ? {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "adopted",
                    }
                  : {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "raced",
                      reason: "owner became live or row changed",
                    };
              } else if (row.status === "ENQUEUED") {
                const cleared =
                  await deps.status.clearVersionOnEnqueuedRecording(
                    {
                      workflowUuid: row.workflowUuid,
                      expectedVersion: row.applicationVersion!,
                      workflowName: row.name,
                    },
                    deps.config.graceMs,
                  );
                decision = cleared.flipped
                  ? {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "enqueued_cleared",
                    }
                  : {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "raced",
                      reason: "owner became live or row changed",
                    };
              } else {
                decision = {
                  workflowUuid: row.workflowUuid,
                  name: row.name,
                  action: "error",
                  reason: `unsupported status ${row.status}`,
                };
              }
            }
          }
        } catch (error) {
          decision = {
            workflowUuid: row.workflowUuid,
            name: row.name,
            action: "error",
            reason: errorMessage(error),
          };
        }
        result.decisions.push(decision);
        if (MUTATING_ACTIONS.has(decision.action)) actions++;
      }

      // A short page means this pass examined the end of the backlog.
      if (rows.length < pageSize) {
        exhausted = true;
        break;
      }
    }

    if (
      !exhausted &&
      actions < deps.config.batchCap &&
      result.scanned >= scanBudget
    ) {
      deps.log.warn(
        {
          component: "dbos-sweep",
          scanned: result.scanned,
          scanBudget,
        },
        "DBOS orphan sweep scan budget exhausted; resuming from the cursor next cycle",
      );
    }
    if (cursor) cursor.value = exhausted ? undefined : after;

    if (deps.alerter) {
      try {
        await deps.alerter.alertDecisions(result.decisions);
        result.alerted = result.decisions.filter((decision) =>
          ALERT_ACTIONS.has(decision.action),
        ).length;
      } catch (error) {
        deps.log.warn(
          { error },
          "DBOS sweep decision alerting failed",
        );
      }

      try {
        result.failureScan =
          await deps.alerter.scanTerminalFailures();
      } catch (error) {
        deps.log.warn(
          { error },
          "DBOS terminal failure scan failed",
        );
      }
    }

    return result;
  } finally {
    await deps.lease.release(deps.owner);
    logCycle(deps.log, result);
  }
}

export interface SweeperDeps extends SweepTickDeps {
  setInterval?: (
    fn: () => void,
    ms: number,
  ) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export class Sweeper {
  readonly #deps: SweeperDeps;
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<SweepTickResult> | null = null;

  constructor(deps: SweeperDeps) {
    // Every Sweeper carries a scan cursor so consecutive ticks rotate
    // through the orphan backlog instead of re-scanning the oldest prefix.
    this.#deps = {
      ...deps,
      scanCursor: deps.scanCursor ?? { value: undefined },
    };
  }

  runOnce(): Promise<SweepTickResult> {
    this.#tick ??= runSweepTick(this.#deps).finally(() => {
      this.#tick = null;
    });
    return this.#tick;
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    // A tick can throw before its own try block (e.g. the lease acquire when
    // PG blips); the interval callback must observe that rejection or it
    // becomes an unhandled rejection and the loop silently degrades.
    this.#timer = schedule(
      () =>
        void this.runOnce().catch((error) =>
          this.#deps.log.warn({ error }, "DBOS orphan sweep tick failed"),
        ),
      this.#deps.config.sweepIntervalMs,
    );
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#tick?.catch(() => {});
  }
}

export interface VersionHeartbeatDeps {
  appVersion: () => string;
  podName: string;
  heartbeats: HeartbeatStore;
  intervalMs: number;
  log: Logger;
  setInterval?: (
    fn: () => void,
    ms: number,
  ) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export class VersionHeartbeat {
  readonly #deps: VersionHeartbeatDeps;
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<void> | null = null;

  constructor(deps: VersionHeartbeatDeps) {
    this.#deps = deps;
  }

  runOnce(): Promise<void> {
    this.#tick ??= this.#deps.heartbeats
      .beat(this.#deps.appVersion(), this.#deps.podName)
      .finally(() => {
        this.#tick = null;
      });
    return this.#tick;
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    // beat() throws when PG blips; observe the rejection so a transient
    // outage degrades to a missed beat instead of an unhandled rejection.
    this.#timer = schedule(
      () =>
        void this.runOnce().catch((error) =>
          this.#deps.log.warn({ error }, "version heartbeat failed"),
        ),
      this.#deps.intervalMs,
    );
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#tick?.catch(() => {});
  }
}
