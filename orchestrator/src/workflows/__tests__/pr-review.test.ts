import { describe, expect, test } from "bun:test";

import type { ReviewControlPlane } from "../review-control-plane.ts";
import type { ReviewInbox } from "../review-inbox.ts";
import { prReviewWorkflowImpl, type StepRunner } from "../pr-review.ts";

function fakeControlPlane(
  overrides: Partial<ReviewControlPlane> = {},
): ReviewControlPlane {
  return {
    ensureReviewRecord: async () => ({ reviewId: "review-1", taskId: "task-1" }),
    createFinderSession: async () => ({ sessionId: "session-1" }),
    bootstrapFinderSession: async () => {},
    sendFinderPrompt: async () => {},
    markReviewFailed: async () => {},
    markReviewHalted: async () => {},
    ...overrides,
  };
}

describe("PrReviewWorkflow shell", () => {
  test("runs finder setup in order, then halts on stop", async () => {
    const messages: Array<ReviewInbox | null> = [
      {
        kind: "trigger",
        repo: "openai/engrams",
        prNumber: 100,
        trigger: "opened",
        headSha: "head-sha",
      },
      { kind: "stop" },
    ];
    const calls: Array<{ name: string; input?: unknown }> = [];
    const steps: string[] = [];
    const step: StepRunner = async (fn, name) => {
      steps.push(name);
      return fn();
    };
    const cp = fakeControlPlane({
      async ensureReviewRecord(input) {
        calls.push({ name: "ensureReviewRecord", input });
        return { reviewId: "review-1", taskId: "task-1" };
      },
      async createFinderSession(input) {
        calls.push({ name: "createFinderSession", input });
        return { sessionId: "session-1" };
      },
      async bootstrapFinderSession(sessionId, input) {
        calls.push({ name: "bootstrapFinderSession", input: { sessionId, ...input } });
      },
      async sendFinderPrompt(sessionId, input) {
        calls.push({ name: "sendFinderPrompt", input: { sessionId, ...input } });
      },
      async markReviewHalted(repo, prNumber) {
        calls.push({ name: "markReviewHalted", input: [repo, prNumber] });
      },
    });

    await prReviewWorkflowImpl({
      controlPlane: cp,
      step,
      recv: async () => messages.shift() ?? null,
    });

    expect(calls.map((call) => call.name)).toEqual([
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "sendFinderPrompt",
      "markReviewHalted",
    ]);
    expect(steps).toEqual([
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "sendFinderPrompt",
      "markReviewHalted",
    ]);
    expect(calls[0]?.input).toEqual({
      repo: "openai/engrams",
      prNumber: 100,
      headSha: "head-sha",
      baseSha: "",
      trigger: "opened",
    });
    expect(calls[2]?.input).toEqual({
      sessionId: "session-1",
      repo: "openai/engrams",
      headSha: "head-sha",
    });
    expect(calls[3]?.input).toMatchObject({
      sessionId: "session-1",
      reviewId: "review-1",
      baseSha: "",
    });
  });

  test("marks the review failed and returns when finder setup throws", async () => {
    const messages: ReviewInbox[] = [{
      kind: "trigger",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "opened",
      headSha: "head-sha",
      focus: "Inspect retries",
    }];
    const calls: string[] = [];
    const steps: string[] = [];
    const cp = fakeControlPlane({
      ensureReviewRecord: async () => {
        calls.push("ensureReviewRecord");
        return { reviewId: "review-1", taskId: "task-1" };
      },
      createFinderSession: async () => {
        calls.push("createFinderSession");
        return { sessionId: "session-1" };
      },
      bootstrapFinderSession: async () => {
        calls.push("bootstrapFinderSession");
        throw new Error("clone failed");
      },
      sendFinderPrompt: async () => {
        calls.push("sendFinderPrompt");
      },
      markReviewFailed: async () => {
        calls.push("markReviewFailed");
      },
    });

    await prReviewWorkflowImpl({
      controlPlane: cp,
      step: async (fn, name) => {
        steps.push(name);
        return fn();
      },
      recv: async () => messages.shift() ?? null,
    });

    expect(calls).toEqual([
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "markReviewFailed",
    ]);
    expect(steps).toEqual([
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "markReviewFailed",
    ]);
  });
});
