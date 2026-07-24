import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemorySweepLedgerStore,
  makeInMemorySweepLookupStore,
  type FailedDbosWorkflowRow,
} from "../../db/dbos-sweep.ts";
import { makeSweepAlerter, type SweepAlerterDeps } from "../alerts.ts";
import type { ResolvedPolicy, SweepContext } from "../policy.ts";
import type { SweepDecision } from "../sweeper.ts";

const NOW = new Date("2026-07-23T12:00:00.000Z");
const log: Logger = pino({ level: "silent" });

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

/** Ledger + status wired the way production is: the status store's unhandled
 * scan anti-joins the ledger's completion marks. */
function scanFixture(
  rows: FailedDbosWorkflowRow[],
  policies: (name: string) => ResolvedPolicy,
  opts: {
    maxCleanupAttempts?: number;
    /** Runs before a post is recorded; throw to simulate a failed post. */
    onPost?: (text: string) => void | Promise<void>;
  } = {},
) {
  const ledger = makeInMemorySweepLedgerStore(() => new Date(NOW));
  const status = makeInMemoryDbosStatusStore(rows, () => new Date(NOW), {
    isTerminalFailureHandled: async (workflowUuid) => {
      const row = await ledger.get(workflowUuid);
      return Boolean(row?.terminalAlertedAt && row?.cleanupDoneAt);
    },
  });
  const posts: string[] = [];
  const deps: SweepAlerterDeps = {
    ledger,
    status,
    post: async (text) => {
      await opts.onPost?.(text);
      posts.push(text);
    },
    policies,
    cleanupCtx: cleanupContext(),
    log,
    maxCleanupAttempts: opts.maxCleanupAttempts ?? 3,
  };
  return { ledger, status, posts, deps, alerter: makeSweepAlerter(deps) };
}

describe("SweepAlerter.alertDecisions", () => {
  test("deduplicates alert-worthy decisions through alertedAt", async () => {
    const f = scanFixture([], () => ({ mode: "alert-only" }));
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

    expect(f.posts).toHaveLength(1);
    expect(f.posts[0]).toContain("ToolExecWorkflow");
    expect(f.posts[0]).toContain("wf-alert");
    expect(f.posts[0]).toContain("error");
    expect(f.posts[0]).toContain("compare-and-set lost");
    expect((await f.ledger.get("wf-alert"))?.alertedAt).not.toBeNull();
  });

  test("dedup sticks for a workflow with no prior ledger row", async () => {
    // alert_only workflows are never swept, so no recordSweep row exists;
    // markAlerted must upsert or the ops channel gets the alert every cycle.
    const f = scanFixture([], () => ({ mode: "alert-only" }));
    const decisions: SweepDecision[] = [
      {
        workflowUuid: "wf-never-swept",
        name: "DeletedWorkflowName",
        action: "alert_only",
      },
    ];

    await f.alerter.alertDecisions(decisions);
    await f.alerter.alertDecisions(decisions);

    expect(f.posts).toHaveLength(1);
    expect((await f.ledger.get("wf-never-swept"))?.alertedAt).not.toBeNull();
  });

  test("a cancel alert posts even after an earlier alert_only alert", async () => {
    // The cancellation is the transition operators must see; alertedAt dedup
    // applies only to the recurring actions (alert_only, error). A successful
    // cancel is once-per-workflow by construction — the row turns CANCELLED
    // and leaves the scan set — so this cannot re-alert every cycle.
    const f = scanFixture([], () => ({ mode: "alert-only" }));

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
    // Every cancel class posts, including policy-driven cancels.
    await f.alerter.alertDecisions([
      {
        workflowUuid: "wf-policy",
        name: "CancelModeWorkflow",
        action: "cancelled_policy",
      },
    ]);

    expect(f.posts).toHaveLength(3);
    expect(f.posts[0]).toContain("alert_only");
    expect(f.posts[1]).toContain("cancelled_stale");
    expect(f.posts[2]).toContain("cancelled_policy");
  });

  test("contains a posting failure and continues to the next decision", async () => {
    const f = scanFixture([], () => ({ mode: "alert-only" }), {
      onPost: (text) => {
        if (text.includes("wf-bad")) throw new Error("ops unavailable");
      },
    });
    await f.ledger.recordSweep("wf-bad", "ToolExecWorkflow");
    await f.ledger.recordSweep("wf-next", "ToolExecWorkflow");

    await expect(
      f.alerter.alertDecisions([
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

    expect(f.posts).toHaveLength(1);
    expect(f.posts[0]).toContain("wf-next");
    expect((await f.ledger.get("wf-bad"))?.alertedAt).toBeNull();
    expect((await f.ledger.get("wf-next"))?.alertedAt).not.toBeNull();
  });
});

describe("SweepAlerter.scanTerminalFailures", () => {
  test("an empty scan reports zeroes", async () => {
    const f = scanFixture([], () => ({ mode: "alert-only" }));

    expect(await f.alerter.scanTerminalFailures()).toEqual({
      scanned: 0,
      alerted: 0,
      cleanupsRun: 0,
      cleanupsFailed: 0,
    });
  });

  test("alerted rows with no cleanup are marked handled and leave the set", async () => {
    const first = failedRow("wf-first", {
      updatedAtEpochMs: NOW.getTime() - 62_000,
    });
    const second = failedRow("wf-second", {
      updatedAtEpochMs: NOW.getTime() - 61_000,
    });
    const f = scanFixture(
      [first, second],
      () => ({ mode: "adopt", staleAfterHours: 1 }),
    );

    expect(await f.alerter.scanTerminalFailures()).toEqual({
      scanned: 2,
      alerted: 2,
      cleanupsRun: 0,
      cleanupsFailed: 0,
    });
    expect(f.posts).toHaveLength(2);
    // "Nothing to clean up" is recorded so the anti-join excludes the row.
    expect((await f.ledger.get("wf-first"))?.cleanupFn).toBe("none");

    expect(await f.alerter.scanTerminalFailures()).toEqual({
      scanned: 0,
      alerted: 0,
      cleanupsRun: 0,
      cleanupsFailed: 0,
    });
    expect(f.posts).toHaveLength(2);
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
    const f = scanFixture([processed, lateCommit], () => ({
      mode: "adopt",
      staleAfterHours: 1,
    }));
    const underlying = f.deps.status;
    f.deps.status = {
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
    };
    const alerter = makeSweepAlerter(f.deps);

    const first = await alerter.scanTerminalFailures();
    expect(first.scanned).toBe(1);
    expect(first.alerted).toBe(1);

    lateCommitVisible = true;
    const second = await alerter.scanTerminalFailures();
    expect(second.scanned).toBe(1);
    expect(second.alerted).toBe(1);
    expect(f.posts.some((post) => post.includes("wf-late-commit"))).toBe(
      true,
    );
    // The already-handled tie is excluded by the anti-join, not re-alerted.
    expect(f.posts.filter((post) => post.includes("wf-early"))).toHaveLength(
      1,
    );
  });

  test("a sweep-decision alert does not suppress the terminal-failure alert", async () => {
    // The two alert streams dedup on distinct markers: an adoption-race
    // error decision stamps alertedAt, but if that workflow later fails
    // terminally on its new owner, the terminal-failure alarm must still fire.
    const row = failedRow("wf-raced-then-failed");
    const f = scanFixture([row], () => ({
      mode: "adopt",
      staleAfterHours: 1,
    }));
    await f.ledger.markAlerted(row.workflowUuid, row.name);

    const result = await f.alerter.scanTerminalFailures();

    expect(result.alerted).toBe(1);
    expect(f.posts).toHaveLength(1);
    expect(f.posts[0]).toContain("terminal failure");
    expect(
      (await f.ledger.get(row.workflowUuid))?.terminalAlertedAt,
    ).not.toBeNull();

    const second = await f.alerter.scanTerminalFailures();
    expect(second.scanned).toBe(0);
    expect(f.posts).toHaveLength(1);
  });

  test("a batch cutting through tied timestamps still reaches every row", async () => {
    // 51 rows tied at one timestamp with a batch limit of 50: handled rows
    // drop out of the anti-join, so the second scan returns the remainder.
    // No cursor or tie arithmetic is involved.
    const tiedAt = NOW.getTime() - 60_000;
    const rows = Array.from({ length: 51 }, (_, index) =>
      failedRow(`wf-tied-batch-${index.toString().padStart(2, "0")}`, {
        updatedAtEpochMs: tiedAt,
      }),
    );
    const f = scanFixture(rows, () => ({
      mode: "adopt",
      staleAfterHours: 1,
    }));

    const firstScan = await f.alerter.scanTerminalFailures();
    expect(firstScan.scanned).toBe(50);
    expect(firstScan.alerted).toBe(50);

    const secondScan = await f.alerter.scanTerminalFailures();
    expect(secondScan.scanned).toBe(1);
    expect(secondScan.alerted).toBe(1);

    expect((await f.alerter.scanTerminalFailures()).scanned).toBe(0);
    expect(f.posts).toHaveLength(51);
    expect(new Set(f.posts).size).toBe(51);
  });

  test("a failing cleanup keeps the row unhandled, retries, then gives up at the cap", async () => {
    const row = failedRow("wf-broken-cleanup");
    let cleanupCalls = 0;
    async function brokenCleanup(): Promise<void> {
      cleanupCalls++;
      throw new Error("cleanup broke");
    }
    const f = scanFixture(
      [row],
      () => ({
        mode: "adopt",
        staleAfterHours: 1,
        onTerminalFailure: brokenCleanup,
      }),
      { maxCleanupAttempts: 2 },
    );

    expect(await f.alerter.scanTerminalFailures()).toEqual({
      scanned: 1,
      alerted: 1,
      cleanupsRun: 1,
      cleanupsFailed: 1,
    });
    expect((await f.ledger.get(row.workflowUuid))?.cleanupDoneAt).toBeNull();

    // Still unhandled: retried, hits the cap, abandoned with an alert.
    expect(await f.alerter.scanTerminalFailures()).toEqual({
      scanned: 1,
      alerted: 0,
      cleanupsRun: 1,
      cleanupsFailed: 1,
    });
    expect(cleanupCalls).toBe(2);
    expect((await f.ledger.get(row.workflowUuid))?.cleanupFn).toBe(
      "gave-up:brokenCleanup",
    );
    expect(f.posts).toHaveLength(2);
    expect(f.posts[1]).toContain("abandoned");
    expect(f.posts[1]).toContain("brokenCleanup");

    // The durable gave-up mark removes the row from the unhandled set.
    expect((await f.alerter.scanTerminalFailures()).scanned).toBe(0);
  });

  test("retries abandonment alerting before recording cleanup as gave up", async () => {
    const row = failedRow("wf-abandonment-alert-retry");
    let cleanupCalls = 0;
    let abandonmentAttempts = 0;
    async function brokenCleanup(): Promise<void> {
      cleanupCalls++;
      throw new Error("cleanup remains broken");
    }
    const f = scanFixture(
      [row],
      () => ({
        mode: "adopt",
        staleAfterHours: 1,
        onTerminalFailure: brokenCleanup,
      }),
      {
        maxCleanupAttempts: 1,
        onPost: (text) => {
          if (text.startsWith("DBOS cleanup abandoned:")) {
            abandonmentAttempts++;
            if (abandonmentAttempts === 1) {
              throw new Error("ops channel temporarily unavailable");
            }
          }
        },
      },
    );

    await f.alerter.scanTerminalFailures();
    expect((await f.ledger.get(row.workflowUuid))?.cleanupDoneAt).toBeNull();

    const secondScan = await f.alerter.scanTerminalFailures();
    expect(secondScan.scanned).toBe(1);
    expect(cleanupCalls).toBe(2);
    expect(abandonmentAttempts).toBe(2);
    expect((await f.ledger.get(row.workflowUuid))?.cleanupFn).toBe(
      "gave-up:brokenCleanup",
    );
    // One generic terminal alert + one successful abandonment alert.
    expect(f.posts).toHaveLength(2);
  });

  test("a wedged cleanup does not block later failures", async () => {
    const wedged = failedRow("wf-wedged", {
      name: "SlackThreadWorkflow",
      updatedAtEpochMs: NOW.getTime() - 62_000,
    });
    const newer = failedRow("wf-newer", {
      updatedAtEpochMs: NOW.getTime() - 61_000,
    });
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
    const f = scanFixture([wedged, newer], policies, {
      maxCleanupAttempts: 5,
    });

    const result = await f.alerter.scanTerminalFailures();

    // Both rows alerted even though the older row's cleanup is wedged.
    expect(result.alerted).toBe(2);
    expect(f.posts.some((p) => p.includes("wf-wedged"))).toBe(true);
    expect(f.posts.some((p) => p.includes("wf-newer"))).toBe(true);

    // Only the wedged row remains unhandled; its re-scan is alert-quiet.
    const second = await f.alerter.scanTerminalFailures();
    expect(second.scanned).toBe(1);
    expect(second.alerted).toBe(0);
    expect(second.cleanupsRun).toBe(1);
  });

  test("posts the generic alert before a throwing cleanup callback", async () => {
    const row = failedRow("wf-order");
    const events: string[] = [];
    async function throwingCleanup(): Promise<void> {
      events.push("cleanup");
      throw new Error("bad callback");
    }
    const f = scanFixture(
      [row],
      () => ({
        mode: "adopt",
        staleAfterHours: 1,
        onTerminalFailure: throwingCleanup,
      }),
      {
        onPost: () => {
          events.push("alert");
        },
      },
    );

    await expect(f.alerter.scanTerminalFailures()).resolves.toBeDefined();

    expect(events).toEqual(["alert", "cleanup"]);
    expect(
      (await f.ledger.get(row.workflowUuid))?.terminalAlertedAt,
    ).not.toBeNull();
  });

  test("skips a callback already recorded as complete", async () => {
    const row = failedRow("wf-cleaned");
    let cleanupCalls = 0;
    const f = scanFixture([row], () => ({
      mode: "adopt",
      staleAfterHours: 1,
      async onTerminalFailure() {
        cleanupCalls++;
      },
    }));
    await f.ledger.markCleanupDone(row.workflowUuid, row.name, "priorCleanup");

    const result = await f.alerter.scanTerminalFailures();

    // The alert still fires (its own marker was absent), the cleanup does not.
    expect(result.alerted).toBe(1);
    expect(cleanupCalls).toBe(0);
    expect(result.cleanupsRun).toBe(0);

    expect((await f.alerter.scanTerminalFailures()).scanned).toBe(0);
  });
});
