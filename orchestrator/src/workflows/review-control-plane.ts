/** Durable review-record operations behind the PrReviewWorkflow seam. */

import { getDb } from "../db/client.ts";
import { makeReviewStore, type ReviewStore } from "../db/reviews.ts";
import { task as taskTable } from "../db/schema.ts";

export interface EnsureReviewRecordInput {
  repo: string;
  prNumber: number;
  headSha: string;
  baseSha: string;
  trigger: string;
}

export interface ReviewControlPlane {
  ensureReviewRecord(
    input: EnsureReviewRecordInput,
  ): Promise<{ reviewId: string; taskId: string }>;
  markReviewHalted(repo: string, prNumber: number): Promise<void>;
}

interface ReviewControlPlaneStore extends Pick<
  ReviewStore,
  "createReview" | "getActiveReviewForPr" | "updateReviewStatus"
> {}

export interface ReviewControlPlaneDeps {
  reviews?: ReviewControlPlaneStore;
  db?: ReturnType<typeof getDb>;
  insertTask?: (input: { repo: string; prNumber: number }) => Promise<string>;
}

/** Insert only the automation-owned task row. Review phase sessions attach
 * task_session rows later in the ADR 0100 execution PR. */
export async function insertReviewTask(
  db: ReturnType<typeof getDb>,
  input: { repo: string; prNumber: number },
): Promise<string> {
  const taskId = crypto.randomUUID();
  await db.insert(taskTable).values({
    id: taskId,
    type: "pr_review",
    title: `Review ${input.repo}#${input.prNumber}`,
    status: "working",
    createdByUserId: null,
    source: {
      provider: "github",
      repo: input.repo,
      prNumber: input.prNumber,
    },
  });
  return taskId;
}

export function makeReviewControlPlane(
  deps: ReviewControlPlaneDeps = {},
): ReviewControlPlane {
  const reviews = deps.reviews ?? makeReviewStore(deps.db ?? getDb());
  const insertTask = deps.insertTask ?? ((input) =>
    insertReviewTask(deps.db ?? getDb(), input));

  return {
    async ensureReviewRecord(input) {
      const active = await reviews.getActiveReviewForPr(
        input.repo,
        input.prNumber,
      );
      if (active) return { reviewId: active.id, taskId: active.taskId };

      const taskId = await insertTask({
        repo: input.repo,
        prNumber: input.prNumber,
      });
      const reviewId = await reviews.createReview({
        ...input,
        taskId,
        status: "queued",
      });
      return { reviewId, taskId };
    },

    async markReviewHalted(repo, prNumber) {
      const active = await reviews.getActiveReviewForPr(repo, prNumber);
      if (active) await reviews.updateReviewStatus(active.id, "halted");
    },
  };
}
