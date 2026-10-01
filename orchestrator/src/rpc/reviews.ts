/** Orchestrator-native ReviewService (ADR 0100). */

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireUser } from "./require.ts";
import { getDb } from "../db/client.ts";
import {
  makeEnrollmentStore,
  type EnrollmentRow,
  type EnrollmentStore,
  type ReviewAutofix,
  type ReviewTriggerMode,
} from "../db/enrollments.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { makeReviewEnrollmentSync, type ReviewEnrollmentSync } from "../db/review-enrollment-sync.ts";
import { retryAutomationReview } from "../reviews/automation-review.ts";
import { log as rootLog } from "../log.ts";
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
  type RepoEnrollment,
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
  enrollments?: EnrollmentStore;
  profiles?: { get(id: string): Promise<{ id: string } | null> };
  db?: ReturnType<typeof getDb>;
  /** The one writer of the enrollment row + the built-in's repos map (one
   *  transaction). */
  enrollmentSync?: ReviewEnrollmentSync;
  /** Admit a fresh built-in run for the review's original trigger. */
  retryAutomation?: (automationRunId: string) => Promise<string>;
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
  let enrollmentStore = deps?.enrollments;
  const enrollments = (): EnrollmentStore =>
    (enrollmentStore ??= makeEnrollmentStore(deps?.db ?? getDb()));
  let profileStore = deps?.profiles;
  const profiles = (): { get(id: string): Promise<{ id: string } | null> } =>
    (profileStore ??= makeProfileStore(deps?.db ?? getDb()));
  const enrollmentLog = rootLog.child({ component: "review-enrollment" });
  const enrollmentSync = (): ReviewEnrollmentSync =>
    deps?.enrollmentSync ?? makeReviewEnrollmentSync(deps?.db ?? getDb());
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
      if (!(await enrollments().get(repo))) {
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
      // An omitted profile_id keeps the stored override; an explicit empty
      // string clears it. The product's dialog no longer carries the field,
      // and a trigger/autofix edit must never wipe a per-repo profile.
      const stored = await enrollments().get(repo);
      const profileId =
        req.profileId === undefined ? (stored?.profileId ?? null) : req.profileId.trim() || null;
      if (profileId != null && profileId !== stored?.profileId && !(await profiles().get(profileId))) {
        throw new ConnectError("profile_id does not exist", Code.InvalidArgument);
      }
      // Enrolling IS a write to the PR-review built-in: the row and the
      // built-in's repos map (+ enabled on the first repo) land in one
      // transaction, so a crash between them leaves no repo shown enrolled
      // that nothing reviews.
      const result = await enrollmentSync().enroll({
        repo,
        triggerMode: req.triggerMode,
        autofix: req.autofix,
        profileId,
      });
      switch (result.kind) {
        case "not_seeded":
          // The seeder runs fire-and-forget at boot; an enrollment before it
          // lands is an operator error, not a silent no-op.
          throw new ConnectError(
            "the PR-review built-in is not seeded yet; retry after the orchestrator finishes booting",
            Code.FailedPrecondition,
          );
        case "applied":
          enrollmentLog.info({ repo }, "review enrollment mirrored into the built-in");
          return { enrollment: enrollmentToProto(result.enrollment) };
      }
    },

    async deleteEnrollment(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("manage", "Review")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      const repo = req.repo.trim();
      if (!repo) throw new ConnectError("repo is required", Code.InvalidArgument);
      // The map entry and the row go in one transaction.
      await enrollmentSync().unenroll(repo);
      return {};
    },
  });
}
