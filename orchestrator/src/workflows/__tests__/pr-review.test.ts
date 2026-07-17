import { describe, expect, test } from "bun:test";

import type { ReviewControlPlane } from "../review-control-plane.ts";
import type { ReviewInbox } from "../review-inbox.ts";
import { prReviewWorkflowImpl, type StepRunner } from "../pr-review.ts";

const STEP: StepRunner = (fn) => fn();

describe("PrReviewWorkflow shell", () => {
  test("records the first trigger once, then halts on stop", async () => {
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
    const ensured: unknown[] = [];
    const halted: unknown[] = [];
    const cp: ReviewControlPlane = {
      async ensureReviewRecord(input) {
        ensured.push(input);
        return { reviewId: "review-1", taskId: "task-1" };
      },
      async markReviewHalted(repo, prNumber) {
        halted.push([repo, prNumber]);
      },
    };

    await prReviewWorkflowImpl({
      controlPlane: cp,
      step: STEP,
      recv: async () => messages.shift() ?? null,
    });

    expect(ensured).toEqual([{
      repo: "openai/engrams",
      prNumber: 100,
      headSha: "head-sha",
      baseSha: "",
      trigger: "opened",
    }]);
    expect(halted).toEqual([["openai/engrams", 100]]);
  });
});
