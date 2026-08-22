import { describe, expect, test } from "bun:test";

import { RetryAutomationError, retryAutomationReview } from "../retry-automation.ts";
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
      inputsSchema: [],
      settings: { concurrency: { keyTemplate: "${{ event.raw.pull_request.html_url }}", policy: "supersede" }, endSessionsOnFinish: true },
      createdByUserId: null,
      createdAt: NOW,
    },
  };
}

function originalRun(): AutomationRunRow {
  return {
    id: "autorun:b1:github:d1",
    automationId: "b1",
    version: 2,
    trigger: {
      source: "integration",
      receivedAt: "2026-08-21T00:00:00Z",
      eventKey: "pull_request.opened",
      payload: { pull_request: { html_url: "https://github.com/engrams/engrams/pull/7" } },
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
  const deps = {
    store: store as never,
    starter: { start: async (_i: { runId: string; automationId: string }, id: string) => void started.push(id) },
    sender: { send: async () => {} },
    now: () => NOW,
    randomUUID: () => "uuid-1",
  };
  return { deps, admitted, started };
}

describe("retryAutomationReview", () => {
  test("admits a fresh built-in run with a retry: delivery key and the original trigger payload", async () => {
    const h = harness(originalRun());
    const runId = await retryAutomationReview("autorun:b1:github:d1", h.deps);
    expect(runId).toBe("autorun:b1:retry:uuid-1");
    expect(h.admitted).toHaveLength(1);
    expect(h.admitted[0]!.deliveryKey).toBe("retry:uuid-1");
    // The original PR payload rides the retry so the built-in resolves the same PR.
    expect(h.admitted[0]!.trigger).toMatchObject({
      source: "integration",
      payload: { pull_request: { html_url: "https://github.com/engrams/engrams/pull/7" } },
    });
    expect(h.started).toEqual([runId]);
  });

  test("throws when the original run no longer exists", async () => {
    const h = harness(null);
    await expect(retryAutomationReview("gone", h.deps)).rejects.toBeInstanceOf(RetryAutomationError);
  });
});
