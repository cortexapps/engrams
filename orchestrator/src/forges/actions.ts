/** The built-in actions every git forge has: `post_pr_review`,
 * `upsert_pr_comment` and `update_pr_comment`. The forge's connector declares
 * them; the client is the provider's `GitForge`. */

import {
  defineActions,
  type BuiltinActionContext,
  type BuiltinActionFn,
} from "../automations/actions/builtin.ts";
import { IntegrationActionError } from "../automations/actions/errors.ts";
import { optionalString, requireInteger, requireString } from "../automations/actions/params.ts";
import { runIntegrationOp } from "../integrations/run-op.ts";
import { ForgeRequestError, type GitForge, type InlineComment } from "./forge.ts";
import { requireForge } from "./registry.ts";

/** A forge failure as an action failure. */
async function forgeCall<T>(forge: GitForge, call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (error) {
    if (!(error instanceof ForgeRequestError)) throw error;
    throw new IntegrationActionError(error.message, forge.isPermanentError(error), error.status);
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
    const side = record["side"] ?? "RIGHT";
    if (side !== "LEFT" && side !== "RIGHT") {
      throw new IntegrationActionError(`comments[${i}].side must be LEFT or RIGHT`, true);
    }
    // A caller with a findings ledger names each comment's finding, so the
    // ledger row and the posted comment stay linked. A caller without one
    // gets a positional id.
    const findingId = typeof record["finding_id"] === "string" ? record["finding_id"] : `automation:${i}`;
    const startLine = typeof record["start_line"] === "number" ? record["start_line"] : undefined;
    return { findingId, path, line, ...(startLine !== undefined ? { startLine } : {}), side, body };
  });
}

function requireMarker(ctx: BuiltinActionContext): string {
  if (ctx.marker === null) {
    throw new IntegrationActionError("this action needs marker_comment idempotency", true);
  }
  return ctx.marker;
}

const postPrReview: BuiltinActionFn<GitForge> = async (params, ctx, forge) => {
  const repo = requireString(params, "repo");
  const prNumber = requireInteger(params, "prNumber");
  const summary = requireString(params, "summary");
  // The body for a review whose inline comments are not all on the diff: a
  // caller with a findings ledger passes one that quotes every finding.
  // Without it the summary is the body either way.
  const fallbackSummary = optionalString(params, "fallbackSummary") ?? summary;
  const marker = ctx.marker ?? `<!-- engrams-automation:${ctx.runId}:${ctx.stepPath} -->`;
  return forgeCall(forge, async () => {
    // The guard for a crash between a posted review and its checkpoint.
    if (await forge.hasReviewWithMarker(repo, prNumber, marker)) {
      return { posted: false, already_posted: true };
    }
    const commitId =
      optionalString(params, "commitId") ?? (await forge.fetchPullRequest(repo, prNumber)).headSha;
    const result = await forge.postReview({
      repo,
      prNumber,
      commitId,
      buildSummary: (inlinePosted) => `${inlinePosted ? summary : fallbackSummary}\n\n${marker}`,
      marker,
      comments: readComments(params),
    });
    return {
      posted: true,
      inline_posted: result.inlinePosted,
      inline_finding_ids: result.inlineFindingIds,
      already_posted: false,
      ...(result.providerReviewId !== undefined
        ? {
            review_id: result.providerReviewId,
            // Stored automations read the id under this name.
            github_review_id: result.providerReviewId,
          }
        : {}),
    };
  });
};

/** Post a comment on a pull request once per marker: a later call with the
 * same marker returns the id of the comment an earlier call posted. */
const upsertPrComment: BuiltinActionFn<GitForge> = async (params, ctx, forge) => {
  const repo = requireString(params, "repo");
  const number = requireInteger(params, "number");
  const marker = requireMarker(ctx);
  const body = `${requireString(params, "body")}\n\n${marker}`;
  return forgeCall(forge, async () => ({
    commentId:
      (await forge.findCommentWithMarker(repo, number, marker)) ??
      (await forge.createComment(repo, number, body)),
  }));
};

/** Rewrite a comment that `upsert_pr_comment` posted. The marker goes back
 * on the new body, so a later upsert still finds the comment. */
const updatePrComment: BuiltinActionFn<GitForge> = async (params, ctx, forge) => {
  const commentId = requireString(params, "commentId");
  const body = `${requireString(params, "body")}\n\n${requireMarker(ctx)}`;
  await forgeCall(forge, () => forge.updateComment(requireString(params, "repo"), commentId, body));
  return { commentId };
};

export const forgeActions = defineActions<GitForge>({
  client: (provider, runOpDeps) =>
    requireForge(provider, {
      runIntegrationOp: (name, request) => runIntegrationOp(name, request, runOpDeps),
    }),
  actions: {
    post_pr_review: postPrReview,
    upsert_pr_comment: upsertPrComment,
    update_pr_comment: updatePrComment,
  },
});
