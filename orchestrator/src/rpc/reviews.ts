/** Orchestrator-native ReviewService (ADR 0100). */

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireUser } from "./require.ts";
import { makeAutomationStore } from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { retryAutomationReview } from "../reviews/automation-review.ts";
import { makeEnrolledRepos, type EnrolledRepos } from "../reviews/enrolled-repos.ts";
import {
  makeReviewStore,
  type FindingCounts,
  type ReviewEventRow,
  type ReviewFindingRow,
  type ReviewListRow,
  type ReviewRow,
  type ReviewStore,
  type ReviewVerdictRow,
} from "../db/reviews.ts";
import { isActiveReviewStatus } from "../db/schema.ts";
import { isHumanReviewTrigger } from "../reviews/review-trigger.ts";
import {
  ReviewService,
  type Review as ReviewProto,
  type ReviewEvent as ReviewEventProto,
  type ReviewFinding as ReviewFindingProto,
  type ReviewVerdict as ReviewVerdictProto,
} from "../gen/engram/app/v1/review_pb.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null };
} | null>;

export interface ReviewDeps {
  getSession?: GetSession;
  reviews?: ReviewStore;
  /** The PR-review built-in's `repos` input: which repos get reviews. */
  enrolledRepos?: Pick<EnrolledRepos, "get">;
  db?: ReturnType<typeof getDb>;
  /** Admit a fresh built-in run for the review's original trigger. */
  retryAutomation?: (automationRunId: string) => Promise<string>;
}

function findingCounts(findings: ReviewFindingRow[]): FindingCounts {
  const counts: FindingCounts = {
    critical: 0,
    high: 0,
    medium: 0,
    low: 0,
    total: findings.length,
  };
  for (const finding of findings) {
    if (finding.severity === "critical") counts.critical++;
    else if (finding.severity === "high") counts.high++;
    else if (finding.severity === "medium") counts.medium++;
    else if (finding.severity === "low") counts.low++;
  }
  return counts;
}

function reviewToProto(row: ReviewRow, counts: FindingCounts): ReviewProto {
  return {
    id: row.id,
    repo: row.repo,
    prNumber: row.prNumber,
    taskId: row.taskId,
    headSha: row.headSha,
    baseSha: row.baseSha,
    trigger: row.trigger,
    status: row.status,
    ...(row.githubReviewId != null ? { githubReviewId: row.githubReviewId } : {}),
    ...(row.finderSessionId != null ? { finderSessionId: row.finderSessionId } : {}),
    ...(row.verifierSessionId != null ? { verifierSessionId: row.verifierSessionId } : {}),
    ...(row.summaryMd != null ? { summaryMd: row.summaryMd } : {}),
    // ADR 0100 decision 9. Each field is emitted only when present, so a
    // pre-decision review arrives with none of them set and the UI falls back
    // to the PR's coordinates rather than rendering empty strings.
    targetId: row.targetId,
    provider: row.provider,
    ...(row.providerId != null ? { providerId: row.providerId } : {}),
    ...(row.prUrl != null ? { prUrl: row.prUrl } : {}),
    ...(row.prTitle != null ? { prTitle: row.prTitle } : {}),
    ...(row.prAuthor != null ? { prAuthor: row.prAuthor } : {}),
    ...(row.headBranch != null ? { headBranch: row.headBranch } : {}),
    ...(row.baseBranch != null ? { baseBranch: row.baseBranch } : {}),
    ...(row.prState != null ? { prState: row.prState } : {}),
    ...(row.automationRunId != null ? { automationRunId: row.automationRunId } : {}),
    ...(row.additions != null ? { additions: row.additions } : {}),
    ...(row.deletions != null ? { deletions: row.deletions } : {}),
    ...(row.changedFiles != null ? { changedFiles: row.changedFiles } : {}),
    active: isActiveReviewStatus(row.status),
    humanTrigger: isHumanReviewTrigger(row.trigger),
    createdAt: timestampFromDate(row.createdAt),
    updatedAt: timestampFromDate(row.updatedAt),
    findingCounts: counts,
  } as ReviewProto;
}

function listReviewToProto(row: ReviewListRow): ReviewProto {
  return reviewToProto(row, row.findingCounts);
}

function findingToProto(row: ReviewFindingRow): ReviewFindingProto {
  return {
    id: row.id,
    reviewId: row.reviewId,
    path: row.path,
    ...(row.startLine != null ? { startLine: row.startLine } : {}),
    ...(row.endLine != null ? { endLine: row.endLine } : {}),
    ...(row.side != null ? { side: row.side } : {}),
    category: row.category,
    severity: row.severity,
    confidence: row.confidence,
    title: row.title,
    bodyMd: row.bodyMd,
    ...(row.suggestedFix != null ? { suggestedFix: row.suggestedFix } : {}),
    evidence: row.evidence,
    state: row.state,
    ...(row.verdictReason != null ? { verdictReason: row.verdictReason } : {}),
    ...(row.githubThreadId != null ? { githubThreadId: row.githubThreadId } : {}),
    ...(row.resolution != null ? { resolution: row.resolution } : {}),
    sessionId: row.sessionId,
    createdAt: timestampFromDate(row.createdAt),
  } as ReviewFindingProto;
}

function verdictToProto(row: ReviewVerdictRow): ReviewVerdictProto {
  return {
    findingId: row.findingId,
    verdict: row.verdict,
    confidence: row.confidence,
    reasoning: row.reasoning,
    sessionId: row.sessionId,
  } as ReviewVerdictProto;
}

function eventToProto(row: ReviewEventRow): ReviewEventProto {
  return {
    id: row.id,
    reviewId: row.reviewId,
    kind: row.kind,
    ...(row.detail != null ? { detail: row.detail } : {}),
    createdAt: timestampFromDate(row.createdAt),
  } as ReviewEventProto;
}

export function registerReviews(router: ConnectRouter, deps?: ReviewDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.reviews;
  const reviews = (): ReviewStore =>
    (store ??= makeReviewStore(deps?.db ?? getDb()));
  let enrolled = deps?.enrolledRepos;
  const enrolledRepos = (): Pick<EnrolledRepos, "get"> =>
    (enrolled ??= makeEnrolledRepos(makeAutomationStore(deps?.db ?? getDb())));
  const retryAutomation =
    deps?.retryAutomation ?? ((runId: string) => retryAutomationReview(runId));

  router.service(ReviewService, {
    async listReviews(req, ctx) {
      await requireUser(ctx, getSession);
      const page = await reviews().listReviews({
        repos: req.repos,
        search: req.search,
        authors: req.authors,
        prStates: req.prStates,
        statuses: req.statuses,
        severities: req.severities,
        page: Math.max(1, req.page || 1),
        // page_size 0 = unpaginated; clamped otherwise, like ListTasks.
        pageSize: req.pageSize <= 0 ? 0 : Math.min(req.pageSize, 1000),
      });
      return {
        reviews: page.reviews.map(listReviewToProto),
        totalCount: page.totalCount,
        facets: page.facets,
      };
    },

    async getReview(req, ctx) {
      await requireUser(ctx, getSession);
      const detail = await reviews().getReview(req.id);
      if (!detail) throw new ConnectError("not found", Code.NotFound);
      const [events, passes] = await Promise.all([
        reviews().listEvents(req.id),
        reviews().listPasses(detail.review.targetId),
      ]);
      return {
        review: reviewToProto(detail.review, findingCounts(detail.findings)),
        findings: detail.findings.map(findingToProto),
        verdicts: detail.verdicts.map(verdictToProto),
        events: events.map(eventToProto),
        passes: passes.map(listReviewToProto),
      };
    },

    async retryReview(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("create", "Review")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      const id = req.id?.trim();
      if (!id) {
        throw new ConnectError("review id is required", Code.InvalidArgument);
      }
      const detail = await reviews().getReview(id);
      if (!detail) throw new ConnectError("not found", Code.NotFound);
      const { repo } = detail.review;
      if (!(await enrolledRepos().get(repo))) {
        throw new ConnectError("repo is not enrolled", Code.FailedPrecondition);
      }
      // A retry admits a fresh built-in run with the ORIGINAL run's trigger
      // (the review row links to it). A pass from before the automation
      // engine has no run to re-admit; a new push or @mention starts one.
      if (!detail.review.automationRunId) {
        throw new ConnectError(
          "this review predates the automation engine and cannot be retried; push or @mention to start a new pass",
          Code.FailedPrecondition,
        );
      }
      const runId = await retryAutomation(detail.review.automationRunId);
      return { workflowId: runId };
    },
  });
}
