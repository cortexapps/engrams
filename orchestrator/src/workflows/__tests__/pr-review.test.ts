import { describe, expect, test } from "bun:test";

import type { ReviewControlPlane } from "../review-control-plane.ts";
import type { ReviewInbox } from "../review-inbox.ts";
import { prReviewWorkflowImpl, type StepRunner } from "../pr-review.ts";

const trigger: ReviewInbox = {
  kind: "trigger",
  reviewId: "review-1",
  taskId: "task-1",
  repo: "openai/engrams",
  prNumber: 100,
  trigger: "opened",
  headSha: "head-sha",
  baseSha: "base-sha",
};

function fakeControlPlane(
  overrides: Partial<ReviewControlPlane> = {},
): ReviewControlPlane {
  return {
    resolvePrHeads: async () => {
      throw new Error("pass workflow must not resolve pull requests");
    },
    resolveReviewTarget: async () => {
      throw new Error("pass workflow must not resolve targets");
    },
    createReviewPass: async () => {
      throw new Error("pass workflow must not create review rows");
    },
    updateReviewPassContext: async () => {
      throw new Error("pass workflow must not update ingress context");
    },
    startReviewPass: async () => {
      throw new Error("pass workflow must not dispatch itself");
    },
    abandonIngress: async () => {
      throw new Error("pass workflow must not abandon ingress");
    },
    acknowledgeReviewPass: async () => {
      throw new Error("pass workflow must not acknowledge ingress passes");
    },
    signalSupersededPass: async () => {
      throw new Error("pass workflow must not signal predecessors");
    },
    createFinderSession: async () => ({ sessionId: "finder-session" }),
    bootstrapFinderSession: async () => {},
    sendFinderPrompt: async () => {},
    // One candidate by default → the finder hands off to a verifier.
    concludeFinderPhase: async () => ({ candidateCount: 1 }),
    createVerifierSession: async () => ({ sessionId: "verifier-session" }),
    bootstrapVerifierSession: async () => {},
    sendVerifierPrompt: async () => {},
    postReviewResults: async () => {},
    failReview: async () => {},
    haltReview: async () => {},
    cleanupSupersededReview: async () => {},
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
  test("dedupes completion signals and advances finder → verifier → post", async () => {
    const calls: Array<{ name: string; input?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      async createFinderSession(input) {
        calls.push({ name: "createFinderSession", input });
        return { sessionId: "finder-session" };
      },
      async concludeFinderPhase(reviewId, opts) {
        calls.push({ name: "concludeFinderPhase", input: { reviewId, ...opts } });
        return { candidateCount: 1 };
      },
      async createVerifierSession(input) {
        calls.push({ name: "createVerifierSession", input });
        return { sessionId: "verifier-session" };
      },
      async postReviewResults(reviewId, opts) {
        calls.push({ name: "postReviewResults", input: { reviewId, ...opts } });
      },
    });

    await run(cp, [
      trigger,
      { kind: "phase_done", role: "finder" },
      // A duplicate finder completion must be ignored.
      { kind: "session_idle", role: "finder" },
      { kind: "phase_done", role: "verifier" },
    ], steps.step);

    expect(steps.steps).toEqual([
      "createFinderSession",
      "bootstrapFinderSession",
      "sendFinderPrompt",
      "concludeFinderPhase",
      "createVerifierSession",
      "bootstrapVerifierSession",
      "sendVerifierPrompt",
      "postReviewResults",
    ]);
    // The finder session is retired via concludeFinderPhase; the verifier
    // session is retired inside postReviewResults.
    expect(calls.find((c) => c.name === "concludeFinderPhase")?.input)
      .toEqual({ reviewId: "review-1", sessionId: "finder-session" });
    expect(calls.find((c) => c.name === "postReviewResults")?.input)
      .toEqual({ reviewId: "review-1", sessionId: "verifier-session" });
    expect(calls.find((c) => c.name === "createFinderSession")?.input)
      .toMatchObject({ workflowId: "review-wf-1" });
    expect(calls.find((c) => c.name === "createVerifierSession")?.input)
      .toMatchObject({ workflowId: "review-wf-1" });
  });

  test("finder completion with zero candidates posts and returns without a verifier", async () => {
    const created: string[] = [];
    const steps = runner();
    const cp = fakeControlPlane({
      concludeFinderPhase: async () => ({ candidateCount: 0 }),
      createVerifierSession: async () => {
        created.push("verifier");
        return { sessionId: "unexpected" };
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-session", outcome: "completed" },
    ], steps.step);

    expect(created).toEqual([]);
    expect(steps.steps).toContain("concludeFinderPhase");
    expect(steps.steps).not.toContain("createVerifierSession");
    expect(steps.steps.at(-1)).toBe("postReviewResults");
  });

  test("a failed finder session fails the review in one step, no retry", async () => {
    let finderCreates = 0;
    const failed: Array<{ reviewId: string; opts?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      createFinderSession: async () => ({ sessionId: `finder-session-${++finderCreates}` }),
      failReview: async (reviewId, opts) => {
        failed.push({ reviewId, opts });
      },
    });

    // A second failure is queued to prove the workflow returns without a retry.
    await run(cp, [
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-session-1", outcome: "failed" },
      { kind: "session_ended", role: "finder", sessionId: "finder-session-2", outcome: "failed" },
    ], steps.step);

    expect(finderCreates).toBe(1);
    expect(failed).toHaveLength(1);
    expect(failed[0]?.opts).toMatchObject({ sessionId: "finder-session-1" });
    expect(steps.steps.at(-1)).toBe("failReview");
    expect(steps.steps).not.toContain("createVerifierSession");
  });

  test("a failed finder run (idle+runFailed) fails, never posting", async () => {
    let failedCount = 0;
    let posted = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      // A zero-candidate conclude would post "no findings" — prove the errored
      // run never reaches conclude/post at all.
      concludeFinderPhase: async () => ({ candidateCount: 0 }),
      postReviewResults: async () => {
        posted++;
      },
      failReview: async () => {
        failedCount++;
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_idle", role: "finder", runFailed: true },
    ], steps.step);

    expect(failedCount).toBe(1);
    expect(posted).toBe(0);
    expect(steps.steps).not.toContain("concludeFinderPhase");
    expect(steps.steps).not.toContain("postReviewResults");
    expect(steps.steps.at(-1)).toBe("failReview");
  });

  test("a clean finder run with zero candidates still posts (idle without runFailed)", async () => {
    let posted = 0;
    let failedCount = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      concludeFinderPhase: async () => ({ candidateCount: 0 }),
      postReviewResults: async () => {
        posted++;
      },
      failReview: async () => {
        failedCount++;
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_idle", role: "finder" },
    ], steps.step);

    expect(posted).toBe(1);
    expect(failedCount).toBe(0);
    expect(steps.steps.at(-1)).toBe("postReviewResults");
  });

  test("verifier completion posts once and returns cleanly", async () => {
    let posted = 0;
    let failedCount = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      postReviewResults: async () => {
        posted++;
      },
      failReview: async () => {
        failedCount++;
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-session", outcome: "completed" },
      { kind: "session_ended", role: "verifier", sessionId: "verifier-session", outcome: "completed" },
    ], steps.step);

    expect(posted).toBe(1);
    expect(failedCount).toBe(0);
    expect(steps.steps.at(-1)).toBe("postReviewResults");
  });

  test("a posting failure fails the review", async () => {
    let failedCount = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      postReviewResults: async () => {
        throw new Error("GitHub unavailable");
      },
      failReview: async () => {
        failedCount++;
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-session", outcome: "completed" },
      { kind: "session_ended", role: "verifier", sessionId: "verifier-session", outcome: "completed" },
    ], steps.step);

    expect(failedCount).toBe(1);
    expect(steps.steps.slice(-2)).toEqual(["postReviewResults", "failReview"]);
  });

  test("a failed verifier session fails the review in one step, no retry", async () => {
    let verifierCreates = 0;
    let failedCount = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      createVerifierSession: async () => ({ sessionId: `verifier-session-${++verifierCreates}` }),
      failReview: async () => {
        failedCount++;
      },
    });

    await run(cp, [
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-session", outcome: "completed" },
      { kind: "session_ended", role: "verifier", sessionId: "verifier-session-1", outcome: "neutral" },
    ], steps.step);

    expect(verifierCreates).toBe(1);
    expect(failedCount).toBe(1);
    expect(steps.steps.at(-1)).toBe("failReview");
  });

  test("stop halts the review in one step", async () => {
    const halted: Array<{ reviewId: string; opts?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      haltReview: async (reviewId, opts) => {
        halted.push({ reviewId, opts });
      },
    });

    await run(cp, [trigger, { kind: "stop" }], steps.step);

    expect(halted).toEqual([
      { reviewId: "review-1", opts: { sessionId: "finder-session" } },
    ]);
    expect(steps.steps.at(-1)).toBe("haltReview");
    expect(steps.steps).not.toContain("deleteReviewSession");
  });

  test("fails the review after a phase receive-window timeout", async () => {
    const failed: Array<{ reviewId: string; opts?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      failReview: async (reviewId, opts) => {
        failed.push({ reviewId, opts });
      },
    });

    await run(cp, [trigger, null], steps.step);

    expect(failed).toHaveLength(1);
    expect(failed[0]?.opts).toMatchObject({ sessionId: "finder-session" });
    expect(steps.steps.at(-1)).toBe("failReview");
  });

  test("fails the review when initial finder setup throws", async () => {
    const failed: Array<{ reviewId: string; opts?: unknown }> = [];
    const steps = runner();
    const cp = fakeControlPlane({
      bootstrapFinderSession: async () => {
        throw new Error("clone failed");
      },
      failReview: async (reviewId, opts) => {
        failed.push({ reviewId, opts });
      },
    });

    await run(cp, [trigger], steps.step);

    expect(failed).toHaveLength(1);
    expect(failed[0]?.opts).toMatchObject({ sessionId: "finder-session" });
    expect(steps.steps).toEqual([
      "createFinderSession",
      "bootstrapFinderSession",
      "failReview",
    ]);
  });

  test("uses the review and task ids ingress put on the trigger", async () => {
    const finderInputs: unknown[] = [];
    const steps = runner();
    const cp = fakeControlPlane({
      async createFinderSession(input) {
        finderInputs.push(input);
        return { sessionId: "finder-session" };
      },
    });

    await run(cp, [trigger, { kind: "stop" }], steps.step);

    expect(finderInputs).toEqual([{
      reviewId: "review-1",
      taskId: "task-1",
      repo: "openai/engrams",
      prNumber: 100,
      workflowId: "review-wf-1",
    }]);
    expect(steps.steps).not.toContain("resolvePrHeads");
    expect(steps.steps).not.toContain("createReviewPass");
  });

  test("supersede tears down the active worker without changing status", async () => {
    const cleaned: Array<{ reviewId: string; opts?: unknown }> = [];
    let halted = 0;
    let failed = 0;
    const steps = runner();
    const cp = fakeControlPlane({
      cleanupSupersededReview: async (reviewId, opts) => {
        cleaned.push({ reviewId, opts });
      },
      haltReview: async () => {
        halted++;
      },
      failReview: async () => {
        failed++;
      },
    });

    await run(cp, [trigger, { kind: "supersede" }], steps.step);

    expect(cleaned).toEqual([{
      reviewId: "review-1",
      opts: { sessionId: "finder-session" },
    }]);
    expect(halted).toBe(0);
    expect(failed).toBe(0);
    expect(steps.steps.at(-1)).toBe("cleanupSupersededReview");
  });
});
