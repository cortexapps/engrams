/** GitHub review posting over the coordinator-owned IntegrationOp seam. */

import type { ReviewFindingRow } from "../db/reviews.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "../integrations/run-op.ts";
import type {
  FindingDecision,
  PolicyDecision,
} from "./policy-gate.ts";
import { readPrContext, type PrContext } from "./pr-context.ts";

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
  /** Renders the summary body for the actual outcome: `true` when the inline
   *  comments post (concise — they carry their own detail), `false` on the 422
   *  fallback (must re-quote every surviving finding so none is lost). */
  buildSummary: (inlinePosted: boolean) => string;
  comments: InlineComment[];
}

export interface PostReviewResult {
  githubReviewId?: string;
  posted: boolean;
  inlinePosted: boolean;
  /** The summary body actually posted — persist this on the review record. */
  summaryMd: string;
}

export interface UpsertStatusCommentInput {
  repo: string;
  prNumber: number;
  /** The comment to edit in place; omit to post a fresh one. */
  commentId?: string;
  body: string;
}

/** One inline review comment on a pull request, replies included. */
export interface PrReviewComment {
  id: string;
  /** The parent comment when this one is a thread reply; null for a root. */
  inReplyToId: string | null;
  authorLogin: string;
  body: string;
  path: string | null;
}

export interface GithubReviewPoster {
  /** The heads a pass pins itself to, plus the PR context recorded on the
   *  review record. The SHAs are load-bearing and throw when absent; `pr` is
   *  best-effort. */
  fetchPrContext(repo: string, prNumber: number): Promise<{
    headSha: string;
    baseSha: string;
    pr: PrContext;
  }>;
  alreadyPosted(repo: string, prNumber: number, reviewId: string): Promise<boolean>;
  /** Every inline review comment on the pull request, replies included —
   *  the author-response context a re-review round reads. Read-only. */
  listReviewComments(repo: string, prNumber: number): Promise<PrReviewComment[]>;
  postReview(input: PostReviewInput): Promise<PostReviewResult>;
  /** Post (or edit, when `commentId` is given) the sticky status comment and
   *  return the live comment id. A PATCH against a comment that has since been
   *  deleted (404) transparently falls back to a fresh POST. */
  upsertStatusComment(input: UpsertStatusCommentInput): Promise<{ commentId: string }>;
}

export type ReviewStatusPhase =
  | "acknowledged"
  | "finding"
  | "verifying"
  | "posted"
  | "failed"
  | "halted";

export interface StatusCommentInput {
  reviewId: string;
  phase: ReviewStatusPhase;
  /** Candidate count (verifying) or posted-finding count (posted). */
  count?: number;
  reviewUrl?: string;
}

/** Render the sticky status body. The hidden marker is deliberately the last
 *  line so it never bleeds into the visible text. */
export function buildStatusComment(input: StatusCommentInput): string {
  const n = input.count ?? 0;
  const plural = n === 1 ? "" : "s";
  const link = input.reviewUrl ? ` · [View details](${input.reviewUrl})` : "";
  const line = ((): string => {
    switch (input.phase) {
      case "acknowledged":
        return "👀 **engrams review** — acknowledged, queued.";
      case "finding":
        return "⏳ **engrams review** — analyzing the diff…";
      case "verifying":
        return `⏳ **engrams review** — confirming ${n} candidate finding${plural}…`;
      case "posted":
        return `✅ **engrams review** — complete. ${n} finding${plural} posted.${link}`;
      case "failed":
        return "⚠️ **engrams review** — the run failed. It will retry on the next push or @mention.";
      case "halted":
        return "🛑 **engrams review** — stopped.";
    }
  })();
  return `${line}\n\n<!-- engrams-status:${input.reviewId} -->`;
}

export interface ReviewSummaryInput {
  reviewId: string;
  reviewUrl: string;
  decision: PolicyDecision;
  /** False means every non-suppressed finding must be preserved in the body. */
  inlinePosted: boolean;
}

type RunIntegrationOp = typeof defaultRunIntegrationOp;

const CATEGORY_LABELS: Readonly<Record<string, string>> = {
  "security-privacy": "🔒 Security & Privacy",
  "stability-availability": "🩺 Stability & Availability",
  "data-integrity-integration": "🗄️ Data Integrity & Integration",
  "functional-correctness": "🎯 Functional Correctness",
  "performance-scalability": "🚀 Performance & Scalability",
  "maintainability-quality": "📐 Maintainability & Code Quality",
};
const REVIEWS_PER_PAGE = 100;
const MAX_REVIEW_PAGES = 50;
const COMMENTS_PER_PAGE = 100;
// Comments are advisory context for re-review rounds, so an enormous thread
// truncates instead of failing the bootstrap that reads it.
const MAX_COMMENT_PAGES = 10;

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function decodeBody(body: Uint8Array): string {
  return new TextDecoder().decode(body);
}

function parseJson(result: IntegrationOpResult, operation: string): unknown {
  const text = decodeBody(result.body);
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${operation} returned invalid JSON`);
  }
}

/**
 * A GitHub response we could not use, carrying the status so callers can tell a
 * permanent failure from a transient one.
 *
 * That distinction is load-bearing for review ingress: a 404 must fail the review
 * immediately, because retrying a pull request that is gone wastes calls and
 * still fails, while a 502 must retry, because the pull request is fine and
 * GitHub is not.
 */
/** Shared by the class and the structural reader below, so the two cannot drift. */
const GITHUB_REQUEST_ERROR_NAME = "GithubRequestError";

export class GithubRequestError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly responseBody: string,
  ) {
    super(message);
    this.name = GITHUB_REQUEST_ERROR_NAME;
  }
}

/**
 * Read the fields of a GitHub request failure — whether it is a live
 * `GithubRequestError` or one DBOS revived from a durable step's recorded error.
 *
 * `instanceof` cannot be used here. DBOS records a step error with
 * `serializeError` and revives it with `deserializeError`, which returns a plain
 * `Error` carrying the original's own properties. Verified by round-tripping the
 * real class: `instanceof` is false afterwards, while `name`, `status` and
 * `responseBody` all survive. So a replayed workflow that classified by class
 * would take the OPPOSITE branch from the original run — a permanent 404 would
 * look retryable and be rethrown, leaving its review row stuck active forever.
 * Classifying on the fields that survive is what makes replay agree with the
 * first execution.
 */
function readGithubFailure(
  error: unknown,
): { status: number; responseBody: string } | null {
  if (typeof error !== "object" || error === null) return null;
  // Sound: `error` is a confirmed non-null object, and every property read off
  // this view is validated before it is used.
  const candidate = error as {
    name?: unknown;
    status?: unknown;
    responseBody?: unknown;
  };
  if (candidate.name !== GITHUB_REQUEST_ERROR_NAME) return null;
  if (typeof candidate.status !== "number") return null;
  return {
    status: candidate.status,
    responseBody: typeof candidate.responseBody === "string"
      ? candidate.responseBody
      : "",
  };
}

function isRateLimitResponseBody(body: string): boolean {
  try {
    const parsed: unknown = JSON.parse(body);
    return (
      isObject(parsed)
      && typeof parsed["message"] === "string"
      && /rate limit/i.test(parsed["message"])
    );
  } catch {
    // A non-JSON 403 has no trustworthy rate-limit signal, so it retains the
    // permanent-by-default classification below.
    return false;
  }
}

/**
 * True when GitHub will not change its mind: the pull request is deleted, was
 * transferred away, or our installation no longer has access. GitHub answers 404
 * rather than 403 for a private repository we cannot see, so 404 is the common
 * case here and it is NOT retryable.
 */
export function isPermanentGithubFailure(error: unknown): boolean {
  const failure = readGithubFailure(error);
  if (!failure) return false;
  if (failure.status === 403) {
    // IntegrationOpResult intentionally carries no response headers across the
    // coordinator boundary, so x-ratelimit-remaining/retry-after are unavailable
    // here. GitHub includes "rate limit" in the JSON message for both primary
    // and secondary throttles; those 403s must retry, while other 403s fail fast.
    return !isRateLimitResponseBody(failure.responseBody);
  }
  return failure.status === 401
    || failure.status === 404
    || failure.status === 410;
}

function responseError(operation: string, result: IntegrationOpResult): Error {
  const detail = decodeBody(result.body).trim();
  return new GithubRequestError(
    result.status,
    `${operation} failed with GitHub status ${result.status}${detail ? `: ${detail}` : ""}`,
    detail,
  );
}

function reviewIdFrom(result: IntegrationOpResult): string | undefined {
  if (result.body.length === 0) return undefined;
  const value = parseJson(result, "create pull request review");
  if (!isObject(value)) return undefined;
  const id = value["id"];
  return typeof id === "string" || typeof id === "number" ? String(id) : undefined;
}

function commentIdFrom(result: IntegrationOpResult): string | undefined {
  if (result.body.length === 0) return undefined;
  const value = parseJson(result, "status comment");
  if (!isObject(value)) return undefined;
  const id = value["id"];
  return typeof id === "string" || typeof id === "number" ? String(id) : undefined;
}

function commentPayload(comment: InlineComment): Record<string, unknown> {
  return {
    path: comment.path,
    side: comment.side,
    line: comment.line,
    ...(comment.startLine != null && comment.startLine !== comment.line
      ? { start_line: comment.startLine, start_side: comment.side }
      : {}),
    body: comment.body,
  };
}

function findingLocation(item: FindingDecision): string {
  const line = item.finding.endLine ?? item.finding.startLine;
  return line == null ? item.finding.path : `${item.finding.path}:L${line}`;
}

function quoteFinding(item: FindingDecision): string {
  const body = item.finding.bodyMd
    .split("\n")
    .map((line) => `> ${line}`)
    .join("\n");
  return [
    `> \`${findingLocation(item)}\` — **${item.finding.title}**`,
    ">",
    body,
  ].join("\n");
}

/** Build the durable GitHub summary. The marker is deliberately the last line. */
export function buildReviewSummary(input: ReviewSummaryInput): string {
  const visible = [...input.decision.toPost, ...input.decision.uiOnly];
  const categoryCounts = new Map<string, number>();
  for (const item of visible) {
    categoryCounts.set(
      item.finding.category,
      (categoryCounts.get(item.finding.category) ?? 0) + 1,
    );
  }

  const verdict = input.decision.counts.total === 0
    ? "No findings."
    : input.inlinePosted
    ? `${input.decision.toPost.length} finding${input.decision.toPost.length === 1 ? "" : "s"} posted inline.`
    : `${input.decision.counts.total} finding${input.decision.counts.total === 1 ? "" : "s"} included in this summary.`;
  const severities = input.decision.counts;
  const categories = [...categoryCounts.entries()]
    .map(([category, count]) => `${CATEGORY_LABELS[category] ?? category}: ${count}`)
    .join(" · ");
  const details = input.inlinePosted ? input.decision.uiOnly : visible;

  const lines = [
    "## Engrams review",
    "",
    `**Verdict:** ${verdict}`,
    `**Severity:** Critical ${severities.critical} · High ${severities.high} · Medium ${severities.medium} · Low ${severities.low}`,
    ...(categories ? [`**Categories:** ${categories}`] : []),
    "",
    `[View the full engrams review](${input.reviewUrl})`,
    ...(input.decision.overflow > 0
      ? [
          "",
          `${input.decision.overflow} more finding${input.decision.overflow === 1 ? "" : "s"} on the review page.`,
        ]
      : []),
    ...(details.length > 0
      ? [
          "",
          "### Findings on the review page",
          "",
          details.map(quoteFinding).join("\n\n"),
        ]
      : []),
    "",
    `<!-- engrams-review:${input.reviewId} -->`,
  ];
  return lines.join("\n");
}

export function buildInlineCommentBody(finding: ReviewFindingRow): string {
  const category = CATEGORY_LABELS[finding.category] ?? finding.category;
  return [
    `**${category} · ${finding.severity.toUpperCase()} — ${finding.title}**`,
    "",
    finding.bodyMd,
    ...(finding.suggestedFix
      ? ["", "```suggestion", finding.suggestedFix, "```"]
      : []),
  ].join("\n");
}

/**
 * Production uses a sessionless IntegrationOp mint. It is installation-wide
 * today (owner=None), so multi-installation owner routing remains a follow-up;
 * credentials and HTTP execution stay entirely in the coordinator either way.
 */
export function makeGithubReviewPoster(
  deps: { runIntegrationOp?: RunIntegrationOp } = {},
): GithubReviewPoster {
  const runOp = deps.runIntegrationOp ?? defaultRunIntegrationOp;

  return {
    async fetchPrContext(repo, prNumber) {
      const response = await runOp("github", {
        method: "GET",
        path: `/repos/${repo}/pulls/${prNumber}`,
        contentType: "application/json",
      });
      if (response.status < 200 || response.status >= 300) {
        throw responseError("get pull request", response);
      }
      const value = parseJson(response, "get pull request");
      if (!isObject(value) || !isObject(value["head"]) || !isObject(value["base"])) {
        throw new Error("get pull request response is missing head/base");
      }
      const headSha = value["head"]["sha"];
      const baseSha = value["base"]["sha"];
      if (typeof headSha !== "string" || typeof baseSha !== "string") {
        throw new Error("get pull request response is missing head/base SHAs");
      }
      return { headSha, baseSha, pr: readPrContext(value) };
    },

    async alreadyPosted(repo, prNumber, reviewId) {
      const marker = `<!-- engrams-review:${reviewId} -->`;
      for (let page = 1; page <= MAX_REVIEW_PAGES; page++) {
        const response = await runOp("github", {
          method: "GET",
          path:
            `/repos/${repo}/pulls/${prNumber}/reviews?per_page=${REVIEWS_PER_PAGE}&page=${page}`,
          contentType: "application/json",
        });
        if (response.status < 200 || response.status >= 300) {
          throw responseError("list pull request reviews", response);
        }
        const value = parseJson(response, "list pull request reviews");
        if (!Array.isArray(value)) {
          throw new Error("list pull request reviews returned a non-array response");
        }
        if (
          value.some((item) =>
            isObject(item)
            && typeof item["body"] === "string"
            && item["body"].includes(marker)
          )
        ) {
          return true;
        }
        if (value.length < REVIEWS_PER_PAGE) return false;
      }
      throw new Error(
        `list pull request reviews exceeded ${MAX_REVIEW_PAGES} pages`,
      );
    },

    async listReviewComments(repo, prNumber) {
      const comments: PrReviewComment[] = [];
      for (let page = 1; page <= MAX_COMMENT_PAGES; page++) {
        const response = await runOp("github", {
          method: "GET",
          path:
            `/repos/${repo}/pulls/${prNumber}/comments?per_page=${COMMENTS_PER_PAGE}&page=${page}`,
          contentType: "application/json",
        });
        if (response.status < 200 || response.status >= 300) {
          throw responseError("list pull request review comments", response);
        }
        const value = parseJson(response, "list pull request review comments");
        if (!Array.isArray(value)) {
          throw new Error(
            "list pull request review comments returned a non-array response",
          );
        }
        for (const item of value) {
          if (!isObject(item)) continue;
          const id = item["id"];
          if (typeof id !== "string" && typeof id !== "number") continue;
          const inReplyTo = item["in_reply_to_id"];
          const user = item["user"];
          comments.push({
            id: String(id),
            inReplyToId: typeof inReplyTo === "string" || typeof inReplyTo === "number"
              ? String(inReplyTo)
              : null,
            authorLogin: isObject(user) && typeof user["login"] === "string"
              ? user["login"]
              : "",
            body: typeof item["body"] === "string" ? item["body"] : "",
            path: typeof item["path"] === "string" ? item["path"] : null,
          });
        }
        if (value.length < COMMENTS_PER_PAGE) break;
      }
      return comments;
    },

    async postReview(input) {
      const path = `/repos/${input.repo}/pulls/${input.prNumber}/reviews`;
      const inlineBody = input.buildSummary(true);
      let response = await runOp("github", {
        method: "POST",
        path,
        body: JSON.stringify({
          commit_id: input.commitId,
          event: "COMMENT",
          body: inlineBody,
          comments: input.comments.map(commentPayload),
        }),
        contentType: "application/json",
      });

      if (response.status >= 200 && response.status < 300) {
        const githubReviewId = reviewIdFrom(response);
        return {
          ...(githubReviewId ? { githubReviewId } : {}),
          posted: true,
          inlinePosted: true,
          summaryMd: inlineBody,
        };
      }
      if (response.status !== 422) {
        throw responseError("create pull request review", response);
      }

      // GitHub validates the whole batch atomically. A single stale/non-diff
      // anchor yields 422, so v1 coarse-demotes every inline finding and retries
      // exactly once without comments — with a fuller body that re-quotes every
      // finding so none is lost. Per-finding pre-validation is deferred.
      const summaryOnlyBody = input.buildSummary(false);
      response = await runOp("github", {
        method: "POST",
        path,
        body: JSON.stringify({
          commit_id: input.commitId,
          event: "COMMENT",
          body: summaryOnlyBody,
        }),
        contentType: "application/json",
      });
      if (response.status < 200 || response.status >= 300) {
        throw responseError("create summary-only pull request review", response);
      }
      const githubReviewId = reviewIdFrom(response);
      return {
        ...(githubReviewId ? { githubReviewId } : {}),
        posted: true,
        inlinePosted: false,
        summaryMd: summaryOnlyBody,
      };
    },

    async upsertStatusComment(input) {
      const create = async (): Promise<{ commentId: string }> => {
        const response = await runOp("github", {
          method: "POST",
          path: `/repos/${input.repo}/issues/${input.prNumber}/comments`,
          body: JSON.stringify({ body: input.body }),
          contentType: "application/json",
        });
        if (response.status < 200 || response.status >= 300) {
          throw responseError("create status comment", response);
        }
        const id = commentIdFrom(response);
        if (id === undefined) throw new Error("create status comment returned no id");
        return { commentId: id };
      };

      if (input.commentId === undefined) return create();

      const response = await runOp("github", {
        method: "PATCH",
        path: `/repos/${input.repo}/issues/comments/${input.commentId}`,
        body: JSON.stringify({ body: input.body }),
        contentType: "application/json",
      });
      // The sticky comment was deleted out from under us — post a fresh one.
      if (response.status === 404) return create();
      if (response.status < 200 || response.status >= 300) {
        throw responseError("edit status comment", response);
      }
      return { commentId: input.commentId };
    },
  };
}
