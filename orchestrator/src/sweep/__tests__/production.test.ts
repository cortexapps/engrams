import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemoryHeartbeatStore,
  makeInMemorySweepLeaseStore,
  makeInMemorySweepLedgerStore,
  makeInMemorySweepLookupStore,
  type InMemoryDbosStatusSeed,
} from "../../db/dbos-sweep.ts";
import type { SlackPostClient } from "../policy.ts";
import { makeSweepRuntime } from "../production.ts";

const NOW = new Date("2026-07-24T12:00:00.000Z");
const CURRENT_VERSION = "version-current";

function recordingLog(): {
  log: Logger;
  records: Array<Record<string, unknown>>;
} {
  const records: Array<Record<string, unknown>> = [];
  const log = pino(
    { level: "warn" },
    {
      write(line: string) {
        records.push(JSON.parse(line) as Record<string, unknown>);
      },
    },
  );
  return { log, records };
}

function slackFake(): {
  client: SlackPostClient;
  posts: Array<{ channel: string; thread_ts?: string; text?: string }>;
} {
  const posts: Array<{
    channel: string;
    thread_ts?: string;
    text?: string;
  }> = [];
  return {
    posts,
    client: {
      chat: {
        async postMessage(args) {
          posts.push(args);
          return {};
        },
      },
    },
  };
}

function terminalRow(
  workflowUuid: string,
  name: string,
): InMemoryDbosStatusSeed {
  return {
    workflowUuid,
    name,
    status: "ERROR",
    applicationVersion: "version-dead",
    createdAtEpochMs: NOW.getTime() - 60_000,
    updatedAtEpochMs: NOW.getTime() - 1_000,
    recoveryAttempts: 1,
  };
}

function runtimeFixture(options: {
  alertChannel: string;
  row: ReturnType<typeof terminalRow>;
  threadSource?: unknown;
}) {
  const now = () => new Date(NOW);
  const heartbeats = makeInMemoryHeartbeatStore(now);
  const lease = makeInMemorySweepLeaseStore(now);
  const ledger = makeInMemorySweepLedgerStore(now);
  const status = makeInMemoryDbosStatusStore([options.row], now);
  const lookups = makeInMemorySweepLookupStore({
    threadSources:
      options.threadSource === undefined
        ? {}
        : { [options.row.workflowUuid]: options.threadSource },
  });
  const slack = slackFake();
  const { log, records } = recordingLog();
  const runtime = makeSweepRuntime({
    config: {
      sweepDisabled: false,
      sweepAlertChannel: options.alertChannel,
      sweepIntervalMs: 60_000,
      sweepGraceMs: 600_000,
      sweepHeartbeatIntervalMs: 30_000,
    },
    slack: async () => slack.client,
    failReview: async () => {},
    log,
    runtime: {
      owner: "test-owner",
      podName: "test-pod",
      appVersion: () => CURRENT_VERSION,
      cancelWorkflow: async () => {},
      now,
      heartbeats,
      lease,
      ledger,
      status,
      lookups,
    },
  });
  return { runtime, slack, records, ledger };
}

describe("makeSweepRuntime", () => {
  test("posts terminal-failure alerts to the configured Slack channel", async () => {
    const fixture = runtimeFixture({
      alertChannel: "C012OPS",
      row: terminalRow("wf-tool", "ToolExecWorkflow"),
    });

    await fixture.runtime.heartbeat.runOnce();
    const result = await fixture.runtime.sweeper.runOnce();

    expect(result.failureScan).toMatchObject({
      scanned: 1,
      alerted: 1,
      cleanupsRun: 0,
    });
    expect(fixture.slack.posts).toEqual([
      {
        channel: "C012OPS",
        text: "DBOS workflow terminal failure: ToolExecWorkflow wf-tool — ERROR",
      },
    ]);
  });

  test("logs generic alerts but still runs the Slack thread cleanup without an ops channel", async () => {
    const fixture = runtimeFixture({
      alertChannel: "",
      row: terminalRow("wf-thread", "SlackThreadWorkflow"),
      threadSource: {
        team: "T012",
        channel: "C034THREAD",
        threadRoot: "1720000000.000100",
      },
    });

    await fixture.runtime.heartbeat.runOnce();
    const result = await fixture.runtime.sweeper.runOnce();

    expect(result.failureScan).toMatchObject({
      scanned: 1,
      alerted: 1,
      cleanupsRun: 1,
      cleanupsFailed: 0,
    });
    expect(fixture.records).toContainEqual(
      expect.objectContaining({
        alert:
          "DBOS workflow terminal failure: SlackThreadWorkflow wf-thread — ERROR",
        msg: "DBOS sweep alert (no ops channel configured)",
      }),
    );
    expect(fixture.slack.posts).toEqual([
      {
        channel: "C034THREAD",
        thread_ts: "1720000000.000100",
        text:
          "This conversation hit a snag after a redeploy. Please start a fresh thread—new messages here will no longer reach the agent.",
      },
    ]);
    expect((await fixture.ledger.get("wf-thread"))?.cleanupFn).toBe(
      "notifyThread",
    );
  });
});
