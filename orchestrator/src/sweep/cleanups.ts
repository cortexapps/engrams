import { z } from "zod";

import type { FailedWorkflow, SweepContext } from "./policy.ts";

const threadSourceSchema = z.object({
  team: z.string().min(1),
  channel: z.string().min(1),
  threadRoot: z.string().min(1),
});

/**
 * Warn the owner of a terminally failed Slack thread workflow.
 *
 * This callback is at-least-once: a crash after Slack accepts the message but
 * before the ledger records completion can post the note twice.
 */
export async function notifyThread(
  ctx: SweepContext,
  wf: FailedWorkflow,
): Promise<void> {
  const source = await ctx.lookups.threadSourceForWorkflow(wf.workflowUuid);
  if (source === null) {
    ctx.log.info(
      { workflowUuid: wf.workflowUuid },
      "terminal Slack workflow never had a session; no thread to notify",
    );
    return;
  }

  const parsed = threadSourceSchema.safeParse(source);
  if (!parsed.success) {
    ctx.log.warn(
      {
        workflowUuid: wf.workflowUuid,
        source,
        error: parsed.error,
      },
      "terminal Slack workflow has a malformed thread source",
    );
    return;
  }

  await (await ctx.slack()).chat.postMessage({
    channel: parsed.data.channel,
    thread_ts: parsed.data.threadRoot,
    text:
      "This conversation hit a snag after a redeploy. Please start a fresh thread—new messages here will no longer reach the agent.",
  });
}

/**
 * Fail the durable review record associated with a terminal workflow.
 *
 * The callback is at-least-once. The active-status guard makes a repeat after
 * failReview completes a no-op, while RetryReview remains the recovery path.
 */
export async function failReviewCleanup(
  ctx: SweepContext,
  wf: FailedWorkflow,
): Promise<void> {
  const review = await ctx.lookups.reviewForWorkflow(wf.workflowUuid);
  if (review === null) {
    ctx.log.info(
      { workflowUuid: wf.workflowUuid },
      "terminal review workflow has no linked review",
    );
    return;
  }

  if (!["queued", "finding", "verifying"].includes(review.status)) {
    ctx.log.info(
      {
        workflowUuid: wf.workflowUuid,
        reviewId: review.id,
        status: review.status,
      },
      "linked review is already terminal",
    );
    return;
  }

  await ctx.failReview(review.id, {
    reason:
      `review workflow ${wf.workflowUuid} failed terminally (${wf.status}); ` +
      "swept by orphan sweep",
  });
}
