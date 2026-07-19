import { describe, expect, test } from "bun:test";

import type { ReviewDetail } from "../../db/reviews.ts";
import type { ReviewControlPlane } from "../review-control-plane.ts";
import type { ReviewInbox } from "../review-inbox.ts";
import { prReviewWorkflowImpl, type StepRunner } from "../pr-review.ts";

const candidateDetail: ReviewDetail = {
  review: {
    id: "review-1",
    repo: "openai/engrams",
    prNumber: 100,
    taskId: "task-1",
    headSha: "head-sha",
    baseSha: "",
    trigger: "opened",
    status: "finding",
    githubReviewId: null,
    statusCommentId: null,
    summaryMd: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
  },
  findings: [{
    id: "finding-1",
    reviewId: "review-1",
    path: "src/index.ts",
    startLine: 10,
    endLine: 10,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "high",
    confidence: "high",
    title: "Wrong result",
    bodyMd: "The result is wrong.",
    suggestedFix: null,
    evidence: ["src/index.ts"],
    state: "candidate",
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: "finding-call-1",
    createdAt: new Date(0),
  }],
  verdicts: [],
};

const trigger: ReviewInbox = {
  kind: "trigger",
  repo: "openai/engrams",
  prNumber: 100,
  trigger: "opened",
  headSha: "head-sha",
};

function fakeControlPlane(
  overrides: Partial<ReviewControlPlane> = {},
): ReviewControlPlane {
  return {
    resolvePrHeads: async () => ({ headSha: "resolved-head", baseSha: "resolved-base" }),
    ensureReviewRecord: async () => ({ reviewId: "review-1", taskId: "task-1" }),
    createFinderSession: async () => ({ sessionId: "finder-session" }),
    bootstrapFinderSession: async () => {},
    sendFinderPrompt: async () => {},
    getReview: async () => candidateDetail,
    createVerifierSession: async () => ({ sessionId: "verifier-session" }),
    bootstrapVerifierSession: async () => {},
    sendVerifierPrompt: async () => {},
    deleteReviewSession: async () => {},
    postReviewResults: async () => {},
    markReviewFailed: async () => {},
    markReviewHalted: async () => {},
    ...overrides,
  };
}

function runner() {
  const steps: string[] = [];
  const step: StepRunner = async (fn, name) => {
    steps.push(name);
    return fn();
  };
  return { step, steps };
}

async function run(
  cp: ReviewControlPlane,
  messages: Array<ReviewInbox | null>,
  step: StepRunner,
): Promise<void> {
  await prReviewWorkflowImpl({
    controlPlane: cp,
    step,
    workflowId: "review-wf-1",
    recv: async () => messages.shift() ?? null,
  });
}

describe("PrReviewWorkflow", () => {
  test("dedupes completion signals while deleting each worker and advancing phases", async () => {
    const calls: Array<{ name: string; input?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      async ensureReviewRecord(input) {
        calls.push({ name: "ensureReviewRecord", input });
        return { reviewId: "review-1", taskId: "task-1" };
      },
      async createFinderSession(input) {
        calls.push({ name: "createFinderSession", input });
        return { sessionId: "finder-session" };
      },
      async bootstrapFinderSession(sessionId, input) {
        calls.push({ name: "bootstrapFinderSession", input: { sessionId, ...input } });
      },
      async sendFinderPrompt(sessionId, input) {
        calls.push({ name: "sendFinderPrompt", input: { sessionId, ...input } });
      },
      async deleteReviewSession(sessionId) {
        calls.push({ name: "deleteReviewSession", input: sessionId });
      },
      async getReview(reviewId) {
        calls.push({ name: "getReview", input: reviewId });
        return candidateDetail;
      },
      async createVerifierSession(input) {
        calls.push({ name: "createVerifierSession", input });
        return { sessionId: "verifier-session" };
      },
      async bootstrapVerifierSession(sessionId, input) {
        calls.push({ name: "bootstrapVerifierSession", input: { sessionId, ...input } });
      },
      async sendVerifierPrompt(sessionId, input) {
        calls.push({ name: "sendVerifierPrompt", input: { sessionId, ...input } });
      },
      async postReviewResults(reviewId) {
        calls.push({ name: "postReviewResults", input: reviewId });
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "phase_done",
        role: "finder",
      },
      {
        kind: "session_idle",
        role: "finder",
      },
      {
        kind: "phase_done",
        role: "verifier",
      },
    ], steps.step);

    expect(calls.map((call) => call.name)).toEqual([
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "sendFinderPrompt",
      "deleteReviewSession",
      "getReview",
      "createVerifierSession",
      "bootstrapVerifierSession",
      "sendVerifierPrompt",
      "deleteReviewSession",
      "postReviewResults",
    ]);
    expect(steps.steps).toEqual([
      "resolvePrHeads",
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "sendFinderPrompt",
      "deleteReviewSession",
      "getReviewAfterFinder",
      "createVerifierSession",
      "bootstrapVerifierSession",
      "sendVerifierPrompt",
      "deleteReviewSession",
      "postReviewResults",
    ]);
    expect(calls.find((call) => call.name === "createFinderSession")?.input)
      .toMatchObject({ workflowId: "review-wf-1" });
    expect(calls.find((call) => call.name === "createVerifierSession")?.input)
      .toMatchObject({ workflowId: "review-wf-1" });
  });

  test("finder completion with zero candidates posts and returns without a verifier", async () => {
    const calls: string[] = [];
    const steps = runner();
    const cp = fakeControlPlane({
      createVerifierSession: async () => {
        calls.push("createVerifierSession");
        return { sessionId: "unexpected" };
      },
      getReview: async () => ({ ...candidateDetail, findings: [] }),
      postReviewResults: async () => {
        calls.push("postReviewResults");
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session",
        outcome: "completed",
      },
    ], steps.step);

    expect(calls).toEqual(["postReviewResults"]);
    expect(steps.steps).toContain("getReviewAfterFinder");
    expect(steps.steps).toContain("deleteReviewSession");
    expect(steps.steps).not.toContain("createVerifierSession");
    expect(steps.steps.at(-1)).toBe("postReviewResults");
  });

  test("a failed finder gets one fresh-session retry, then fails the review", async () => {
    let finderCreates = 0;
    let markedFailed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      createFinderSession: async () => ({
        sessionId: `finder-session-${++finderCreates}`,
      }),
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session-1",
        outcome: "failed",
      },
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session-2",
        outcome: "failed",
      },
    ], steps.step);

    expect(finderCreates).toBe(2);
    expect(markedFailed).toBe(1);
    expect(steps.steps.filter((name) => name === "createFinderSession")).toHaveLength(2);
    expect(steps.steps.at(-1)).toBe("markReviewFailed");
  });

  test("verifier completion posts once and returns cleanly", async () => {
    let verifierPrompts = 0;
    let posted = 0;
    let markedFailed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      sendVerifierPrompt: async () => {
        verifierPrompts++;
      },
      postReviewResults: async () => {
        posted++;
      },
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session",
        outcome: "completed",
      },
      {
        kind: "session_ended",
        role: "verifier",
        sessionId: "verifier-session",
        outcome: "completed",
      },
    ], steps.step);

    expect(verifierPrompts).toBe(1);
    expect(posted).toBe(1);
    expect(markedFailed).toBe(0);
    expect(steps.steps.at(-1)).toBe("postReviewResults");
  });

  test("a posting failure marks the review failed", async () => {
    let markedFailed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      postReviewResults: async () => {
        throw new Error("GitHub unavailable");
      },
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session",
        outcome: "completed",
      },
      {
        kind: "session_ended",
        role: "verifier",
        sessionId: "verifier-session",
        outcome: "completed",
      },
    ], steps.step);

    expect(markedFailed).toBe(1);
    expect(steps.steps.slice(-2)).toEqual([
      "postReviewResults",
      "markReviewFailed",
    ]);
  });

  test("a failed verifier gets one fresh-session retry, then fails the review", async () => {
    let verifierCreates = 0;
    let markedFailed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      createVerifierSession: async () => ({
        sessionId: `verifier-session-${++verifierCreates}`,
      }),
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [
      trigger,
      {
        kind: "session_ended",
        role: "finder",
        sessionId: "finder-session",
        outcome: "completed",
      },
      {
        kind: "session_ended",
        role: "verifier",
        sessionId: "verifier-session-1",
        outcome: "neutral",
      },
      {
        kind: "session_ended",
        role: "verifier",
        sessionId: "verifier-session-2",
        outcome: "failed",
      },
    ], steps.step);

    expect(verifierCreates).toBe(2);
    expect(markedFailed).toBe(1);
    expect(steps.steps.filter((name) => name === "createVerifierSession"))
      .toHaveLength(2);
  });

  test("stop marks the active review halted", async () => {
    const halted: Array<[string, number]> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      markReviewHalted: async (repo, prNumber) => {
        halted.push([repo, prNumber]);
      },
    });

    await run(cp, [trigger, { kind: "stop" }], steps.step);

    expect(halted).toEqual([["openai/engrams", 100]]);
    expect(steps.steps).toContain("deleteReviewSession");
    expect(steps.steps.at(-1)).toBe("markReviewHalted");
  });

  test("marks the review failed after two phase receive timeouts", async () => {
    let markedFailed = 0;
    const deleted: string[] = [];
    const steps = runner();
    const cp = fakeControlPlane({
      deleteReviewSession: async (sessionId) => {
        deleted.push(sessionId);
      },
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [trigger, null, null], steps.step);

    expect(deleted).toEqual(["finder-session"]);
    expect(markedFailed).toBe(1);
    expect(steps.steps.slice(-2)).toEqual([
      "deleteReviewSession",
      "markReviewFailed",
    ]);
  });

  test("marks the review failed when initial finder setup throws", async () => {
    let markedFailed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      bootstrapFinderSession: async () => {
        throw new Error("clone failed");
      },
      markReviewFailed: async () => {
        markedFailed++;
      },
    });

    await run(cp, [trigger], steps.step);

    expect(markedFailed).toBe(1);
    expect(steps.steps).toEqual([
      "resolvePrHeads",
      "ensureReviewRecord",
      "createFinderSession",
      "bootstrapFinderSession",
      "deleteReviewSession",
      "markReviewFailed",
    ]);
  });
});
