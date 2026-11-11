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
import { readPrContext } from "./pr.ts";
import { GITHUB_REVIEW } from "./review-variant.ts";

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

/** A 2xx response whose body is not what the operation returns. It carries
 * the status, so no caller treats it as permanent. */
function unusable(operation: string, result: IntegrationOpResult, what: string): ForgeRequestError {
  return new ForgeRequestError(result.status, `${operation} returned ${what}`, "");
}

function parseJson(result: IntegrationOpResult, operation: string): unknown {
  try {
    return JSON.parse(decodeBody(result.body));
  } catch {
    throw unusable(operation, result, "invalid JSON");
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

/** The id of a review that is posted. A body that cannot be read gives no
 * id; it must not fail the post. */
function reviewIdFrom(result: IntegrationOpResult): string | undefined {
  let value: unknown;
  try {
    value = JSON.parse(decodeBody(result.body));
  } catch {
    return undefined;
  }
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
    if (!Array.isArray(value)) throw unusable(operation, response, "a non-array response");
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
        throw unusable("get pull request", response, "no head/base");
      }
      const headSha = value["head"]["sha"];
      const baseSha = value["base"]["sha"];
      if (typeof headSha !== "string" || typeof baseSha !== "string") {
        throw unusable("get pull request", response, "no head/base SHAs");
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
      let value: unknown;
      try {
        value = JSON.parse(decodeBody(response.body));
      } catch {
        return null;
      }
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
  review: GITHUB_REVIEW,
};
