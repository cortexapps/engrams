/** Orchestrator-native ReviewService (ADR 0100). */

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { getSessionFromHeaders } from "../auth/session.ts";
import { getDb } from "../db/client.ts";
import {
  makeReviewStore,
  type FindingCounts,
  type ReviewFindingRow,
  type ReviewListRow,
  type ReviewRow,
  type ReviewStore,
  type ReviewVerdictRow,
} from "../db/reviews.ts";
import {
  ReviewService,
  type Review as ReviewProto,
  type ReviewFinding as ReviewFindingProto,
  type ReviewVerdict as ReviewVerdictProto,
} from "../gen/engram/app/v1/review_pb.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string };
} | null>;

export interface ReviewDeps {
  getSession?: GetSession;
  reviews?: ReviewStore;
  db?: ReturnType<typeof getDb>;
}

async function requireUser(
  ctx: HandlerContext,
  getSession: GetSession,
): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
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
    ...(row.summaryMd != null ? { summaryMd: row.summaryMd } : {}),
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
  } as ReviewVerdictProto;
}

export function registerReviews(router: ConnectRouter, deps?: ReviewDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.reviews;
  const reviews = (): ReviewStore =>
    (store ??= makeReviewStore(deps?.db ?? getDb()));

  router.service(ReviewService, {
    async listReviews(req, ctx) {
      await requireUser(ctx, getSession);
      const repo = req.repo?.trim();
      const rows = await reviews().listReviews(repo ? { repo } : {});
      return { reviews: rows.map(listReviewToProto) };
    },

    async getReview(req, ctx) {
      await requireUser(ctx, getSession);
      const detail = await reviews().getReview(req.id);
      if (!detail) throw new ConnectError("not found", Code.NotFound);
      return {
        review: reviewToProto(detail.review, findingCounts(detail.findings)),
        findings: detail.findings.map(findingToProto),
        verdicts: detail.verdicts.map(verdictToProto),
      };
    },
  });
}
