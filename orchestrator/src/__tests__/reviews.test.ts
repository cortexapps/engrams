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
  ReviewEventRow,
  ReviewFindingRow,
  ReviewListQuery,
  ReviewListRow,
  ReviewRow,
  ReviewStore,
  ReviewVerdictRow,
} from "../db/reviews.ts";
import { ReviewService } from "../gen/engram/app/v1/review_pb.ts";
import { registerReviews } from "../rpc/reviews.ts";
import type { ReviewEnrollmentSync } from "../db/review-enrollment-sync.ts";

const REVIEW_ID = "00000000-0000-4000-8000-000000000001";
const FINDING_ID = "00000000-0000-4000-8000-000000000002";
const CREATED_AT = new Date("2026-07-17T10:11:12.123Z");
const UPDATED_AT = new Date("2026-07-17T10:12:13.456Z");

function reviewRow(overrides: Partial<ReviewRow> = {}): ReviewRow {
  return {
    id: REVIEW_ID,
    targetId: "target-1",
    provider: "github",
    repo: "openai/engrams",
    prNumber: 100,
    taskId: "task-review-1",
    headSha: "head-sha",
    baseSha: "base-sha",
    trigger: "dispatch",
    status: "verifying",
    githubReviewId: null,
    finderSessionId: null,
    verifierSessionId: null,
    automationRunId: null,
    summaryMd: "Finder summary",
    providerId: null,
    prUrl: null,
    prTitle: null,
    prAuthor: null,
    headBranch: null,
    baseBranch: null,
    prState: null,
    additions: null,
    deletions: null,
    changedFiles: null,
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
  listCalls: ReviewListQuery[];
}

function makeStore(
  detail: ReviewDetail | null,
  events: ReviewEventRow[] = [],
): FakeReviewStore {
  const listCalls: ReviewListQuery[] = [];
  const listRow = (): ReviewListRow | null =>
    detail
      ? {
          ...detail.review,
          findingCounts: {
            critical: 0,
            high: 1,
            medium: 0,
            low: 0,
            total: 1,
          },
        }
      : null;
  return {
    listCalls,
    async claimTargetId() {
      return null;
    },
    async upsertTarget() {
      return { id: "target-1" };
    },
    async getTargetForRefresh() {
      return null;
    },
    async beginReviewPass() {
      throw new Error("unused");
    },
    async listPriorPasses() {
      return [];
    },
    async getReview(id) {
      return detail?.review.id === id ? detail : null;
    },
    async recordEvent() {},
    async listEvents() {
      return events;
    },
    async listReviews(query) {
      listCalls.push(query);
      const row = listRow();
      const hit = row && (!query.repos?.length || query.repos.includes(row.repo));
      return {
        reviews: hit ? [row] : [],
        totalCount: hit ? 1 : 0,
        facets: {
          repos: row ? [row.repo] : [],
          authors: [],
          prStates: [],
          statuses: row ? [row.status] : [],
        },
      };
    },
    async listPasses() {
      const row = listRow();
      return row ? [row] : [];
    },
    async getActiveReviewForAutomationRun() {
      return null;
    },
    async setGithubReviewId() {},
    async getActiveReviewForTarget() {
      return null;
    },
    async getActiveReviewByCoordinate() {
      return null;
    },
    async insertFinding() {
      throw new Error("unused");
    },
    async countFindings() {
      return 0;
    },
    async deleteFindingsForSession() {},
    async insertVerdict() {
      throw new Error("unused");
    },
    async setFinderSummary() {},
    async setReviewSessionId() {},
    async setAutomationRunId() {},
    async updateReviewStatus() {
      return true;
    },
    async updateFindingState() {},
    async finalizeReview() {
      return true;
    },
  };
}

function spawn(
  store: ReviewStore,
  authenticated = true,
  role = "user",
  enrollments?: FakeEnrollmentStore,
  profileExists = true,
  overrides?: {
    enrollmentSync?: ReviewEnrollmentSync;
    retryAutomation?: (automationRunId: string) => Promise<string>;
  },
) {
  // A default sync so the enrollment path resolves offline: it writes the
  // fake store the way the production sync writes the row.
  const defaultSync: ReviewEnrollmentSync = enrollments
    ? recordingSync(enrollments).sync
    : {
        async enroll() {
          throw new Error("test spawned without an enrollment store");
        },
        async unenroll() {
          throw new Error("test spawned without an enrollment store");
        },
      };
  const transport = createRouterTransport((router) =>
    registerReviews(router, {
      getSession: async () =>
        authenticated ? { user: { id: "review-user", role } } : null,
      reviews: store,
      ...(enrollments != null ? { enrollments } : {}),
      profiles: {
        get: async (id) => profileExists ? { id } : null,
      },
      enrollmentSync: overrides?.enrollmentSync ?? defaultSync,
      ...(overrides?.retryAutomation != null ? { retryAutomation: overrides.retryAutomation } : {}),
    }),
  );
  return createClient(ReviewService, transport);
}

interface FakeEnrollmentStore extends EnrollmentStore {
  rows: Map<string, EnrollmentRow>;
}

function makeEnrollmentStore(rows: EnrollmentRow[]): FakeEnrollmentStore {
  const stored = new Map(rows.map((row) => [row.repo, row]));
  return {
    rows: stored,
    async list() {
      return [...stored.values()];
    },
    async get(repo) {
      return stored.get(repo) ?? null;
    },
  };
}

/** A sync over the fake store: the one writer, recording what it wrote. */
function recordingSync(store: FakeEnrollmentStore) {
  const enrolls: EnrollmentInput[] = [];
  const unenrolls: string[] = [];
  const sync: ReviewEnrollmentSync = {
    async enroll(input) {
      enrolls.push(input);
      const now = new Date("2026-07-17T12:00:00Z");
      const row = { ...input, createdAt: now, updatedAt: now };
      store.rows.set(input.repo, row);
      return { kind: "applied", enrollment: row };
    },
    async unenroll(repo) {
      unenrolls.push(repo);
      store.rows.delete(repo);
    },
  };
  return { sync, enrolls, unenrolls };
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
    const response = await spawn(store).listReviews({
      repos: ["openai/engrams"],
      pageSize: 25,
      page: 3,
    });

    expect(store.listCalls).toEqual([
      {
        repos: ["openai/engrams"],
        search: "",
        authors: [],
        prStates: [],
        statuses: [],
        severities: [],
        page: 3,
        pageSize: 25,
      },
    ]);
    expect(response.totalCount).toBe(1);
    expect(response.facets).toMatchObject({ repos: ["openai/engrams"], statuses: ["verifying"] });
    expect(response.reviews[0]).toMatchObject({
      id: REVIEW_ID,
      repo: "openai/engrams",
      prNumber: 100,
      taskId: "task-review-1",
      status: "verifying",
      active: true,
      humanTrigger: true,
      summaryMd: "Finder summary",
      findingCounts: { high: 1, total: 1 },
    });
    expect(response.reviews[0]?.githubReviewId).toBeUndefined();
    expect(timestampDate(response.reviews[0]!.createdAt!)).toEqual(CREATED_AT);
    expect(timestampDate(response.reviews[0]!.updatedAt!)).toEqual(UPDATED_AT);
  });

  test("GetReview returns the durable row, findings, verdicts and passes", async () => {
    const response = await spawn(makeStore(detail)).getReview({ id: REVIEW_ID });

    expect(response.review?.findingCounts).toMatchObject({ high: 1, total: 1 });
    // The pass history rides the detail, not the paged list.
    expect(response.passes.map((pass) => pass.id)).toEqual([REVIEW_ID]);
    expect(response.passes[0]?.findingCounts).toMatchObject({ high: 1, total: 1 });
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

  test("GetReview surfaces the review activity log, oldest first", async () => {
    const events: ReviewEventRow[] = [
      {
        id: "e1",
        reviewId: REVIEW_ID,
        kind: "queued",
        detail: null,
        createdAt: new Date("2026-07-17T10:00:00Z"),
      },
      {
        id: "e2",
        reviewId: REVIEW_ID,
        kind: "cloning",
        detail: "finder",
        createdAt: new Date("2026-07-17T10:00:05Z"),
      },
    ];
    const response = await spawn(makeStore(detail, events)).getReview({ id: REVIEW_ID });

    expect(response.events.map((e) => e.kind)).toEqual(["queued", "cloning"]);
    expect(response.events[1]).toMatchObject({ kind: "cloning", detail: "finder" });
    expect(response.events[1]?.createdAt).toBeDefined();
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

  test("RetryReview re-admits the built-in run behind the review", async () => {
    const store = makeStore({
      ...detail,
      review: reviewRow({ status: "failed", automationRunId: "autorun:b1:github:d1" }),
    });
    const enrollments = makeEnrollmentStore([{
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "off",
      profileId: null,
      createdAt: CREATED_AT,
      updatedAt: UPDATED_AT,
    }]);
    const retried: string[] = [];
    const response = await spawn(store, true, "user", enrollments, true, {
      retryAutomation: async (runId) => {
        retried.push(runId);
        return "autorun:b1:retry:x";
      },
    }).retryReview({ id: REVIEW_ID });

    expect(retried).toEqual(["autorun:b1:github:d1"]);
    expect(response.workflowId).toBe("autorun:b1:retry:x");
  });

  test("RetryReview on a pass with no automation run is a FailedPrecondition", async () => {
    const store = makeStore({
      ...detail,
      review: reviewRow({ status: "failed", automationRunId: null }),
    });
    const enrollments = makeEnrollmentStore([{
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "off",
      profileId: null,
      createdAt: CREATED_AT,
      updatedAt: UPDATED_AT,
    }]);
    await expectConnectError(
      spawn(store, true, "user", enrollments, true).retryReview({ id: REVIEW_ID }),
      Code.FailedPrecondition,
    );
  });

  test("UpsertEnrollment and DeleteEnrollment go through the one-transaction sync", async () => {
    const enrollmentStore = makeEnrollmentStore([]);
    const recorder = recordingSync(enrollmentStore);
    const admin = spawn(makeStore(null), true, "admin", enrollmentStore, true, { enrollmentSync: recorder.sync });
    const response = await admin.upsertEnrollment({
      repo: "openai/engrams",
      triggerMode: "auto",
      autofix: "manual",
    });
    expect(response.enrollment?.repo).toBe("openai/engrams");
    // The RPC never writes the row itself: the sync writes the row and the
    // built-in's map together.
    expect(recorder.enrolls).toEqual([
      { repo: "openai/engrams", triggerMode: "auto", autofix: "manual", profileId: null },
    ]);

    await admin.deleteEnrollment({ repo: "openai/engrams" });
    expect(recorder.unenrolls).toEqual(["openai/engrams"]);
    expect(await enrollmentStore.get("openai/engrams")).toBeNull();
  });

  test("UpsertEnrollment before the built-in is seeded is a FailedPrecondition", async () => {
    const enrollmentStore = makeEnrollmentStore([]);
    const enrollmentSync: ReviewEnrollmentSync = {
      async enroll() {
        return { kind: "not_seeded" };
      },
      async unenroll() {},
    };
    const admin = spawn(makeStore(null), true, "admin", enrollmentStore, true, { enrollmentSync });
    await expectConnectError(
      admin.upsertEnrollment({ repo: "openai/engrams", triggerMode: "auto", autofix: "off" }),
      Code.FailedPrecondition,
    );
  });

  test("UpsertEnrollment without profile_id keeps the stored override; an empty one clears it", async () => {
    const enrollmentStore = makeEnrollmentStore([{
      repo: "openai/engrams",
      triggerMode: "manual",
      autofix: "off",
      profileId: "profile-1",
      createdAt: new Date("2026-07-17T10:00:00Z"),
      updatedAt: new Date("2026-07-17T10:00:00Z"),
    }]);
    const admin = spawn(makeStore(null), true, "admin", enrollmentStore);
    const kept = await admin.upsertEnrollment({ repo: "openai/engrams", triggerMode: "auto", autofix: "off" });
    expect(kept.enrollment?.profileId).toBe("profile-1");
    const cleared = await admin.upsertEnrollment({ repo: "openai/engrams", triggerMode: "auto", autofix: "off", profileId: "" });
    expect(cleared.enrollment?.profileId ?? "").toBe("");
  });

  test("RetryReview requires authentication", async () => {
    await expectConnectError(
      spawn(makeStore(detail), false).retryReview({ id: REVIEW_ID }),
      Code.Unauthenticated,
    );
  });

  test("RetryReview returns NotFound for an unknown review", async () => {
    await expectConnectError(
      spawn(makeStore(null)).retryReview({ id: REVIEW_ID }),
      Code.NotFound,
    );
  });

  test("RetryReview fails when the repo is no longer enrolled", async () => {
    await expectConnectError(
      spawn(makeStore(detail), true, "user", makeEnrollmentStore([]), true, {
        retryAutomation: async () => {
          throw new Error("must not re-admit");
        },
      }).retryReview({ id: REVIEW_ID }),
      Code.FailedPrecondition,
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
    const recorder = recordingSync(adminStore);
    const admin = spawn(makeStore(null), true, "admin", adminStore, true, { enrollmentSync: recorder.sync });
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
    expect(recorder.unenrolls).toEqual(["openai/engrams"]);
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

  test("rejects an enrollment repo that isn't owner/name form", async () => {
    const admin = spawn(makeStore(null), true, "admin", makeEnrollmentStore([]));
    for (const repo of ["openai/engrams/", "engrams", "owner/name/extra", "bad repo/name"]) {
      await expectConnectError(admin.upsertEnrollment({
        repo,
        triggerMode: "manual",
        autofix: "off",
      }), Code.InvalidArgument);
    }
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
