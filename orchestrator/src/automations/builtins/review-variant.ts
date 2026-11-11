/** What a git forge decides about its code-review built-in, and the
 * templates its strings may use. It imports no product module: a forge's
 * provider module reads it while the forge registry loads. */

import type { ProfileNetwork } from "../../db/schema.ts";
import type { BlockDef } from "../engine/definition.ts";

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
  /** The `facts` step: `value` is null to reject the event, else `admit`,
   * `repo`, `repo_name`, `pr_number`, `pr_url`, `trigger`, `head_sha`,
   * `base_sha`, `pr_context`, `categories` and `instructions`. It accepts
   * the forge's events and the dispatch event for any enrolled repository. */
  facts: Omit<BlockDef, "id">;
  /** The payload of the dispatch event for a pull request named by
   * coordinate. The facts step and both key templates read it. */
  dispatchPayload(pr: { repo: string; prNumber: number; url: string }): Record<string, unknown>;
  /** A shell command that leaves `REVIEW_HEAD_SHA` checked out in
   * `/workspace/<repo_name>`. */
  checkoutCommand: string;
  /** What a worker session may do on the forge, besides the review tools,
   * and what it may reach. */
  capabilities: string[];
  network: ProfileNetwork;
  /** Connector action ids. `upsertStatus` takes `repo`, `number`, `key` and
   * `body` and outputs `commentId`; `updateStatus` takes `repo`,
   * `commentId`, `key` and `body`; `postReview` is the forge's
   * `post_pr_review`. */
  actions: { upsertStatus: string; updateStatus: string; postReview: string };
  /** The workstream key and the concurrency key of an event, as templates
   * over `event.raw`. Both name one pull request. */
  instanceKeyTemplate: string;
  concurrencyKeyTemplate: string;
  /** The seeded value of the `mention` input. */
  defaultMention(): string;
}
