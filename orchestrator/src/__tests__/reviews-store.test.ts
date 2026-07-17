import { describe, expect, test } from "bun:test";
import { eq, inArray } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  makeReviewStore,
  type CreateReviewInput,
  type ReviewFindingInput,
} from "../db/reviews.ts";
import {
  review as reviewTable,
  task as taskTable,
} from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

function reviewInput(
  taskId: string,
  overrides: Partial<CreateReviewInput> = {},
): CreateReviewInput {
  return {
    repo: "openai/engrams",
    prNumber: 100,
    taskId,
    headSha: "head-sha",
    baseSha: "base-sha",
    trigger: "dispatch",
    ...overrides,
  };
}

function findingInput(
  reviewId: string,
  overrides: Partial<ReviewFindingInput> = {},
): ReviewFindingInput {
  return {
    reviewId,
    path: "orchestrator/src/index.ts",
    startLine: 10,
    endLine: 12,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "critical",
    confidence: "high",
    title: "Wrong branch",
    bodyMd: "This branch returns the wrong result.",
    suggestedFix: null,
    evidence: ["orchestrator/src/index.ts"],
    state: "candidate",
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: "finder-call-1",
    ...overrides,
  };
}

describe("ReviewStore", () => {
  test.skipIf(!dbReachable)(
    "CRUD, replay anchors, first verdict, active resolution, and severity aggregates",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const taskId = `review-store-${crypto.randomUUID()}`;
      const repo = `review-store-${crypto.randomUUID()}/engrams`;
      const reviewIds: string[] = [];
      await db.insert(taskTable).values({
        id: taskId,
        type: "pr_review",
        title: "Review store test",
      });

      try {
        const firstReviewId = await store.createReview(reviewInput(taskId, { repo }));
        reviewIds.push(firstReviewId);
        await db
          .update(reviewTable)
          .set({ createdAt: new Date("2026-07-17T10:00:00Z") })
          .where(eq(reviewTable.id, firstReviewId));

        const firstFinding = await store.insertFinding(findingInput(firstReviewId));
        const replayedFinding = await store.insertFinding(findingInput(firstReviewId));
        expect(replayedFinding).toEqual({ id: firstFinding.id, replayed: true });

        await store.insertFinding(findingInput(firstReviewId, {
          severity: "low",
          toolCallId: "finder-call-2",
          path: "orchestrator/src/db/reviews.ts",
        }));

        const firstVerdict = await store.insertVerdict({
          findingId: firstFinding.id,
          verdict: "confirmed",
          confidence: "high",
          reasoning: "The failing path is reachable.",
          sessionId: "verifier-session",
          toolCallId: "verifier-call-1",
        });
        const replayedVerdict = await store.insertVerdict({
          findingId: firstFinding.id,
          verdict: "confirmed",
          confidence: "high",
          reasoning: "The failing path is reachable.",
          sessionId: "verifier-session",
          toolCallId: "verifier-call-1",
        });
        expect(replayedVerdict).toEqual({ id: firstVerdict.id, replayed: true });

        const secondWriter = await store.insertVerdict({
          findingId: firstFinding.id,
          verdict: "refuted",
          confidence: "low",
          reasoning: "A later writer must not replace the first verdict.",
          sessionId: "other-verifier-session",
          toolCallId: "verifier-call-2",
        });
        expect(secondWriter).toEqual({ id: firstVerdict.id, replayed: true });

        await store.setFinderSummary(firstReviewId, "Two candidates found.");
        await store.updateReviewStatus(firstReviewId, "finding");
        const detail = await store.getReview(firstReviewId);
        expect(detail?.review).toMatchObject({
          id: firstReviewId,
          status: "finding",
          summaryMd: "Two candidates found.",
        });
        expect(detail?.findings).toHaveLength(2);
        expect(detail?.verdicts).toHaveLength(1);

        const terminalReviewId = await store.createReview(reviewInput(taskId, {
          repo,
          prNumber: 101,
          status: "posted",
          headSha: "terminal-head",
        }));
        reviewIds.push(terminalReviewId);
        await db
          .update(reviewTable)
          .set({ createdAt: new Date("2026-07-17T11:00:00Z") })
          .where(eq(reviewTable.id, terminalReviewId));

        const activeReviewId = await store.createReview(reviewInput(taskId, {
          repo,
          prNumber: 102,
          status: "verifying",
          headSha: "active-head",
        }));
        reviewIds.push(activeReviewId);
        await db
          .update(reviewTable)
          .set({ createdAt: new Date("2026-07-17T12:00:00Z") })
          .where(eq(reviewTable.id, activeReviewId));

        expect((await store.getActiveReviewForTask(taskId))?.id).toBe(activeReviewId);

        const listed = await store.listReviews({ repo });
        expect(listed.map((row) => row.id).slice(0, 3)).toEqual([
          activeReviewId,
          terminalReviewId,
          firstReviewId,
        ]);
        expect(listed.find((row) => row.id === firstReviewId)?.findingCounts).toEqual({
          critical: 1,
          high: 0,
          medium: 0,
          low: 1,
          total: 2,
        });
      } finally {
        if (reviewIds.length > 0) {
          await db
            .delete(reviewTable)
            .where(inArray(reviewTable.id, reviewIds))
            .catch(() => {});
        }
        await db.delete(taskTable).where(eq(taskTable.id, taskId)).catch(() => {});
      }
    },
  );
});
