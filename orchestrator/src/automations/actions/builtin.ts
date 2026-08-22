/** The builtin action table (ADR 0119 D5).
 *
 * Connector `actions[]` entries with `execute.kind: "builtin"` name an id
 * from connectors/builtin-actions-allowlist.ts; this module holds the TS
 * implementations. Every provider call rides an injected seam (runOp, the
 * Slack SDK client, the Linear GraphQL client) so tests never touch the
 * network or the coordinator.
 */

import {
  makeGithubReviewPoster,
  type GithubReviewPoster,
  type InlineComment,
} from "../../reviews/github-review.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type RunOpDeps,
} from "../../integrations/run-op.ts";
import { getSlackClient } from "../../integrations/slack.ts";
import {
  makeLinearIssueClient,
  type LinearIssueClient,
} from "../../integrations/linear-issues.ts";
import { actionClientId } from "./client-id.ts";
import { IntegrationActionError } from "./errors.ts";

type RunIntegrationOp = typeof defaultRunIntegrationOp;

/** The narrow Slack surface the builtins use (structural, fake-friendly;
 * WebClient satisfies it because these argument shapes inhabit its unions). */
export interface SlackChatClient {
  chat: {
    postMessage(args: {
      channel: string;
      text: string;
      thread_ts?: string;
    }): Promise<{ ts?: string; channel?: string }>;
    update(args: {
      channel: string;
      ts: string;
      text: string;
    }): Promise<{ ts?: string; channel?: string }>;
  };
}

export interface BuiltinActionContext {
  runId: string;
  stepPath: string;
  /** Rendered marker for marker_comment idempotency; null otherwise. */
  marker: string | null;
}

export interface BuiltinActionDeps {
  runOp: RunIntegrationOp;
  githubPoster(): GithubReviewPoster;
  slackClient(): Promise<SlackChatClient>;
  linearClient(): LinearIssueClient;
}

export type BuiltinActionFn = (
  params: Record<string, unknown>,
  ctx: BuiltinActionContext,
  deps: BuiltinActionDeps,
) => Promise<Record<string, unknown>>;

export type BuiltinActionTable = Record<string, BuiltinActionFn>;

export function defaultBuiltinActionDeps(runOpDeps?: RunOpDeps): BuiltinActionDeps {
  const runOp: RunIntegrationOp = (provider, req) => defaultRunIntegrationOp(provider, req, runOpDeps);
  return {
    runOp,
    githubPoster: () => makeGithubReviewPoster({ runIntegrationOp: runOp }),
    slackClient: () => getSlackClient(runOpDeps),
    linearClient: () => makeLinearIssueClient({ runIntegrationOp: runOp }),
  };
}

function requireString(params: Record<string, unknown>, key: string): string {
  const value = params[key];
  if (typeof value !== "string" || value === "") {
    throw new IntegrationActionError(`missing required string "${key}"`, true);
  }
  return value;
}

function optionalString(params: Record<string, unknown>, key: string): string | undefined {
  const value = params[key];
  return typeof value === "string" && value !== "" ? value : undefined;
}

const REVIEW_SCAN_PAGES = 3;
const REVIEW_SCAN_PER_PAGE = 50;

/** Scan the PR's reviews for a marker string — the crash-window guard between
 * a posted review and its checkpoint (generalizes the review product's
 * alreadyPosted, which hardcodes its own marker shape). */
async function markerAlreadyPosted(
  runOp: RunIntegrationOp,
  repo: string,
  prNumber: number,
  marker: string,
): Promise<boolean> {
  for (let page = 1; page <= REVIEW_SCAN_PAGES; page += 1) {
    const response = await runOp("github", {
      method: "GET",
      path: `/repos/${repo}/pulls/${prNumber}/reviews?per_page=${REVIEW_SCAN_PER_PAGE}&page=${page}`,
      contentType: "application/json",
    });
    if (response.status < 200 || response.status >= 300) {
      throw new IntegrationActionError(
        `list pull request reviews returned ${response.status}`,
        response.status >= 400 && response.status < 500 && response.status !== 429,
        response.status,
      );
    }
    const value: unknown = JSON.parse(new TextDecoder().decode(response.body));
    if (!Array.isArray(value)) return false;
    for (const review of value) {
      const body = (review as { body?: unknown }).body;
      if (typeof body === "string" && body.includes(marker)) return true;
    }
    if (value.length < REVIEW_SCAN_PER_PAGE) return false;
  }
  return false;
}

function readComments(params: Record<string, unknown>): InlineComment[] {
  const raw = params["comments"];
  if (raw === undefined) return [];
  if (!Array.isArray(raw)) {
    throw new IntegrationActionError(`"comments" must be an array`, true);
  }
  return raw.map((item, i) => {
    const record = item as Record<string, unknown>;
    const path = record["path"];
    const line = record["line"];
    const body = record["body"];
    if (typeof path !== "string" || typeof line !== "number" || typeof body !== "string") {
      throw new IntegrationActionError(`comments[${i}] needs path, line, body`, true);
    }
    return {
      findingId: `automation:${i}`,
      path,
      line,
      side: typeof record["side"] === "string" ? (record["side"] as string) : "RIGHT",
      body,
    };
  });
}

export const BUILTIN_ACTIONS: BuiltinActionTable = {
  "github.post_pr_review": async (params, ctx, deps) => {
    const repo = requireString(params, "repo");
    const prNumber = params["prNumber"];
    if (typeof prNumber !== "number") {
      throw new IntegrationActionError(`missing required integer "prNumber"`, true);
    }
    const summary = requireString(params, "summary");
    const marker = ctx.marker ?? `<!-- engrams-automation:${ctx.runId}:${ctx.stepPath} -->`;
    if (await markerAlreadyPosted(deps.runOp, repo, prNumber, marker)) {
      return { posted: false, already_posted: true };
    }
    const poster = deps.githubPoster();
    const commitId =
      optionalString(params, "commitId") ?? (await poster.fetchPrContext(repo, prNumber)).headSha;
    const result = await poster.postReview({
      repo,
      prNumber,
      commitId,
      // The generic action has no findings ledger to re-quote on the 422
      // fallback; the caller-authored summary is the body either way.
      buildSummary: () => `${summary}\n\n${marker}`,
      comments: readComments(params),
    });
    return {
      posted: result.posted,
      inline_posted: result.inlinePosted,
      already_posted: false,
      ...(result.githubReviewId !== undefined ? { github_review_id: result.githubReviewId } : {}),
    };
  },

  "slack.post_message": async (params, _ctx, deps) => {
    const client = await deps.slackClient();
    const threadTs = optionalString(params, "threadTs");
    const response = await client.chat.postMessage({
      channel: requireString(params, "channel"),
      text: requireString(params, "text"),
      ...(threadTs !== undefined ? { thread_ts: threadTs } : {}),
    });
    return { ts: response.ts ?? null, channel: response.channel ?? null };
  },

  "slack.update_message": async (params, _ctx, deps) => {
    const client = await deps.slackClient();
    const response = await client.chat.update({
      channel: requireString(params, "channel"),
      ts: requireString(params, "ts"),
      text: requireString(params, "text"),
    });
    return { ts: response.ts ?? null, channel: response.channel ?? null };
  },

  "linear.create_issue": async (params, ctx, deps) => {
    // CreateLinearIssueInput takes a caller-minted id (the in-repo spec-ticket
    // precedent), so client_id idempotency is real: a retried create adopts.
    const issue = await deps.linearClient().createIssue({
      id: actionClientId(ctx.runId, ctx.stepPath),
      teamId: requireString(params, "teamId"),
      title: requireString(params, "title"),
      description: optionalString(params, "description") ?? "",
    });
    return { id: issue.id, identifier: issue.identifier, url: issue.url };
  },
};
