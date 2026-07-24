import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemoryHeartbeatStore,
  makeInMemorySweepLeaseStore,
  makeInMemorySweepLedgerStore,
  type InMemoryDbosStatusSeed,
} from "../../db/dbos-sweep.ts";
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

describe("makeSweepRuntime", () => {
  test("logs terminal failures at error level and marks them handled", async () => {
    const now = () => new Date(NOW);
    const heartbeats = makeInMemoryHeartbeatStore(now);
    const lease = makeInMemorySweepLeaseStore(now);
    const ledger = makeInMemorySweepLedgerStore(now);
    const status = makeInMemoryDbosStatusStore(
      [terminalRow("wf-tool", "ToolExecWorkflow")],
      now,
      {
        isTerminalFailureHandled: async (workflowUuid) =>
          Boolean((await ledger.get(workflowUuid))?.terminalAlertedAt),
      },
    );
    const { log, records } = recordingLog();
    const runtime = makeSweepRuntime({
      config: {
        sweepDisabled: false,
        sweepIntervalMs: 60_000,
        sweepGraceMs: 600_000,
        sweepHeartbeatIntervalMs: 30_000,
      },
      log,
      runtime: {
        owner: "test-owner",
        podName: "test-pod",
        appVersion: () => CURRENT_VERSION,
        cancelWorkflow: async () => {},
        heartbeats,
        lease,
        ledger,
        status,
      },
    });

    await runtime.heartbeat.runOnce();
    const result = await runtime.sweeper.runOnce();

    expect(result.failureScan).toEqual({ scanned: 1 });
    expect(records).toContainEqual(
      expect.objectContaining({
        level: 50,
        msg: "DBOS workflow terminal failure",
        workflowUuid: "wf-tool",
        name: "ToolExecWorkflow",
      }),
    );
    expect((await ledger.get("wf-tool"))?.terminalAlertedAt).not.toBeNull();

    // Marked handled: the next cycle's scan is empty.
    const second = await runtime.sweeper.runOnce();
    expect(second.failureScan).toEqual({ scanned: 0 });
  });
});
