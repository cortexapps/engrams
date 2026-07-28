/** One idempotent dispatch seam for every GitHub review trigger (ADR 0100). */

import { DBOS } from "@dbos-inc/dbos-sdk";

import type { EnrollmentStore } from "../db/enrollments.ts";
import { makeReviewStore, type ReviewStore } from "../db/reviews.ts";
import { prReviewWorkflow } from "./pr-review.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "./review-inbox.ts";

/**
 * A comment or a stop, aimed at whatever review is already running.
 *
 * These need no resolution: they carry their own payload and are only meaningful
 * to a live pass. So they skip ingress entirely and go straight to the mailbox.
 */
export interface DispatchReviewInput {
  provider?: string;
  repo: string;
  prNumber: number;
  trigger: string;
  idempotencyKey: string;
  focus?: string;
  commentBody?: string;
  commentId?: string;
  stop?: boolean;
}

/**
 * A review request that ingress has already resolved (ADR 0100 decision 11).
 * Every identifying field is present, so the review workflow needs no fallback.
 */
export interface DispatchReviewPassInput {
  reviewId: string;
  taskId: string;
  repo: string;
  prNumber: number;
  trigger: string;
  idempotencyKey: string;
  headSha: string;
  baseSha: string;
  focus?: string;
}

export interface DispatchReviewResult {
  enrolled: boolean;
  activePass: boolean;
  workflowId?: string;
  reviewId?: string;
}

export interface DispatchDbosOps {
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
  reviews?: Pick<ReviewStore, "getActiveReviewByCoordinate">;
  dbos?: DispatchDbosOps;
}

const defaultDbos: DispatchDbosOps = {
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
  if (!(await deps.enrollments.get(input.repo))) {
    return { enrolled: false, activePass: false };
  }
  const reviews = deps.reviews ?? makeReviewStore();
  const active = await reviews.getActiveReviewByCoordinate(
    input.provider ?? "github",
    input.repo,
    input.prNumber,
  );
  if (!active) return { enrolled: true, activePass: false };

  const dbos = deps.dbos ?? defaultDbos;
  const workflowId = `review:${active.id}`;

  const message: ReviewInbox = input.stop
    ? { kind: "stop" }
    : {
        kind: "comment",
        commentId: input.commentId ?? "",
        body: input.commentBody ?? "",
      };

  await dbos.send(
    workflowId,
    message,
    REVIEW_TOPIC,
    input.idempotencyKey,
  );
  return {
    enrolled: true,
    activePass: true,
    workflowId,
    reviewId: active.id,
  };
}

/**
 * Send a resolved review request to its one-pass workflow.
 *
 * Called from the ingress workflow's body, so it must be safe to re-run on
 * replay. It is, by construction: the workflow id is derived from the review
 * row's UUID, and the trigger message carries an idempotency key.
 */
export async function dispatchReviewPass(
  input: DispatchReviewPassInput,
  deps: { dbos?: DispatchDbosOps } = {},
): Promise<{ workflowId: string }> {
  const dbos = deps.dbos ?? defaultDbos;
  const workflowId = `review:${input.reviewId}`;
  const message: ReviewInbox = {
    kind: "trigger",
    reviewId: input.reviewId,
    taskId: input.taskId,
    repo: input.repo,
    prNumber: input.prNumber,
    trigger: input.trigger,
    headSha: input.headSha,
    baseSha: input.baseSha,
    ...(input.focus !== undefined ? { focus: input.focus } : {}),
  };
  await dbos.startWorkflow(workflowId);
  await dbos.send(workflowId, message, REVIEW_TOPIC, input.idempotencyKey);
  return { workflowId };
}

/** Tear down a predecessor after ingress has atomically marked it superseded. */
export async function dispatchReviewSupersede(
  reviewId: string,
  idempotencyKey: string,
  deps: { dbos?: DispatchDbosOps } = {},
): Promise<void> {
  const dbos = deps.dbos ?? defaultDbos;
  await dbos.send(
    `review:${reviewId}`,
    { kind: "supersede" },
    REVIEW_TOPIC,
    idempotencyKey,
  );
}
