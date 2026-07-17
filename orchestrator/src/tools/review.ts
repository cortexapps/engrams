import { z } from "zod";

import {
  makeReviewStore,
  type ReviewRow,
  type ReviewStore,
} from "../db/reviews.ts";
import {
  tools,
  type ToolContext,
  type ToolProtocolError,
  type ToolRegistry,
} from "./registry.ts";

export const PR_REVIEW_CAPABILITY = "engram:pr_review";

const CategorySchema = z.enum([
  "security-privacy",
  "stability-availability",
  "data-integrity-integration",
  "functional-correctness",
  "performance-scalability",
  "maintainability-quality",
]);
const SeveritySchema = z.enum(["critical", "high", "medium", "low"]);
const ConfidenceSchema = z.enum(["high", "medium", "low"]);
const NO_ACTIVE_REVIEW: ToolProtocolError = {
  error: "no active review for this session",
};

export interface ReviewToolDeps {
  reviews: ReviewStore;
}

async function activeReview(
  ctx: ToolContext,
  reviews: ReviewStore,
): Promise<ReviewRow | ToolProtocolError> {
  if (ctx.taskId == null) return NO_ACTIVE_REVIEW;
  return (await reviews.getActiveReviewForTask(ctx.taskId)) ?? NO_ACTIVE_REVIEW;
}

function isToolError(
  value: ReviewRow | ToolProtocolError,
): value is ToolProtocolError {
  return "error" in value;
}

/** Register the capability-gated tools used by PR review worker sessions. */
export function registerReviewTools(
  registry: ToolRegistry = tools,
  deps?: ReviewToolDeps,
): void {
  const reviews = deps?.reviews ?? makeReviewStore();

  registry.register({
    name: "submit_finding",
    description:
      "Submit a review finding to the durable review record. Anything not submitted through this tool does not exist.",
    input: z.object({
      path: z.string(),
      start_line: z.number().int().optional(),
      end_line: z.number().int().optional(),
      side: z.enum(["LEFT", "RIGHT"]).optional(),
      category: CategorySchema,
      severity: SeveritySchema,
      confidence: ConfidenceSchema,
      title: z.string(),
      body_md: z.string(),
      suggested_fix: z.string().optional(),
      evidence: z.array(z.string()),
    }),
    output: z.object({ recorded: z.boolean(), finding_id: z.string() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;

      const inserted = await reviews.insertFinding({
        reviewId: active.id,
        path: args.path,
        startLine: args.start_line ?? null,
        endLine: args.end_line ?? null,
        side: args.side ?? null,
        category: args.category,
        severity: args.severity,
        confidence: args.confidence,
        title: args.title,
        bodyMd: args.body_md,
        suggestedFix: args.suggested_fix ?? null,
        evidence: args.evidence,
        state: "candidate",
        verdictReason: null,
        githubThreadId: null,
        resolution: null,
        sessionId: ctx.sessionId,
        toolCallId: ctx.toolCallId,
      });
      return { recorded: true, finding_id: inserted.id };
    },
  });

  registry.register({
    name: "finder_done",
    description: "Record the finder phase summary for the active review.",
    input: z.object({ summary_md: z.string() }),
    output: z.object({ recorded: z.boolean() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;
      await reviews.setFinderSummary(active.id, args.summary_md);
      return { recorded: true };
    },
  });

  registry.register({
    name: "submit_verdict",
    description: "Record the verifier's first judgment for a finding in the active review.",
    input: z.object({
      finding_id: z.string().uuid(),
      verdict: z.enum(["confirmed", "refuted"]),
      confidence: ConfidenceSchema,
      reasoning: z.string(),
    }),
    output: z.object({ recorded: z.boolean() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;

      const detail = await reviews.getReview(active.id);
      if (!detail?.findings.some((finding) => finding.id === args.finding_id)) {
        return { error: "finding does not belong to the active review" };
      }

      await reviews.insertVerdict({
        findingId: args.finding_id,
        verdict: args.verdict,
        confidence: args.confidence,
        reasoning: args.reasoning,
        sessionId: ctx.sessionId,
        toolCallId: ctx.toolCallId,
      });
      return { recorded: true };
    },
  });
}
