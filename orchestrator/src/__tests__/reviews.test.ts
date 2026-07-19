import { describe, expect, test } from "bun:test";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Code,
  ConnectError,
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";

import type {
  EnrollmentInput,
  EnrollmentRow,
  EnrollmentStore,
} from "../db/enrollments.ts";
import type {
  ReviewDetail,
  ReviewFindingRow,
  ReviewListRow,
  ReviewRow,
  ReviewStore,
  ReviewVerdictRow,
} from "../db/reviews.ts";
import { ReviewService } from "../gen/engram/app/v1/review_pb.ts";
import { registerReviews } from "../rpc/reviews.ts";

const REVIEW_ID = "00000000-0000-4000-8000-000000000001";
const FINDING_ID = "00000000-0000-4000-8000-000000000002";
const CREATED_AT = new Date("2026-07-17T10:11:12.123Z");
const UPDATED_AT = new Date("2026-07-17T10:12:13.456Z");

function reviewRow(overrides: Partial<ReviewRow> = {}): ReviewRow {
  return {
    id: REVIEW_ID,
    repo: "openai/engrams",
    prNumber: 100,
    taskId: "task-review-1",
    headSha: "head-sha",
    baseSha: "base-sha",
    trigger: "dispatch",
    status: "verifying",
    githubReviewId: null,
    statusCommentId: null,
    summaryMd: "Finder summary",
    createdAt: CREATED_AT,
    updatedAt: UPDATED_AT,
    ...overrides,
  };
}

function findingRow(): ReviewFindingRow {
  return {
    id: FINDING_ID,
    reviewId: REVIEW_ID,
    path: "orchestrator/src/index.ts",
    startLine: 10,
    endLine: 12,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "high",
    confidence: "high",
    title: "Wrong branch",
    bodyMd: "This branch returns the wrong result.",
    suggestedFix: "Return the expected value.",
    evidence: ["orchestrator/src/index.ts"],
    state: "confirmed",
    verdictReason: "Reproduced from the caller.",
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: "finder-call",
    createdAt: CREATED_AT,
  };
}

function verdictRow(): ReviewVerdictRow {
  return {
    id: "00000000-0000-4000-8000-000000000003",
    findingId: FINDING_ID,
    verdict: "confirmed",
    confidence: "high",
    reasoning: "The failing path is reachable.",
    sessionId: "verifier-session",
    toolCallId: "verifier-call",
    createdAt: UPDATED_AT,
  };
}

interface FakeReviewStore extends ReviewStore {
  listCalls: Array<{ repo?: string }>;
}

function makeStore(detail: ReviewDetail | null): FakeReviewStore {
  const listCalls: Array<{ repo?: string }> = [];
  return {
    listCalls,
    async createReview() {
      throw new Error("unused");
    },
    async getReview(id) {
      return detail?.review.id === id ? detail : null;
    },
    async listReviews(opts) {
      listCalls.push(opts);
      if (!detail || (opts.repo != null && detail.review.repo !== opts.repo)) return [];
      const row: ReviewListRow = {
        ...detail.review,
        findingCounts: {
          critical: 0,
          high: 1,
          medium: 0,
          low: 0,
          total: 1,
        },
      };
      return [row];
    },
    async getActiveReviewForTask() {
      return null;
    },
    async getActiveReviewForPr() {
      return null;
    },
    async insertFinding() {
      throw new Error("unused");
    },
    async insertVerdict() {
      throw new Error("unused");
    },
    async setFinderSummary() {},
    async setStatusCommentId() {},
    async updateReviewStatus() {},
    async updateFindingState() {},
    async finalizeReview() {},
  };
}

function spawn(
  store: ReviewStore,
  authenticated = true,
  role = "user",
  enrollments?: EnrollmentStore,
  profileExists = true,
) {
  const transport = createRouterTransport((router) =>
    registerReviews(router, {
      getSession: async () =>
        authenticated ? { user: { id: "review-user", role } } : null,
      reviews: store,
      ...(enrollments != null ? { enrollments } : {}),
      profiles: {
        get: async (id) => profileExists ? { id } : null,
      },
    }),
  );
  return createClient(ReviewService, transport);
}

interface FakeEnrollmentStore extends EnrollmentStore {
  upserts: EnrollmentInput[];
  deletes: string[];
}

function makeEnrollmentStore(rows: EnrollmentRow[]): FakeEnrollmentStore {
  const stored = new Map(rows.map((row) => [row.repo, row]));
  const upserts: EnrollmentInput[] = [];
  const deletes: string[] = [];
  return {
    upserts,
    deletes,
    async list() {
      return [...stored.values()];
    },
    async get(repo) {
      return stored.get(repo) ?? null;
    },
    async upsert(input) {
      upserts.push(input);
      const now = new Date("2026-07-17T12:00:00Z");
      const row = { ...input, createdAt: now, updatedAt: now };
      stored.set(input.repo, row);
      return row;
    },
    async delete(repo) {
      deletes.push(repo);
      stored.delete(repo);
    },
  };
}

async function expectConnectError(
  promise: Promise<unknown>,
  code: Code,
): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ConnectError(${Code[code]})`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
  }
}

describe("ReviewService", () => {
  const detail: ReviewDetail = {
    review: reviewRow(),
    findings: [findingRow()],
    verdicts: [verdictRow()],
  };

  test("ListReviews filters by repo and maps counts and timestamps", async () => {
    const store = makeStore(detail);
    const response = await spawn(store).listReviews({ repo: "openai/engrams" });

    expect(store.listCalls).toEqual([{ repo: "openai/engrams" }]);
    expect(response.reviews[0]).toMatchObject({
      id: REVIEW_ID,
      repo: "openai/engrams",
      prNumber: 100,
      taskId: "task-review-1",
      status: "verifying",
      summaryMd: "Finder summary",
      findingCounts: { high: 1, total: 1 },
    });
    expect(response.reviews[0]?.githubReviewId).toBeUndefined();
    expect(timestampDate(response.reviews[0]!.createdAt!)).toEqual(CREATED_AT);
    expect(timestampDate(response.reviews[0]!.updatedAt!)).toEqual(UPDATED_AT);
  });

  test("GetReview returns the durable row, findings, and verdicts", async () => {
    const response = await spawn(makeStore(detail)).getReview({ id: REVIEW_ID });

    expect(response.review?.findingCounts).toMatchObject({ high: 1, total: 1 });
    expect(response.findings[0]).toMatchObject({
      id: FINDING_ID,
      reviewId: REVIEW_ID,
      startLine: 10,
      category: "functional-correctness",
      state: "confirmed",
      evidence: ["orchestrator/src/index.ts"],
    });
    expect(response.verdicts[0]).toMatchObject({
      findingId: FINDING_ID,
      verdict: "confirmed",
      reasoning: "The failing path is reachable.",
      sessionId: "verifier-session",
    });
  });

  test("GetReview returns NotFound for an unknown id", async () => {
    await expectConnectError(
      spawn(makeStore(null)).getReview({ id: REVIEW_ID }),
      Code.NotFound,
    );
  });

  test("requires an authenticated org user", async () => {
    await expectConnectError(
      spawn(makeStore(detail), false).listReviews({}),
      Code.Unauthenticated,
    );
  });

  test("members can list enrollments with mapped timestamps", async () => {
    const createdAt = new Date("2026-07-17T10:00:00Z");
    const updatedAt = new Date("2026-07-17T11:00:00Z");
    const enrollments = makeEnrollmentStore([{
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "manual",
      profileId: "profile-1",
      createdAt,
      updatedAt,
    }]);
    const response = await spawn(
      makeStore(null),
      true,
      "user",
      enrollments,
    ).listEnrollments({});
    expect(response.enrollments[0]).toMatchObject({
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "manual",
      profileId: "profile-1",
    });
    expect(timestampDate(response.enrollments[0]!.createdAt!)).toEqual(createdAt);
    expect(timestampDate(response.enrollments[0]!.updatedAt!)).toEqual(updatedAt);
  });

  test("only admins can upsert and delete enrollments", async () => {
    const memberStore = makeEnrollmentStore([]);
    const member = spawn(makeStore(null), true, "user", memberStore);
    await expectConnectError(member.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "manual",
      autofix: "off",
    }), Code.PermissionDenied);
    await expectConnectError(
      member.deleteEnrollment({ repo: "openai/engrams" }),
      Code.PermissionDenied,
    );

    const adminStore = makeEnrollmentStore([]);
    const admin = spawn(makeStore(null), true, "admin", adminStore);
    const response = await admin.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "manual",
      profileId: "profile-1",
    });
    expect(response.enrollment).toMatchObject({
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "manual",
      profileId: "profile-1",
    });
    await admin.deleteEnrollment({ repo: "openai/engrams" });
    expect(adminStore.deletes).toEqual(["openai/engrams"]);
  });

  test("rejects invalid enrollment enums", async () => {
    const admin = spawn(
      makeStore(null),
      true,
      "admin",
      makeEnrollmentStore([]),
    );
    await expectConnectError(admin.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "sometimes",
      autofix: "off",
    }), Code.InvalidArgument);
    await expectConnectError(admin.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "manual",
      autofix: "always",
    }), Code.InvalidArgument);
  });

  test("rejects an unknown enrollment profile_id", async () => {
    const admin = spawn(
      makeStore(null),
      true,
      "admin",
      makeEnrollmentStore([]),
      false,
    );
    await expectConnectError(admin.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "manual",
      autofix: "off",
      profileId: "missing-profile",
    }), Code.InvalidArgument);
  });
});
