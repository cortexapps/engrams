/** The legacy PR-review workflow's view of the review control plane.
 *
 * The control plane itself is a workflow-agnostic library in
 * ../reviews/control-plane.ts (ADR 0119 phase 4.1): both this DBOS graph and
 * the built-in automation's system blocks drive the same review-record and
 * worker-session operations. This adapter only binds the two seams that are
 * specific to the legacy graph — handing a resolved pass to `PrReviewWorkflow`
 * and signalling a superseded pass — and re-exports the library surface so
 * existing import sites stay stable until the graph is retired (phase 4.7).
 */

import {
  makeReviewControlPlane as makeLibraryControlPlane,
  type ReviewControlPlane,
  type ReviewControlPlaneDeps,
  type StartReviewPassInput,
} from "../reviews/control-plane.ts";

export {
  ReviewSetupError,
  type ReviewControlPlane,
  type ReviewControlPlaneDeps,
  type ReviewIngressSource,
  type ReviewSessionsClient,
  type ResolveReviewTargetInput,
  type StartReviewPassInput,
} from "../reviews/control-plane.ts";

export function makeReviewControlPlane(
  deps: ReviewControlPlaneDeps = {},
): ReviewControlPlane {
  const dispatchPass = deps.dispatchPass ?? (async (input: StartReviewPassInput) => {
    // Imported at call time, not at module load: `dispatch-review` reaches
    // `pr-review`, which reaches this module. A top-level import would close the
    // cycle and leave one of the three partially initialised.
    const { dispatchReviewPass } = await import("./dispatch-review.ts");
    await dispatchReviewPass(input);
  });
  const signalSupersededPass = deps.signalSupersededPass
    ?? (async (reviewId: string, idempotencyKey: string) => {
      const { dispatchReviewSupersede } = await import("./dispatch-review.ts");
      await dispatchReviewSupersede(reviewId, idempotencyKey);
    });
  return makeLibraryControlPlane({ ...deps, dispatchPass, signalSupersededPass });
}
