/**
 * Durable review ingress (ADR 0100 decision 11).
 *
 * Every way of asking for a review lands here first: a webhook delivery, an
 * `@mention` command, or an API dispatch. Ingress does one job — turn a request
 * into a fully identified change — and only then starts the review.
 *
 * Why this is a workflow and not a function call. Resolving the pull request
 * needs the network for two of the three entry points. When that resolution lived
 * inside the review workflow, a GitHub blip did not delay a review, it FAILED
 * one, and it left behind a review record that could not name what it was about.
 * A durable step retries instead. So the review workflow can now assume its input
 * is complete, and the whole "review with no name" state stops existing rather
 * than being papered over.
 *
 * As with `prReviewWorkflowImpl`, the operation graph lives inline in the
 * registered function body on purpose: DBOS derives the application version from
 * that source, so a change to the graph rotates the version and version-gates
 * replay instead of running a recovered execution through changed code.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { log as rootLog } from "../log.ts";
import { isPermanentGithubFailure } from "../reviews/github-review.ts";
import { isCompletePrContext, type PrContext } from "../reviews/pr-context.ts";
import { isHumanReviewTrigger } from "../reviews/review-trigger.ts";
import type { ReviewControlPlane } from "./review-control-plane.ts";

const log = rootLog.child({ component: "review-ingress" });

export interface ReviewIngressInput {
  /** Which forge this came from. `github` today; the column is not GitHub-shaped. */
  provider: string;
  repo: string;
  prNumber: number;
  trigger: string;
  /** Dedups the downstream trigger message; a redelivered webhook reuses it. */
  idempotencyKey: string;
  /** A retry already knows its dossier target. That lets ingress create the
   *  queued pass before a possibly-permanent GitHub resolution failure. */
  targetId?: string;
  /** Present on a webhook delivery, which describes the change in full. */
  headSha?: string;
  baseSha?: string;
  pr?: PrContext;
  /** The GitHub comment that carried a command. Part of fallback identity when
   *  a delivery id is absent, so two distinct comments never collapse. */
  commentId?: string;
  focus?: string;
}

export type IngressStepRunner = <T>(
  fn: () => Promise<T>,
  name: string,
  options: { retry: boolean; shouldRetry?: (error: unknown) => boolean },
) => Promise<T>;

export interface ReviewIngressDeps {
  controlPlane?: ReviewControlPlane;
  step?: IngressStepRunner;
}

let controlPlane: ReviewControlPlane | undefined;

export function setReviewIngressControlPlane(cp: ReviewControlPlane | undefined): void {
  controlPlane = cp;
}

function requireControlPlane(): ReviewControlPlane {
  if (!controlPlane) throw new Error("review control plane not configured");
  return controlPlane;
}

export async function reviewIngressWorkflowImpl(
  input: ReviewIngressInput,
  deps: ReviewIngressDeps = {},
): Promise<void> {
  const cp = deps.controlPlane ?? requireControlPlane();
  const step: IngressStepRunner = deps.step
    ?? ((fn, name, options) =>
      DBOS.runStep(fn, {
        name,
        retriesAllowed: options.retry,
        ...(options.shouldRetry ? { shouldRetry: options.shouldRetry } : {}),
      }));

  let headSha = input.headSha ?? "";
  let baseSha = input.baseSha ?? "";
  let pr = input.pr;
  let targetId = input.targetId;
  let pass:
    | { reviewId: string; taskId: string }
    | undefined;

  const createPass = async (resolvedTargetId: string): Promise<boolean> => {
    const result = await step(
      () => cp.createReviewPass({
        provider: input.provider,
        targetId: resolvedTargetId,
        repo: input.repo,
        prNumber: input.prNumber,
        trigger: input.trigger,
        headSha,
        baseSha,
        headBranch: pr?.headBranch ?? null,
        baseBranch: pr?.baseBranch ?? null,
        additions: pr?.additions ?? null,
        deletions: pr?.deletions ?? null,
        changedFiles: pr?.changedFiles ?? null,
        deduplicateSameHead: !isHumanReviewTrigger(input.trigger),
      }),
      "createReviewPass",
      { retry: true },
    );
    if (result.kind === "deduplicated") {
      log.info(
        {
          repo: input.repo,
          prNumber: input.prNumber,
          trigger: input.trigger,
          reviewId: result.reviewId,
          headSha,
        },
        "review ingress deduplicated onto the active pass",
      );
      return false;
    }

    pass = { reviewId: result.reviewId, taskId: result.taskId };
    if (result.supersededReviewId !== undefined) {
      await cp.signalSupersededPass(
        result.supersededReviewId,
        `${input.idempotencyKey || result.reviewId}:supersede:${result.supersededReviewId}`,
      );
    }
    return true;
  };

  // Retry is the one entry point that already owns a stable target id. Create
  // its queued row before touching GitHub so a permanent resolution failure is
  // a visible failed pass instead of an RPC-success spinner with no record.
  if (targetId !== undefined && !(await createPass(targetId))) return;

  // A webhook delivery already carries both SHAs and the whole pull request, so
  // there is nothing left to ask GitHub. Anything short of complete resolves the
  // slow way. The gate is on the DATA, not on a list of webhook actions we assume
  // are complete: a wrong assumption there would silently persist nulls.
  try {
    if (headSha === "" || baseSha === "" || !pr || !isCompletePrContext(pr)) {
      const resolved = await step(
        () => cp.resolvePrHeads(input.repo, input.prNumber),
        "resolvePrHeads",
        // Retry a busy or broken GitHub. Do NOT retry a pull request that is gone,
        // transferred away, or invisible to our installation — that answer will
        // not change, and the review cannot proceed without it.
        { retry: true, shouldRetry: (error) => !isPermanentGithubFailure(error) },
      );
      // Every per-pass fact must describe the SAME commit as the head we pin. So
      // this takes the resolved head AND the resolved branches and counts
      // together, rather than pinning the delivery's head and describing a later
      // one.
      headSha = resolved.headSha;
      baseSha = resolved.baseSha;
      pr = resolved.pr;
    }
  } catch (error) {
    if (!isPermanentGithubFailure(error)) throw error;
    const reason = error instanceof Error ? error.message : String(error);
    const failedPass = pass;
    if (failedPass) {
      await step(
        () => cp.failReview(failedPass.reviewId, { reason }),
        "failIngressReview",
        { retry: true },
      );
    } else {
      log.error(
        { repo: input.repo, prNumber: input.prNumber, trigger: input.trigger, error },
        "review ingress failed permanently before it could identify a target",
      );
    }
    return;
  }

  if (pr.providerId === null) {
    // The forge answered but gave no id. Nothing downstream can key on identity,
    // so refuse rather than write a row that cannot be reconciled later.
    const reason =
      `review ingress: ${input.provider} returned no id for ${input.repo}#${input.prNumber}`;
    const failedPass = pass;
    if (failedPass) {
      await step(
        () => cp.failReview(failedPass.reviewId, { reason }),
        "failIngressReview",
        { retry: true },
      );
    } else {
      log.error(
        { repo: input.repo, prNumber: input.prNumber, trigger: input.trigger },
        reason,
      );
    }
    return;
  }
  const providerId = pr.providerId;

  const resolvedTarget = await step(
    () => cp.resolveReviewTarget({
      provider: input.provider,
      providerId,
      repo: input.repo,
      number: input.prNumber,
      title: pr.title,
      author: pr.author,
      state: pr.state,
      url: pr.url,
      providerUpdatedAt: pr.providerUpdatedAt,
    }),
    "resolveReviewTarget",
    { retry: true },
  );
  if (targetId !== undefined && resolvedTarget.targetId !== targetId) {
    const reason =
      `review ingress resolved target ${resolvedTarget.targetId}, expected ${targetId}`;
    const mismatchedPass = pass;
    if (!mismatchedPass) {
      throw new Error("review ingress target mismatch had no pass row");
    }
    await step(
      () => cp.failReview(mismatchedPass.reviewId, { reason }),
      "failIngressReview",
      { retry: true },
    );
    return;
  }
  targetId = resolvedTarget.targetId;

  if (!pass) {
    if (!(await createPass(targetId))) return;
  } else {
    const existingPass = pass;
    const updated = await step(
      () => cp.updateReviewPassContext(existingPass.reviewId, {
        headSha,
        baseSha,
        headBranch: pr.headBranch,
        baseBranch: pr.baseBranch,
        additions: pr.additions,
        deletions: pr.deletions,
        changedFiles: pr.changedFiles,
      }),
      "updateReviewPassContext",
      { retry: true },
    );
    if (!updated) return;
  }
  const readyPass = pass;
  if (!readyPass) throw new Error("review ingress created no pass");

  // Only now does the pass workflow start, with a row and immutable workflow id
  // that ingress already chose.
  //
  // Called from the workflow body, NOT from inside a step: this starts a child
  // workflow and sends it a message, and DBOS wants both from a workflow context.
  // Re-running it on replay is safe by construction — the review workflow id is
  // derived, not minted, and the trigger message carries an idempotency key.
  await cp.startReviewPass({
    reviewId: readyPass.reviewId,
    taskId: readyPass.taskId,
    repo: input.repo,
    prNumber: input.prNumber,
    trigger: input.trigger,
    idempotencyKey: input.idempotencyKey,
    headSha,
    baseSha,
    ...(input.focus !== undefined ? { focus: input.focus } : {}),
  });

  log.info(
    {
      repo: input.repo,
      prNumber: input.prNumber,
      trigger: input.trigger,
      targetId,
      reviewId: readyPass.reviewId,
    },
    "review ingress dispatched a pass",
  );
}

export const reviewIngressWorkflow = DBOS.registerWorkflow(reviewIngressWorkflowImpl, {
  name: "ReviewIngressWorkflow",
});

export type ReviewIngressStart = ReviewIngressInput;

/**
 * Start ingress for one review request.
 *
 * The workflow id is derived from the caller's idempotency key — the GitHub
 * delivery id for a webhook, a fresh uuid for a re-run — so a redelivered webhook
 * maps onto the same execution instead of resolving and reviewing twice. The
 * fallback covers a delivery that arrives with no id at all; it is deterministic
 * so it dedups rather than multiplying.
 */
export function reviewIngressWorkflowId(input: ReviewIngressInput): string {
  const fallback = [
    input.provider,
    `${input.repo}#${input.prNumber}`,
    input.trigger,
    input.headSha ?? "",
    input.commentId ?? "",
    input.focus ?? "",
  ].map(encodeURIComponent).join(":");
  const key = input.idempotencyKey || fallback;
  return `review-ingress:${key}`;
}

export async function startReviewIngress(input: ReviewIngressInput): Promise<void> {
  await DBOS.startWorkflow(reviewIngressWorkflow, {
    workflowID: reviewIngressWorkflowId(input),
  })(input);
}
