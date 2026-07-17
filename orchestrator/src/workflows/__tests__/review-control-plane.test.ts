import { describe, expect, test } from "bun:test";

import type { ReviewRow } from "../../db/reviews.ts";
import { makeReviewControlPlane } from "../review-control-plane.ts";

const active: ReviewRow = {
  id: "review-1",
  repo: "openai/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "head-sha",
  baseSha: "",
  trigger: "opened",
  status: "queued",
  githubReviewId: null,
  summaryMd: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

describe("ReviewControlPlane", () => {
  test("dedupes against an active review without inserting a task", async () => {
    let taskInserts = 0;
    const cp = makeReviewControlPlane({
      reviews: {
        getActiveReviewForPr: async () => active,
        createReview: async () => { throw new Error("unexpected create"); },
        updateReviewStatus: async () => {},
      },
      insertTask: async () => {
        taskInserts++;
        return "task-new";
      },
    });
    expect(await cp.ensureReviewRecord({
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
      trigger: "command",
    })).toEqual({ reviewId: active.id, taskId: active.taskId });
    expect(taskInserts).toBe(0);
  });

  test("creates a bare task/review and marks an active review halted", async () => {
    let current: ReviewRow | null = null;
    const creates: unknown[] = [];
    const statuses: unknown[] = [];
    const cp = makeReviewControlPlane({
      reviews: {
        getActiveReviewForPr: async () => current,
        createReview: async (input) => {
          creates.push(input);
          current = { ...active, id: "review-new", taskId: input.taskId };
          return "review-new";
        },
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
        },
      },
      insertTask: async () => "task-new",
    });
    expect(await cp.ensureReviewRecord({
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: "new-head",
      baseSha: "",
      trigger: "dispatch",
    })).toEqual({ reviewId: "review-new", taskId: "task-new" });
    expect(creates).toEqual([{
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: "new-head",
      baseSha: "",
      trigger: "dispatch",
      taskId: "task-new",
      status: "queued",
    }]);
    await cp.markReviewHalted(active.repo, active.prNumber);
    expect(statuses).toEqual([["review-new", "halted"]]);
  });
});
