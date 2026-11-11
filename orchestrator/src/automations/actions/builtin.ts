/** The builtin action table (ADR 0119 D5).
 *
 * Connector `actions[]` entries with `execute.kind: "builtin"` name an id
 * from connectors/builtin-actions-allowlist.ts; this module holds the TS
 * implementations. Every provider call rides an injected seam (runOp, the
 * Slack SDK client, the Linear GraphQL client) so tests never touch the
 * network or the coordinator.
 */

import { ForgeRequestError, type GitForge, type InlineComment } from "../../forges/forge.ts";
import { forgeProviders, requireForge } from "../../forges/registry.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type RunOpDeps,
} from "../../integrations/run-op.ts";
import type { KnownBlock } from "@slack/types";

import { getSlackClient } from "../../integrations/slack.ts";
import { buildMessageBlocks } from "../../integrations/slack-blocks.ts";
import { replyText, type SlackReply } from "../../integrations/slack-message-text.ts";
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
      blocks: KnownBlock[];
    }): Promise<{ ts?: string; channel?: string }>;
  };
  conversations: {
    join(args: { channel: string }): Promise<{ channel?: { id?: string } }>;
    replies(args: {
      channel: string;
      ts: string;
      oldest?: string;
      limit?: number;
    }): Promise<{ messages?: SlackReply[] }>;
  };
}

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

  "slack.join_channel": async (params, _ctx, deps) => {
    // Rung 2 companion: message events only flow where the bot is a channel
    // MEMBER, so a flow that claims `slack:<channel>` joins first.
    // conversations.join is idempotent for public channels (already-in →
    // ok with a warning); private channels need a human /invite and fail
    // here with Slack's typed error.
    const client = await deps.slackClient();
    const response = await client.conversations.join({
      channel: requireString(params, "channel"),
    });
    return { channel: response.channel?.id ?? null };
  },

  "slack.list_replies": async (params, _ctx, deps) => {
    // One page of a thread, every content surface of each message rendered
    // to text (text, blocks, attachments, files — the legacy `replyText`).
    // The caller folds it into a prompt; this action only reads.
    const client = await deps.slackClient();
    const oldest = optionalString(params, "oldest");
    const response = await client.conversations.replies({
      channel: requireString(params, "channel"),
      ts: requireString(params, "threadTs"),
      ...(oldest !== undefined ? { oldest } : {}),
      limit: 500,
    });
    return {
      // `subtype` is on every Slack message the API returns but not on the
      // SDK's reply type; widen the one field honestly.
      messages: (response.messages ?? []).map((m: SlackReply & { subtype?: string }) => ({
        ts: m.ts ?? "",
        user: m.user ?? "",
        bot_id: m.bot_id ?? "",
        subtype: m.subtype ?? "",
        text: replyText(m),
      })),
    };
  },

  "slack.update_message": async (params, _ctx, deps) => {
    const client = await deps.slackClient();
    // The message becomes exactly this text: its blocks are replaced too, so
    // updating an interactive card (a choice) removes its buttons.
    const text = requireString(params, "text");
    const response = await client.chat.update({
      channel: requireString(params, "channel"),
      ts: requireString(params, "ts"),
      text,
      blocks: buildMessageBlocks(text),
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
