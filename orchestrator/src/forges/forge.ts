/** The git-forge role: what the product needs from a code host to review a
 * pull request and to link one to the session that opened it. A provider
 * implements `GitForge` under `providers/<name>/` and names it in its
 * provider spec. */

import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
import type { runIntegrationOp } from "../integrations/run-op.ts";
import type { PrContext } from "./pr-context.ts";

/** A repository path that is safe inside a shell command and a URL path:
 * two or more `/`-separated names. No name is `.` or `..`, and none starts
 * with `-`. It is a charset rule, not a forge's own naming rule. */
export const SAFE_REPO_PATH =
  /^(?!-|\.{1,2}(?:\/|$))[A-Za-z0-9._-]+(?:\/(?!-|\.{1,2}(?:\/|$))[A-Za-z0-9._-]+)+$/;

export interface InlineComment {
  /** The finding this comment states. `PostReviewResult.inlineFindingIds`
   * names it when the comment is on the diff. */
  findingId: string;
  path: string;
  line: number;
  /** LEFT is a line of the old file; RIGHT is a line of the new file. */
  side: "LEFT" | "RIGHT";
  startLine?: number;
  body: string;
}

export interface PostReviewInput {
  repo: string;
  prNumber: number;
  commitId: string;
  /** The summary body for the outcome: `true` when every inline comment is
   * on the diff, `false` when the summary must quote every finding. */
  buildSummary: (inlinePosted: boolean) => string;
  /** The text that marks this post; `buildSummary` ends with it. A forge
   * that posts the comments apart from the summary adds it to each. */
  marker: string;
  comments: InlineComment[];
}

export interface PostReviewResult {
  /** The forge's id for the posted review. */
  providerReviewId?: string;
  /** True when every inline comment is on the diff. When false, the summary
   * quotes every finding. */
  inlinePosted: boolean;
  /** The findings whose comments are on the diff. It is every finding when
   * `inlinePosted` is true, and can be some of them when it is false. */
  inlineFindingIds: string[];
  /** The summary body that was posted. */
  summaryMd: string;
}

/** One inline review comment on a pull request, replies included. */
export interface PrReviewComment {
  id: string;
  /** The parent comment when this one is a reply; null for a root. */
  inReplyToId: string | null;
  /** The author's user name on the forge. */
  author: string;
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

/** A pull request as an event names it. Empty SHAs or a partial context make
 * the pass ask the forge. */
export interface ReviewEventPullRequest {
  repo: string;
  number: number;
  url: string;
  headSha: string;
  baseSha: string;
  /** What the event carried. A field the event does not carry is null. */
  context: PrContext;
}

/** A comment on a pull request. */
export interface ReviewCommentEvent {
  kind: "comment";
  body: string;
  authorId: string;
  fromBot: boolean;
  /** The author's access to the repository as the event states it, in the
   * forge's own terms; null when the event states none. Only the forge's
   * `mayRequestReview` reads it. */
  authorAccess: string | null;
  pr: ReviewEventPullRequest;
}

/** What a forge event means to the code-review built-in. */
export type ReviewEvent =
  | {
      /** The pull request opened, got commits, or left draft. */
      kind: "change";
      trigger: "opened" | "synchronize" | "ready_for_review";
      draft: boolean;
      pr: ReviewEventPullRequest;
    }
  /** The pull request changed in a way that starts no review: it closed, or
   * its title or target changed. */
  | { kind: "update"; pr: ReviewEventPullRequest }
  | ReviewCommentEvent
  /** The dispatch event of the forge's review variant. */
  | { kind: "dispatch"; pr: ReviewEventPullRequest };

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

  /** A 4xx that a retry cannot change: all of them except a timeout (408)
   * and a rate limit (429). A forge that reports a rate limit with another
   * status excludes it in its own `isPermanentError`. */
  get isClientError(): boolean {
    return this.status >= 400 && this.status < 500 && this.status !== 408 && this.status !== 429;
  }
}

/** A file at a commit, with the lines of a finding. */
export interface FileLocation {
  repo: string;
  sha: string;
  path: string;
  startLine?: number | null;
  endLine?: number | null;
}

/** The web pages of a forge that a review links to. `prUrl` is the pull
 * request's own URL, as the forge gave it. */
export interface ForgeLinks {
  /** The pull request from its coordinates, for a record with no stored
   * URL. Null when the forge has no fixed host. */
  pullRequest(repo: string, number: number): string | null;
  review(prUrl: string, reviewId: string): string;
  file(prUrl: string, at: FileLocation): string;
}

/** A git path as URL path segments. A `#` or `?` in a file name must not
 * start the fragment or the query. */
export function escapeGitPath(path: string): string {
  return path.split("/").map(encodeURIComponent).join("/");
}

/** The lines a file link points at: a range, one line, or none. */
export function linkLines(at: FileLocation): { from: number; to?: number } | null {
  const { startLine, endLine } = at;
  if (startLine && endLine && startLine !== endLine) return { from: startLine, to: endLine };
  const line = endLine ?? startLine;
  return line ? { from: line } : null;
}

export interface GitForge {
  readonly provider: string;
  readonly links: ForgeLinks;

  /** The form of a repository name that is equal for two names of one
   * repository. Handles, trigger scopes and enrolment compare this form. */
  repoKey(repo: string): string;
  /** The repository and number a pull-request URL names; null when the path
   * is not this forge's pull-request path. The host is not checked. `number`
   * is null when the URL has none. */
  parsePullRequestUrl(url: string): { repo: string; number: number | null } | null;
  /** True when a retry cannot succeed: the forge refused the request, and
   * not for a rate limit. */
  isPermanentError(error: unknown): boolean;

  /** The meaning of an event for a review; null when the event is not about
   * a pull request. `eventKey` is the key the forge's event source gives the
   * delivery, or `REVIEW_DISPATCH_EVENT_KEY`. No I/O. */
  reviewEvent(eventKey: string, payload: Record<string, unknown>): ReviewEvent | null;
  /** Whether the author of a comment may give the review app a command. */
  mayRequestReview(comment: ReviewCommentEvent): Promise<boolean>;

  fetchPullRequest(repo: string, prNumber: number): Promise<PullRequestHeads>;
  /** Every inline review comment on the pull request. Read-only. */
  listReviewComments(repo: string, prNumber: number): Promise<PrReviewComment[]>;
  postReview(input: PostReviewInput): Promise<PostReviewResult>;
  /** Whether a review that contains `marker` is on the pull request. */
  hasReviewWithMarker(repo: string, prNumber: number, marker: string): Promise<boolean>;

  /** Post a comment on the pull request. The id it returns is the forge's
   * own token for the comment: it holds what the forge needs to find the
   * comment again, and `updateComment` takes it back unchanged. */
  createComment(repo: string, prNumber: number, body: string): Promise<string>;
  updateComment(repo: string, commentId: string, body: string): Promise<void>;
  /** The id of the newest comment on the pull request that this app posted
   * and that contains `marker`; null when there is none. */
  findCommentWithMarker(repo: string, prNumber: number, marker: string): Promise<string | null>;
}

export interface ForgeDeps {
  runIntegrationOp?: typeof runIntegrationOp;
}

/** What a provider declares to be a git forge. Its connector declares the
 * `post_pr_review`, `upsert_pr_comment` and `update_pr_comment` actions. */
export interface GitForgeSpec {
  /** Stored rows and block configs that name no forge mean this one. */
  legacyDefault?: true;
  make(deps: ForgeDeps): GitForge;
  /** The forge's code-review built-in. */
  review: ReviewVariant;
}
