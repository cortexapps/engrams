/** GitHub as a git forge. Every call goes through the coordinator's
 * IntegrationOp, which holds the credential: an installation-wide mint. */

import {
  ForgeRequestError,
  type GitForge,
  type GitForgeSpec,
  type InlineComment,
  type PrReviewComment,
} from "../../forges/forge.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "../../integrations/run-op.ts";
import type { PrContext } from "../../reviews/pr-context.ts";

const PROVIDER = "github";
const PER_PAGE = 100;
// The marker scan guards the window between a posted review and its
// checkpoint, so it gives up, and the action posts, after this many pages.
const MAX_REVIEW_PAGES = 10;
// Comments are advisory context for a re-review, so a very long thread is cut
// short and does not fail the read.
const MAX_COMMENT_PAGES = 10;

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function decodeBody(body: Uint8Array): string {
  return new TextDecoder().decode(body);
}

function parseJson(result: IntegrationOpResult, operation: string): unknown {
  try {
    return JSON.parse(decodeBody(result.body));
  } catch {
    throw new Error(`${operation} returned invalid JSON`);
  }
}

function succeeded(result: IntegrationOpResult): boolean {
  return result.status >= 200 && result.status < 300;
}

function responseError(operation: string, result: IntegrationOpResult): ForgeRequestError {
  const detail = decodeBody(result.body).trim();
  return new ForgeRequestError(
    result.status,
    `${operation} failed with GitHub status ${result.status}${detail ? `: ${detail}` : ""}`,
    detail,
  );
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
    return false;
  }
}

/** GitHub answers 404, not 403, for a private repository the installation
 * cannot see, so 404 is the common permanent failure. A 403 is permanent
 * unless it is a rate limit: IntegrationOp returns no response headers, and
 * GitHub names the limit in the JSON message for both throttles. */
function isPermanentError(error: unknown): boolean {
  if (!(error instanceof ForgeRequestError)) return false;
  if (error.status === 403) return !isRateLimitResponseBody(error.responseBody);
  return error.status === 401 || error.status === 404 || error.status === 410;
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

function parsePullRequestUrl(url: string): { repo: string; number: number | null } | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const segments = parsed.pathname.split("/").filter(Boolean);
  if (segments.length < 4 || (segments[2] !== "pull" && segments[2] !== "pulls")) return null;
  const number = /^[1-9]\d*$/.test(segments[3] ?? "") ? Number(segments[3]) : null;
  return { repo: `${segments[0]}/${segments[1]}`, number };
}

/**
 * GitHub's pull-request object as a `PrContext`. The same object is the
 * `GET /repos/{repo}/pulls/{n}` response and the webhook payload's
 * `pull_request` member. Every field falls back to null on its own. The two
 * SHAs are not read here: callers require them.
 */
export function readPrContext(pr: Record<string, unknown>): PrContext {
  const head = isObject(pr["head"]) ? pr["head"] : null;
  const base = isObject(pr["base"]) ? pr["base"] : null;
  const user = isObject(pr["user"]) ? pr["user"] : null;
  return {
    providerId: identifier(pr["id"]),
    title: nullableString(pr["title"]),
    author: user ? nullableString(user["login"]) : null,
    state: readPrState(pr),
    url: nullableString(pr["html_url"]),
    providerUpdatedAt: nullableDate(pr["updated_at"]),
    headBranch: head ? nullableString(head["ref"]) : null,
    baseBranch: base ? nullableString(base["ref"]) : null,
    additions: nullableCount(pr["additions"]),
    deletions: nullableCount(pr["deletions"]),
    changedFiles: nullableCount(pr["changed_files"]),
  };
}

/** GitHub gives `state` as open or closed, with `merged` and `draft` apart.
 * Merged outranks closed; draft applies only while open. */
function readPrState(pr: Record<string, unknown>): string | null {
  if (pr["merged"] === true) return "merged";
  const state = nullableString(pr["state"]);
  if (state === "open") return pr["draft"] === true ? "draft" : "open";
  return state;
}

/** GitHub ids arrive as JSON numbers; they are carried as strings so a large
 * one cannot round. */
function identifier(value: unknown): string | null {
  if (typeof value === "string" && value !== "") return value;
  if (typeof value === "number" && Number.isSafeInteger(value) && value > 0) {
    return String(value);
  }
  return null;
}

function nullableDate(value: unknown): Date | null {
  if (typeof value !== "string" || value === "") return null;
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function nullableString(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function nullableCount(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

export function makeGithubForge(
  deps: { runIntegrationOp?: typeof defaultRunIntegrationOp } = {},
): GitForge {
  const runOp = deps.runIntegrationOp ?? defaultRunIntegrationOp;
  const get = (path: string): Promise<IntegrationOpResult> =>
    runOp(PROVIDER, { method: "GET", path, contentType: "application/json" });
  const post = (path: string, body: unknown): Promise<IntegrationOpResult> =>
    runOp(PROVIDER, {
      method: "POST",
      path,
      body: JSON.stringify(body),
      contentType: "application/json",
    });
  const listPage = async (path: string, page: number, operation: string): Promise<unknown[]> => {
    const response = await get(`${path}?per_page=${PER_PAGE}&page=${page}`);
    if (!succeeded(response)) throw responseError(operation, response);
    const value = parseJson(response, operation);
    if (!Array.isArray(value)) throw new Error(`${operation} returned a non-array response`);
    return value;
  };

  return {
    provider: PROVIDER,
    caseInsensitiveRepos: true,
    pullRequestUrl: (repo, prNumber) => `https://github.com/${repo}/pull/${prNumber}`,
    parsePullRequestUrl,
    isPermanentError,

    async fetchPullRequest(repo, prNumber) {
      const response = await get(`/repos/${repo}/pulls/${prNumber}`);
      if (!succeeded(response)) throw responseError("get pull request", response);
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

    async hasReviewWithMarker(repo, prNumber, marker) {
      for (let page = 1; page <= MAX_REVIEW_PAGES; page++) {
        const reviews = await listPage(
          `/repos/${repo}/pulls/${prNumber}/reviews`,
          page,
          "list pull request reviews",
        );
        if (
          reviews.some((item) =>
            isObject(item) && typeof item["body"] === "string" && item["body"].includes(marker)
          )
        ) {
          return true;
        }
        if (reviews.length < PER_PAGE) return false;
      }
      return false;
    },

    async findCommentWithMarker(repo, prNumber, marker) {
      const response = await get(
        `/repos/${repo}/issues/${prNumber}/comments?per_page=${PER_PAGE}&sort=created&direction=desc`,
      );
      if (!succeeded(response)) return null;
      const value = parseJson(response, "list pull request comments");
      if (!Array.isArray(value)) return null;
      return (
        value.find(
          (item): item is Record<string, unknown> =>
            isObject(item) && typeof item["body"] === "string" && item["body"].includes(marker),
        ) ?? null
      );
    },

    async listReviewComments(repo, prNumber) {
      const comments: PrReviewComment[] = [];
      for (let page = 1; page <= MAX_COMMENT_PAGES; page++) {
        const items = await listPage(
          `/repos/${repo}/pulls/${prNumber}/comments`,
          page,
          "list pull request review comments",
        );
        for (const item of items) {
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
            authorLogin: isObject(user) && typeof user["login"] === "string" ? user["login"] : "",
            body: typeof item["body"] === "string" ? item["body"] : "",
            path: typeof item["path"] === "string" ? item["path"] : null,
          });
        }
        if (items.length < PER_PAGE) break;
      }
      return comments;
    },

    async postReview(input) {
      const path = `/repos/${input.repo}/pulls/${input.prNumber}/reviews`;
      const review = { commit_id: input.commitId, event: "COMMENT" };
      const inlineBody = input.buildSummary(true);
      let response = await post(path, {
        ...review,
        body: inlineBody,
        comments: input.comments.map(commentPayload),
      });
      let inlinePosted = true;
      let summaryMd = inlineBody;
      if (response.status === 422) {
        // GitHub validates the comments as one batch: one anchor outside the
        // diff refuses them all. Post once more with no comments and the
        // body that quotes every finding.
        inlinePosted = false;
        summaryMd = input.buildSummary(false);
        response = await post(path, { ...review, body: summaryMd });
      }
      if (!succeeded(response)) {
        throw responseError(
          inlinePosted ? "create pull request review" : "create summary-only pull request review",
          response,
        );
      }
      const providerReviewId = reviewIdFrom(response);
      return {
        ...(providerReviewId ? { providerReviewId } : {}),
        posted: true,
        inlinePosted,
        summaryMd,
      };
    },
  };
}

export const GITHUB_FORGE: GitForgeSpec = {
  provider: PROVIDER,
  legacyDefault: true,
  make: makeGithubForge,
};
