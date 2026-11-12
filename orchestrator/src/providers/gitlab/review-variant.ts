/** GitLab's variant of the code-review built-in: merge-request review. */

import {
  REVIEW_FACTS,
  REVIEW_FACTS_TYPE,
  REVIEW_HEAD_SHA,
  REVIEW_PR_NUMBER,
  REVIEW_REPO,
  type ReviewVariant,
} from "../../automations/builtins/review-variant.ts";

const PROVIDER = "gitlab";
const WORKSPACE = `/workspace/\${{ ${REVIEW_FACTS}.repo_name }}`;
/** The project and the merge request of an event, the dispatch event too.
 * A comment event names the merge request apart from its own attributes. */
const PROJECT = "${{ event.raw.project.path_with_namespace }}";
const IID = '${{ event.raw | coalesce: "merge_request.iid", "object_attributes.iid" }}';

export const GITLAB_REVIEW: ReviewVariant = {
  provider: PROVIDER,
  key: "gitlab_review",
  name: "Merge request review",
  description:
    "Reviews merge requests with a finder/verifier pair and posts the confirmed findings as GitLab comments.",
  definitionVersion: 1,
  reviewEvents: [
    "merge_request.open",
    "merge_request.reopen",
    "merge_request.update",
    "note.merge_request",
  ],
  closedEvents: ["merge_request.close", "merge_request.merge"],
  facts: { type: REVIEW_FACTS_TYPE, tunable: [], config: { provider: PROVIDER } },
  // The session has the connection's host in GITLAB_HOST, and the egress
  // proxy adds the token to git over HTTPS. The checkout falls back to the
  // merge request's own ref: GitLab keeps `refs/merge-requests/<iid>/head`
  // for a time after the source branch is deleted.
  checkoutCommand:
    `rm -rf ${WORKSPACE} && ` +
    `git clone "https://\${GITLAB_HOST:?}/${REVIEW_REPO}.git" ${WORKSPACE} && ` +
    `(git -C ${WORKSPACE} checkout ${REVIEW_HEAD_SHA} || ` +
    `(git -C ${WORKSPACE} fetch origin +refs/merge-requests/${REVIEW_PR_NUMBER}/head && ` +
    `git -C ${WORKSPACE} checkout ${REVIEW_HEAD_SHA}))`,
  capabilities: ["gitlab:repository:read"],
  // The capability opens the connection's host; a worker reaches nothing else.
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  actions: {
    upsertStatus: "upsert_pr_comment",
    updateStatus: "update_pr_comment",
    postReview: "post_pr_review",
  },
  // `group/project#iid`, the handle the connector declares for every
  // merge-request event.
  instanceKeyTemplate: `${PROJECT}#${IID}`,
  concurrencyKeyTemplate: `${PROJECT}#${IID}`,
  dispatchPayload: ({ repo, prNumber }) => ({
    project: { path_with_namespace: repo },
    object_attributes: { iid: prNumber },
  }),
  defaultMention: () => "@engrams",
};
