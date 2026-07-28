import { describe, expect, test } from "bun:test";

import { GithubRequestError } from "../../reviews/github-review.ts";
import type { PrContext } from "../../reviews/pr-context.ts";
import {
  reviewIngressWorkflowImpl,
  reviewIngressWorkflowId,
  type IngressStepRunner,
  type ReviewIngressInput,
} from "../review-ingress.ts";
import type {
  ReviewControlPlane,
  StartReviewPassInput,
} from "../review-control-plane.ts";

const COMPLETE_PR: PrContext = {
  providerId: "2158810101",
  url: "https://github.com/openai/engrams/pull/100",
  providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
  title: "Review ingress",
  author: "octocat",
  state: "open",
  headBranch: "feature",
  baseBranch: "main",
  additions: 12,
  deletions: 4,
  changedFiles: 2,
};

function controlPlane(
  overrides: Partial<ReviewControlPlane> = {},
): ReviewControlPlane {
  return {
    resolvePrHeads: async () => ({
      headSha: "resolved-head",
      baseSha: "resolved-base",
      pr: COMPLETE_PR,
    }),
    resolveReviewTarget: async () => ({ targetId: "target-1" }),
    createReviewPass: async () => ({
      kind: "created",
      reviewId: "review-1",
      taskId: "task-1",
    }),
    updateReviewPassContext: async () => true,
    startReviewPass: async () => {},
    signalSupersededPass: async () => {},
    createFinderSession: async () => ({ sessionId: "finder-1" }),
    bootstrapFinderSession: async () => {},
    sendFinderPrompt: async () => {},
    concludeFinderPhase: async () => ({ candidateCount: 0 }),
    createVerifierSession: async () => ({ sessionId: "verifier-1" }),
    bootstrapVerifierSession: async () => {},
    sendVerifierPrompt: async () => {},
    postReviewResults: async () => {},
    failReview: async () => {},
    haltReview: async () => {},
    cleanupSupersededReview: async () => {},
    ...overrides,
  };
}

const directStep: IngressStepRunner = async (fn) => fn();

function commandInput(overrides: Partial<ReviewIngressInput> = {}): ReviewIngressInput {
  return {
    provider: "github",
    repo: "openai/engrams",
    prNumber: 100,
    trigger: "command",
    idempotencyKey: "",
    commentId: "42",
    focus: "auth",
    ...overrides,
  };
}

describe("review ingress workflow id", () => {
  test("fallback identity distinguishes comments and focus but dedups redelivery", () => {
    const first = commandInput();
    expect(reviewIngressWorkflowId(first)).toBe(reviewIngressWorkflowId({ ...first }));
    expect(reviewIngressWorkflowId(first)).not.toBe(
      reviewIngressWorkflowId(commandInput({ commentId: "43" })),
    );
    expect(reviewIngressWorkflowId(first)).not.toBe(
      reviewIngressWorkflowId(commandInput({ focus: "storage" })),
    );
  });

  test("GitHub delivery id remains the preferred identity", () => {
    expect(reviewIngressWorkflowId(commandInput({
      idempotencyKey: "delivery-1",
      commentId: "42",
    }))).toBe("review-ingress:delivery-1");
    expect(reviewIngressWorkflowId(commandInput({
      idempotencyKey: "delivery-1",
      commentId: "43",
    }))).toBe("review-ingress:delivery-1");
  });
});

describe("ReviewIngressWorkflow", () => {
  test("a complete webhook payload resolves with no GitHub call", async () => {
    let resolveCalls = 0;
    const starts: StartReviewPassInput[] = [];
    const cp = controlPlane({
      resolvePrHeads: async () => {
        resolveCalls++;
        throw new Error("must not fetch a complete delivery");
      },
      startReviewPass: async (input) => {
        starts.push(input);
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "opened",
      idempotencyKey: "delivery-1",
      headSha: "delivery-head",
      baseSha: "delivery-base",
      pr: COMPLETE_PR,
    }, { controlPlane: cp, step: directStep });

    expect(resolveCalls).toBe(0);
    expect(starts).toEqual([{
      reviewId: "review-1",
      taskId: "task-1",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "opened",
      idempotencyKey: "delivery-1",
      headSha: "delivery-head",
      baseSha: "delivery-base",
    }]);
  });

  test("an incomplete payload resolves all pass facts together", async () => {
    let resolveCalls = 0;
    const passInputs: unknown[] = [];
    const cp = controlPlane({
      resolvePrHeads: async () => {
        resolveCalls++;
        return {
          headSha: "resolved-head",
          baseSha: "resolved-base",
          pr: COMPLETE_PR,
        };
      },
      createReviewPass: async (input) => {
        passInputs.push(input);
        return {
          kind: "created",
          reviewId: "review-1",
          taskId: "task-1",
        };
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "comment-42",
      headSha: "delivery-head",
      pr: COMPLETE_PR,
    }, { controlPlane: cp, step: directStep });

    expect(resolveCalls).toBe(1);
    expect(passInputs[0]).toMatchObject({
      targetId: "target-1",
      headSha: "resolved-head",
      baseSha: "resolved-base",
      headBranch: COMPLETE_PR.headBranch,
      changedFiles: COMPLETE_PR.changedFiles,
    });
  });

  test.each([
    {
      label: "404",
      error: new GithubRequestError(
        404,
        "GitHub pull request request failed (404)",
        JSON.stringify({ message: "Not Found" }),
      ),
      expectedRetry: false,
      returnsNormally: true,
    },
    {
      label: "5xx",
      error: new GithubRequestError(
        503,
        "GitHub pull request request failed (503)",
        JSON.stringify({ message: "unavailable" }),
      ),
      expectedRetry: true,
      returnsNormally: false,
    },
    {
      label: "rate-limit 403",
      error: new GithubRequestError(
        403,
        "GitHub pull request request failed (403)",
        JSON.stringify({ message: "You have exceeded a secondary RATE LIMIT" }),
      ),
      expectedRetry: true,
      returnsNormally: false,
    },
    {
      label: "plain 403",
      error: new GithubRequestError(
        403,
        "GitHub pull request request failed (403)",
        JSON.stringify({ message: "Forbidden" }),
      ),
      expectedRetry: false,
      returnsNormally: true,
    },
  ])("$label uses the durable step retry classifier", async ({
    error,
    expectedRetry,
    returnsNormally,
  }) => {
    const decisions: boolean[] = [];
    const step: IngressStepRunner = async (fn, name, options) => {
      try {
        return await fn();
      } catch (caught) {
        if (name === "resolvePrHeads") {
          decisions.push(options.shouldRetry?.(caught) ?? options.retry);
        }
        throw caught;
      }
    };
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw error;
      },
    });
    const run = reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "comment-42",
    }, { controlPlane: cp, step });

    if (returnsNormally) {
      await expect(run).resolves.toBeUndefined();
    } else {
      await expect(run).rejects.toBe(error);
    }
    expect(decisions).toEqual([expectedRetry]);
  });

  test("same-head automation deduplicates without starting a pass", async () => {
    let starts = 0;
    const cp = controlPlane({
      createReviewPass: async (input) => {
        expect(input.deduplicateSameHead).toBe(true);
        return {
          kind: "deduplicated",
          reviewId: "review-live",
          taskId: "task-live",
        };
      },
      startReviewPass: async () => {
        starts++;
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "synchronize",
      idempotencyKey: "delivery-2",
      headSha: "same-head",
      baseSha: "base",
      pr: COMPLETE_PR,
    }, { controlPlane: cp, step: directStep });

    expect(starts).toBe(0);
  });

  test("a different-head successor signals the superseded pass before starting", async () => {
    const calls: string[] = [];
    const cp = controlPlane({
      createReviewPass: async () => ({
        kind: "created",
        reviewId: "review-new",
        taskId: "task-new",
        supersededReviewId: "review-old",
      }),
      signalSupersededPass: async (reviewId) => {
        calls.push(`supersede:${reviewId}`);
      },
      startReviewPass: async (input) => {
        calls.push(`start:${input.reviewId}`);
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "synchronize",
      idempotencyKey: "delivery-3",
      headSha: "new-head",
      baseSha: "base",
      pr: COMPLETE_PR,
    }, { controlPlane: cp, step: directStep });

    expect(calls).toEqual(["supersede:review-old", "start:review-new"]);
  });

  test("a permanent retry resolution failure leaves its early pass failed", async () => {
    const failed: Array<{ reviewId: string; reason?: string }> = [];
    let started = 0;
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw new GithubRequestError(
          404,
          "GitHub pull request request failed (404)",
          JSON.stringify({ message: "Not Found" }),
        );
      },
      failReview: async (reviewId, opts) => {
        failed.push({ reviewId, reason: opts?.reason });
      },
      startReviewPass: async () => {
        started++;
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      targetId: "target-1",
      trigger: "retry",
      idempotencyKey: "retry-1",
    }, { controlPlane: cp, step: directStep });

    expect(failed).toHaveLength(1);
    expect(failed[0]).toMatchObject({
      reviewId: "review-1",
      reason: "GitHub pull request request failed (404)",
    });
    expect(started).toBe(0);
  });
});
