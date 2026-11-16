/** What a verified forge delivery does besides starting a review: it keeps
 * the record of a reviewed pull request current, and it runs the `stop`
 * mention command. The event route calls it for every forge. */

import { makeAutomationStore, type AutomationStore } from "../db/automations.ts";
import { makeReviewStore, type ReviewStore, type UpsertReviewTargetInput } from "../db/reviews.ts";
import type { GitForge, ReviewCommentEvent, ReviewEventPullRequest } from "../forges/forge.ts";
import { reviewVariantFor } from "../forges/registry.ts";
import { parseReviewCommand } from "../forges/review-command.ts";
import { log as rootLog } from "../log.ts";
import {
  stopAutomationReview,
  type ReviewCoordinate,
  type StopAutomationReviewResult,
} from "./automation-review.ts";

const log = rootLog.child({ component: "forge-deliveries" });

export interface ForgeDeliveryDeps {
  /** Stop the run of the pull request's active pass. The built-in's own
   * admission filters the stop comment out, so this reaches the run. */
  stopAutomationReview?: (input: ReviewCoordinate) => Promise<StopAutomationReviewResult>;
  /** Refresh a target only when it exists: a pull-request event must not
   * create a record for a pull request that has no review. */
  refreshTarget?: (input: UpsertReviewTargetInput) => Promise<boolean>;
  /** The `mention` input of the forge's code-review built-in. */
  mentionHandle?: (provider: string) => Promise<string>;
}

async function refreshExistingTarget(
  reviews: ReviewStore,
  input: UpsertReviewTargetInput,
): Promise<boolean> {
  const existing = await reviews.getTargetForRefresh(input);
  if (!existing) return false;
  if (existing.providerId === null) {
    await reviews.claimTargetId({
      provider: input.provider,
      providerId: input.providerId,
      repo: input.repo,
      number: input.number,
    });
  }
  await reviews.upsertTarget(input);
  return true;
}

async function builtinMention(
  store: Pick<AutomationStore, "getByBuiltinKey">,
  provider: string,
): Promise<string> {
  const variant = reviewVariantFor(provider);
  const builtin = variant === null ? null : await store.getByBuiltinKey(variant.key);
  const mention = builtin?.inputs["mention"];
  // The same value a run reads: a row with no mention has the variant's.
  return typeof mention === "string" ? mention : (variant?.mentionOfUnsetRow ?? "");
}

export function makeForgeDeliveryHandler(deps: ForgeDeliveryDeps = {}) {
  const stop = deps.stopAutomationReview ?? stopAutomationReview;
  let reviews: ReviewStore | undefined;
  const refreshTarget =
    deps.refreshTarget ??
    ((input) => refreshExistingTarget((reviews ??= makeReviewStore()), input));
  let automations: AutomationStore | undefined;
  const mentionHandle =
    deps.mentionHandle ??
    ((provider) => builtinMention((automations ??= makeAutomationStore()), provider));

  async function refresh(forge: GitForge, pr: ReviewEventPullRequest): Promise<void> {
    const where = { provider: forge.provider, repo: pr.repo, prNumber: pr.number };
    const { context } = pr;
    if (context.providerId === null) {
      log.warn(where, "review target refresh skipped: the event has no pull request id");
      return;
    }
    const refreshed = await refreshTarget({
      provider: forge.provider,
      providerId: context.providerId,
      repo: pr.repo,
      number: pr.number,
      title: context.title,
      author: context.author,
      state: context.state,
      url: context.url,
      providerUpdatedAt: context.providerUpdatedAt,
    });
    log.info({ ...where, refreshed }, "a pull request event refreshed the review target");
  }

  async function command(forge: GitForge, comment: ReviewCommentEvent): Promise<void> {
    const parsed = parseReviewCommand(comment.body, await mentionHandle(forge.provider));
    // The built-in's own admission starts a review from a `review` comment.
    if (parsed === null || parsed.kind === "review") return;
    const where = { provider: forge.provider, repo: comment.pr.repo, prNumber: comment.pr.number };
    if (comment.fromBot || !(await forge.mayRequestReview(comment))) {
      log.info({ ...where, command: parsed.kind }, "a command from this author is ignored");
      return;
    }
    if (parsed.kind === "fix") {
      log.info(where, "the fix command is not supported");
      return;
    }
    log.info({ ...where, ...(await stop(where)) }, "a stop command reached the review run");
  }

  /** `eventKey` and `payload` are the delivery's, as the forge's event
   * source read them. */
  return async function handleForgeDelivery(
    forge: GitForge,
    eventKey: string,
    payload: Record<string, unknown>,
  ): Promise<void> {
    const event = forge.reviewEvent(eventKey, payload);
    if (event === null) return;
    switch (event.kind) {
      case "change":
      case "update":
        return refresh(forge, event.pr);
      case "comment":
        return command(forge, event);
      case "dispatch":
        return;
    }
  };
}
