/** GitHub as a git forge. Every call goes through the coordinator's
 * IntegrationOp, which holds the credential: an installation-wide mint. */

import { REVIEW_DISPATCH_EVENT_KEY } from "../../automations/builtins/review-variant.ts";
import {
  escapeGitPath,
  ForgeRequestError,
  linkLines,
  type ForgeLinks,
  type GitForge,
  type GitForgeSpec,
  type InlineComment,
  type PrReviewComment,
  type ReviewEvent,
  type ReviewEventPullRequest,
} from "../../forges/forge.ts";
import {
  decodeBody,
  isObject,
  parseJson,
  responseError,
  succeeded,
  unusable,
  type Json,
} from "../../forges/http.ts";
import { config } from "../../config.ts";
import { EMPTY_PR_CONTEXT } from "../../forges/pr-context.ts";
import { normalizeMentionHandle } from "../../forges/review-command.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "../../integrations/run-op.ts";
import { log as rootLog } from "../../log.ts";
import { readPrContext } from "./pr.ts";
import { GITHUB_REVIEW } from "./review-variant.ts";

const PROVIDER = "github";
const FORGE = "GitHub";
const log = rootLog.child({ component: "github-forge" });
const PER_PAGE = 100;
// The marker scan guards the window between a posted review and its
// checkpoint, so it gives up, and the action posts, after this many pages.
const MAX_REVIEW_PAGES = 10;
// Comments are advisory context for a re-review, so a very long thread is cut
// short and does not fail the read.
const MAX_COMMENT_PAGES = 10;
/** The `author_association` values of a person with a role in the
 * repository or its organization. */
const MAY_REQUEST_REVIEW: ReadonlySet<string> = new Set(["OWNER", "MEMBER", "COLLABORATOR"]);
/** The pull-request actions that change the pull request and start no review. */
const UPDATE_ACTIONS: ReadonlySet<string> = new Set([
  "closed",
  "reopened",
  "edited",
  "converted_to_draft",
]);

function str(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function positiveInt(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0 ? value : null;
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
  return error.isClientError;
}

/** The id of a review that is posted. A body that cannot be read gives no
 * id; it must not fail the post. */
function reviewIdFrom(result: IntegrationOpResult, repo: string, prNumber: number): string | undefined {
  let value: unknown;
  try {
    value = JSON.parse(decodeBody(result.body));
  } catch {
    value = undefined;
  }
  const id = isObject(value) ? value["id"] : undefined;
  if (typeof id === "string" || typeof id === "number") return String(id);
  log.warn(
    { repo, prNumber, status: result.status, truncated: result.truncated },
    "a posted review returned no readable id",
  );
  return undefined;
}

function commentPayload(comment: InlineComment): Json {
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

const GITHUB_LINKS: ForgeLinks = {
  pullRequest: (repo, number) => `https://github.com/${repo}/pull/${number}`,
  review: (prUrl, reviewId) => `${prUrl}#pullrequestreview-${reviewId}`,
  file(_prUrl, at) {
    const base = `https://github.com/${at.repo}/blob/${at.sha}/${escapeGitPath(at.path)}`;
    const lines = linkLines(at);
    if (lines === null) return base;
    return lines.to === undefined ? `${base}#L${lines.from}` : `${base}#L${lines.from}-L${lines.to}`;
  },
};

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

/** The pull request of a `pull_request` event or of the dispatch event. */
function eventPullRequest(payload: Json): ReviewEventPullRequest | null {
  const repository = isObject(payload["repository"]) ? payload["repository"] : null;
  const pr = isObject(payload["pull_request"]) ? payload["pull_request"] : null;
  const repo = repository && str(repository["full_name"]);
  const number = pr && (positiveInt(pr["number"]) ?? positiveInt(payload["number"]));
  if (!repo || pr === null || !number) return null;
  const head = isObject(pr["head"]) ? pr["head"] : {};
  const base = isObject(pr["base"]) ? pr["base"] : {};
  return {
    repo,
    number,
    url: str(pr["html_url"]) ?? "",
    headSha: str(head["sha"]) ?? "",
    baseSha: str(base["sha"]) ?? "",
    context: readPrContext(pr),
  };
}

function reviewEvent(eventKey: string, payload: Json): ReviewEvent | null {
  if (eventKey === REVIEW_DISPATCH_EVENT_KEY) {
    const pr = eventPullRequest(payload);
    return pr && { kind: "dispatch", pr };
  }

  if (eventKey.startsWith("pull_request.")) {
    const pr = eventPullRequest(payload);
    if (pr === null) return null;
    const action = eventKey.slice("pull_request.".length);
    if (action === "opened" || action === "synchronize" || action === "ready_for_review") {
      const draft = isObject(payload["pull_request"]) && payload["pull_request"]["draft"] === true;
      return { kind: "change", trigger: action, draft, pr };
    }
    return UPDATE_ACTIONS.has(action) ? { kind: "update", pr } : null;
  }

  const inline = eventKey === "pull_request_review_comment.created";
  if (eventKey === "issue_comment.created" || inline) {
    const repository = isObject(payload["repository"]) ? payload["repository"] : null;
    // An inline review comment names its pull request; any other comment
    // names an issue, which is a pull request when it has `pull_request`.
    const subject = payload[inline ? "pull_request" : "issue"];
    const issue = isObject(subject) ? subject : null;
    const comment = isObject(payload["comment"]) ? payload["comment"] : null;
    const sender = isObject(payload["sender"]) ? payload["sender"] : {};
    const repo = repository && str(repository["full_name"]);
    const number = issue && positiveInt(issue["number"]);
    const body = comment && str(comment["body"]);
    const author = comment && isObject(comment["user"]) ? str(comment["user"]["login"]) : null;
    // A comment on an issue that is not a pull request is not for a review.
    if (!repo || !issue || !number || !body || !author) return null;
    if (!inline && !isObject(issue["pull_request"])) return null;
    const url = str(issue["html_url"]);
    return {
      kind: "comment",
      body,
      authorId: author,
      fromBot: sender["type"] === "Bot",
      authorAccess: comment ? str(comment["author_association"]) : null,
      pr: {
        repo,
        number,
        url: url ?? "",
        headSha: "",
        baseSha: "",
        context: { ...EMPTY_PR_CONTEXT, url },
      },
    };
  }
  return null;
}

export function makeGithubForge(
  deps: {
    runIntegrationOp?: typeof defaultRunIntegrationOp;
    /** The app's login (`GITHUB_APP_LOGIN`); empty when it is not set. */
    appLogin?: string;
  } = {},
): GitForge {
  const runOp = deps.runIntegrationOp ?? defaultRunIntegrationOp;
  const appSlug = normalizeMentionHandle(deps.appLogin ?? config.githubAppLogin).toLowerCase();
  const isBot = (user: unknown): user is Record<string, unknown> =>
    isObject(user) && user["type"] === "Bot";
  const hasAppLogin = (user: Record<string, unknown>): boolean =>
    String(user["login"] ?? "").toLowerCase() === `${appSlug}[bot]`;
  const send = (method: string, path: string, body?: unknown): Promise<IntegrationOpResult> =>
    runOp(PROVIDER, {
      method,
      path,
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      contentType: "application/json",
    });
  const list = async (operation: string, path: string): Promise<unknown[]> => {
    const response = await send("GET", path);
    if (!succeeded(response)) throw responseError(FORGE, operation, response);
    const value = parseJson(response, operation);
    if (!Array.isArray(value)) throw unusable(operation, "a non-array response");
    return value;
  };
  const listPage = (path: string, page: number, operation: string): Promise<unknown[]> =>
    list(operation, `${path}?per_page=${PER_PAGE}&page=${page}`);

  return {
    provider: PROVIDER,
    // GitHub finds a repository by its name in any case.
    repoKey: (repo) => repo.toLowerCase(),
    parsePullRequestUrl,
    links: GITHUB_LINKS,
    isPermanentError,
    reviewEvent,

    async mayRequestReview(comment) {
      return comment.authorAccess !== null && MAY_REQUEST_REVIEW.has(comment.authorAccess);
    },

    async fetchPullRequest(repo, prNumber) {
      const response = await send("GET", `/repos/${repo}/pulls/${prNumber}`);
      if (!succeeded(response)) throw responseError(FORGE, "get pull request", response);
      const value = parseJson(response, "get pull request");
      if (!isObject(value) || !isObject(value["head"]) || !isObject(value["base"])) {
        throw unusable("get pull request", "no head/base");
      }
      const headSha = value["head"]["sha"];
      const baseSha = value["base"]["sha"];
      if (typeof headSha !== "string" || typeof baseSha !== "string") {
        throw unusable("get pull request", "no head/base SHAs");
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

    async createComment(repo, prNumber, body) {
      const response = await send("POST", `/repos/${repo}/issues/${prNumber}/comments`, { body });
      if (!succeeded(response)) throw responseError(FORGE, "create comment", response);
      const value = parseJson(response, "create comment");
      const id = isObject(value) ? positiveInt(value["id"]) : null;
      if (id === null) throw unusable("create comment", "no id");
      return String(id);
    },

    async updateComment(repo, commentId, body) {
      if (!/^[1-9]\d*$/.test(commentId)) {
        throw new ForgeRequestError(400, `"${commentId}" is not a GitHub comment id`, "");
      }
      const response = await send("PATCH", `/repos/${repo}/issues/comments/${commentId}`, { body });
      if (!succeeded(response)) throw responseError(FORGE, "update comment", response);
    },

    // The newest 100 comments. A marker in a comment proves nothing by
    // itself: any commenter can post one, so the comment must be a bot's.
    // The forge has no call that names the app, and `GITHUB_APP_LOGIN` can
    // be a mention alias and not the app's login: a comment with that login
    // is preferred, and the newest bot comment is taken when there is none.
    async findCommentWithMarker(repo, prNumber, marker) {
      const comments = await list(
        "list comments",
        `/repos/${repo}/issues/${prNumber}/comments?per_page=${PER_PAGE}&sort=created&direction=desc`,
      );
      let newestBot: string | null = null;
      for (const item of comments) {
        if (!isObject(item) || typeof item["body"] !== "string" || !item["body"].includes(marker)) continue;
        const id = positiveInt(item["id"]);
        const user = item["user"];
        if (id === null || !isBot(user)) continue;
        if (hasAppLogin(user)) return String(id);
        newestBot ??= String(id);
      }
      return newestBot;
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
            author: isObject(user) && typeof user["login"] === "string" ? user["login"] : "",
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
      let response = await send("POST", path, {
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
        response = await send("POST", path, { ...review, body: summaryMd });
      }
      if (!succeeded(response)) {
        throw responseError(
          FORGE,
          inlinePosted ? "create pull request review" : "create summary-only pull request review",
          response,
        );
      }
      const providerReviewId = reviewIdFrom(response, input.repo, input.prNumber);
      return {
        ...(providerReviewId ? { providerReviewId } : {}),
        inlinePosted,
        inlineFindingIds: inlinePosted ? input.comments.map((comment) => comment.findingId) : [],
        summaryMd,
      };
    },
  };
}

export const GITHUB_FORGE: GitForgeSpec = {
  legacyDefault: true,
  make: makeGithubForge,
  review: GITHUB_REVIEW,
};
