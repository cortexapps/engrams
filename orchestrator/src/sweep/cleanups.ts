import type { FailedWorkflow, SweepContext } from "./policy.ts";

export async function notifyThread(
  _ctx: SweepContext,
  _wf: FailedWorkflow,
): Promise<void> {
  throw new Error("implemented in T3");
}

export async function failReviewCleanup(
  _ctx: SweepContext,
  _wf: FailedWorkflow,
): Promise<void> {
  throw new Error("implemented in T3");
}
