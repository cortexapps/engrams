import { DBOS } from "@dbos-inc/dbos-sdk";
import { hostname } from "node:os";
import type { Logger } from "pino";

import {
  makeDbosStatusStore,
  makeHeartbeatStore,
  makeSweepLeaseStore,
  makeSweepLedgerStore,
  makeSweepLookupStore,
  type DbosStatusStore,
  type HeartbeatStore,
  type SweepLeaseStore,
  type SweepLedgerStore,
  type SweepLookupStore,
} from "../db/dbos-sweep.ts";
import { log as rootLog } from "../log.ts";
import { makeSweepAlerter } from "./alerts.ts";
import { resolvePolicy, type SlackPostClient } from "./policy.ts";
import {
  DEFAULT_SWEEP_CONFIG,
  MAX_SWEEPS,
  Sweeper,
  VersionHeartbeat,
} from "./sweeper.ts";

export interface SweepRuntimeConfig {
  sweepDisabled: boolean;
  sweepAlertChannel: string;
  sweepIntervalMs: number;
  sweepGraceMs: number;
  sweepHeartbeatIntervalMs: number;
}

/**
 * Optional deterministic replacements for process/DBOS/Postgres state. The
 * production call omits this bundle; unit tests provide it as a complete
 * graph so the real alerter and cleanup callbacks can run end to end.
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
  lookups: SweepLookupStore;
}

export interface SweepRuntimeDeps {
  config: SweepRuntimeConfig;
  slack: () => Promise<SlackPostClient>;
  failReview: (
    reviewId: string,
    opts: { reason?: string },
  ) => Promise<void>;
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
  const lookups = deps.runtime?.lookups ?? makeSweepLookupStore();
  const appVersion =
    deps.runtime?.appVersion ?? (() => DBOS.applicationVersion);
  const cancelWorkflow =
    deps.runtime?.cancelWorkflow ??
    ((workflowUuid: string) => DBOS.cancelWorkflow(workflowUuid));

  const cleanupCtx = {
    log,
    lookups,
    slack: deps.slack,
    failReview: deps.failReview,
  };
  const post = deps.config.sweepAlertChannel
    ? async (text: string): Promise<void> => {
        const client = await deps.slack();
        await client.chat.postMessage({
          channel: deps.config.sweepAlertChannel,
          text,
        });
      }
    : async (text: string): Promise<void> => {
        log.warn(
          { alert: text },
          "DBOS sweep alert (no ops channel configured)",
        );
      };
  const alerter = makeSweepAlerter({
    ledger,
    status,
    post,
    policies: resolvePolicy,
    cleanupCtx,
    log,
    maxCleanupAttempts: MAX_SWEEPS,
  });
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
