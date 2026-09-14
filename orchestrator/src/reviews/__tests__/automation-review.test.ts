import { describe, expect, test } from "bun:test";

import {
  RetryAutomationError,
  dispatchAutomationReview,
  retryAutomationReview,
  stopAutomationReview,
} from "../automation-review.ts";
import type { AutomationRunRow } from "../../db/automations.ts";

const NOW = new Date("2026-08-22T00:00:00Z");

function builtin() {
  return {
    id: "b1",
    name: "PR review",
    description: "",
    kind: "builtin",
    builtinKey: "pr_review",
    enabled: true,
    currentVersion: 2,
    inputs: { repos: {} },
    blockOverrides: {},
    endSessionsOnFinish: true,
    createdByUserId: null,
    nextFireAt: null,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
    version: {
      automationId: "b1",
      version: 2,
      trigger: { kind: "integration", provider: "github", connectionId: "c", eventKeys: ["pull_request.opened"] },
      blocks: [],
      entrypoints: [],
      inputsSchema: [],
      settings: {
        instance: { keyTemplate: "${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}" },
        concurrency: { keyTemplate: "${{ event.raw.pull_request.html_url }}", policy: "supersede" },
        endSessionsOnFinish: true,
      },
      createdByUserId: null,
      createdAt: NOW,
    },
  };
}

function originalRun(): AutomationRunRow {
  return {
    id: "autorun:b1:github:d1",
    automationId: "b1",
    entrypointId: "main",
    instanceId: "",
    version: 2,
    trigger: {
      source: "integration",
      receivedAt: "2026-08-21T00:00:00Z",
      eventKey: "pull_request.opened",
      payload: { repository: { full_name: "engrams/engrams" }, pull_request: { number: 7, html_url: "https://github.com/engrams/engrams/pull/7" } },
    },
    deliveryKey: "github:d1",
    concurrencyKey: null,
    renderedPrompt: null,
    renderedTitle: null,
    taskId: null,
    sessionId: null,
    status: "completed",
    error: null,
    scheduledFor: null,
    leaseOwner: null,
    leaseExpiresAt: null,
    startedAt: NOW,
    endedAt: NOW,
    createdAt: NOW,
    dryRun: false,
  } as AutomationRunRow;
}

function harness(run: AutomationRunRow | null) {
  const admitted: Array<{ runId: string; deliveryKey: string; trigger: unknown }> = [];
  const started: string[] = [];
  const store = {
    getByBuiltinKey: async () => (run ? (builtin() as never) : (builtin() as never)),
    getRun: async () => run,
    insertRun: async (input: { id: string; deliveryKey: string | null; trigger: unknown }) => {
      admitted.push({ runId: input.id, deliveryKey: input.deliveryKey ?? "", trigger: input.trigger });
      return { id: input.id } as never;
    },
    claimConcurrency: async () => ({ claimed: true as const }),
    getConcurrencyHolder: async () => null,
    casConcurrency: async () => true,
  };
  const opened: Array<{ key: string; openedBy: string }> = [];
  const instances = {
    resolveHandles: async () => [],
    getOpenInstanceByKey: async () => null,
    getInstance: async () => null,
    openInstance: async (input: { key: string; openedBy: string; inputs: Record<string, unknown> }) => {
      opened.push({ key: input.key, openedBy: input.openedBy });
      return { id: "ai_pr", key: input.key, status: "open", inputs: input.inputs } as never;
    },
  };
  const deps = {
    store: store as never,
    starter: { start: async (_i: { runId: string; automationId: string }, id: string) => void started.push(id) },
    sender: { send: async () => {} },
    instances: instances as never,
    now: () => NOW,
    randomUUID: () => "uuid-1",
  };
  return { deps, admitted, started, opened };
}

describe("retryAutomationReview", () => {
  test("admits a fresh built-in run with a retry: delivery key and the original trigger payload", async () => {
    const h = harness(originalRun());
    const runId = await retryAutomationReview("autorun:b1:github:d1", h.deps);
    // Bound to the PR's workstream (ADR 0120), opened by the key template.
    expect(runId).toBe("autorun:b1:main:i-ai_pr:retry:uuid-1");
    expect(h.opened).toEqual([{ key: "engrams/engrams#7", openedBy: "review:retry:uuid-1" }]);
    expect(h.admitted).toHaveLength(1);
    expect(h.admitted[0]!.deliveryKey).toBe("retry:uuid-1");
    // The original PR payload rides the retry so the built-in resolves the same PR.
    expect(h.admitted[0]!.trigger).toMatchObject({
      source: "integration",
      payload: { repository: { full_name: "engrams/engrams" }, pull_request: { number: 7, html_url: "https://github.com/engrams/engrams/pull/7" } },
    });
    expect(h.started).toEqual([runId]);
  });

  test("throws when the original run no longer exists", async () => {
    const h = harness(null);
    await expect(retryAutomationReview("gone", h.deps)).rejects.toBeInstanceOf(RetryAutomationError);
  });
});

describe("dispatchAutomationReview", () => {
  test("admits a built-in run under review.dispatch with a coordinate-only payload", async () => {
    const h = harness(null);
    const runId = await dispatchAutomationReview({ repo: "engrams/engrams", prNumber: 42 }, h.deps);
    expect(runId).toBe("autorun:b1:main:i-ai_pr:dispatch:uuid-1");
    expect(h.opened).toEqual([{ key: "engrams/engrams#42", openedBy: "review:dispatch:uuid-1" }]);
    expect(h.admitted).toHaveLength(1);
    expect(h.admitted[0]!.deliveryKey).toBe("dispatch:uuid-1");
    expect(h.admitted[0]!.trigger).toEqual({
      source: "manual",
      eventKey: "review.dispatch",
      receivedAt: NOW.toISOString(),
      scopeValue: "engrams/engrams",
      payload: {
        repository: { full_name: "engrams/engrams", name: "engrams" },
        pull_request: { number: 42, html_url: "https://github.com/engrams/engrams/pull/42" },
      },
    });
    expect(h.started).toEqual([runId]);
  });
});

describe("stopAutomationReview", () => {
  const coordinate = { repo: "engrams/engrams", prNumber: 7 };
  function stopHarness(options: { active?: { automationRunId: string | null } | null; runStatus?: string }) {
    const sent: Array<{ runId: string; message: unknown; key: string }> = [];
    const deps = {
      reviews: {
        getActiveReviewByCoordinate: async () =>
          options.active === null || options.active === undefined
            ? null
            : ({ id: "review-1", automationRunId: options.active.automationRunId } as never),
      },
      runs: {
        getRun: async (id: string) =>
          options.runStatus === undefined ? null : ({ id, status: options.runStatus } as never),
      },
      sender: {
        send: async (runId: string, message: unknown, key: string) => {
          sent.push({ runId, message, key });
        },
      },
      randomUUID: () => "req-1",
    };
    return { deps, sent };
  }

  test("sends a stop into the live run behind the PR's active pass", async () => {
    const h = stopHarness({ active: { automationRunId: "autorun:b1:github:d1" }, runStatus: "running" });
    expect(await stopAutomationReview(coordinate, h.deps)).toEqual({ stopped: true, runId: "autorun:b1:github:d1" });
    expect(h.sent).toEqual([
      {
        runId: "autorun:b1:github:d1",
        message: { kind: "stop", reason: "stopped by @mention command" },
        key: "autorun:autorun:b1:github:d1:stop:req-1",
      },
    ]);
  });

  test("an idle PR, a legacy pass, or a finished run is a quiet result, not an error", async () => {
    expect(await stopAutomationReview(coordinate, stopHarness({ active: null }).deps)).toEqual({
      stopped: false,
      reason: "no_active_review",
    });
    expect(
      await stopAutomationReview(coordinate, stopHarness({ active: { automationRunId: null } }).deps),
    ).toEqual({ stopped: false, reason: "not_on_engine" });
    const done = stopHarness({ active: { automationRunId: "autorun:b1:github:d1" }, runStatus: "completed" });
    expect(await stopAutomationReview(coordinate, done.deps)).toEqual({ stopped: false, reason: "run_not_live" });
    expect(done.sent).toEqual([]);
  });
});
