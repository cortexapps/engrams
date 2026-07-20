/**
 * Durable per-PR review workflow (ADR 0100).
 *
 * The registered function body holds the operation graph on purpose: the recv
 * loop and the ORDER + NAMES of every `step(...)` call live here, inline. DBOS
 * derives the application version from registered workflow function source
 * (`computeAppVersion` → `origFunction.toString()`, which does NOT recurse into
 * module-level helpers), and it replays an in-flight workflow only against code
 * of its own version. Keeping the graph in this function means a change to it
 * rotates the version, so DBOS version-gates replay instead of running a
 * recovered review through a changed graph — which would raise
 * `DBOSUnexpectedStepError` or silently take a wrong branch.
 *
 * The heavy work — session lifecycle, GitHub, persistence — lives behind the
 * injected `ReviewControlPlane`, exactly as ToolExecWorkflow keeps its logic in
 * the functions its steps call. A completed step is memoized on replay, so a
 * control-plane method's internals evolve freely without touching the graph;
 * only the graph itself (this body) is version-sensitive.
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

  // Worker session ids are workflow-local state, rebuilt deterministically on
  // replay from the checkpointed step outputs. The setup/teardown closures are
  // defined here (not hoisted to module scope) so their step sequence is part
  // of this function's source and therefore of the version hash.
  let finderSessionId: string | undefined;
  let verifierSessionId: string | undefined;

  const setupPhase = async (role: Role): Promise<void> => {
    if (role === "finder") {
      const { sessionId } = await step(
        () => cp.createFinderSession({ reviewId, taskId, repo: first.repo, prNumber: first.prNumber, workflowId }),
        "createFinderSession",
      );
      finderSessionId = sessionId;
      await step(
        () => cp.bootstrapFinderSession(sessionId, { reviewId, repo: first.repo, headSha }),
        "bootstrapFinderSession",
      );
      await step(
        () => cp.sendFinderPrompt(sessionId, {
          reviewId, repo: first.repo, prNumber: first.prNumber, headSha, baseSha,
          ...(first.focus !== undefined ? { focus: first.focus } : {}),
        }),
        "sendFinderPrompt",
      );
    } else {
      const { sessionId } = await step(
        () => cp.createVerifierSession({ reviewId, taskId, repo: first.repo, prNumber: first.prNumber, workflowId }),
        "createVerifierSession",
      );
      verifierSessionId = sessionId;
      await step(
        () => cp.bootstrapVerifierSession(sessionId, { reviewId, repo: first.repo, headSha }),
        "bootstrapVerifierSession",
      );
      await step(
        () => cp.sendVerifierPrompt(sessionId, { reviewId, repo: first.repo, prNumber: first.prNumber, headSha, baseSha }),
        "sendVerifierPrompt",
      );
    }
  };

  const sessionIdFor = (role: string): string | undefined =>
    role === "finder" ? finderSessionId : verifierSessionId;

  const teardownWorker = async (role: Role): Promise<void> => {
    const sessionId = sessionIdFor(role);
    if (sessionId === undefined) return;
    await step(() => cp.deleteReviewSession(sessionId), "deleteReviewSession");
    if (role === "finder") finderSessionId = undefined;
    else verifierSessionId = undefined;
  };

  const teardownWorkerBestEffort = async (role: Role): Promise<void> => {
    try {
      await teardownWorker(role);
    } catch (err) {
      log.error(
        { repo: first.repo, prNumber: first.prNumber, role, err },
        "review worker cleanup failed",
      );
    }
  };

  try {
    await setupPhase("finder");
  } catch (err) {
    log.error(
      { repo: first.repo, prNumber: first.prNumber, err },
      "finder setup failed",
    );
    await teardownWorkerBestEffort("finder");
    await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
    return;
  }

  // Workflow-local phase state, replay-deterministic from the recv history.
  let finderDone = false;
  let verifierDone = false;

  for (;;) {
    const message = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
    const activeRole: Role = finderDone ? "verifier" : "finder";

    if (message === null) {
      // The active worker went silent for a full receive window. There is no
      // in-workflow retry: mark failed and stop. Re-running a review is an
      // explicit action (the /reviews retry button / dispatch endpoint), which
      // mints a fresh review record + workflow epoch.
      log.error(
        { repo: first.repo, prNumber: first.prNumber, role: activeRole, deadlineSeconds: RECV_TIMEOUT_S },
        "review phase deadline expired",
      );
      await teardownWorkerBestEffort(activeRole);
      await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      return;
    }

    if (message.kind === "stop") {
      await teardownWorkerBestEffort(activeRole);
      await step(() => cp.markReviewHalted(first.repo, first.prNumber), "markReviewHalted");
      return;
    }

    // Only role-bearing worker events drive phase transitions; anything else
    // (e.g. an unhandled comment) is ignored.
    const messageRole = message.kind === "phase_done"
      || message.kind === "session_idle"
      || message.kind === "session_ended"
      ? message.role
      : undefined;
    if (messageRole === undefined) {
      log.info(
        { repo: first.repo, prNumber: first.prNumber, kind: message.kind },
        "ignoring review workflow message that does not match the active phase",
      );
      continue;
    }

    // A harness run that ERRORED returns the reusable session to idle exactly
    // like a clean turn. It is NOT a completion — reporting it as one would post
    // a false "no findings" review — so it counts as a phase failure.
    const failedIdle = message.kind === "session_idle" && message.runFailed === true;

    // Failure of the active phase (errored run, or the worker session died
    // without completing) → mark failed, no retry.
    const phaseFailed = messageRole === activeRole
      && (failedIdle
        || (message.kind === "session_ended"
          && message.outcome !== "completed"
          && message.sessionId === sessionIdFor(activeRole)));
    if (phaseFailed) {
      await teardownWorkerBestEffort(activeRole);
      await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      return;
    }

    const completionRole = !failedIdle
      && (message.kind === "phase_done"
        || message.kind === "session_idle"
        || (message.kind === "session_ended" && message.outcome === "completed"))
      ? messageRole
      : undefined;

    // A completed terminal from a session other than the active one is a late
    // echo of an already-finished phase — ignore it.
    if (
      message.kind === "session_ended"
      && message.outcome === "completed"
      && message.sessionId !== sessionIdFor(message.role)
    ) {
      continue;
    }

    if (completionRole === "finder") {
      if (finderDone) continue;
      finderDone = true;
      try {
        await teardownWorker("finder");
        const detail = await step(() => cp.getReview(reviewId), "getReviewAfterFinder");
        if (!detail) throw new Error(`review not found: ${reviewId}`);
        const candidateCount = detail.findings.filter(
          (finding) => finding.state === "candidate",
        ).length;
        if (candidateCount === 0) {
          await step(() => cp.postReviewResults(reviewId), "postReviewResults");
          return;
        }
        await setupPhase("verifier");
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
        await teardownWorker("verifier");
        await step(() => cp.postReviewResults(reviewId), "postReviewResults");
      } catch (err) {
        log.error(
          { repo: first.repo, prNumber: first.prNumber, err },
          "review posting failed",
        );
        await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
      }
      return;
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
