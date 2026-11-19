/** GitHub's variant of the code-review built-in. */

import {
  REVIEW_FACTS,
  REVIEW_HEAD_SHA,
  REVIEW_PR_NUMBER,
  REVIEW_REPO,
  type ReviewVariant,
} from "../../automations/builtins/review-variant.ts";
import { config } from "../../config.ts";
import { normalizeMentionHandle } from "../../forges/review-command.ts";

/** The seeded `mention` input: the review App's own login
 * (`GITHUB_APP_LOGIN`), else `@engrams`. */
export function defaultMentionHandle(appLogin: string = config.githubAppLogin): string {
  const slug = normalizeMentionHandle(appLogin);
  return slug ? `@${slug}` : "@engrams";
}

const WORKSPACE = `/workspace/\${{ ${REVIEW_FACTS}.repo_name }}`;

export const GITHUB_REVIEW: ReviewVariant = {
  provider: "github",
  key: "pr_review",
  name: "PR review",
  description:
    "Reviews pull requests with a finder/verifier pair and posts the confirmed findings as a GitHub review.",
  definitionVersion: 10,
  reviewEvents: [
    "pull_request.opened",
    "pull_request.synchronize",
    "pull_request.ready_for_review",
    "issue_comment.created",
  ],
  closedEvents: ["pull_request.closed"],
  // The checkout falls back to the pull request's own ref: a merged pull
  // request's branch is deleted, but `refs/pull/<n>/head` stays.
  checkoutCommand:
    `rm -rf ${WORKSPACE} && ` +
    `git clone https://github.com/${REVIEW_REPO}.git ${WORKSPACE} && ` +
    `(git -C ${WORKSPACE} checkout ${REVIEW_HEAD_SHA} || ` +
    `(git -C ${WORKSPACE} fetch origin +refs/pull/${REVIEW_PR_NUMBER}/head && ` +
    `git -C ${WORKSPACE} checkout ${REVIEW_HEAD_SHA}))`,
  capabilities: [`github:contents:read@${REVIEW_REPO}`],
  // The clone and codeload hosts are listed because the connector declares
  // only api.github.com.
  network: {
    default: "deny",
    allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
    allowHostPatterns: [],
  },
  // `owner/repo#number`, the handle the connector declares for every pull
  // request event. A review-command comment names the issue; the dispatch
  // and a retry carry `pull_request.number`.
  instanceKeyTemplate:
    '${{ event.raw.repository.full_name }}#${{ event.raw | coalesce: "pull_request.number", "issue.number" }}',
  // The pull request's `html_url`, on the pull request or on the issue.
  concurrencyKeyTemplate: '${{ event.raw | coalesce: "pull_request.html_url", "issue.html_url" }}',
  dispatchPayload: ({ repo, prNumber }) => ({
    repository: { full_name: repo, name: repo.split("/")[1] ?? "" },
    pull_request: { number: prNumber, html_url: `https://github.com/${repo}/pull/${prNumber}` },
  }),
  defaultMention: defaultMentionHandle,
  mentionOfUnsetRow: "@engrams",
};
