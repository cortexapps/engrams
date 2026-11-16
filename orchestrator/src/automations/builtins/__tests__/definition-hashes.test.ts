/** The shipped built-in definitions keep their content hash.
 *
 * A changed hash makes the boot seed insert a new version and stop every
 * active run of the old one. Change a hash here only when that is intended.
 */

import { expect, test } from "bun:test";

import { reviewDefinition } from "../review.ts";
import { GITHUB_REVIEW } from "../../../providers/github/review-variant.ts";
import { definitionContentHash } from "../seed.ts";
import { chatThreadsDefinition } from "../chat-threads.ts";
import { SLACK_THREADS } from "../../../providers/slack/threads-variant.ts";

const SLACK_BRAIN_DEFINITION = chatThreadsDefinition(SLACK_THREADS);

const PR_REVIEW_DEFINITION = reviewDefinition(GITHUB_REVIEW);

test("the PR review definition hash is unchanged", () => {
  expect(definitionContentHash(PR_REVIEW_DEFINITION)).toBe("942e1fca24480cf8a998905e917fd088908a868f030be747b07365a067d66baa");
});

test("the Slack threads definition hash is unchanged", () => {
  expect(definitionContentHash(SLACK_BRAIN_DEFINITION)).toBe("149b38fdd5b2065dff989f54201ead291522add58dd209ed446ef753f7cfb719");
});
