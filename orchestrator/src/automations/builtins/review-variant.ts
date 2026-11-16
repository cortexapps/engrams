/** What a git forge decides about its code-review built-in, and the
 * templates its strings may use. It imports no product module: a forge's
 * provider module reads it while the forge registry loads. */

import type { ProfileNetwork } from "../../db/schema.ts";

/** The event key the dispatch edge admits a run under; no forge delivery
 * carries it. Its payload is the variant's `dispatchPayload`. */
export const REVIEW_DISPATCH_EVENT_KEY = "review.dispatch";

/** The facts step: the forge reads the event, and shared code admits it. */
export const REVIEW_FACTS_TYPE = "review_facts";

export const REVIEW_FACTS = "steps.facts.value";
export const REVIEW_REPO = `\${{ ${REVIEW_FACTS}.repo }}`;
export const REVIEW_PR_NUMBER = `\${{ ${REVIEW_FACTS}.pr_number }}`;
/** The commit a pass reviews: the event's head, or the one `open` resolved. */
export const REVIEW_HEAD_SHA = "${{ steps.open.head_sha }}";

/** What a git forge decides about its code-review built-in. */
export interface ReviewVariant {
  /** The forge's connector provider. */
  provider: string;
  /** The built-in's key. It is stored; it does not change. */
  key: string;
  name: string;
  description: string;
  /** Bump on any change to the definition this variant builds. */
  definitionVersion: number;
  /** The events that start a pass, and the ones that close the pull request. */
  reviewEvents: string[];
  closedEvents: string[];
  /** The payload of the dispatch event for a pull request named by
   * coordinate. The forge's `reviewEvent` and both key templates read it. */
  dispatchPayload(pr: { repo: string; prNumber: number }): Record<string, unknown>;
  /** A shell command that leaves `REVIEW_HEAD_SHA` checked out in
   * `/workspace/<repo_name>`. */
  checkoutCommand: string;
  /** What a worker session may do on the forge, besides the review tools,
   * and what it may reach. */
  capabilities: string[];
  network: ProfileNetwork;
  /** The workstream key and the concurrency key of an event, as templates
   * over `event.raw`. Both name one pull request. */
  instanceKeyTemplate: string;
  concurrencyKeyTemplate: string;
  /** The seeded value of the `mention` input. */
  defaultMention(): string;
  /** The `mention` of a stored row that has none. It is part of the
   * definition, so it does not read the deployment's configuration. */
  mentionOfUnsetRow: string;
}
