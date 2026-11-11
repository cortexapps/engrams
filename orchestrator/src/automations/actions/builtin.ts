/** The builtin action table (ADR 0119 D5).
 *
 * Connector `actions[]` entries with `execute.kind: "builtin"` name an id
 * from connectors/builtin-actions-allowlist.ts; this module holds the TS
 * implementations. Every provider call rides an injected seam, so tests
 * never touch the network or the coordinator.
 */

import { ForgeRequestError, type GitForge, type InlineComment } from "../../forges/forge.ts";
import { forgeProviders, requireForge } from "../../forges/registry.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type RunOpDeps,
} from "../../integrations/run-op.ts";
import { SLACK_ACTIONS, type SlackChatClient } from "../../providers/slack/actions.ts";
import { getSlackClient } from "../../providers/slack/client.ts";
import {
  makeLinearIssueClient,
  type LinearIssueClient,
} from "../../integrations/linear-issues.ts";
import { actionClientId } from "./client-id.ts";
import { IntegrationActionError } from "./errors.ts";
import { optionalString, requireString } from "./params.ts";

type RunIntegrationOp = typeof defaultRunIntegrationOp;

export interface BuiltinActionContext {
  runId: string;
  stepPath: string;
  /** Rendered marker for marker_comment idempotency; null otherwise. */
  marker: string | null;
}

export interface BuiltinActionDeps {
  forge(provider: string): GitForge;
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
    forge: (provider) => requireForge(provider, { runIntegrationOp: runOp }),
    slackClient: () => getSlackClient(runOpDeps),
    linearClient: () => makeLinearIssueClient({ runIntegrationOp: runOp }),
  };
}

async function hasReviewWithMarker(
  forge: GitForge,
  repo: string,
  prNumber: number,
  marker: string,
): Promise<boolean> {
  try {
    return await forge.hasReviewWithMarker(repo, prNumber, marker);
  } catch (error) {
    if (!(error instanceof ForgeRequestError)) throw error;
    const { status } = error;
    throw new IntegrationActionError(
      `list pull request reviews returned ${status}`,
      status >= 400 && status < 500 && status !== 429,
      status,
    );
  }
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
    // A caller with a findings ledger (the PR-review built-in's review_settle
    // output) names each comment's finding so the ledger row and the posted
    // comment stay linked; a caller without one gets a positional id.
    const findingId = typeof record["finding_id"] === "string" ? record["finding_id"] : `automation:${i}`;
    const startLine = typeof record["start_line"] === "number" ? record["start_line"] : undefined;
    return {
      findingId,
      path,
      line,
      ...(startLine !== undefined ? { startLine } : {}),
      side: typeof record["side"] === "string" ? (record["side"] as string) : "RIGHT",
      body,
    };
  });
}

/** Post a review on a pull request of `provider`'s forge. */
function postPrReview(provider: string): BuiltinActionFn {
  return async (params, ctx, deps) => {
    const forge = deps.forge(provider);
    const repo = requireString(params, "repo");
    const prNumber = params["prNumber"];
    if (typeof prNumber !== "number") {
      throw new IntegrationActionError(`missing required integer "prNumber"`, true);
    }
    const summary = requireString(params, "summary");
    // The body for a review the forge takes only without its inline
    // comments: a caller with a findings ledger passes one that quotes every
    // finding. Without it the summary is the body either way.
    const fallbackSummary = optionalString(params, "fallbackSummary") ?? summary;
    const marker = ctx.marker ?? `<!-- engrams-automation:${ctx.runId}:${ctx.stepPath} -->`;
    // The guard for a crash between a posted review and its checkpoint.
    if (await hasReviewWithMarker(forge, repo, prNumber, marker)) {
      return { posted: false, already_posted: true };
    }
    const commitId =
      optionalString(params, "commitId") ?? (await forge.fetchPullRequest(repo, prNumber)).headSha;
    const result = await forge.postReview({
      repo,
      prNumber,
      commitId,
      buildSummary: (inlinePosted) => `${inlinePosted ? summary : fallbackSummary}\n\n${marker}`,
      comments: readComments(params),
    });
    return {
      posted: result.posted,
      inline_posted: result.inlinePosted,
      already_posted: false,
      // The output name is in stored automations; it is the forge's review id.
      ...(result.providerReviewId !== undefined ? { github_review_id: result.providerReviewId } : {}),
    };
  };
}

export const BUILTIN_ACTIONS: BuiltinActionTable = {
  ...Object.fromEntries(
    forgeProviders().map((provider) => [`${provider}.post_pr_review`, postPrReview(provider)]),
  ),

  ...SLACK_ACTIONS,

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
