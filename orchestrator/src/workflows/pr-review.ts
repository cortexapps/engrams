/**
 * Durable per-PR review workflow (ADR 0100).
 *
 * The registered workflow body is a deliberately tiny, stable forwarder. DBOS
 * computes the application version by hashing the *source text of every
 * registered workflow function* (`computeAppVersion` → `origFunction.toString()`).
 * A version bump orphans in-flight reviews on redeploy: a review parked on
 * `recv` under the old hash is not recovered by an executor running the new
 * hash, and the completion `send` piles up unconsumed. So all evolving
 * orchestration — phase setup, message classification, logging, validation —
 * lives in the plain module functions below, which never enter the hash. Same
 * discipline as ToolExecWorkflow and SlackThreadWorkflow.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { log as rootLog } from "../log.ts";
import type { ReviewControlPlane } from "./review-control-plane.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "./review-inbox.ts";

const log = rootLog.child({ component: "github-review" });

// One durable receive window bounds a silent phase (a worker that never
// signals) at two hours, without consulting non-deterministic wall-clock time:
// a full window with no message is the phase deadline.
const RECV_TIMEOUT_S = 7_200;

type Role = "finder" | "verifier";

export type StepRunner = <T>(fn: () => Promise<T>, name: string) => Promise<T>;
export type ReviewReceiver = (
  topic: string,
  timeoutSeconds: number,
) => Promise<ReviewInbox | null>;

export interface PrReviewWorkflowDeps {
  controlPlane?: ReviewControlPlane;
  step?: StepRunner;
  recv?: ReviewReceiver;
  /** Focused test seam; production uses the active DBOS workflow ID. */
  workflowId?: string;
}

let controlPlane: ReviewControlPlane | undefined;

export function setReviewControlPlane(
  cp: ReviewControlPlane | undefined,
): void {
  controlPlane = cp;
}

function requireControlPlane(): ReviewControlPlane {
  if (!controlPlane) throw new Error("review control plane not configured");
  return controlPlane;
}

/**
 * Live review state, threaded through the module functions. `cp`/`step` are the
 * injected seams; the rest is workflow-local state rebuilt deterministically on
 * replay from the checkpointed recv history + step outputs, so it needs no
 * table and no continue-as-new.
 */
interface Review {
  cp: ReviewControlPlane;
  step: StepRunner;
  workflowId: string;
  repo: string;
  prNumber: number;
  trigger: string;
  reviewId: string;
  taskId: string;
  headSha: string;
  baseSha: string;
  focus?: string;
  finderSessionId?: string;
  verifierSessionId?: string;
  finderDone: boolean;
  verifierDone: boolean;
}

/**
 * Resolve the PR heads and create (or adopt) the durable review record.
 * Returns null when head resolution fails — the review is already marked failed
 * so no worker can bootstrap against unresolved code.
 */
async function beginReview(
  cp: ReviewControlPlane,
  step: StepRunner,
  trigger: Extract<ReviewInbox, { kind: "trigger" }>,
  workflowId: string,
): Promise<Review | null> {
  let headSha = trigger.headSha ?? "";
  let baseSha = "";
  try {
    // The trigger contract does not carry baseSha, so every trigger resolves
    // the PR heads. Preserve an event-provided head SHA and only fill missing
    // values so a later push cannot change the commit this workflow reviews.
    const resolved = await step(
      () => cp.resolvePrHeads(trigger.repo, trigger.prNumber),
      "resolvePrHeads",
    );
    if (headSha === "") headSha = resolved.headSha;
    baseSha = resolved.baseSha;
  } catch (err) {
    log.error(
      { repo: trigger.repo, prNumber: trigger.prNumber, err },
      "pull request head resolution failed",
    );
    // markReviewFailed is review-ID based, so retain a durable failed record
    // while ensuring no worker can bootstrap against unresolved code.
    const { reviewId } = await step(
      () => cp.ensureReviewRecord({
        repo: trigger.repo,
        prNumber: trigger.prNumber,
        headSha,
        baseSha,
        trigger: trigger.trigger,
      }),
      "ensureReviewRecord",
    );
    await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
    return null;
  }

  const { reviewId, taskId } = await step(
    () => cp.ensureReviewRecord({
      repo: trigger.repo,
      prNumber: trigger.prNumber,
      headSha,
      baseSha,
      trigger: trigger.trigger,
    }),
    "ensureReviewRecord",
  );

  return {
    cp,
    step,
    workflowId,
    repo: trigger.repo,
    prNumber: trigger.prNumber,
    trigger: trigger.trigger,
    reviewId,
    taskId,
    headSha,
    baseSha,
    ...(trigger.focus !== undefined ? { focus: trigger.focus } : {}),
    finderDone: false,
    verifierDone: false,
  };
}

function activeRole(review: Review): Role {
  return review.finderDone ? "verifier" : "finder";
}

function sessionIdFor(review: Review, role: string): string | undefined {
  return role === "finder" ? review.finderSessionId : review.verifierSessionId;
}

/**
 * Create, bootstrap, and prompt the worker for one phase. Returns true on
 * success; on any failure the worker is torn down, the review is marked failed,
 * and it returns false (the caller must stop the workflow).
 */
async function setupPhase(review: Review, role: Role): Promise<boolean> {
  const { cp, step } = review;
  try {
    if (role === "finder") {
      const { sessionId } = await step(
        () => cp.createFinderSession({
          reviewId: review.reviewId,
          taskId: review.taskId,
          repo: review.repo,
          prNumber: review.prNumber,
          workflowId: review.workflowId,
        }),
        "createFinderSession",
      );
      review.finderSessionId = sessionId;
      await step(
        () => cp.bootstrapFinderSession(sessionId, {
          reviewId: review.reviewId,
          repo: review.repo,
          headSha: review.headSha,
        }),
        "bootstrapFinderSession",
      );
      await step(
        () => cp.sendFinderPrompt(sessionId, {
          reviewId: review.reviewId,
          repo: review.repo,
          prNumber: review.prNumber,
          headSha: review.headSha,
          baseSha: review.baseSha,
          ...(review.focus !== undefined ? { focus: review.focus } : {}),
        }),
        "sendFinderPrompt",
      );
    } else {
      const { sessionId } = await step(
        () => cp.createVerifierSession({
          reviewId: review.reviewId,
          taskId: review.taskId,
          repo: review.repo,
          prNumber: review.prNumber,
          workflowId: review.workflowId,
        }),
        "createVerifierSession",
      );
      review.verifierSessionId = sessionId;
      await step(
        () => cp.bootstrapVerifierSession(sessionId, {
          reviewId: review.reviewId,
          repo: review.repo,
          headSha: review.headSha,
        }),
        "bootstrapVerifierSession",
      );
      await step(
        () => cp.sendVerifierPrompt(sessionId, {
          reviewId: review.reviewId,
          repo: review.repo,
          prNumber: review.prNumber,
          headSha: review.headSha,
          baseSha: review.baseSha,
        }),
        "sendVerifierPrompt",
      );
    }
    return true;
  } catch (err) {
    log.error(
      { repo: review.repo, prNumber: review.prNumber, role, err },
      `${role} setup failed`,
    );
    await teardownWorkerBestEffort(review, role);
    await step(() => cp.markReviewFailed(review.reviewId), "markReviewFailed");
    return false;
  }
}

/** Delete a phase worker's session and clear its id. */
async function teardownWorker(review: Review, role: Role): Promise<void> {
  const sessionId = sessionIdFor(review, role);
  if (sessionId === undefined) return;
  await review.step(
    () => review.cp.deleteReviewSession(sessionId),
    "deleteReviewSession",
  );
  if (role === "finder") review.finderSessionId = undefined;
  else review.verifierSessionId = undefined;
}

async function teardownWorkerBestEffort(
  review: Review,
  role: Role,
): Promise<void> {
  try {
    await teardownWorker(review, role);
  } catch (err) {
    log.error(
      { repo: review.repo, prNumber: review.prNumber, role, err },
      "review worker cleanup failed",
    );
  }
}

/**
 * Handle the finder phase completing: tear down the finder, then post directly
 * when there are no candidates, otherwise start the verifier. Returns true when
 * the workflow should stop (posted, deduped, or failed).
 */
async function completeFinder(review: Review): Promise<boolean> {
  if (review.finderDone) return false;
  review.finderDone = true;
  const { cp, step } = review;
  try {
    await teardownWorker(review, "finder");
    const detail = await step(
      () => cp.getReview(review.reviewId),
      "getReviewAfterFinder",
    );
    if (!detail) throw new Error(`review not found: ${review.reviewId}`);
    const candidateCount = detail.findings.filter(
      (finding) => finding.state === "candidate",
    ).length;
    if (candidateCount === 0) {
      await step(() => cp.postReviewResults(review.reviewId), "postReviewResults");
      return true;
    }
  } catch (err) {
    log.error(
      { repo: review.repo, prNumber: review.prNumber, err },
      "verifier setup failed",
    );
    await step(() => cp.markReviewFailed(review.reviewId), "markReviewFailed");
    return true;
  }
  // setupPhase owns its own failure path (teardown + markReviewFailed), so a
  // false result means the workflow is already settled and must stop.
  return !(await setupPhase(review, "verifier"));
}

/** Handle the verifier phase completing: tear it down and post the results. */
async function completeVerifier(review: Review): Promise<boolean> {
  if (!review.finderDone || review.verifierDone) return false;
  review.verifierDone = true;
  const { cp, step } = review;
  try {
    await teardownWorker(review, "verifier");
    await step(() => cp.postReviewResults(review.reviewId), "postReviewResults");
  } catch (err) {
    log.error(
      { repo: review.repo, prNumber: review.prNumber, err },
      "review posting failed",
    );
    await step(() => cp.markReviewFailed(review.reviewId), "markReviewFailed");
  }
  return true;
}

/**
 * Interpret one mailbox message (or a receive-window timeout) against the active
 * phase. Returns true when the workflow should stop.
 *
 * A phase that cannot produce a result — the worker died (terminal
 * `session_ended`) or its harness run errored (`session_idle` + `runFailed`) —
 * marks the review failed with no in-workflow retry. Re-running a review is an
 * explicit action (the /reviews retry button / dispatch endpoint), which mints
 * a fresh review record + workflow epoch.
 */
async function advanceReview(
  review: Review,
  message: ReviewInbox | null,
): Promise<boolean> {
  const { cp, step, repo, prNumber } = review;
  const role = activeRole(review);

  if (message === null) {
    log.error(
      { repo, prNumber, role, deadlineSeconds: RECV_TIMEOUT_S },
      "review phase deadline expired",
    );
    await teardownWorkerBestEffort(review, role);
    await step(() => cp.markReviewFailed(review.reviewId), "markReviewFailed");
    return true;
  }

  if (message.kind === "stop") {
    await teardownWorkerBestEffort(review, role);
    await step(() => cp.markReviewHalted(repo, prNumber), "markReviewHalted");
    return true;
  }

  // Only role-bearing signals (worker phase events) drive phase transitions;
  // anything else (e.g. an unhandled comment) falls through to "ignore".
  const messageRole = message.kind === "phase_done"
    || message.kind === "session_idle"
    || message.kind === "session_ended"
    ? message.role
    : undefined;
  if (messageRole === undefined) {
    log.info(
      { repo, prNumber, kind: message.kind },
      "ignoring review workflow message that does not match the active phase",
    );
    return false;
  }

  // A harness run that ERRORED returns the reusable session to idle exactly
  // like a clean turn. It is NOT a completion — reporting it as one would post
  // a false "no findings" review — so it counts as a phase failure below.
  const failedIdle = message.kind === "session_idle" && message.runFailed === true;

  const completionRole = !failedIdle
    && (message.kind === "phase_done"
      || message.kind === "session_idle"
      || (message.kind === "session_ended" && message.outcome === "completed"))
    ? messageRole
    : undefined;

  const phaseFailed = messageRole === role
    && (failedIdle
      || (message.kind === "session_ended"
        && message.outcome !== "completed"
        && message.sessionId === sessionIdFor(review, role)));
  if (phaseFailed) {
    await teardownWorkerBestEffort(review, role);
    await step(() => cp.markReviewFailed(review.reviewId), "markReviewFailed");
    return true;
  }

  // A completed terminal from a session other than the active one is a late
  // echo of an already-finished phase — ignore it.
  if (
    message.kind === "session_ended"
    && message.outcome === "completed"
    && message.sessionId !== sessionIdFor(review, message.role)
  ) {
    return false;
  }

  if (completionRole === "finder") return completeFinder(review);
  if (completionRole === "verifier") return completeVerifier(review);

  log.info(
    { repo, prNumber, kind: message.kind },
    "ignoring review workflow message that does not match the active phase",
  );
  return false;
}

/** The full review lifecycle, kept out of the hashed workflow body. */
export async function runPrReview(
  cp: ReviewControlPlane,
  step: StepRunner,
  recv: ReviewReceiver,
  workflowId: string | undefined,
): Promise<void> {
  const first = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
  if (first === null || first.kind !== "trigger") return;
  if (!workflowId) throw new Error("Review workflow ID is unavailable");

  const review = await beginReview(cp, step, first, workflowId);
  if (review === null) return;

  if (!(await setupPhase(review, "finder"))) return;

  for (;;) {
    const message = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
    if (await advanceReview(review, message)) return;
  }
}

export async function prReviewWorkflowImpl(
  deps: PrReviewWorkflowDeps = {},
): Promise<void> {
  await runPrReview(
    deps.controlPlane ?? requireControlPlane(),
    deps.step ?? ((fn, name) => DBOS.runStep(fn, { name })),
    deps.recv ?? ((topic, timeout) => DBOS.recv(topic, timeout)),
    deps.workflowId ?? DBOS.workflowID,
  );
}

export const prReviewWorkflow = DBOS.registerWorkflow(prReviewWorkflowImpl, {
  name: "PrReviewWorkflow",
});
