import { DBOS } from "@dbos-inc/dbos-sdk";
import { hostname } from "node:os";
import type { Logger } from "pino";

import {
  makeDbosStatusStore,
  makeHeartbeatStore,
  makeSweepLeaseStore,
  makeSweepLedgerStore,
  type DbosStatusStore,
  type HeartbeatStore,
  type SweepLeaseStore,
  type SweepLedgerStore,
} from "../db/dbos-sweep.ts";
import { log as rootLog } from "../log.ts";
import {
  makeDisabledSweepAlerter,
  type FailureScanResult,
  type SweepAlerter,
} from "./alerts.ts";
import { resolvePolicy, type ResolvedPolicy } from "./policy.ts";

export const HEARTBEAT_INTERVAL_MS = 30_000;
export const SWEEP_INTERVAL_MS = 60_000;
export const SWEEP_GRACE_MS = 600_000;
export const SWEEP_BATCH_CAP = 5;
export const MAX_SWEEPS = 3;

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
    | "cancelled_policy"
    | "alert_only"
    | "suppressed"
    | "ignored"
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
  "cancelled_policy",
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

    const rows = await deps.status.listNonTerminalOnVersionsNotIn(
      liveVersions,
      deps.config.batchCap * 4,
    );
    result.scanned = rows.length;
    let actions = 0;
    const nowMs = (deps.now ?? (() => new Date()))().getTime();

    for (const row of rows) {
      if (actions >= deps.config.batchCap) break;
      let decision: SweepDecision;
      try {
        const policy = (deps.resolvePolicy ?? resolvePolicy)(row.name);
        if (policy.mode === "alert-only") {
          decision = {
            workflowUuid: row.workflowUuid,
            name: row.name,
            action: "alert_only",
          };
        } else if (policy.mode === "ignore") {
          decision = {
            workflowUuid: row.workflowUuid,
            name: row.name,
            action: "ignored",
          };
        } else {
          const prior = await deps.ledger.get(row.workflowUuid);
          if (prior?.suppressed) {
            decision = {
              workflowUuid: row.workflowUuid,
              name: row.name,
              action: "suppressed",
            };
          } else {
            const ageHours =
              (nowMs - row.createdAtEpochMs) / (60 * 60 * 1_000);
            if (
              policy.mode === "adopt" &&
              ageHours > policy.staleAfterHours
            ) {
              await deps.cancelWorkflow(row.workflowUuid);
              await deps.ledger.recordSweep(row.workflowUuid, row.name);
              decision = {
                workflowUuid: row.workflowUuid,
                name: row.name,
                action: "cancelled_stale",
              };
            } else if (
              prior !== null &&
              prior.sweepCount >= deps.config.maxSweeps
            ) {
              await deps.cancelWorkflow(row.workflowUuid);
              decision = {
                workflowUuid: row.workflowUuid,
                name: row.name,
                action: "cancelled_capped",
              };
            } else if (policy.mode === "cancel") {
              await deps.cancelWorkflow(row.workflowUuid);
              await deps.ledger.recordSweep(row.workflowUuid, row.name);
              decision = {
                workflowUuid: row.workflowUuid,
                name: row.name,
                action: "cancelled_policy",
              };
            } else {
              await deps.ledger.recordSweep(row.workflowUuid, row.name);
              if (row.status === "PENDING") {
                const adopted = await deps.status.adoptPending([
                  row.workflowUuid,
                ]);
                decision = adopted.includes(row.workflowUuid)
                  ? {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "adopted",
                    }
                  : {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "error",
                      reason: "no longer PENDING",
                    };
              } else if (row.status === "ENQUEUED") {
                const cleared = await deps.status.clearVersionOnEnqueued([
                  row.workflowUuid,
                ]);
                decision = cleared.includes(row.workflowUuid)
                  ? {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "enqueued_cleared",
                    }
                  : {
                      workflowUuid: row.workflowUuid,
                      name: row.name,
                      action: "error",
                      reason: "no longer ENQUEUED",
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

    if (deps.alerter) {
      try {
        await deps.alerter.alertDecisions(result.decisions);
        result.alerted = result.decisions.filter((decision) =>
          ALERT_ACTIONS.has(decision.action),
        ).length;
      } catch (error) {
        deps.log.error(
          { error },
          "DBOS sweep decision alerting failed",
        );
      }

      try {
        result.failureScan =
          await deps.alerter.scanTerminalFailures();
      } catch (error) {
        deps.log.error(
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
    this.#deps = deps;
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
          this.#deps.log.error({ error }, "DBOS orphan sweep tick failed"),
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
          this.#deps.log.error({ error }, "version heartbeat failed"),
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

function sweepConfig(overrides: Partial<SweepConfig>): SweepConfig {
  return { ...DEFAULT_SWEEP_CONFIG, ...overrides };
}

export function makeProductionSweeper(
  cfg: Partial<SweepConfig> = {},
  options: { alerter?: SweepAlerter } = {},
): Sweeper {
  const config = sweepConfig(cfg);
  const owner = `${hostname()}:${process.pid}:${crypto.randomUUID()}`;
  const log = rootLog.child({ component: "dbos-sweep" });
  return new Sweeper({
    owner,
    // Public getter; only populated after DBOS.launch(), hence the lazy read.
    appVersion: () => DBOS.applicationVersion,
    config,
    heartbeats: makeHeartbeatStore(),
    lease: makeSweepLeaseStore(),
    ledger: makeSweepLedgerStore(),
    status: makeDbosStatusStore(),
    cancelWorkflow: (workflowUuid) => DBOS.cancelWorkflow(workflowUuid),
    alerter: options.alerter ?? makeDisabledSweepAlerter(log),
    log,
    now: () => new Date(),
  });
}

export function makeProductionVersionHeartbeat(
  cfg: Partial<SweepConfig> = {},
): VersionHeartbeat {
  const config = sweepConfig(cfg);
  const podName = hostname();
  return new VersionHeartbeat({
    appVersion: () => DBOS.applicationVersion,
    podName,
    heartbeats: makeHeartbeatStore(),
    intervalMs: config.heartbeatIntervalMs,
    log: rootLog.child({ component: "dbos-sweep-heartbeat" }),
  });
}
