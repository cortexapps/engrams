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

  await step(
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

  // TODO(ADR 0100 execution PR): finder → verifier → policy gate → post
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
    log.info(
      { repo: first.repo, prNumber: first.prNumber, kind: message.kind },
      "github review queued; execution not yet implemented",
    );
  }
}

export const prReviewWorkflow = DBOS.registerWorkflow(prReviewWorkflowImpl, {
  name: "PrReviewWorkflow",
});
