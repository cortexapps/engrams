/** One idempotent dispatch seam for every GitHub review trigger (ADR 0100). */

import { DBOS } from "@dbos-inc/dbos-sdk";

import type { EnrollmentStore } from "../db/enrollments.ts";
import { prReviewWorkflow } from "./pr-review.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "./review-inbox.ts";
import { reviewHash, selectReviewWorkflowId } from "./review-workflow-id.ts";

const TERMINAL_WF = new Set([
  "SUCCESS",
  "ERROR",
  "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
  "CANCELLED",
]);

export interface DispatchReviewInput {
  repo: string;
  prNumber: number;
  trigger: string;
  idempotencyKey: string;
  headSha?: string;
  focus?: string;
  commentBody?: string;
  commentId?: string;
  stop?: boolean;
}

export interface DispatchReviewResult {
  enrolled: boolean;
  workflowId?: string;
  reviewId?: string;
}

export interface DispatchDbosOps {
  getWorkflowStatus(workflowId: string): Promise<{ status: string } | null>;
  startWorkflow(workflowId: string): Promise<void>;
  send(
    workflowId: string,
    message: ReviewInbox,
    topic: string,
    idempotencyKey: string,
  ): Promise<void>;
}

export interface DispatchReviewDeps {
  enrollments: Pick<EnrollmentStore, "get">;
  dbos?: DispatchDbosOps;
}

const defaultDbos: DispatchDbosOps = {
  async getWorkflowStatus(workflowId) {
    const status = await DBOS.getWorkflowStatus(workflowId);
    return status == null ? null : { status: status.status };
  },
  async startWorkflow(workflowId) {
    await DBOS.startWorkflow(prReviewWorkflow, { workflowID: workflowId })();
  },
  async send(workflowId, message, topic, idempotencyKey) {
    await DBOS.send<ReviewInbox>(
      workflowId,
      message,
      topic,
      idempotencyKey,
    );
  },
};

export async function dispatchReview(
  deps: DispatchReviewDeps,
  input: DispatchReviewInput,
): Promise<DispatchReviewResult> {
  if (!(await deps.enrollments.get(input.repo))) return { enrolled: false };
  const dbos = deps.dbos ?? defaultDbos;
  const workflowId = await selectReviewWorkflowId(
    reviewHash(input.repo, input.prNumber),
    async (id) => {
      const status = await dbos.getWorkflowStatus(id);
      return status != null && TERMINAL_WF.has(status.status);
    },
  );

  let message: ReviewInbox;
  if (input.stop) {
    message = { kind: "stop" };
  } else if (input.commentBody != null && input.commentId != null) {
    message = {
      kind: "comment",
      commentId: input.commentId,
      body: input.commentBody,
    };
  } else {
    message = {
      kind: "trigger",
      repo: input.repo,
      prNumber: input.prNumber,
      trigger: input.trigger,
      ...(input.headSha != null ? { headSha: input.headSha } : {}),
      ...(input.focus != null ? { focus: input.focus } : {}),
    };
  }

  await dbos.startWorkflow(workflowId);
  await dbos.send(
    workflowId,
    message,
    REVIEW_TOPIC,
    input.idempotencyKey,
  );
  return { enrolled: true, workflowId };
}
