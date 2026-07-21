/** Orchestrator-native ReviewService (ADR 0100). */

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { getDb } from "../db/client.ts";
import {
  makeEnrollmentStore,
  type EnrollmentRow,
  type EnrollmentStore,
  type ReviewAutofix,
  type ReviewTriggerMode,
} from "../db/enrollments.ts";
import { makeProfileStore } from "../db/profiles.ts";
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
import {
  ReviewService,
  type RepoEnrollment,
  type Review as ReviewProto,
  type ReviewEvent as ReviewEventProto,
  type ReviewFinding as ReviewFindingProto,
  type ReviewVerdict as ReviewVerdictProto,
} from "../gen/engram/app/v1/review_pb.ts";
import {
  dispatchReview,
  type DispatchReviewInput,
  type DispatchReviewResult,
} from "../workflows/dispatch-review.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null };
} | null>;

export interface ReviewDeps {
  getSession?: GetSession;
  reviews?: ReviewStore;
  enrollments?: EnrollmentStore;
  profiles?: { get(id: string): Promise<{ id: string } | null> };
  db?: ReturnType<typeof getDb>;
  dispatch?: (input: DispatchReviewInput) => Promise<DispatchReviewResult>;
  randomUUID?: () => string;
}

async function requireUser(
  ctx: HandlerContext,
  getSession: GetSession,
): Promise<{ id: string; role: string }> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  return { id: session.user.id, role: session.user.role ?? "user" };
}

function enrollmentToProto(row: EnrollmentRow): RepoEnrollment {
  return {
    repo: row.repo,
    triggerMode: row.triggerMode,
    autofix: row.autofix,
    ...(row.profileId != null ? { profileId: row.profileId } : {}),
    createdAt: timestampFromDate(row.createdAt),
    updatedAt: timestampFromDate(row.updatedAt),
  } as RepoEnrollment;
}

const TRIGGER_MODES: ReadonlySet<string> = new Set(["auto", "manual"]);
const AUTOFIX_MODES: ReadonlySet<string> = new Set(["auto", "manual", "off"]);

// GitHub owner/repo charset ([A-Za-z0-9._-]). An enrollment whose repo doesn't
// match a webhook `full_name` (e.g. a trailing slash) is silently dead, so
// reject it at the door instead of storing an un-triggerable row.
const REPO_RE = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;

function isTriggerMode(value: string): value is ReviewTriggerMode {
  return TRIGGER_MODES.has(value);
}

function isAutofixMode(value: string): value is ReviewAutofix {
  return AUTOFIX_MODES.has(value);
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
  let enrollmentStore = deps?.enrollments;
  const enrollments = (): EnrollmentStore =>
    (enrollmentStore ??= makeEnrollmentStore(deps?.db ?? getDb()));
  let profileStore = deps?.profiles;
  const profiles = (): { get(id: string): Promise<{ id: string } | null> } =>
    (profileStore ??= makeProfileStore(deps?.db ?? getDb()));
  const dispatch = deps?.dispatch
    ?? ((input: DispatchReviewInput) =>
      dispatchReview({ enrollments: enrollments() }, input));
  const randomUUID = deps?.randomUUID ?? (() => crypto.randomUUID());

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
      const events = await reviews().listEvents(req.id);
      return {
        review: reviewToProto(detail.review, findingCounts(detail.findings)),
        findings: detail.findings.map(findingToProto),
        verdicts: detail.verdicts.map(verdictToProto),
        events: events.map(eventToProto),
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
      const { repo, prNumber } = detail.review;
      // A fresh dispatch mints a new review record (a terminal review is not
      // "active") and a successor workflow epoch, re-reviewing the PR's current
      // head. The old record stays as history.
      const result = await dispatch({
        repo,
        prNumber,
        trigger: "retry",
        idempotencyKey: randomUUID(),
      });
      if (!result.enrolled || !result.workflowId) {
        throw new ConnectError("repo is not enrolled", Code.FailedPrecondition);
      }
      return {
        workflowId: result.workflowId,
        ...(result.reviewId != null ? { reviewId: result.reviewId } : {}),
      };
    },

    async listEnrollments(_req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("read", "Review")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      return {
        enrollments: (await enrollments().list()).map(enrollmentToProto),
      };
    },

    async upsertEnrollment(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("manage", "Review")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      const repo = req.repo.trim();
      if (!repo) throw new ConnectError("repo is required", Code.InvalidArgument);
      if (!REPO_RE.test(repo)) {
        throw new ConnectError(
          "repo must be in owner/name form",
          Code.InvalidArgument,
        );
      }
      if (!isTriggerMode(req.triggerMode)) {
        throw new ConnectError(
          "trigger_mode must be auto or manual",
          Code.InvalidArgument,
        );
      }
      if (!isAutofixMode(req.autofix)) {
        throw new ConnectError(
          "autofix must be auto, manual, or off",
          Code.InvalidArgument,
        );
      }
      const profileId = req.profileId?.trim() || null;
      if (profileId != null && !(await profiles().get(profileId))) {
        throw new ConnectError("profile_id does not exist", Code.InvalidArgument);
      }
      const enrollment = await enrollments().upsert({
        repo,
        triggerMode: req.triggerMode,
        autofix: req.autofix,
        profileId,
      });
      return { enrollment: enrollmentToProto(enrollment) };
    },

    async deleteEnrollment(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("manage", "Review")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      const repo = req.repo.trim();
      if (!repo) throw new ConnectError("repo is required", Code.InvalidArgument);
      await enrollments().delete(repo);
      return {};
    },
  });
}
