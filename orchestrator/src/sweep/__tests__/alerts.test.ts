import { describe, expect, test } from "bun:test";
import type { Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemorySweepLedgerStore,
  type FailedDbosWorkflowRow,
} from "../../db/dbos-sweep.ts";
import { makeSweepAlerter, type SweepAlerterDeps } from "../alerts.ts";
import type { SweepDecision } from "../sweeper.ts";

const NOW = new Date("2026-07-23T12:00:00.000Z");
const ALERT_MESSAGES = new Set([
  "DBOS orphan sweep alert",
  "DBOS workflow terminal failure",
]);

function failedRow(
  workflowUuid: string,
  overrides: Partial<FailedDbosWorkflowRow> = {},
): FailedDbosWorkflowRow {
  return {
    workflowUuid,
    name: "ToolExecWorkflow",
    status: "ERROR",
    applicationVersion: "dead-version",
    createdAtEpochMs: NOW.getTime() - 120_000,
    updatedAtEpochMs: NOW.getTime() - 60_000,
    recoveryAttempts: 3,
    ...overrides,
  };
}

/** Ledger + status wired the way production is: the status store's unhandled
 * scan anti-joins the ledger's terminal-alert mark, and alerts land as
 * error-level log lines (captured into `alerts`). */
function scanFixture(rows: FailedDbosWorkflowRow[]) {
  const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
  const status = makeInMemoryDbosStatusStore(rows, () => new Date(NOW), {
    isTerminalFailureHandled: async (workflowUuid) =>
      Boolean((await ledger.get(workflowUuid))?.terminalAlertedAt),
  });
  const alerts: string[] = [];
  const warns: string[] = [];
  const log = {
    info() {},
    warn(_fields: Record<string, unknown>, message: string) {
      warns.push(message);
    },
    error(fields: Record<string, unknown>, message: string) {
      if (ALERT_MESSAGES.has(message)) {
        alerts.push(`${message} ${JSON.stringify(fields)}`);
      }
    },
  } as unknown as Logger;
  const deps: SweepAlerterDeps = { ledger, status, log };
  return { ledger, status, alerts, warns, deps, alerter: makeSweepAlerter(deps) };
}

describe("SweepAlerter.alertDecisions", () => {
  test("deduplicates alert-worthy decisions through alertedAt", async () => {
    const f = scanFixture([]);
    await f.ledger.recordSweep("wf-alert", "ToolExecWorkflow");
    const decisions: SweepDecision[] = [
      {
        workflowUuid: "wf-alert",
        name: "ToolExecWorkflow",
        action: "error",
        reason: "compare-and-set lost",
      },
      {
        workflowUuid: "wf-ignored",
        name: "ToolExecWorkflow",
        action: "adopted",
      },
    ];

    await f.alerter.alertDecisions(decisions);
    await f.alerter.alertDecisions(decisions);

    expect(f.alerts).toHaveLength(1);
    expect(f.alerts[0]).toContain("ToolExecWorkflow");
    expect(f.alerts[0]).toContain("wf-alert");
    expect(f.alerts[0]).toContain("error");
    expect(f.alerts[0]).toContain("compare-and-set lost");
    expect((await f.ledger.get("wf-alert"))?.alertedAt).not.toBeNull();
  });

  test("dedup sticks for a workflow with no prior ledger row", async () => {
    // alert_only workflows are never swept, so no recordSweep row exists;
    // markAlerted must upsert or the alert line repeats every cycle.
    const f = scanFixture([]);
    const decisions: SweepDecision[] = [
      {
        workflowUuid: "wf-never-swept",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ];

    await f.alerter.alertDecisions(decisions);
    await f.alerter.alertDecisions(decisions);

    expect(f.alerts).toHaveLength(1);
    expect((await f.ledger.get("wf-never-swept"))?.alertedAt).not.toBeNull();
  });

  test("a cancel alert logs even after an earlier alert_only alert", async () => {
    // alertedAt dedup applies only to the recurring actions (alert_only,
    // error). A successful cancel is once-per-workflow by construction — the
    // row turns CANCELLED and leaves the scan set — so it always logs.
    const f = scanFixture([]);

    await f.alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ]);
    await f.alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ]);
    await f.alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "cancelled_stale",
        reason: "unregistered workflow name past the stale window",
      },
    ]);

    expect(f.alerts).toHaveLength(2);
    expect(f.alerts[0]).toContain("alert_only");
    expect(f.alerts[1]).toContain("cancelled_stale");
  });

  test("contains a ledger failure and continues to the next decision", async () => {
    const f = scanFixture([]);
    const failingLedger = {
      ...f.ledger,
      async markAlerted(workflowUuid: string, workflowName: string) {
        if (workflowUuid === "wf-bad") throw new Error("ledger unavailable");
        return f.ledger.markAlerted(workflowUuid, workflowName);
      },
    };
    const alerter = makeSweepAlerter({ ...f.deps, ledger: failingLedger });

    await expect(
      alerter.alertDecisions([
        {
          workflowUuid: "wf-bad",
          name: "ToolExecWorkflow",
          action: "alert_only",
        },
        {
          workflowUuid: "wf-next",
          name: "ToolExecWorkflow",
          action: "cancelled_stale",
        },
      ]),
    ).resolves.toBeUndefined();

    // Both logged; wf-bad's dedup mark did not stick so it re-logs next
    // cycle, which is the correct degradation for a transient ledger error.
    expect(f.alerts).toHaveLength(2);
    expect((await f.ledger.get("wf-bad"))?.alertedAt ?? null).toBeNull();
    expect((await f.ledger.get("wf-next"))?.alertedAt).not.toBeNull();
  });
});

describe("SweepAlerter.scanTerminalFailures", () => {
  test("an empty scan reports zero", async () => {
    const f = scanFixture([]);

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 0 });
  });

  test("logged failures are marked and leave the unhandled set", async () => {
    const first = failedRow("wf-first", {
      updatedAtEpochMs: NOW.getTime() - 62_000,
    });
    const second = failedRow("wf-second", {
      updatedAtEpochMs: NOW.getTime() - 61_000,
    });
    const f = scanFixture([first, second]);

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 2 });
    expect(f.alerts).toHaveLength(2);
    expect((await f.ledger.get("wf-first"))?.terminalAlertedAt).not.toBeNull();

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 0 });
    expect(f.alerts).toHaveLength(2);
  });

  test("a failure that becomes visible after a scan is picked up next cycle", async () => {
    // The SDK stamps updated_at in JS before the write commits, so a row can
    // appear late — even tied to an already-processed timestamp. With no
    // watermark to advance there is nothing to land behind: the row is
    // simply still unhandled next cycle.
    const processed = failedRow("wf-early", {
      updatedAtEpochMs: NOW.getTime() - 1_000,
    });
    const lateCommit = failedRow("wf-late-commit", {
      updatedAtEpochMs: NOW.getTime() - 1_000,
    });
    let lateCommitVisible = false;
    const f = scanFixture([processed, lateCommit]);
    const underlying = f.deps.status;
    const alerter = makeSweepAlerter({
      ...f.deps,
      status: {
        ...underlying,
        async listUnhandledTerminalFailures(lookbackMs, limit) {
          const rows = await underlying.listUnhandledTerminalFailures(
            lookbackMs,
            limit,
          );
          return lateCommitVisible
            ? rows
            : rows.filter(
                (row) => row.workflowUuid !== lateCommit.workflowUuid,
              );
        },
      },
    });

    expect(await alerter.scanTerminalFailures()).toEqual({ scanned: 1 });

    lateCommitVisible = true;
    expect(await alerter.scanTerminalFailures()).toEqual({ scanned: 1 });
    expect(f.alerts.some((line) => line.includes("wf-late-commit"))).toBe(
      true,
    );
    // The already-handled tie is excluded by the anti-join, not re-logged.
    expect(
      f.alerts.filter((line) => line.includes("wf-early")),
    ).toHaveLength(1);
  });

  test("a sweep-decision alert does not suppress the terminal-failure alert", async () => {
    // The two alert streams dedup on distinct markers: an adoption-race
    // error decision stamps alertedAt, but if that workflow later fails
    // terminally on its new owner, the terminal-failure alarm must still fire.
    const row = failedRow("wf-raced-then-failed");
    const f = scanFixture([row]);
    await f.ledger.markAlerted(row.workflowUuid, row.name);

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 1 });
    expect(f.alerts).toHaveLength(1);
    expect(f.alerts[0]).toContain("terminal failure");
    expect(
      (await f.ledger.get(row.workflowUuid))?.terminalAlertedAt,
    ).not.toBeNull();

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 0 });
    expect(f.alerts).toHaveLength(1);
  });

  test("a batch cutting through tied timestamps still reaches every row", async () => {
    // 51 rows tied at one timestamp with a batch limit of 50: handled rows
    // drop out of the anti-join, so the second scan returns the remainder.
    const tiedAt = NOW.getTime() - 60_000;
    const rows = Array.from({ length: 51 }, (_, index) =>
      failedRow(`wf-tied-batch-${index.toString().padStart(2, "0")}`, {
        updatedAtEpochMs: tiedAt,
      }),
    );
    const f = scanFixture(rows);

    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 50 });
    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 1 });
    expect(await f.alerter.scanTerminalFailures()).toEqual({ scanned: 0 });
    expect(f.alerts).toHaveLength(51);
    expect(new Set(f.alerts).size).toBe(51);
  });

  test("a failed terminal-alert mark warns and re-logs next cycle", async () => {
    const row = failedRow("wf-mark-blip");
    const f = scanFixture([row]);
    let markAttempts = 0;
    const alerter = makeSweepAlerter({
      ...f.deps,
      ledger: {
        ...f.ledger,
        async markTerminalAlerted(workflowUuid: string, name: string) {
          markAttempts++;
          if (markAttempts === 1) throw new Error("ledger blip");
          return f.ledger.markTerminalAlerted(workflowUuid, name);
        },
      },
    });

    expect(await alerter.scanTerminalFailures()).toEqual({ scanned: 1 });
    expect(f.warns).toContain("failed to record DBOS terminal failure alert");

    // The mark did not stick, so the row re-logs; the second mark succeeds.
    expect(await alerter.scanTerminalFailures()).toEqual({ scanned: 1 });
    expect(f.alerts).toHaveLength(2);

    expect(await alerter.scanTerminalFailures()).toEqual({ scanned: 0 });
  });
});
