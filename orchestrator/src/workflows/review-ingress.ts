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
 *
 * The corollary is that everything which is NOT the graph must stay out. Log
 * calls in particular: a reworded message is not a behaviour change, but hashed
 * inline it rotates the version all the same and strands in-flight executions
 * (ADR 0104). So the body below contains no logging. Each control-plane method
 * reports its own entry and outcome, and every give-up path funnels through the
 * one `abandonIngress` step, which owns both arms and their logging.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { isPermanentGithubFailure } from "../reviews/github-review.ts";
import { isCompletePrContext, type PrContext } from "../reviews/pr-context.ts";
import { isHumanReviewTrigger } from "../reviews/review-trigger.ts";
import type { ReviewControlPlane } from "./review-control-plane.ts";

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

  const source = {
    provider: input.provider,
    repo: input.repo,
    prNumber: input.prNumber,
    trigger: input.trigger,
  };
  let pass: { reviewId: string; taskId: string } | undefined;

  /** The one give-up path. Reads `pass` at call time, so it fails the row when
   *  ingress had already created one and reports when it had not. */
  const abandon = (reason: string): Promise<void> =>
    step(
      () => cp.abandonIngress(source, reason, pass?.reviewId),
      "abandonIngress",
      { retry: true },
    );

  /** Create this request's pass row. False means an equivalent pass already
   *  runs, so there is nothing left to do. */
  const createPass = async (
    targetId: string,
    facts: { headSha: string; baseSha: string; pr?: PrContext },
  ): Promise<boolean> => {
    const result = await step(
      () => cp.createReviewPass({
        ...source,
        targetId,
        headSha: facts.headSha,
        baseSha: facts.baseSha,
        headBranch: facts.pr?.headBranch ?? null,
        baseBranch: facts.pr?.baseBranch ?? null,
        additions: facts.pr?.additions ?? null,
        deletions: facts.pr?.deletions ?? null,
        changedFiles: facts.pr?.changedFiles ?? null,
        deduplicateSameHead: !isHumanReviewTrigger(input.trigger),
      }),
      "createReviewPass",
      { retry: true },
    );
    if (result.kind === "deduplicated") return false;
    pass = { reviewId: result.reviewId, taskId: result.taskId };
    // Its own step, deliberately. Inside createReviewPass this GitHub call could
    // throw after the row was committed, and DBOS would re-invoke that whole
    // callback — creating a second pass. Out here it cannot reach the transaction.
    await step(
      () => cp.acknowledgeReviewPass(result.reviewId),
      "acknowledgeReviewPass",
      { retry: true },
    );
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
  const expectedTargetId = input.targetId;
  if (expectedTargetId !== undefined
    && !(await createPass(expectedTargetId, { headSha: "", baseSha: "" }))) {
    return;
  }

  // A webhook delivery already carries both SHAs and the whole pull request, so
  // there is nothing left to ask GitHub. Anything short of complete resolves the
  // slow way. The gate is on the DATA, not on a list of webhook actions we assume
  // are complete: a wrong assumption there would silently persist nulls.
  //
  // Resolving as one value (rather than reassigning three variables) is what
  // guarantees every per-pass fact describes the SAME commit as the head we pin.
  const { headSha: sentHead, baseSha: sentBase, pr: sentPr } = input;
  let resolved: { headSha: string; baseSha: string; pr: PrContext };
  if (sentHead && sentBase && sentPr && isCompletePrContext(sentPr)) {
    resolved = { headSha: sentHead, baseSha: sentBase, pr: sentPr };
  } else {
    try {
      resolved = await step(
        () => cp.resolvePrHeads(input.repo, input.prNumber),
        "resolvePrHeads",
        // Retry a busy or broken GitHub. Do NOT retry a pull request that is gone,
        // transferred away, or invisible to our installation — that answer will
        // not change, and the review cannot proceed without it.
        { retry: true, shouldRetry: (error) => !isPermanentGithubFailure(error) },
      );
    } catch (error) {
      // Abandon on EVERY failure, not only a permanent one. The pass cannot
      // proceed without the SHAs, and an early retry row that outlives this step
      // is worse than a failed one: it stays active, holds the
      // one-active-per-target index, and automation deduplicates onto a pass that
      // will never run. Exhausting the step's retry budget throws
      // DBOSMaxStepRetriesError, which is not a GitHub failure at all — that used
      // to rethrow straight past this and orphan the row.
      await abandon(error instanceof Error ? error.message : String(error));
      // A permanent answer is an ordinary outcome, so end cleanly. Anything else
      // means GitHub or we are broken, so let the workflow end ERROR where an
      // operator can see it rather than looking like a normal no-op.
      if (!isPermanentGithubFailure(error)) throw error;
      return;
    }
  }
  const { headSha, baseSha, pr } = resolved;

  // The forge answered but gave no id. Nothing downstream can key on identity,
  // so refuse rather than write a row that cannot be reconciled later.
  const providerId = pr.providerId;
  if (providerId === null) {
    await abandon(`${input.provider} returned no id for ${input.repo}#${input.prNumber}`);
    return;
  }

  const target = await step(
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
  if (expectedTargetId !== undefined && target.targetId !== expectedTargetId) {
    await abandon(`resolved target ${target.targetId}, expected ${expectedTargetId}`);
    return;
  }

  // The retry row was created before GitHub answered, so its pass facts are
  // still blank; fill them now. Every other entry point creates the row here,
  // already complete.
  const early = pass;
  if (early) {
    const filled = await step(
      () => cp.updateReviewPassContext(early.reviewId, {
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
    if (!filled) return;
  } else if (!(await createPass(target.targetId, { headSha, baseSha, pr }))) {
    return;
  }
  const ready = pass;
  if (!ready) throw new Error("review ingress created no pass");

  // Only now does the pass workflow start, with a row and immutable workflow id
  // that ingress already chose.
  //
  // Called from the workflow body, NOT from inside a step: this starts a child
  // workflow and sends it a message, and DBOS wants both from a workflow context.
  // Re-running it on replay is safe by construction — the review workflow id is
  // derived, not minted, and the trigger message carries an idempotency key.
  await cp.startReviewPass({
    reviewId: ready.reviewId,
    taskId: ready.taskId,
    repo: input.repo,
    prNumber: input.prNumber,
    trigger: input.trigger,
    idempotencyKey: input.idempotencyKey,
    headSha,
    baseSha,
    ...(input.focus !== undefined ? { focus: input.focus } : {}),
  });
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
 * maps onto the same execution instead of resolving and reviewing twice.
 *
 * There is no fallback for a missing key, on purpose. Every entry point now owns
 * a real one: the webhook route refuses a delivery with no delivery id, and the
 * API and retry paths mint a uuid. An empty key must never reach DBOS, because
 * its notifications table conflicts on the message id alone.
 */
export function reviewIngressWorkflowId(input: ReviewIngressInput): string {
  if (!input.idempotencyKey) {
    throw new Error("review ingress requires a non-empty idempotency key");
  }
  // Not encoded: `rpc/reviews.ts` and `routes/reviews-dispatch.ts` report this id
  // back to their callers by building the same string, so the two must agree.
  return `review-ingress:${input.idempotencyKey}`;
}

export async function startReviewIngress(input: ReviewIngressInput): Promise<void> {
  await DBOS.startWorkflow(reviewIngressWorkflow, {
    workflowID: reviewIngressWorkflowId(input),
  })(input);
}
