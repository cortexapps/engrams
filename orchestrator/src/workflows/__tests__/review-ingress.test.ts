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
    abandonIngress: async () => {},
    acknowledgeReviewPass: async () => {},
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
    idempotencyKey: "delivery-1",
    focus: "auth",
    ...overrides,
  };
}

describe("review ingress workflow id", () => {
  test("the delivery id is the identity, so a redelivery maps onto one execution", () => {
    expect(reviewIngressWorkflowId(commandInput())).toBe("review-ingress:delivery-1");
    expect(reviewIngressWorkflowId(commandInput({ focus: "storage" })))
      .toBe("review-ingress:delivery-1");
  });

  test("an empty key is refused rather than given a derived identity", () => {
    // An empty key reaches DBOS.send as a real message id, and its notifications
    // table conflicts on that id alone — the first empty-key send would silently
    // swallow every later one system-wide. Every entry point owns a real key now,
    // so this can only fire on a programming error, and it must fire loudly.
    expect(() => reviewIngressWorkflowId(commandInput({ idempotencyKey: "" })))
      .toThrow("non-empty idempotency key");
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
    // `abandonIngress` carries the review id, which is what makes the pass fail
    // rather than merely be reported (review-control-plane.test.ts asserts that
    // arm actually reaches failReview).
    const abandoned: Array<{ reviewId?: string; reason: string }> = [];
    let started = 0;
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw new GithubRequestError(
          404,
          "GitHub pull request request failed (404)",
          JSON.stringify({ message: "Not Found" }),
        );
      },
      abandonIngress: async (_source, reason, reviewId) => {
        abandoned.push({ reviewId, reason });
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

    expect(abandoned).toEqual([{
      reviewId: "review-1",
      reason: "GitHub pull request request failed (404)",
    }]);
    expect(started).toBe(0);
  });

  test("exhausted transient retries abandon the early pass instead of orphaning it", async () => {
    // DBOS gives up after a finite number of attempts and throws
    // DBOSMaxStepRetriesError, which is not a GitHub error at all. That used to
    // rethrow straight past the catch, leaving the early retry row active
    // forever: it holds the one-active-per-target index, so automation would
    // dedupe onto a pass that can never run.
    const abandoned: Array<{ reviewId?: string; reason: string }> = [];
    const exhausted = new Error("Step reached maximum retries");
    exhausted.name = "DBOSMaxStepRetriesError";
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw exhausted;
      },
      abandonIngress: async (_source, reason, reviewId) => {
        abandoned.push({ reviewId, reason });
      },
    });

    await expect(reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      targetId: "target-1",
      trigger: "retry",
      idempotencyKey: "retry-3",
    }, { controlPlane: cp, step: directStep })).rejects.toBe(exhausted);

    expect(abandoned).toEqual([{
      reviewId: "review-1",
      reason: "Step reached maximum retries",
    }]);
  });

  test("a revived permanent error still fails the pass after replay", async () => {
    // DBOS records a step error with serializeError and revives it with
    // deserializeError, which returns a plain Error carrying the original's own
    // properties — so `instanceof GithubRequestError` is false on replay while
    // name/status/responseBody survive. Classifying by class made a replayed 404
    // look retryable and rethrow, stranding the row. This is that revived shape.
    const revived = new Error("GitHub pull request request failed (404)");
    revived.name = "GithubRequestError";
    Object.assign(revived, {
      status: 404,
      responseBody: JSON.stringify({ message: "Not Found" }),
    });
    expect(revived instanceof GithubRequestError).toBe(false);

    const abandoned: Array<{ reviewId?: string; reason: string }> = [];
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw revived;
      },
      abandonIngress: async (_source, reason, reviewId) => {
        abandoned.push({ reviewId, reason });
      },
    });

    // Resolves rather than rejects: the revived error is classified permanent, so
    // ingress ends cleanly instead of erroring the workflow.
    await expect(reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      targetId: "target-1",
      trigger: "retry",
      idempotencyKey: "retry-4",
    }, { controlPlane: cp, step: directStep })).resolves.toBeUndefined();

    expect(abandoned).toEqual([{
      reviewId: "review-1",
      reason: "GitHub pull request request failed (404)",
    }]);
  });

  test("the status ack is a separate step from the non-idempotent create", async () => {
    // Creation and acknowledgement must not share a step. beginReviewPass is one
    // non-idempotent transaction and the step allows retries, so DBOS re-invokes
    // the whole callback on any throw — a GitHub ack inside it would turn one
    // transient failure into a second pass.
    const names: string[] = [];
    const step: IngressStepRunner = async (fn, name) => {
      names.push(name);
      return fn();
    };
    let acked: string | undefined;
    const cp = controlPlane({
      acknowledgeReviewPass: async (reviewId) => {
        acked = reviewId;
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "opened",
      idempotencyKey: "delivery-9",
      headSha: "delivery-head",
      baseSha: "delivery-base",
      pr: COMPLETE_PR,
    }, { controlPlane: cp, step });

    expect(acked).toBe("review-1");
    expect(names).toEqual([
      "resolveReviewTarget",
      "createReviewPass",
      "acknowledgeReviewPass",
    ]);
  });

  test("a permanent failure with no pass yet abandons without a review id", async () => {
    const abandoned: Array<{ reviewId?: string; reason: string }> = [];
    let created = 0;
    const cp = controlPlane({
      resolvePrHeads: async () => {
        throw new GithubRequestError(
          404,
          "GitHub pull request request failed (404)",
          JSON.stringify({ message: "Not Found" }),
        );
      },
      createReviewPass: async () => {
        created++;
        throw new Error("must not create a pass for an unidentifiable PR");
      },
      abandonIngress: async (_source, reason, reviewId) => {
        abandoned.push({ reviewId, reason });
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "comment-42",
    }, { controlPlane: cp, step: directStep });

    expect(created).toBe(0);
    expect(abandoned).toEqual([{
      reviewId: undefined,
      reason: "GitHub pull request request failed (404)",
    }]);
  });

  test("a resolved PR with no provider id never reaches the target table", async () => {
    const abandoned: string[] = [];
    let resolvedTargets = 0;
    const cp = controlPlane({
      resolvePrHeads: async () => ({
        headSha: "resolved-head",
        baseSha: "resolved-base",
        pr: { ...COMPLETE_PR, providerId: null },
      }),
      resolveReviewTarget: async () => {
        resolvedTargets++;
        return { targetId: "target-1" };
      },
      abandonIngress: async (_source, reason) => {
        abandoned.push(reason);
      },
    });

    await reviewIngressWorkflowImpl({
      provider: "github",
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "comment-43",
    }, { controlPlane: cp, step: directStep });

    expect(resolvedTargets).toBe(0);
    expect(abandoned).toEqual(["github returned no id for openai/engrams#100"]);
  });

  test("a target id that does not match the retry's own abandons the pass", async () => {
    const abandoned: Array<{ reviewId?: string; reason: string }> = [];
    let started = 0;
    const cp = controlPlane({
      resolveReviewTarget: async () => ({ targetId: "target-other" }),
      abandonIngress: async (_source, reason, reviewId) => {
        abandoned.push({ reviewId, reason });
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
      idempotencyKey: "retry-2",
    }, { controlPlane: cp, step: directStep });

    expect(started).toBe(0);
    expect(abandoned).toEqual([{
      reviewId: "review-1",
      reason: "resolved target target-other, expected target-1",
    }]);
  });
});
