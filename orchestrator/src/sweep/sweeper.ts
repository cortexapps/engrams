import type { Logger } from "pino";

import type {
  DbosStatusStore,
  HeartbeatStore,
  SweepLeaseStore,
  SweepLedgerStore,
} from "../db/dbos-sweep.ts";
import type { FailureScanResult, SweepAlerter } from "./alerts.ts";
import { resolvePolicy, type ResolvedPolicy } from "./policy.ts";
import { runRetentionTick, type RetentionDeps, type RetentionResult } from "./retention.ts";

export const HEARTBEAT_INTERVAL_MS = 30_000;
export const SWEEP_INTERVAL_MS = 60_000;
export const SWEEP_GRACE_MS = 600_000;
/** How long a pod may go without a beat before its PENDING workflows are
 * re-enqueued for a live pod: six beats. Shorter than the version grace —
 * a pod that is gone is gone, and a thread mid-turn waits this long after
 * a roll. Double execution is bounded as for a wedged pod (ADR 0104). */
export const POD_GRACE_MS = 180_000;
export const SWEEP_BATCH_CAP = 5;
// An unregistered workflow name means no live binary carries its code, so it
// can never execute again on any current version. Alerting gives operators
// this window to suppress before the sweep cancels it to a terminal state.
export const ALERT_ONLY_STALE_AFTER_HOURS = 48;
// dbos_version_heartbeats accrues one row per (version, pod) across deploys
// and nothing else deletes them. Rows older than the grace window are already
// dead for liveness, so pruning at a comfortable multiple only bounds growth.
// INVARIANT: retention must exceed every policy's staleAfterHours — the
// heartbeat history is also the staleness clock (a version with no surviving
// rows reads as abandoned forever), so pruning too aggressively would make a
// freshly-stranded workflow look ancient.
export const HEARTBEAT_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;

export interface SweepConfig {
  heartbeatIntervalMs: number;
  sweepIntervalMs: number;
  graceMs: number;
  podGraceMs: number;
  batchCap: number;
}

export const DEFAULT_SWEEP_CONFIG = {
  heartbeatIntervalMs: HEARTBEAT_INTERVAL_MS,
  sweepIntervalMs: SWEEP_INTERVAL_MS,
  graceMs: SWEEP_GRACE_MS,
  podGraceMs: POD_GRACE_MS,
  batchCap: SWEEP_BATCH_CAP,
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
  /** The retention collector's seams; absent = no pruning (tests, dev
   * without a policy). Runs under the same lease, after the scans. */
  retention?: Omit<RetentionDeps, "log" | "now"> & { now?: () => Date };
  log: Logger;
}

export type SweepDecision = {
  workflowUuid: string;
  name: string;
  action:
    | "adopted"
    // Re-enqueued: a live-version workflow whose executor pod is gone.
    | "requeued"
    | "enqueued_cleared"
    | "cancelled_stale"
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
  retention?: RetentionResult;
}

const MUTATING_ACTIONS = new Set<SweepDecision["action"]>([
  "adopted",
  "requeued",
  "enqueued_cleared",
  "cancelled_stale",
]);

const ALERT_ACTIONS = new Set<SweepDecision["action"]>([
  "alert_only",
  "cancelled_stale",
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

    // Staleness is measured from ABANDONMENT — when the workflow's owning
    // version last heartbeat — never from creation: a thread workflow lives
    // for its whole session, so a days-old created_at says nothing about
    // whether the work was active moments before a deploy stranded it. A
    // version with no surviving heartbeat history (pre-feature orphans, or
    // rows past HEARTBEAT_RETENTION_MS) is abandoned forever. The age is
    // computed inside Postgres — the same clock that stamps heartbeats and
    // drives liveVersions and the flip fences — so pod↔PG skew can't shift
    // a staleness decision.
    // Pods, not versions. DBOS recovers PENDING work per executor id at
    // launch, and a pod's id is its name (dbosExecutorId): a pod that is
    // gone never recovers what it was running, even though the version is
    // live. Re-enqueue those on DBOS's internal queue — exactly one live pod
    // pulls each and replays it from its recorded steps. The fence (no beat
    // from that pod inside the pod grace AND the row untouched inside it)
    // also leaves a just-started pod's fresh rows alone before its first
    // beat. The legacy id "local" (the SDK default every pod once shared) is
    // simply a pod that never beats.
    let actions = 0;
    try {
      const stranded = await deps.status.listPendingOnDeadExecutors(
        liveVersions,
        deps.config.podGraceMs,
        deps.config.batchCap,
      );
      for (const row of stranded) {
        result.scanned++;
        let decision: SweepDecision;
        try {
          const policy = (deps.resolvePolicy ?? resolvePolicy)(row.name);
          const prior = await deps.ledger.get(row.workflowUuid);
          if (prior?.suppressed) {
            decision = { workflowUuid: row.workflowUuid, name: row.name, action: "suppressed" };
          } else if (policy.mode === "alert-only") {
            decision = {
              workflowUuid: row.workflowUuid,
              name: row.name,
              action: "alert_only",
              reason: `stranded on executor ${row.executorId}`,
            };
          } else {
            const requeued = await deps.status.requeueStrandedPendingRecording(
              { workflowUuid: row.workflowUuid, executorId: row.executorId, workflowName: row.name },
              deps.config.podGraceMs,
            );
            decision = requeued.flipped
              ? {
                  workflowUuid: row.workflowUuid,
                  name: row.name,
                  action: "requeued",
                  reason: `executor ${row.executorId} is gone`,
                }
              : {
                  workflowUuid: row.workflowUuid,
                  name: row.name,
                  action: "raced",
                  reason: "executor beat again or row changed",
                };
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
    } catch (error) {
      deps.log.warn(
        { error },
        "DBOS stranded-executor scan failed; version scan continues",
      );
    }

    const abandonedMsByVersion = await deps.heartbeats.abandonedMsByVersion();

    const pageSize = deps.config.batchCap * 4;
    const scanBudget = deps.config.batchCap * 40;
    const cursor = deps.scanCursor;
    let after = cursor?.value;
    let exhausted = false;

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
          } else {
            // No automatic sweep-count cap: re-stranding only happens via
            // version death (once per deploy), so a healthy long-lived
            // workflow is adopted once per deploy indefinitely — that's
            // normal, not pathology. A replay that fails loud goes terminal
            // ERROR and is never re-swept; the rare silent zombie is an
            // operator call via the ledger's `suppressed` flag, with
            // sweep_count kept as the evidence trail.
            const abandonedMs = abandonedMsByVersion.get(
              row.applicationVersion!,
            );
            const abandonedHours =
              abandonedMs === undefined
                ? Number.POSITIVE_INFINITY
                : abandonedMs / (60 * 60 * 1_000);
            const staleAfterHours =
              policy.mode === "alert-only"
                ? ALERT_ONLY_STALE_AFTER_HOURS
                : policy.staleAfterHours;
            if (abandonedHours > staleAfterHours) {
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

    // Contained like the alerts: a failed prune never fails the sweep.
    if (deps.retention) {
      try {
        result.retention = await runRetentionTick({
          ...deps.retention,
          now: deps.retention.now ?? (() => new Date()),
          log: deps.log,
        });
      } catch (error) {
        deps.log.warn({ error }, "retention collector failed; next cycle retries");
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
