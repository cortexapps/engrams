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

/** Admission + fact derivation. Returns null to reject (the filter block
 * after it ends the run as `filtered`), else the facts object. Pure, no I/O,
 * runs in the QuickJS cage. */
export const REVIEW_FACTS_SOURCE = `
export default ({ event, inputs, trigger }) => {
  const raw = event.raw ?? {};
  const fullName = raw.repository?.full_name ?? "";
  const repoKey = fullName.toLowerCase();
  const entry = Object.entries(inputs.repos ?? {}).find(([k]) => k.toLowerCase() === repoKey)?.[1];
  if (!entry) return null;
  // No reviewer profile picked yet (the Reviews page's one setup step):
  // nothing can run, so the delivery is filtered rather than failed.
  if (!inputs.profile) return null;

  const key = trigger.event ?? "";
  let mode = null;
  if (key.startsWith("pull_request.")) {
    if (raw.pull_request?.draft === true) return null;
    if (entry.mode !== "auto") return null;
    mode = key.slice("pull_request.".length);
  } else if (key === "issue_comment.created") {
    if (!raw.issue?.pull_request) return null;
    if (raw.sender?.type === "Bot") return null;
    if (!["OWNER", "MEMBER", "COLLABORATOR"].includes(raw.comment?.author_association)) return null;
    const handle = String(inputs.mention ?? "@engrams").replace(/[.*+?^$()|[\\]\\\\{}]/g, "\\\\$&");
    const re = new RegExp("(^|\\\\s)" + handle + "(\\\\[bot\\\\])?\\\\s+review(\\\\s|$)", "im");
    if (!re.test(raw.comment?.body ?? "")) return null;
    mode = "command";
  } else if (key === "review.dispatch") {
    // An explicit dispatch (CI, an operator) is a request, like a comment
    // command: it does not need mode "auto", and it carries no SHAs, so
    // open_review_pass resolves the heads from GitHub.
    mode = "dispatch";
  } else {
    return null;
  }

  const pr = raw.pull_request ?? {};
  const number = pr.number ?? raw.issue?.number ?? 0;
  const str = (v) => (v === undefined || v === null ? "" : String(v));
  return {
    admit: true,
    repo: fullName,
    repo_name: fullName.split("/")[1] ?? "",
    pr_number: number,
    pr_url: str(pr.html_url || raw.issue?.html_url),
    trigger: mode,
    head_sha: str(pr.head?.sha),
    base_sha: str(pr.base?.sha),
    pr_context: {
      providerId: str(pr.id),
      title: str(pr.title),
      author: str(pr.user?.login),
      state: str(pr.state),
      url: str(pr.html_url),
      providerUpdatedAt: str(pr.updated_at),
      headBranch: str(pr.head?.ref),
      baseBranch: str(pr.base?.ref),
      additions: typeof pr.additions === "number" ? pr.additions : null,
      deletions: typeof pr.deletions === "number" ? pr.deletions : null,
      changedFiles: typeof pr.changed_files === "number" ? pr.changed_files : null,
    },
    categories: Array.isArray(inputs.categories) ? inputs.categories : [],
    instructions: str(inputs.instructions),
  };
};
`.trim();

/** The seeded `mention` default: the review App's own login
 * (`GITHUB_APP_LOGIN`, the same handle the legacy route matches), so a
 * fresh deployment answers `@<app> review` out of the box. A deployment
 * without an App login gets the historical placeholder. The org can still
 * edit the input afterwards; the seeder never overwrites an org value. */
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
  definitionVersion: 9,
  reviewEvents: [
    "pull_request.opened",
    "pull_request.synchronize",
    "pull_request.ready_for_review",
    "issue_comment.created",
  ],
  closedEvents: ["pull_request.closed"],
  facts: {
    type: "code",
    tunable: [],
    config: { source: REVIEW_FACTS_SOURCE, mode: "value" },
  },
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
  actions: {
    upsertStatus: "upsert_issue_comment",
    updateStatus: "update_pinned_comment",
    postReview: "post_pr_review",
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
};
