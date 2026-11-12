/** The git-forge role: what the product needs from a code host to review a
 * pull request and to link one to the session that opened it. A provider
 * implements `GitForge` under `providers/<name>/` and joins `registry.ts`. */

import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
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
  /** The text that marks this post; `buildSummary` ends with it. A forge
   * that posts the comments apart from the summary adds it to each. */
  marker: string;
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

/** A pull request as an event names it. The context is what the event
 * carried, in the shape the `review_open_pass` step takes. Empty SHAs or a
 * partial context make the pass ask the forge. */
export interface ReviewEventPullRequest {
  repo: string;
  number: number;
  url: string;
  headSha: string;
  baseSha: string;
  context: {
    providerId?: string | null;
    title?: string | null;
    author?: string | null;
    state?: string | null;
    url?: string | null;
    /** ISO 8601. */
    providerUpdatedAt?: string | null;
    headBranch?: string | null;
    baseBranch?: string | null;
  };
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
  | {
      /** A person commented on the pull request. */
      kind: "comment";
      body: string;
      authorId: string;
      fromBot: boolean;
      pr: ReviewEventPullRequest;
    }
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
  /** The inline comment a finding became. */
  comment(prUrl: string, commentId: string): string;
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
  /** Present when the forge's review variant uses the typed facts step and
   * the built-in comment actions. */
  readonly typedReview?: TypedReviewForge;
  /** Repository names compare without case, so handles and trigger scopes
   * for this forge are stored in lower case. */
  readonly caseInsensitiveRepos: boolean;

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
   * returned it, with the comment's id in `id`; null when there is none or
   * the list cannot be read. It does not throw on a forge response. */
  findCommentWithMarker(
    repo: string,
    prNumber: number,
    marker: string,
  ): Promise<Record<string, unknown> | null>;
}

/** What a forge implements so that its review variant needs no code of its
 * own in the connector: the typed `review_facts` step, and the
 * `<provider>.upsert_pr_comment` and `<provider>.update_pr_comment` actions. */
export interface TypedReviewForge {
  /** The meaning of an event for a review; null when it starts none. No I/O. */
  reviewEvent(eventKey: string, payload: Record<string, unknown>): ReviewEvent | null;
  /** Whether the author of a comment may ask for a review of `repo`. */
  mayRequestReview(repo: string, authorId: string): Promise<boolean>;
  /** Post a comment on the pull request. The id it returns is the forge's
   * own; `updateComment` and `findCommentWithMarker` use the same id. */
  createComment(repo: string, prNumber: number, body: string): Promise<string>;
  updateComment(repo: string, commentId: string, body: string): Promise<void>;
}

export interface ForgeDeps {
  runIntegrationOp?: typeof runIntegrationOp;
}

export interface GitForgeSpec {
  provider: string;
  /** Stored rows and block configs that name no forge mean this one. */
  legacyDefault?: true;
  make(deps: ForgeDeps): GitForge;
  /** The forge's code-review built-in. */
  review?: ReviewVariant;
}
