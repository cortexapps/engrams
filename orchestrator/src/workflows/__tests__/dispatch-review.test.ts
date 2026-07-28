import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../../db/enrollments.ts";
import type { ReviewRow } from "../../db/reviews.ts";
import {
  dispatchReview,
  dispatchReviewPass,
  dispatchReviewSupersede,
  type DispatchDbosOps,
} from "../dispatch-review.ts";
import type { ReviewInbox } from "../review-inbox.ts";

const enrollment: EnrollmentRow = {
  repo: "openai/engrams",
  triggerMode: "manual",
  autofix: "off",
  profileId: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

function activeReview(): ReviewRow {
  return {
    id: "review-row-1",
    targetId: "target-1",
    provider: "github",
    repo: enrollment.repo,
    prNumber: 100,
    taskId: "task-1",
    headSha: "head-sha",
    baseSha: "base-sha",
    trigger: "opened",
    status: "finding",
    githubReviewId: null,
    statusCommentId: null,
    finderSessionId: null,
    verifierSessionId: null,
    summaryMd: null,
    providerId: "2158810101",
    prTitle: "Review me",
    prAuthor: "octocat",
    prState: "open",
    prUrl: "https://github.com/openai/engrams/pull/100",
    headBranch: "feature",
    baseBranch: "main",
    additions: 4,
    deletions: 2,
    changedFiles: 1,
    createdAt: new Date("2026-07-17T00:00:00Z"),
    updatedAt: new Date("2026-07-17T00:01:00Z"),
  };
}

function dbosFixture() {
  const starts: string[] = [];
  const sends: Array<{
    workflowId: string;
    message: ReviewInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const dbos: DispatchDbosOps = {
    async startWorkflow(workflowId) {
      starts.push(workflowId);
    },
    async send(workflowId, message, topic, idempotencyKey) {
      sends.push({ workflowId, message, topic, idempotencyKey });
    },
  };
  return { dbos, starts, sends };
}

describe("dispatchReview", () => {
  test("returns unenrolled without looking up or starting a pass", async () => {
    const fixture = dbosFixture();
    const result = await dispatchReview({
      enrollments: { get: async () => null },
      reviews: {
        getActiveReviewByCoordinate: async () => {
          throw new Error("must not look up a pass");
        },
      },
      dbos: fixture.dbos,
    }, {
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "comment",
      idempotencyKey: "comment-1",
    });

    expect(result).toEqual({ enrolled: false, activePass: false });
    expect(fixture.starts).toEqual([]);
    expect(fixture.sends).toEqual([]);
  });

  test("a comment with no active pass starts and sends nothing", async () => {
    const fixture = dbosFixture();
    const result = await dispatchReview({
      enrollments: { get: async () => enrollment },
      reviews: { getActiveReviewByCoordinate: async () => null },
      dbos: fixture.dbos,
    }, {
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "comment",
      idempotencyKey: "comment-1",
      commentId: "42",
      commentBody: "please focus on auth",
    });

    expect(result).toEqual({ enrolled: true, activePass: false });
    expect(fixture.starts).toEqual([]);
    expect(fixture.sends).toEqual([]);
  });

  test("a comment targets the active review-id workflow without starting it", async () => {
    const fixture = dbosFixture();
    const result = await dispatchReview({
      enrollments: { get: async () => enrollment },
      reviews: { getActiveReviewByCoordinate: async () => activeReview() },
      dbos: fixture.dbos,
    }, {
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "comment",
      idempotencyKey: "comment-1",
      commentId: "42",
      commentBody: "please focus on auth",
    });

    expect(result).toEqual({
      enrolled: true,
      activePass: true,
      workflowId: "review:review-row-1",
      reviewId: "review-row-1",
    });
    expect(fixture.starts).toEqual([]);
    expect(fixture.sends).toEqual([{
      workflowId: "review:review-row-1",
      message: {
        kind: "comment",
        commentId: "42",
        body: "please focus on auth",
      },
      topic: "review",
      idempotencyKey: "comment-1",
    }]);
  });

  test("a stop targets the active review-id workflow", async () => {
    const fixture = dbosFixture();
    await dispatchReview({
      enrollments: { get: async () => enrollment },
      reviews: { getActiveReviewByCoordinate: async () => activeReview() },
      dbos: fixture.dbos,
    }, {
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "stop-1",
      stop: true,
    });

    expect(fixture.sends[0]).toMatchObject({
      workflowId: "review:review-row-1",
      message: { kind: "stop" },
    });
  });
});

describe("resolved pass dispatch", () => {
  test("starts and triggers exactly review:<reviewId>", async () => {
    const fixture = dbosFixture();
    const result = await dispatchReviewPass({
      reviewId: "review-row-2",
      taskId: "task-2",
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "synchronize",
      idempotencyKey: "delivery-2",
      headSha: "head-2",
      baseSha: "base-2",
      focus: "security",
    }, { dbos: fixture.dbos });

    expect(result).toEqual({ workflowId: "review:review-row-2" });
    expect(fixture.starts).toEqual(["review:review-row-2"]);
    expect(fixture.sends).toEqual([{
      workflowId: "review:review-row-2",
      message: {
        kind: "trigger",
        reviewId: "review-row-2",
        taskId: "task-2",
        repo: enrollment.repo,
        prNumber: 100,
        trigger: "synchronize",
        headSha: "head-2",
        baseSha: "base-2",
        focus: "security",
      },
      topic: "review",
      idempotencyKey: "delivery-2",
    }]);
  });

  test("supersede only signals the predecessor", async () => {
    const fixture = dbosFixture();
    await dispatchReviewSupersede(
      "review-row-1",
      "delivery-2:supersede",
      { dbos: fixture.dbos },
    );

    expect(fixture.starts).toEqual([]);
    expect(fixture.sends).toEqual([{
      workflowId: "review:review-row-1",
      message: { kind: "supersede" },
      topic: "review",
      idempotencyKey: "delivery-2:supersede",
    }]);
  });
});
