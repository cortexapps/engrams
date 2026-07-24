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
import { makeSweepAlerter } from "./alerts.ts";
import {
  DEFAULT_SWEEP_CONFIG,
  Sweeper,
  VersionHeartbeat,
} from "./sweeper.ts";

export interface SweepRuntimeConfig {
  sweepDisabled: boolean;
  sweepIntervalMs: number;
  sweepGraceMs: number;
  sweepHeartbeatIntervalMs: number;
}

/**
 * Optional deterministic replacements for process/DBOS/Postgres state. The
 * production call omits this bundle; unit tests provide it as a complete
 * graph so the real alerter runs end to end.
 */
export interface SweepRuntimeOverrides {
  owner: string;
  podName: string;
  appVersion: () => string;
  cancelWorkflow: (workflowUuid: string) => Promise<void>;
  now: () => Date;
  heartbeats: HeartbeatStore;
  lease: SweepLeaseStore;
  ledger: SweepLedgerStore;
  status: DbosStatusStore;
}

export interface SweepRuntimeDeps {
  config: SweepRuntimeConfig;
  log?: Logger;
  runtime?: SweepRuntimeOverrides;
}

export function makeSweepRuntime(deps: SweepRuntimeDeps): {
  heartbeat: VersionHeartbeat;
  sweeper: Sweeper;
} {
  const log = deps.log ?? rootLog.child({ component: "dbos-sweep" });
  const now = deps.runtime?.now ?? (() => new Date());
  const heartbeats = deps.runtime?.heartbeats ?? makeHeartbeatStore();
  const lease = deps.runtime?.lease ?? makeSweepLeaseStore();
  const ledger = deps.runtime?.ledger ?? makeSweepLedgerStore();
  const status = deps.runtime?.status ?? makeDbosStatusStore();
  const appVersion =
    deps.runtime?.appVersion ?? (() => DBOS.applicationVersion);
  const cancelWorkflow =
    deps.runtime?.cancelWorkflow ??
    ((workflowUuid: string) => DBOS.cancelWorkflow(workflowUuid));

  // Alerts are error-level log lines (component=dbos-sweep); log-based
  // alerting is the operator's concern.
  const alerter = makeSweepAlerter({ ledger, status, log });
  const config = {
    ...DEFAULT_SWEEP_CONFIG,
    sweepIntervalMs: deps.config.sweepIntervalMs,
    graceMs: deps.config.sweepGraceMs,
    heartbeatIntervalMs: deps.config.sweepHeartbeatIntervalMs,
  };
  const sweeper = new Sweeper({
    owner:
      deps.runtime?.owner ??
      `${hostname()}:${process.pid}:${crypto.randomUUID()}`,
    appVersion,
    config,
    heartbeats,
    lease,
    ledger,
    status,
    cancelWorkflow,
    alerter,
    log,
    now,
  });
  const heartbeat = new VersionHeartbeat({
    appVersion,
    podName: deps.runtime?.podName ?? hostname(),
    heartbeats,
    intervalMs: config.heartbeatIntervalMs,
    log: log.child({ component: "dbos-sweep-heartbeat" }),
  });

  return { heartbeat, sweeper };
}
