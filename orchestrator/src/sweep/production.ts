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
import { makeOrgSettingStore, type OrgSettingStore } from "../db/org-settings.ts";
import { makeRetentionStore, type RetentionStore } from "../db/retention.ts";
import { log as rootLog } from "../log.ts";
import { dbosExecutorId } from "../workflows/dbos.ts";
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
  heartbeats: HeartbeatStore;
  lease: SweepLeaseStore;
  ledger: SweepLedgerStore;
  status: DbosStatusStore;
  /** The retention collector's seams; omitted = no pruning. */
  retention?: { settings: Pick<OrgSettingStore, "get">; store: RetentionStore };
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
  // Production prunes by the org's retention policy; a test runtime that
  // passes no retention seams gets no pruning.
  const retention =
    deps.runtime === undefined
      ? { settings: makeOrgSettingStore(), store: makeRetentionStore() }
      : deps.runtime.retention;
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
    ...(retention !== undefined ? { retention } : {}),
    log,
  });
  const heartbeat = new VersionHeartbeat({
    appVersion,
    // The heartbeat's pod name IS the DBOS executor id: the sweep decides a
    // workflow's owner is gone by the absence of this name's beat.
    podName: deps.runtime?.podName ?? dbosExecutorId(),
    heartbeats,
    intervalMs: config.heartbeatIntervalMs,
    log: log.child({ component: "dbos-sweep-heartbeat" }),
  });

  return { heartbeat, sweeper };
}
