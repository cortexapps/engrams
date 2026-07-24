import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemorySweepLedgerStore,
  makeInMemorySweepLookupStore,
  type FailedDbosWorkflowRow,
} from "../../db/dbos-sweep.ts";
import {
  makeSweepAlerter,
  TERMINAL_FAILURE_VISIBILITY_LAG_MS,
} from "../alerts.ts";
import type {
  FailedWorkflow,
  ResolvedPolicy,
  SweepContext,
} from "../policy.ts";
import type { SweepDecision } from "../sweeper.ts";

const NOW = new Date("2026-07-23T12:00:00.000Z");
const DAY_MS = 24 * 60 * 60 * 1_000;
const log: Logger = pino({ level: "silent" });

// Rows default to timestamps older than TERMINAL_FAILURE_VISIBILITY_LAG_MS so
// the wall-clock clamp stays out of the way; the clamp has its own test.
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

function cleanupContext(): SweepContext {
  return {
    log,
    lookups: makeInMemorySweepLookupStore(),
    slack: async () => ({
      chat: {
        async postMessage() {},
      },
    }),
    failReview: async () => {},
  };
}

describe("SweepAlerter.alertDecisions", () => {
  test("deduplicates alert-worthy decisions through alertedAt", async () => {
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep("wf-alert", "ToolExecWorkflow");
    const posts: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore(),
      post: async (text) => {
        posts.push(text);
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });
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

    await alerter.alertDecisions(decisions);
    await alerter.alertDecisions(decisions);

    expect(posts).toHaveLength(1);
    expect(posts[0]).toContain("ToolExecWorkflow");
    expect(posts[0]).toContain("wf-alert");
    expect(posts[0]).toContain("error");
    expect(posts[0]).toContain("compare-and-set lost");
    expect((await ledger.get("wf-alert"))?.alertedAt).not.toBeNull();
  });

  test("dedup sticks for a workflow with no prior ledger row", async () => {
    // alert_only workflows are never swept, so no recordSweep row exists;
    // markAlerted must upsert or the ops channel gets the alert every cycle.
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const posts: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore(),
      post: async (text) => {
        posts.push(text);
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });
    const decisions: SweepDecision[] = [
      {
        workflowUuid: "wf-never-swept",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ];

    await alerter.alertDecisions(decisions);
    await alerter.alertDecisions(decisions);

    expect(posts).toHaveLength(1);
    expect((await ledger.get("wf-never-swept"))?.alertedAt).not.toBeNull();
  });

  test("a cancel alert posts even after an earlier alert_only alert", async () => {
    // The cancellation is the transition operators must see; alertedAt dedup
    // applies only to the recurring actions (alert_only, error). A successful
    // cancel is once-per-workflow by construction — the row turns CANCELLED
    // and leaves the scan set — so this cannot re-alert every cycle.
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const posts: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore(),
      post: async (text) => {
        posts.push(text);
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    await alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ]);
    await alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ]);
    await alerter.alertDecisions([
      {
        workflowUuid: "wf-orphan",
        name: "DeletedWorkflowName",
        action: "cancelled_stale",
        reason: "unregistered workflow name past the stale window",
      },
    ]);

    expect(posts).toHaveLength(2);
    expect(posts[0]).toContain("alert_only");
    expect(posts[1]).toContain("cancelled_stale");
  });

  test("contains a posting failure and continues to the next decision", async () => {
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep("wf-bad", "ToolExecWorkflow");
    await ledger.recordSweep("wf-next", "ToolExecWorkflow");
    const posted: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore(),
      post: async (text) => {
        if (text.includes("wf-bad")) throw new Error("ops unavailable");
        posted.push(text);
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

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

    expect(posted).toHaveLength(1);
    expect(posted[0]).toContain("wf-next");
    expect((await ledger.get("wf-bad"))?.alertedAt).toBeNull();
    expect((await ledger.get("wf-next"))?.alertedAt).not.toBeNull();
  });
});

describe("SweepAlerter.scanTerminalFailures", () => {
  test("starts an empty scan at now minus 24 hours", async () => {
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    let observedSince = Number.NaN;
    const status = makeInMemoryDbosStatusStore();
    const alerter = makeSweepAlerter({
      ledger,
      status: {
        ...status,
        async listNewlyTerminalFailed(since, limit) {
          observedSince = since;
          return status.listNewlyTerminalFailed(since, limit);
        },
      },
      post: async () => {},
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    const result = await alerter.scanTerminalFailures();

    expect(observedSince).toBe(NOW.getTime() - DAY_MS);
    expect(result).toEqual({
      scanned: 0,
      alerted: 0,
      cleanupsRun: 0,
      cleanupsFailed: 0,
      watermark: NOW.getTime() - DAY_MS,
    });
  });

  test("advances the watermark past fully processed rows", async () => {
    const first = failedRow("wf-first", {
      updatedAtEpochMs: NOW.getTime() - 62_000,
    });
    const second = failedRow("wf-second", {
      updatedAtEpochMs: NOW.getTime() - 61_000,
    });
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep(first.workflowUuid, first.name);
    await ledger.recordSweep(second.workflowUuid, second.name);
    const posts: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([first, second]),
      post: async (text) => {
        posts.push(text);
      },
      policies: () => ({ mode: "adopt", staleAfterHours: 1 }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    expect(await alerter.scanTerminalFailures()).toEqual({
      scanned: 2,
      alerted: 2,
      cleanupsRun: 0,
      cleanupsFailed: 0,
      watermark: second.updatedAtEpochMs,
    });
    expect(posts).toHaveLength(2);
    expect(await ledger.getWatermark("terminal_failures")).toBe(
      second.updatedAtEpochMs,
    );
  });

  test("the watermark trails wall clock so a late-committing tie still alerts", async () => {
    // updated_at is stamped in JS before the row's transaction commits: a row
    // tied to (or older than) the last processed timestamp can become visible
    // only after the scan. Without the wall-clock clamp the watermark passes
    // it and its alert + cleanup are dropped forever.
    const processed = failedRow("wf-early", {
      updatedAtEpochMs: NOW.getTime() - 1_000,
    });
    const lateCommit = failedRow("wf-late-commit", {
      updatedAtEpochMs: NOW.getTime() - 1_000,
    });
    let lateCommitVisible = false;
    const underlying = makeInMemoryDbosStatusStore([processed, lateCommit]);
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const posts: string[] = [];
    const alerter = makeSweepAlerter({
      ledger,
      status: {
        ...underlying,
        async listNewlyTerminalFailed(since, limit) {
          const rows = await underlying.listNewlyTerminalFailed(since, limit);
          return lateCommitVisible
            ? rows
            : rows.filter(
                (row) => row.workflowUuid !== lateCommit.workflowUuid,
              );
        },
      },
      post: async (text) => {
        posts.push(text);
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    const first = await alerter.scanTerminalFailures();
    expect(first.watermark).toBe(
      NOW.getTime() - TERMINAL_FAILURE_VISIBILITY_LAG_MS,
    );

    lateCommitVisible = true;
    const second = await alerter.scanTerminalFailures();
    expect(second.alerted).toBe(1);
    expect(posts.some((post) => post.includes("wf-late-commit"))).toBe(true);
    // The already-processed tie is ledger-deduped, not re-alerted.
    expect(posts.filter((post) => post.includes("wf-early"))).toHaveLength(1);
  });

  test("retries the second cleanup when terminal failures share a timestamp", async () => {
    const tiedAt = NOW.getTime() - 60_000;
    const first = failedRow("wf-tied-succeeded", {
      name: "FirstWorkflow",
      updatedAtEpochMs: tiedAt,
    });
    const second = failedRow("wf-tied-retry", {
      name: "SecondWorkflow",
      updatedAtEpochMs: tiedAt,
    });
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const alertPosts = new Map<string, number>();
    const cleanupCalls = new Map<string, number>();
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([first, second]),
      post: async (text) => {
        for (const workflowUuid of [first.workflowUuid, second.workflowUuid]) {
          if (text.includes(workflowUuid)) {
            alertPosts.set(
              workflowUuid,
              (alertPosts.get(workflowUuid) ?? 0) + 1,
            );
          }
        }
      },
      policies: (name) => ({
        mode: "adopt",
        staleAfterHours: 1,
        async onTerminalFailure(_ctx, workflow) {
          const calls = (cleanupCalls.get(workflow.workflowUuid) ?? 0) + 1;
          cleanupCalls.set(workflow.workflowUuid, calls);
          if (name === second.name && calls === 1) {
            throw new Error("retry this cleanup");
          }
        },
      }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    const firstScan = await alerter.scanTerminalFailures();
    expect(firstScan.watermark).toBe(tiedAt - 1);
    expect((await ledger.get(second.workflowUuid))?.cleanupDoneAt).toBeNull();

    const secondScan = await alerter.scanTerminalFailures();
    expect(secondScan.scanned).toBe(2);
    expect((await ledger.get(second.workflowUuid))?.cleanupDoneAt).not.toBeNull();
    expect(cleanupCalls.get(second.workflowUuid)).toBe(2);
    // Re-including the timestamp is quiet for the row that already completed.
    expect(cleanupCalls.get(first.workflowUuid)).toBe(1);
    expect(alertPosts.get(first.workflowUuid)).toBe(1);
    expect(alertPosts.get(second.workflowUuid)).toBe(1);
  });

  test("expands a re-scan when the batch limit cuts through a timestamp tie", async () => {
    const tiedAt = NOW.getTime() - 60_000;
    const rows = Array.from({ length: 51 }, (_, index) =>
      failedRow(`wf-tied-batch-${index.toString().padStart(2, "0")}`, {
        updatedAtEpochMs: tiedAt,
      }),
    );
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const postCounts = new Map<string, number>();
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore(rows),
      post: async (text) => {
        const row = rows.find(({ workflowUuid }) =>
          text.includes(workflowUuid),
        );
        if (row) {
          postCounts.set(
            row.workflowUuid,
            (postCounts.get(row.workflowUuid) ?? 0) + 1,
          );
        }
      },
      policies: () => ({ mode: "alert-only" }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    const firstScan = await alerter.scanTerminalFailures();
    expect(firstScan.scanned).toBe(50);
    expect(firstScan.watermark).toBe(tiedAt - 1);
    expect(postCounts.has(rows[50]!.workflowUuid)).toBe(false);

    const secondScan = await alerter.scanTerminalFailures();
    expect(secondScan.scanned).toBe(51);
    expect(secondScan.watermark).toBe(tiedAt);
    expect(postCounts.get(rows[50]!.workflowUuid)).toBe(1);
    // The 50 re-included rows are ledger-deduped and stay quiet.
    expect([...postCounts.values()].every((count) => count === 1)).toBe(true);
  });

  test("holds the watermark on cleanup failure, retries, then gives up at the cap", async () => {
    const row = failedRow("wf-broken-cleanup");
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep(row.workflowUuid, row.name);
    const posts: string[] = [];
    let cleanupCalls = 0;
    async function brokenCleanup(
      _ctx: SweepContext,
      _wf: FailedWorkflow,
    ): Promise<void> {
      cleanupCalls++;
      throw new Error("cleanup broke");
    }
    const policy: ResolvedPolicy = {
      mode: "adopt",
      staleAfterHours: 1,
      onTerminalFailure: brokenCleanup,
    };
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([row]),
      post: async (text) => {
        posts.push(text);
      },
      policies: () => policy,
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 2,
    });

    expect(await alerter.scanTerminalFailures()).toEqual({
      scanned: 1,
      alerted: 1,
      cleanupsRun: 1,
      cleanupsFailed: 1,
      watermark: row.updatedAtEpochMs - 1,
    });
    expect(await ledger.getWatermark("terminal_failures")).toBe(
      row.updatedAtEpochMs - 1,
    );
    expect((await ledger.get(row.workflowUuid))?.cleanupDoneAt).toBeNull();

    expect(await alerter.scanTerminalFailures()).toEqual({
      scanned: 1,
      alerted: 0,
      cleanupsRun: 1,
      cleanupsFailed: 1,
      watermark: row.updatedAtEpochMs,
    });
    expect(cleanupCalls).toBe(2);
    expect((await ledger.get(row.workflowUuid))?.cleanupFn).toBe(
      "gave-up:brokenCleanup",
    );
    expect(await ledger.getWatermark("terminal_failures")).toBe(
      row.updatedAtEpochMs,
    );
    expect(posts).toHaveLength(2);
    expect(posts[1]).toContain("abandoned");
    expect(posts[1]).toContain("brokenCleanup");
  });

  test("retries abandonment alerting before recording cleanup as gave up", async () => {
    const row = failedRow("wf-abandonment-alert-retry");
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    let cleanupCalls = 0;
    let genericAlerts = 0;
    let abandonmentAttempts = 0;
    let abandonmentAlerts = 0;
    async function brokenCleanup(): Promise<void> {
      cleanupCalls++;
      throw new Error("cleanup remains broken");
    }
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([row]),
      post: async (text) => {
        if (text.startsWith("DBOS cleanup abandoned:")) {
          abandonmentAttempts++;
          if (abandonmentAttempts === 1) {
            throw new Error("ops channel temporarily unavailable");
          }
          abandonmentAlerts++;
        } else {
          genericAlerts++;
        }
      },
      policies: () => ({
        mode: "adopt",
        staleAfterHours: 1,
        onTerminalFailure: brokenCleanup,
      }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 1,
    });

    const firstScan = await alerter.scanTerminalFailures();
    expect(firstScan.watermark).toBe(row.updatedAtEpochMs - 1);
    expect((await ledger.get(row.workflowUuid))?.cleanupDoneAt).toBeNull();

    const secondScan = await alerter.scanTerminalFailures();
    expect(secondScan.scanned).toBe(1);
    expect(secondScan.watermark).toBe(row.updatedAtEpochMs);
    expect(cleanupCalls).toBe(2);
    expect(genericAlerts).toBe(1);
    expect(abandonmentAttempts).toBe(2);
    expect(abandonmentAlerts).toBe(1);
    expect((await ledger.get(row.workflowUuid))?.cleanupFn).toBe(
      "gave-up:brokenCleanup",
    );
  });

  test("a wedged cleanup holds the watermark but later failures still alert", async () => {
    const wedged = failedRow("wf-wedged", {
      name: "SlackThreadWorkflow",
      updatedAtEpochMs: NOW.getTime() - 62_000,
    });
    const newer = failedRow("wf-newer", {
      updatedAtEpochMs: NOW.getTime() - 61_000,
    });
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    const posts: string[] = [];
    const policies = (name: string): ResolvedPolicy =>
      name === "SlackThreadWorkflow"
        ? {
            mode: "adopt",
            staleAfterHours: 48,
            onTerminalFailure: async () => {
              throw new Error("still broken");
            },
          }
        : { mode: "adopt", staleAfterHours: 1 };
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([wedged, newer]),
      post: async (text) => {
        posts.push(text);
      },
      policies,
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 5,
    });

    const result = await alerter.scanTerminalFailures();

    // Both rows alerted even though the older row's cleanup is wedged…
    expect(result.alerted).toBe(2);
    expect(posts.some((p) => p.includes("wf-wedged"))).toBe(true);
    expect(posts.some((p) => p.includes("wf-newer"))).toBe(true);
    // …but the watermark stays immediately before the wedged row so it is
    // re-scanned without replaying unrelated older timestamps.
    expect(result.watermark).toBe(wedged.updatedAtEpochMs - 1);
    expect(await ledger.getWatermark("terminal_failures")).toBe(
      wedged.updatedAtEpochMs - 1,
    );

    // The re-scan stays quiet on alerts (ledger dedup) and retries the cleanup.
    const second = await alerter.scanTerminalFailures();
    expect(second.alerted).toBe(0);
    expect(second.cleanupsRun).toBe(1);
  });

  test("posts the generic alert before a throwing cleanup callback", async () => {
    const row = failedRow("wf-order");
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep(row.workflowUuid, row.name);
    const events: string[] = [];
    async function throwingCleanup(): Promise<void> {
      events.push("cleanup");
      throw new Error("bad callback");
    }
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([row]),
      post: async () => {
        events.push("alert");
      },
      policies: () => ({
        mode: "adopt",
        staleAfterHours: 1,
        onTerminalFailure: throwingCleanup,
      }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    await expect(alerter.scanTerminalFailures()).resolves.toBeDefined();

    expect(events).toEqual(["alert", "cleanup"]);
    expect((await ledger.get(row.workflowUuid))?.alertedAt).not.toBeNull();
  });

  test("skips a callback already recorded as complete", async () => {
    const row = failedRow("wf-cleaned");
    const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
    await ledger.recordSweep(row.workflowUuid, row.name);
    await ledger.markCleanupDone(row.workflowUuid, row.name, "priorCleanup");
    let cleanupCalls = 0;
    const alerter = makeSweepAlerter({
      ledger,
      status: makeInMemoryDbosStatusStore([row]),
      post: async () => {},
      policies: () => ({
        mode: "adopt",
        staleAfterHours: 1,
        async onTerminalFailure() {
          cleanupCalls++;
        },
      }),
      cleanupCtx: cleanupContext(),
      now: () => new Date(NOW),
      log,
      maxCleanupAttempts: 3,
    });

    const result = await alerter.scanTerminalFailures();

    expect(cleanupCalls).toBe(0);
    expect(result.cleanupsRun).toBe(0);
    expect(result.watermark).toBe(row.updatedAtEpochMs);
  });
});
