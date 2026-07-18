/** Durable per-PR review workflow shell (ADR 0100). */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { log as rootLog } from "../log.ts";
import type { ReviewControlPlane } from "./review-control-plane.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "./review-inbox.ts";

const log = rootLog.child({ component: "github-review" });
const RECV_TIMEOUT_S = 3_600;

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

  const { reviewId, taskId } = await step(
    () => cp.ensureReviewRecord({
      repo: first.repo,
      prNumber: first.prNumber,
      headSha: first.headSha ?? "",
      // GitHub comment and dispatch triggers do not carry the merge-base SHA.
      // NOT NULL placeholders are filled after clone in the execution PR.
      baseSha: "",
      trigger: first.trigger,
    }),
    "ensureReviewRecord",
  );

  const headSha = first.headSha ?? "";
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
    await step(
      () => cp.bootstrapFinderSession(sessionId, {
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
        baseSha: "",
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
        baseSha: "",
      }),
      "sendVerifierPrompt",
    );
  };

  try {
    await setupFinder();
  } catch (err) {
    log.error(
      { repo: first.repo, prNumber: first.prNumber, err },
      "finder setup failed",
    );
    await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
    return;
  }

  // These flags are workflow-local on purpose: DBOS replays the same recv
  // history, deterministically rebuilding whether each role spent its one
  // fresh-session retry before it executes new work.
  let finderRetried = false;
  let verifierRetried = false;
  for (;;) {
    const message = await recv(REVIEW_TOPIC, RECV_TIMEOUT_S);
    if (message === null) continue;
    if (message.kind === "stop") {
      await step(
        () => cp.markReviewHalted(first.repo, first.prNumber),
        "markReviewHalted",
      );
      return;
    }
    if (message.kind === "session_ended" && message.role === "finder") {
      if (message.outcome !== "completed") {
        if (finderRetried) {
          await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
          return;
        }
        finderRetried = true;
        try {
          await setupFinder();
        } catch (err) {
          log.error(
            { repo: first.repo, prNumber: first.prNumber, err },
            "finder retry setup failed",
          );
          await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
          return;
        }
        continue;
      }

      try {
        const detail = await step(
          () => cp.getReview(reviewId),
          "getReviewAfterFinder",
        );
        if (!detail) throw new Error(`review not found: ${reviewId}`);
        const candidateCount = detail.findings.filter(
          (finding) => finding.state === "candidate",
        ).length;
        if (candidateCount === 0) {
          // TODO(ADR 0100: post the no-findings summary).
          log.info(
            { repo: first.repo, prNumber: first.prNumber },
            "finder done with no candidates; posting not yet implemented",
          );
          return;
        }
        await setupVerifier();
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
    if (message.kind === "session_ended" && message.role === "verifier") {
      if (message.outcome === "completed") {
        // TODO(ADR 0100: policy gate → post).
        log.info(
          { repo: first.repo, prNumber: first.prNumber },
          "verifier done; posting not yet implemented",
        );
        return;
      }
      if (verifierRetried) {
        await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
        return;
      }
      verifierRetried = true;
      try {
        await setupVerifier();
      } catch (err) {
        log.error(
          { repo: first.repo, prNumber: first.prNumber, err },
          "verifier retry setup failed",
        );
        await step(() => cp.markReviewFailed(reviewId), "markReviewFailed");
        return;
      }
      continue;
    }
    log.info(
      { repo: first.repo, prNumber: first.prNumber, kind: message.kind },
      "github review finder running; later execution phases not yet implemented",
    );
  }
}

export const prReviewWorkflow = DBOS.registerWorkflow(prReviewWorkflowImpl, {
  name: "PrReviewWorkflow",
});
