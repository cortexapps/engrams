import { DBOS } from "@dbos-inc/dbos-sdk";
import { z } from "zod";

import {
  makeReviewSessionStore,
  type ReviewSessionStore,
} from "../db/review-sessions.ts";
import {
  makeReviewStore,
  type ReviewRow,
  type ReviewStore,
} from "../db/reviews.ts";
import { makeAutomationEngineStore } from "../db/automations.ts";
import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  inboxKeys,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import { log as rootLog } from "../log.ts";
import {
  REVIEW_TOPIC,
  type ReviewInbox,
} from "../workflows/review-inbox.ts";
import {
  tools,
  type ToolContext,
  type ToolProtocolError,
  type ToolRegistry,
} from "./registry.ts";

export const PR_REVIEW_CAPABILITY = "engram:pr_review";

const log = rootLog.child({ component: "review-tools" });

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
const NOT_FINDING_PHASE: ToolProtocolError = {
  error: "review is not in the finding phase",
};
const NOT_VERIFYING_PHASE: ToolProtocolError = {
  error: "review is not in the verifying phase",
};

// Bounds on submitted findings. A runaway or prompt-injected finder must not be
// able to flood the record or produce a body that blows GitHub's 65,536-char
// comment limit (which would 422 the batch and oversize the fallback too). The
// per-review cap is generous — real reviews land well under it.
const MAX_PATH_LEN = 1024;
const MAX_TITLE_LEN = 500;
const MAX_BODY_LEN = 20_000;
const MAX_SUGGESTED_FIX_LEN = 20_000;
const MAX_EVIDENCE_ITEMS = 100;
const MAX_EVIDENCE_ITEM_LEN = 1024;
const MAX_SUMMARY_LEN = 20_000;
const MAX_REASONING_LEN = 20_000;
const PER_REVIEW_FINDING_CAP = 200;
const FINDING_CAP_REACHED: ToolProtocolError = {
  error: `finding cap reached (${PER_REVIEW_FINDING_CAP} per review)`,
};

export interface ReviewToolDeps {
  reviews: ReviewStore;
  reviewSessions?: Pick<ReviewSessionStore, "find">;
  notify?: (
    destinationId: string,
    message: ReviewInbox,
    topic: string,
    idempotencyKey: string,
  ) => Promise<void>;
  /** ADR 0119: a worker session owned by a built-in automation run is bound in
   *  automation_session; its phase signals go to the run's mailbox instead. */
  findAutomationBinding?: (sessionId: string) => Promise<{ runId: string } | null>;
  notifyAutomation?: (
    runId: string,
    message: AutomationInbox,
    idempotencyKey: string,
  ) => Promise<void>;
}

/** The signal names the review built-in's send_prompt waits park on. */
export const REVIEW_PHASE_SIGNALS = {
  finder: "finder_done",
  verifier: "verifier_done",
} as const;

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
  let reviewSessionStore = deps?.reviewSessions;
  const reviewSessions = () => (
    reviewSessionStore ??= makeReviewSessionStore()
  );
  const notify = deps?.notify ?? (async (
    destinationId: string,
    message: ReviewInbox,
    topic: string,
    idempotencyKey: string,
  ) => {
    await DBOS.send<ReviewInbox>(
      destinationId,
      message,
      topic,
      idempotencyKey,
    );
  });
  let engineStore: ReturnType<typeof makeAutomationEngineStore> | undefined;
  const findAutomationBinding = deps?.findAutomationBinding ?? (async (sessionId: string) => {
    engineStore ??= makeAutomationEngineStore();
    const binding = await engineStore.findSessionBinding(sessionId);
    return binding === null ? null : { runId: binding.runId };
  });
  const notifyAutomation = deps?.notifyAutomation ?? (async (
    runId: string,
    message: AutomationInbox,
    idempotencyKey: string,
  ) => {
    assertIdempotencyKey(idempotencyKey);
    await DBOS.send<AutomationInbox>(runId, message, AUTOMATION_TOPIC, idempotencyKey);
  });
  const notifyPhaseDone = async (
    ctx: ToolContext,
    role: "finder" | "verifier",
    payload: Record<string, unknown> = {},
  ): Promise<void> => {
    try {
      // ADR 0119: a session the built-in review automation owns signals its
      // run directly; the engine's send_prompt wait parks on this name. The
      // payload lands in steps.<block>.signal, so the built-in's branch reads
      // e.g. candidate_count without a store round-trip.
      const automation = await findAutomationBinding(ctx.sessionId);
      if (automation !== null) {
        const name = REVIEW_PHASE_SIGNALS[role];
        await notifyAutomation(
          automation.runId,
          { kind: "signal", name, sessionId: ctx.sessionId, payload },
          inboxKeys.signal(ctx.sessionId, name, ctx.toolCallId),
        );
        return;
      }
      // legacy path — deleted in phase 4.7
      const binding = await reviewSessions().find(ctx.sessionId);
      if (!binding || binding.role !== role) return;
      await notify(
        binding.reviewWorkflowId,
        { kind: "phase_done", role },
        REVIEW_TOPIC,
        `review:${ctx.sessionId}:${role}-done`,
      );
    } catch (err) {
      // Persisting the tool result is authoritative. The stream fallback and
      // workflow deadline keep a notification outage from failing the tool.
      log.warn(
        { sessionId: ctx.sessionId, role, err },
        "review phase completion notification failed",
      );
    }
  };

  registry.register({
    name: "submit_finding",
    description:
      "Submit a review finding to the durable review record. Anything not submitted through this tool does not exist.",
    input: z.object({
      path: z.string().max(MAX_PATH_LEN),
      start_line: z.number().int().optional(),
      end_line: z.number().int().optional(),
      side: z.enum(["LEFT", "RIGHT"]).optional(),
      category: CategorySchema,
      severity: SeveritySchema,
      confidence: ConfidenceSchema,
      title: z.string().max(MAX_TITLE_LEN),
      body_md: z.string().max(MAX_BODY_LEN),
      suggested_fix: z.string().max(MAX_SUGGESTED_FIX_LEN).optional(),
      evidence: z.array(z.string().max(MAX_EVIDENCE_ITEM_LEN)).max(MAX_EVIDENCE_ITEMS),
    }),
    output: z.object({ recorded: z.boolean(), finding_id: z.string() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;
      if (active.status !== "queued" && active.status !== "finding") {
        return NOT_FINDING_PHASE;
      }
      if (await reviews.countFindings(active.id) >= PER_REVIEW_FINDING_CAP) {
        return FINDING_CAP_REACHED;
      }

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
    input: z.object({ summary_md: z.string().max(MAX_SUMMARY_LEN) }),
    output: z.object({ recorded: z.boolean() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;
      if (active.status !== "queued" && active.status !== "finding") {
        return NOT_FINDING_PHASE;
      }
      await reviews.setFinderSummary(active.id, args.summary_md);
      // Every finding is still a candidate at finder_done (verdicts come
      // later), so the finding count IS the candidate count.
      const candidateCount = await reviews.countFindings(active.id);
      await notifyPhaseDone(ctx, "finder", { candidate_count: candidateCount });
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
      reasoning: z.string().max(MAX_REASONING_LEN),
    }),
    output: z.object({ recorded: z.boolean() }),
    handling: "handled",
    execution: "sync",
    capability: PR_REVIEW_CAPABILITY,
    handler: async (ctx, args) => {
      const active = await activeReview(ctx, reviews);
      if (isToolError(active)) return active;
      if (active.status !== "verifying") return NOT_VERIFYING_PHASE;

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
      const updated = await reviews.getReview(active.id);
      if (updated) {
        const candidateIds = new Set(
          updated.findings
            .filter((finding) => finding.state === "candidate")
            .map((finding) => finding.id),
        );
        const judgedCandidates = new Set(
          updated.verdicts
            .map((verdict) => verdict.findingId)
            .filter((findingId) => candidateIds.has(findingId)),
        );
        if (
          candidateIds.size > 0
          && judgedCandidates.size === candidateIds.size
        ) {
          await notifyPhaseDone(ctx, "verifier");
        }
      }
      return { recorded: true };
    },
  });
}
