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

export interface GithubReviewPoster {
  fetchPrHeads(repo: string, prNumber: number): Promise<{
    headSha: string;
    baseSha: string;
  }>;
  alreadyPosted(repo: string, prNumber: number, reviewId: string): Promise<boolean>;
  postReview(input: PostReviewInput): Promise<PostReviewResult>;
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

function responseError(operation: string, result: IntegrationOpResult): Error {
  const detail = decodeBody(result.body).trim();
  return new Error(
    `${operation} failed with GitHub status ${result.status}${detail ? `: ${detail}` : ""}`,
  );
}

function reviewIdFrom(result: IntegrationOpResult): string | undefined {
  if (result.body.length === 0) return undefined;
  const value = parseJson(result, "create pull request review");
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
    async fetchPrHeads(repo, prNumber) {
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
      return { headSha, baseSha };
    },

    async alreadyPosted(repo, prNumber, reviewId) {
      const response = await runOp("github", {
        method: "GET",
        path: `/repos/${repo}/pulls/${prNumber}/reviews`,
        contentType: "application/json",
      });
      if (response.status < 200 || response.status >= 300) {
        throw responseError("list pull request reviews", response);
      }
      const value = parseJson(response, "list pull request reviews");
      if (!Array.isArray(value)) {
        throw new Error("list pull request reviews returned a non-array response");
      }
      const marker = `<!-- engrams-review:${reviewId} -->`;
      return value.some((item) =>
        isObject(item) && typeof item["body"] === "string" && item["body"].includes(marker)
      );
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
  };
}
