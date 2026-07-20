/** Durable per-PR review workflow shell (ADR 0100). */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { log as rootLog } from "../log.ts";
import type { ReviewControlPlane } from "./review-control-plane.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "./review-inbox.ts";

const log = rootLog.child({ component: "github-review" });
const RECV_TIMEOUT_S = 3_600;
// A worker gets two full durable receive windows (two hours) per attempt. This
// preserves the coarse, low-churn mailbox cadence while bounding a lost-signal
// phase without consulting non-deterministic wall-clock time.
const PHASE_DEADLINE_WINDOWS = 2;

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

export async function prReviewWorkflowImpl(
  deps: PrReviewWorkflowDeps = {},
): Promise<void> {
  const cp = deps.controlPlane ?? requireControlPlane();
  const step: StepRunner = deps.step ?? ((fn, name) => DBOS.runStep(fn, { name }));
  const recv: ReviewReceiver = deps.recv ?? ((topic, timeout) => DBOS.recv(topic, timeout));

  const first = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
  if (first === null || first.kind !== "trigger") return;
  const workflowId = deps.workflowId ?? DBOS.workflowID;
  if (!workflowId) throw new Error("Review workflow ID is unavailable");

  let headSha = first.headSha ?? "";
  let baseSha = "";
  try {
    // The trigger contract does not carry baseSha, so every trigger resolves
    // the PR heads. Preserve an event-provided head SHA and only fill missing
    // values so a later push cannot change the commit this workflow reviews.
    const resolved = await step(
      () => cp.resolvePrHeads(first.repo, first.prNumber),
      "resolvePrHeads",
    );
    if (headSha === "") headSha = resolved.headSha;
    baseSha = resolved.baseSha;
  } catch (err) {
    log.error(
      { repo: first.repo, prNumber: first.prNumber, err },
      "pull request head resolution failed",
    );
    // markReviewFailed is review-ID based, so retain a durable failed record
    // while ensuring no worker can bootstrap against unresolved code.
    const { reviewId } = await step(
      () => cp.ensureReviewRecord({
        repo: first.repo,
        prNumber: first.prNumber,
        headSha,
        baseSha,
        trigger: first.trigger,
      }),
      "ensureReviewRecord",
    );
    await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
    return;
  }

  const { reviewId, taskId } = await step(
    () => cp.ensureReviewRecord({
      repo: first.repo,
      prNumber: first.prNumber,
      headSha,
      baseSha,
      trigger: first.trigger,
    }),
    "ensureReviewRecord",
  );

  let finderSessionId: string | undefined;
  let verifierSessionId: string | undefined;
  const setupFinder = async (): Promise<void> => {
    const { sessionId } = await step(
      () => cp.createFinderSession({
        reviewId,
        taskId,
        repo: first.repo,
        prNumber: first.prNumber,
        workflowId,
      }),
      "createFinderSession",
    );
    finderSessionId = sessionId;
    await step(
      () => cp.bootstrapFinderSession(sessionId, {
        reviewId,
        repo: first.repo,
        headSha,
      }),
      "bootstrapFinderSession",
    );
    await step(
      () => cp.sendFinderPrompt(sessionId, {
        reviewId,
        repo: first.repo,
        prNumber: first.prNumber,
        headSha,
        baseSha,
        ...(first.focus !== undefined ? { focus: first.focus } : {}),
      }),
      "sendFinderPrompt",
    );
  };

  const setupVerifier = async (): Promise<void> => {
    const { sessionId } = await step(
      () => cp.createVerifierSession({
        reviewId,
        taskId,
        repo: first.repo,
        prNumber: first.prNumber,
        workflowId,
      }),
      "createVerifierSession",
    );
    verifierSessionId = sessionId;
    await step(
      () => cp.bootstrapVerifierSession(sessionId, {
        reviewId,
        repo: first.repo,
        headSha,
      }),
      "bootstrapVerifierSession",
    );
    await step(
      () => cp.sendVerifierPrompt(sessionId, {
        reviewId,
        repo: first.repo,
        prNumber: first.prNumber,
        headSha,
        baseSha,
      }),
      "sendVerifierPrompt",
    );
  };

  const deleteWorkerSession = async (
    role: "finder" | "verifier",
  ): Promise<void> => {
    const sessionId = role === "finder" ? finderSessionId : verifierSessionId;
    if (sessionId === undefined) return;
    await step(
      () => cp.deleteReviewSession(sessionId),
      "deleteReviewSession",
    );
    if (role === "finder") finderSessionId = undefined;
    else verifierSessionId = undefined;
  };

  const deleteWorkerSessionBestEffort = async (
    role: "finder" | "verifier",
  ): Promise<void> => {
    try {
      await deleteWorkerSession(role);
    } catch (err) {
      log.error(
        { repo: first.repo, prNumber: first.prNumber, role, err },
        "review worker cleanup failed",
      );
    }
  };

  try {
    await setupFinder();
  } catch (err) {
    log.error(
      { repo: first.repo, prNumber: first.prNumber, err },
      "finder setup failed",
    );
    await deleteWorkerSessionBestEffort("finder");
    await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
    return;
  }

  // These flags are workflow-local on purpose: DBOS replays the same recv
  // history, deterministically rebuilding retries and completion dedup before
  // it executes new work.
  let finderRetried = false;
  let verifierRetried = false;
  let finderDone = false;
  let verifierDone = false;
  let phaseTimeoutWindows = 0;

  // A phase that could not produce a result — the worker died (terminal
  // `session_ended`) or its harness run errored (`session_idle` + `runFailed`)
  // — gets exactly one clean retry, then the review is marked failed. Returns
  // true when the review was failed (the caller must stop the workflow), false
  // when a fresh worker was started (the caller continues the loop).
  const retryPhaseOnceOrFail = async (
    role: "finder" | "verifier",
  ): Promise<boolean> => {
    await deleteWorkerSessionBestEffort(role);
    const alreadyRetried = role === "finder" ? finderRetried : verifierRetried;
    if (alreadyRetried) {
      await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      return true;
    }
    if (role === "finder") finderRetried = true;
    else verifierRetried = true;
    try {
      if (role === "finder") await setupFinder();
      else await setupVerifier();
      phaseTimeoutWindows = 0;
      return false;
    } catch (err) {
      log.error(
        { repo: first.repo, prNumber: first.prNumber, role, err },
        "review phase retry setup failed",
      );
      await deleteWorkerSessionBestEffort(role);
      await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      return true;
    }
  };

  for (;;) {
    const message = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
    if (message === null) {
      phaseTimeoutWindows++;
      if (phaseTimeoutWindows < PHASE_DEADLINE_WINDOWS) continue;
      const role = finderDone ? "verifier" : "finder";
      log.error(
        {
          repo: first.repo,
          prNumber: first.prNumber,
          role,
          deadlineSeconds: RECV_TIMEOUT_S * PHASE_DEADLINE_WINDOWS,
        },
        "review phase deadline expired",
      );
      await deleteWorkerSessionBestEffort(role);
      await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      return;
    }
    if (message.kind === "stop") {
      await deleteWorkerSessionBestEffort(finderDone ? "verifier" : "finder");
      await step(
        () => cp.markReviewHalted(first.repo, first.prNumber),
        "markReviewHalted",
      );
      return;
    }
    // A harness run that ERRORED returns the reusable session to idle exactly
    // like a clean turn. It is NOT a completion — reporting it as one would post
    // a false "no findings" review — so it routes to the retry/fail path below.
    const failedIdle = message.kind === "session_idle" && message.runFailed === true;
    const completionRole = failedIdle
      ? undefined
      : message.kind === "phase_done" || message.kind === "session_idle"
      ? message.role
      : message.kind === "session_ended" && message.outcome === "completed"
      ? message.role
      : undefined;
    if (failedIdle && !finderDone && message.role === "finder") {
      if (await retryPhaseOnceOrFail("finder")) return;
      continue;
    }
    if (failedIdle && finderDone && !verifierDone && message.role === "verifier") {
      if (await retryPhaseOnceOrFail("verifier")) return;
      continue;
    }
    if (
      message.kind === "session_ended"
      && message.outcome === "completed"
      && message.sessionId !== (
        message.role === "finder" ? finderSessionId : verifierSessionId
      )
    ) {
      continue;
    }
    if (completionRole === "finder") {
      if (finderDone) continue;
      finderDone = true;
      try {
        await deleteWorkerSession("finder");
        const detail = await step(
          () => cp.getReview(reviewId),
          "getReviewAfterFinder",
        );
        if (!detail) throw new Error(`review not found: ${reviewId}`);
        const candidateCount = detail.findings.filter(
          (finding) => finding.state === "candidate",
        ).length;
        if (candidateCount === 0) {
          await step(
            () => cp.postReviewResults(reviewId),
            "postReviewResults",
          );
          return;
        }
        await setupVerifier();
        phaseTimeoutWindows = 0;
      } catch (err) {
        log.error(
          { repo: first.repo, prNumber: first.prNumber, err },
          "verifier setup failed",
        );
        await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
        return;
      }
      continue;
    }
    if (completionRole === "verifier") {
      if (!finderDone || verifierDone) continue;
      verifierDone = true;
      try {
        await deleteWorkerSession("verifier");
        await step(
          () => cp.postReviewResults(reviewId),
          "postReviewResults",
        );
      } catch (err) {
        log.error(
          { repo: first.repo, prNumber: first.prNumber, err },
          "review posting failed",
        );
        await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      }
      return;
    }
    if (
      message.kind === "session_ended"
      && message.role === "finder"
      && !finderDone
    ) {
      if (message.sessionId !== finderSessionId) continue;
      if (await retryPhaseOnceOrFail("finder")) return;
      continue;
    }
    if (
      message.kind === "session_ended"
      && message.role === "verifier"
      && !verifierDone
    ) {
      if (message.sessionId !== verifierSessionId) continue;
      if (await retryPhaseOnceOrFail("verifier")) return;
      continue;
    }
    log.info(
      { repo: first.repo, prNumber: first.prNumber, kind: message.kind },
      "ignoring review workflow message that does not match the active phase",
    );
  }
}

export const prReviewWorkflow = DBOS.registerWorkflow(prReviewWorkflowImpl, {
  name: "PrReviewWorkflow",
});
