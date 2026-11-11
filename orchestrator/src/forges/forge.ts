/** The git-forge role: what the product needs from a code host to review a
 * pull request and to link one to the session that opened it. A provider
 * implements `GitForge` under `providers/<name>/` and joins `registry.ts`. */

import type { runIntegrationOp } from "../integrations/run-op.ts";
import type { PrContext } from "./pr-context.ts";

/** A repository path that is safe inside a shell command and a URL path:
 * two or more `/`-separated names. No name is `.` or `..`, and none starts
 * with `-`. It is a charset rule, not a forge's own naming rule. */
export const SAFE_REPO_PATH =
  /^(?!-|\.{1,2}(?:\/|$))[A-Za-z0-9._-]+(?:\/(?!-|\.{1,2}(?:\/|$))[A-Za-z0-9._-]+)+$/;

export interface InlineComment {
  findingId: string;
  path: string;
  line: number;
  side: string;
  startLine?: number;
  body: string;
}

export interface PostReviewInput {
  repo: string;
  prNumber: number;
  commitId: string;
  /** The summary body for the outcome: `true` when the inline comments post,
   * `false` when the forge refuses them and the summary must quote every
   * finding. */
  buildSummary: (inlinePosted: boolean) => string;
  comments: InlineComment[];
}

export interface PostReviewResult {
  /** The forge's id for the posted review. */
  providerReviewId?: string;
  posted: boolean;
  inlinePosted: boolean;
  /** The summary body that was posted. */
  summaryMd: string;
}

/** One inline review comment on a pull request, replies included. */
export interface PrReviewComment {
  id: string;
  /** The parent comment when this one is a reply; null for a root. */
  inReplyToId: string | null;
  authorLogin: string;
  body: string;
  path: string | null;
}

/** The commits a review pass pins itself to, with the pull request as the
 * forge describes it. The SHAs are required; `pr` is best-effort. */
export interface PullRequestHeads {
  headSha: string;
  baseSha: string;
  pr: PrContext;
}

/** A forge response the caller could not use. `status` tells a permanent
 * failure from a transient one. */
export class ForgeRequestError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly responseBody: string,
  ) {
    super(message);
    this.name = "ForgeRequestError";
  }
}

export interface GitForge {
  readonly provider: string;
  /** Repository names compare without case, so handles and trigger scopes
   * for this forge are stored in lower case. */
  readonly caseInsensitiveRepos: boolean;

  pullRequestUrl(repo: string, prNumber: number): string;
  /** The repository and number a pull-request URL names; null when the path
   * is not this forge's pull-request path. The host is not checked. `number`
   * is null when the URL has none. */
  parsePullRequestUrl(url: string): { repo: string; number: number | null } | null;
  /** True when a retry cannot succeed: the pull request is gone or access to
   * it is. */
  isPermanentError(error: unknown): boolean;

  fetchPullRequest(repo: string, prNumber: number): Promise<PullRequestHeads>;
  /** Every inline review comment on the pull request. Read-only. */
  listReviewComments(repo: string, prNumber: number): Promise<PrReviewComment[]>;
  postReview(input: PostReviewInput): Promise<PostReviewResult>;
  /** Whether a review that contains `marker` is on the pull request. */
  hasReviewWithMarker(repo: string, prNumber: number, marker: string): Promise<boolean>;
  /** The newest pull-request comment that contains `marker`, as the forge
   * returned it; null when there is none or the list cannot be read. It does
   * not throw on a forge response. */
  findCommentWithMarker(
    repo: string,
    prNumber: number,
    marker: string,
  ): Promise<Record<string, unknown> | null>;
}

export interface ForgeDeps {
  runIntegrationOp?: typeof runIntegrationOp;
}

export interface GitForgeSpec {
  provider: string;
  /** Stored rows and block configs that name no forge mean this one. */
  legacyDefault?: true;
  make(deps: ForgeDeps): GitForge;
}
