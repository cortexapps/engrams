/** Review blocks (ADR 0119 D7, graduated to the catalog 2026-09).
 *
 * The PR-review product logic that is not a generic primitive — the durable
 * review record (targets, passes, findings), the worker staging, and the
 * policy gate — lives in these four blocks. Writing the engrams review
 * ledger is legitimate product surface, the same way `create_session` is:
 * any automation may open a pass, and it shows on the Reviews page. Each
 * block wraps the extracted review control plane (reviews/control-plane.ts)
 * through an injected seam, so the legacy DBOS graph and the built-in share
 * one library and one set of tables. The Reviews product UI reads those
 * tables unchanged.
 *
 * Four blocks:
 *   - review_open_pass   — identity + pass row (+ stamps the run id)
 *   - review_stage       — stage a worker session for a phase and compose
 *                          its prompt (returned as an output for send_prompt)
 *   - review_settle      — decision + finding settlement; returns the payload
 *                          for the generic github.post_pr_review action
 *   - review_close_pass  — the finalize-hook arm (contract 2): mark the pass
 *                          failed/halted with the sticky status comment +
 *                          activity event, or tear a superseded pass down —
 *                          byte-for-byte the legacy failReview/haltReview/
 *                          cleanup path
 *
 * Supersession itself is the ENGINE's concurrency policy (supersede keyed on
 * the PR url). The built-in reaches the legacy graph's failure/halt/supersede
 * behaviour through `settings.onFinalize` hooks that run review_close_pass.
 */

import { z } from "zod";

import {
  makeReviewControlPlane,
  type ReviewControlPlane,
  type ReviewPostPayload,
} from "../../../reviews/control-plane.ts";
import { isCompletePrContext, type PrContext } from "../../../reviews/pr-context.ts";
import { REVIEW_CATEGORIES, type ReviewCategory } from "../../../reviewers/render.ts";
import { isHumanReviewTrigger } from "../../../reviews/review-trigger.ts";
import { makeReviewStore } from "../../../db/reviews.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";

export const REVIEW_OPEN_PASS_TYPE = "review_open_pass";
export const REVIEW_STAGE_TYPE = "review_stage";
export const REVIEW_SETTLE_TYPE = "review_settle";
export const REVIEW_CLOSE_PASS_TYPE = "review_close_pass";

/** The slice of the control plane the blocks use; injected for tests. */
export type ReviewBlockControlPlane = Pick<
  ReviewControlPlane,
  | "resolvePrHeads"
  | "resolveReviewTarget"
  | "createReviewPass"
  | "bootstrapFinderSession"
  | "bootstrapVerifierSession"
  | "composeFinderPrompt"
  | "composeVerifierPrompt"
  | "markPhasePrompted"
  | "decideReviewResults"
  | "cleanupSupersededReview"
  | "failReview"
  | "haltReview"
>;

export interface ReviewBlockDeps {
  controlPlane(): ReviewBlockControlPlane;
  reviews(): { setAutomationRunId(reviewId: string, runId: string): Promise<void> };
}

let runtimeDeps: ReviewBlockDeps | null = null;

/** Production wiring installs the real control plane; tests install fakes. */
export function setReviewBlockDeps(deps: ReviewBlockDeps | null): void {
  runtimeDeps = deps;
}

function deps(): ReviewBlockDeps {
  if (runtimeDeps) return runtimeDeps;
  let cp: ReviewBlockControlPlane | undefined;
  let store: ReturnType<typeof makeReviewStore> | undefined;
  runtimeDeps = {
    controlPlane: () => (cp ??= makeReviewControlPlane()),
    reviews: () => (store ??= makeReviewStore()),
  };
  return runtimeDeps;
}

// ---------------------------------------------------------------------------
// review_open_pass
// ---------------------------------------------------------------------------

const nullableString = z.string().nullable().optional();
const nullableInt = z.number().int().nullable().optional();

/** PrContext as it rides block config (Liquid renders every field from the
 * trigger payload; `providerUpdatedAt` travels as an ISO string). */
export const prContextConfigSchema = z.object({
  providerId: nullableString,
  title: nullableString,
  author: nullableString,
  state: nullableString,
  url: nullableString,
  providerUpdatedAt: nullableString,
  headBranch: nullableString,
  baseBranch: nullableString,
  additions: nullableInt,
  deletions: nullableInt,
  changedFiles: nullableInt,
});

/** A SHA that a template may have rendered EMPTY (a comment command carries
 * no pull_request object): empty means absent, and absent means "resolve
 * from GitHub". */
const optionalSha = z
  .string()
  .optional()
  .transform((v) => (v === undefined || v === "" ? undefined : v))
  .pipe(z.string().regex(/^[0-9a-fA-F]{7,40}$/).optional());

export const openReviewPassConfigSchema = z.object({
  provider: z.string().min(1).default("github"),
  repo: z.string().regex(/^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/),
  prNumber: z.coerce.number().int().positive(),
  trigger: z.enum(["opened", "synchronize", "ready_for_review", "command", "retry", "dispatch"]),
  headSha: optionalSha,
  baseSha: optionalSha,
  pr: prContextConfigSchema.optional(),
});
export type OpenReviewPassConfig = z.infer<typeof openReviewPassConfigSchema>;

function prContextFromConfig(pr: z.infer<typeof prContextConfigSchema>): PrContext {
  const at = pr.providerUpdatedAt ?? null;
  const parsed = at === null ? null : new Date(at);
  return {
    providerId: pr.providerId ?? null,
    title: pr.title ?? null,
    author: pr.author ?? null,
    state: pr.state ?? null,
    url: pr.url ?? null,
    providerUpdatedAt: parsed !== null && !Number.isNaN(parsed.getTime()) ? parsed : null,
    headBranch: pr.headBranch ?? null,
    baseBranch: pr.baseBranch ?? null,
    additions: pr.additions ?? null,
    deletions: pr.deletions ?? null,
    changedFiles: pr.changedFiles ?? null,
  };
}

async function executeOpenReviewPass(
  config: OpenReviewPassConfig,
  runId: string,
): Promise<BlockOutcome> {
  const cp = deps().controlPlane();

  // Same gate as review-ingress: resolve from GitHub ONLY when the trigger
  // did not carry complete facts (comment commands, retries).
  const sentPr = config.pr ? prContextFromConfig(config.pr) : undefined;
  let resolved: { headSha: string; baseSha: string; pr: PrContext };
  if (config.headSha && config.baseSha && sentPr && isCompletePrContext(sentPr)) {
    resolved = { headSha: config.headSha, baseSha: config.baseSha, pr: sentPr };
  } else {
    resolved = await cp.resolvePrHeads(config.repo, config.prNumber);
  }
  const { headSha, baseSha, pr } = resolved;
  if (pr.providerId === null) {
    return {
      kind: "error",
      code: "review_no_provider_id",
      message: `${config.provider} returned no id for ${config.repo}#${config.prNumber}`,
      retryable: false,
    };
  }

  const target = await cp.resolveReviewTarget({
    provider: config.provider,
    providerId: pr.providerId,
    repo: config.repo,
    number: config.prNumber,
    title: pr.title,
    author: pr.author,
    state: pr.state,
    url: pr.url,
    providerUpdatedAt: pr.providerUpdatedAt,
  });

  const pass = await cp.createReviewPass({
    provider: config.provider,
    repo: config.repo,
    prNumber: config.prNumber,
    trigger: config.trigger,
    targetId: target.targetId,
    headSha,
    baseSha,
    headBranch: pr.headBranch,
    baseBranch: pr.baseBranch,
    additions: pr.additions,
    deletions: pr.deletions,
    changedFiles: pr.changedFiles,
    deduplicateSameHead: !isHumanReviewTrigger(config.trigger),
  });
  if (pass.kind === "deduplicated") {
    return {
      kind: "end_run",
      status: "filtered",
      reason: `same head ${headSha.slice(0, 7)} already under review`,
    };
  }

  await deps().reviews().setAutomationRunId(pass.reviewId, runId);

  return {
    kind: "ok",
    outputs: {
      review_id: pass.reviewId,
      task_id: pass.taskId,
      target_id: target.targetId,
      head_sha: headSha,
      base_sha: baseSha,
      pr_url: pr.url,
      ...(pass.supersededReviewId !== undefined
        ? { superseded_review_id: pass.supersededReviewId }
        : {}),
      deduplicated: false,
    },
  };
}

// ---------------------------------------------------------------------------
// review_stage
// ---------------------------------------------------------------------------
//
// Why a block and not write_files + Liquid: the staged files are
// product logic, not templates — reviewer markdown rendered from the lens
// catalog, prior-findings.json assembled from the review record AND the
// author's GitHub replies, candidates.json from the findings table — and the
// finder prompt depends on review-record reads (re-review scoping after a
// push, prior-pass context). Pushing that through Liquid would re-implement
// the control plane in a template. The block stages (the clone is the
// visible run_command block before it), composes the phase prompt, and
// returns it as an output for the generic send_prompt block, so the prompt
// delivery itself stays a property-editable block. The org-facing knobs
// (categories, instructions, focus) are block config and tunable.

export const reviewStageConfigSchema = z.object({
  phase: z.enum(["finder", "verifier"]),
  reviewId: z.string().min(1),
  sessionId: z.string().min(1),
  repo: z.string().regex(/^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/),
  prNumber: z.coerce.number().int().positive(),
  headSha: z.string().regex(/^[0-9a-fA-F]{7,40}$/),
  baseSha: z.string().regex(/^[0-9a-fA-F]{7,40}$/).optional(),
  /** Tunable: the lenses the reviewer applies (default: all six). */
  enabledCategories: z.array(z.string().min(1)).optional(),
  /** Tunable: org guidance rendered into the reviewer brief. */
  orgInstructions: z.string().optional(),
  /** Tunable (finder only): a focus directive appended to the prompt. */
  focus: z.string().optional(),
});
export type ReviewStageConfig = z.infer<typeof reviewStageConfigSchema>;

function isReviewCategory(value: string): value is ReviewCategory {
  return (REVIEW_CATEGORIES as readonly string[]).includes(value);
}

async function executeReviewStage(config: ReviewStageConfig): Promise<BlockOutcome> {
  const cp = deps().controlPlane();
  const categories = config.enabledCategories?.filter(isReviewCategory);
  const common = {
    reviewId: config.reviewId,
    repo: config.repo,
    prNumber: config.prNumber,
    headSha: config.headSha,
    ...(categories && categories.length > 0 ? { enabledCategories: categories } : {}),
    ...(config.orgInstructions !== undefined && config.orgInstructions !== ""
      ? { orgInstructions: config.orgInstructions }
      : {}),
    skipClone: true,
  };

  if (config.phase === "finder") {
    await cp.bootstrapFinderSession(config.sessionId, common);
    const { prompt, mergeBase } = await cp.composeFinderPrompt(config.sessionId, {
      reviewId: config.reviewId,
      repo: config.repo,
      prNumber: config.prNumber,
      headSha: config.headSha,
      baseSha: config.baseSha ?? "",
      ...(config.focus !== undefined && config.focus !== "" ? { focus: config.focus } : {}),
    });
    await cp.markPhasePrompted(config.reviewId, "finder");
    return { kind: "ok", outputs: { phase: "finder", prompt, merge_base: mergeBase } };
  }

  await cp.bootstrapVerifierSession(config.sessionId, common);
  const { prompt } = cp.composeVerifierPrompt({ repo: config.repo, prNumber: config.prNumber });
  await cp.markPhasePrompted(config.reviewId, "verifier");
  return { kind: "ok", outputs: { phase: "verifier", prompt, merge_base: "" } };
}

// ---------------------------------------------------------------------------
// review_settle
// ---------------------------------------------------------------------------

export const reviewPolicyGateConfigSchema = z.object({
  reviewId: z.string().min(1),
  /** The verifier session to retire first (best-effort), if any. */
  sessionId: z.string().min(1).optional(),
});
export type ReviewPolicyGateConfig = z.infer<typeof reviewPolicyGateConfigSchema>;

async function executeReviewPolicyGate(config: ReviewPolicyGateConfig): Promise<BlockOutcome> {
  const payload: ReviewPostPayload = await deps()
    .controlPlane()
    .decideReviewResults(config.reviewId, config.sessionId ? { sessionId: config.sessionId } : {});
  // ReviewPostPayload is already snake_case; it IS the block's outputs.
  return { kind: "ok", outputs: { ...payload } };
}

// ---------------------------------------------------------------------------
// review_close_pass — the finalize-hook arm
// ---------------------------------------------------------------------------

export const reviewFinalizeConfigSchema = z.object({
  reviewId: z.string().min(1),
  /** Which legacy terminal path to take. Maps from the run's terminal status
   * in the built-in's hooks: failed|deadline → "failed", halted → "halted",
   * superseded → "superseded". */
  outcome: z.enum(["failed", "halted", "superseded"]),
  /** Recorded on the review's activity log (failed only); the built-in passes
   * `${{ run.error }}`. */
  reason: z.string().max(2000).optional(),
});
export type ReviewFinalizeConfig = z.infer<typeof reviewFinalizeConfigSchema>;

/** Worker sessions are ended by the engine's own finalize (the built-in runs
 * with endSessionsOnFinish), so no sessionId is passed: the legacy helpers'
 * teardown is a no-op without one and the rest — status transition, activity
 * event, sticky ❌/halted comment — is exactly what the legacy graph did. */
export async function executeReviewFinalize(config: ReviewFinalizeConfig): Promise<BlockOutcome> {
  const plane = deps().controlPlane();
  switch (config.outcome) {
    case "failed":
      await plane.failReview(config.reviewId, config.reason ? { reason: config.reason } : {});
      break;
    case "halted":
      await plane.haltReview(config.reviewId, {});
      break;
    case "superseded":
      await plane.cleanupSupersededReview(config.reviewId, {});
      break;
  }
  return { kind: "ok", outputs: { review_id: config.reviewId, outcome: config.outcome } };
}

export function registerReviewBlocks(): void {
  registerBlock<ReviewFinalizeConfig>({
    type: REVIEW_CLOSE_PASS_TYPE,
    refusesDryRun: true,
    outputs: ["review_id", "outcome"],
    configSchema: reviewFinalizeConfigSchema,
    async execute(config) {
      return executeReviewFinalize(config);
    },
  });

  registerBlock<OpenReviewPassConfig>({
    type: REVIEW_OPEN_PASS_TYPE,
    refusesDryRun: true,
    outputs: [
      "review_id",
      "task_id",
      "target_id",
      "head_sha",
      "base_sha",
      "pr_url",
      "superseded_review_id",
      "deduplicated",
    ],
    configSchema: openReviewPassConfigSchema,
    async execute(config, ctx) {
      return executeOpenReviewPass(config, ctx.runId);
    },
  });

  registerBlock<ReviewStageConfig>({
    type: REVIEW_STAGE_TYPE,
    refusesDryRun: true,
    outputs: ["phase", "prompt", "merge_base"],
    configSchema: reviewStageConfigSchema,
    async execute(config) {
      return executeReviewStage(config);
    },
  });

  registerBlock<ReviewPolicyGateConfig>({
    type: REVIEW_SETTLE_TYPE,
    refusesDryRun: true,
    outputs: [
      "review_id",
      "repo",
      "pr_number",
      "commit_id",
      "summary_md",
      "comments",
      "to_post_count",
      "ui_only_count",
    ],
    configSchema: reviewPolicyGateConfigSchema,
    async execute(config) {
      return executeReviewPolicyGate(config);
    },
  });

}
