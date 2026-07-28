import { describe, expect, test } from "bun:test";
import { eq, inArray } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  makeReviewStore,
  type ReviewFindingInput,
  type ReviewStore,
  type UpsertReviewTargetInput,
} from "../db/reviews.ts";
import {
  review as reviewTable,
  reviewTarget as targetTable,
  task as taskTable,
} from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

/** A minimal identified capture. Ingress never writes a target until the forge
 *  has supplied its immutable provider id. */
function blankTarget(repo: string, prNumber: number): UpsertReviewTargetInput {
  return {
    provider: "github",
    providerId: `${repo}#${prNumber}`,
    repo,
    number: prNumber,
    title: null,
    author: null,
    state: null,
    url: null,
    providerUpdatedAt: null,
  };
}

/** Create a pass over a PR, minting the PR's target row the way the control
 *  plane does (ADR 0100 d11: target first, then the pass that points at it). */
async function createPass(
  store: ReviewStore,
  taskId: string,
  opts: {
    repo: string;
    prNumber?: number;
    headSha?: string;
    baseSha?: string;
    trigger?: string;
    status?: string;
  },
): Promise<string> {
  const {
    repo,
    prNumber = 100,
    headSha = "head-sha",
    baseSha = "base-sha",
    trigger = "dispatch",
    status = "queued",
  } = opts;
  const target = await store.upsertTarget(blankTarget(repo, prNumber));
  const rows = await getDb()
    .insert(reviewTable)
    .values({
      targetId: target.id,
      taskId,
      headSha,
      baseSha,
      trigger,
      status,
      headBranch: null,
      baseBranch: null,
      additions: null,
      deletions: null,
      changedFiles: null,
    })
    .returning({ id: reviewTable.id });
  if (!rows[0]) throw new Error("test review insert returned no row");
  return rows[0].id;
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
        const firstReviewId = await createPass(store, taskId, { repo });
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
        await store.updateFindingState(firstFinding.id, "posted", {
          githubThreadId: "github-thread-1",
          verdictReason: "Verified against the live branch.",
        });
        await store.finalizeReview(firstReviewId, {
          status: "posted",
          summaryMd: "One finding posted.",
          githubReviewId: "github-review-1",
          headSha: "live-head-sha",
          baseSha: "live-base-sha",
        });
        const detail = await store.getReview(firstReviewId);
        expect(detail?.review).toMatchObject({
          id: firstReviewId,
          status: "posted",
          summaryMd: "One finding posted.",
          githubReviewId: "github-review-1",
          headSha: "live-head-sha",
          baseSha: "live-base-sha",
        });
        expect(detail?.findings).toHaveLength(2);
        expect(detail?.findings.find((row) => row.id === firstFinding.id)).toMatchObject({
          state: "posted",
          githubThreadId: "github-thread-1",
          verdictReason: "Verified against the live branch.",
        });
        expect(detail?.verdicts).toHaveLength(1);

        const terminalReviewId = await createPass(store, taskId, {
          repo,
          prNumber: 101,
          status: "posted",
          headSha: "terminal-head",
        });
        reviewIds.push(terminalReviewId);
        await db
          .update(reviewTable)
          .set({ createdAt: new Date("2026-07-17T11:00:00Z") })
          .where(eq(reviewTable.id, terminalReviewId));

        const activeReviewId = await createPass(store, taskId, {
          repo,
          prNumber: 102,
          status: "verifying",
          headSha: "active-head",
        });
        reviewIds.push(activeReviewId);
        await db
          .update(reviewTable)
          .set({ createdAt: new Date("2026-07-17T12:00:00Z") })
          .where(eq(reviewTable.id, activeReviewId));

        expect((await store.getActiveReviewForTask(taskId))?.id).toBe(activeReviewId);
        const activeTargetId = (await store.getReview(activeReviewId))!.review.targetId;
        const terminalTargetId = (await store.getReview(terminalReviewId))!.review.targetId;
        expect((await store.getActiveReviewForTarget(activeTargetId))?.id).toBe(activeReviewId);
        expect(await store.getActiveReviewForTarget(terminalTargetId)).toBeNull();

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
        // Passes cascade from their target, but the target itself outlives them.
        await db.delete(targetTable).where(eq(targetTable.repo, repo)).catch(() => {});
      }
    },
  );

  test.skipIf(!dbReachable)(
    "countFindings and deleteFindingsForSession scope by review and session",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const taskId = `review-store-${crypto.randomUUID()}`;
      const repo = `review-store-${crypto.randomUUID()}/engrams`;
      const reviewIds: string[] = [];
      await db.insert(taskTable).values({
        id: taskId,
        type: "pr_review",
        title: "Review store delete test",
      });

      try {
        const reviewId = await createPass(store, taskId, { repo });
        reviewIds.push(reviewId);
        const otherReviewId = await createPass(store, taskId, { repo, prNumber: 200 });
        reviewIds.push(otherReviewId);

        // Two findings from a first (failed) finder session, one from the retry.
        await store.insertFinding(findingInput(reviewId, {
          sessionId: "finder-1",
          toolCallId: "c1",
        }));
        await store.insertFinding(findingInput(reviewId, {
          sessionId: "finder-1",
          toolCallId: "c2",
        }));
        await store.insertFinding(findingInput(reviewId, {
          sessionId: "finder-2",
          toolCallId: "c3",
        }));
        // A finding on a different review must never be touched.
        await store.insertFinding(findingInput(otherReviewId, {
          sessionId: "finder-1",
          toolCallId: "c4",
        }));

        expect(await store.countFindings(reviewId)).toBe(3);

        // Dropping the failed session's findings leaves the retry's — and the
        // other review's — intact.
        await store.deleteFindingsForSession(reviewId, "finder-1");
        expect(await store.countFindings(reviewId)).toBe(1);
        expect(await store.countFindings(otherReviewId)).toBe(1);
        const remaining = await store.getReview(reviewId);
        expect(remaining?.findings.map((f) => f.sessionId)).toEqual(["finder-2"]);
      } finally {
        if (reviewIds.length > 0) {
          await db
            .delete(reviewTable)
            .where(inArray(reviewTable.id, reviewIds))
            .catch(() => {});
        }
        await db.delete(taskTable).where(eq(taskTable.id, taskId)).catch(() => {});
        // Passes cascade from their target, but the target itself outlives them.
        await db.delete(targetTable).where(eq(targetTable.repo, repo)).catch(() => {});
      }
    },
  );

  test.skipIf(!dbReachable)(
    "upsertTarget rejects stale facts, preserves null fields, and follows a rename",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const scope = `review-target-${crypto.randomUUID()}`;
      const repo = `${scope}/engrams`;
      const renamed = `${scope}/engrams-renamed`;
      const providerId = `${Date.now()}${Math.trunc(performance.now())}`;

      try {
        const first = await store.upsertTarget({
          provider: "github",
          repo,
          number: 881,
          providerId,
          title: "Bump quinn-proto",
          author: "dependabot",
          state: "open",
          url: `https://github.com/${repo}/pull/881`,
          providerUpdatedAt: new Date("2026-07-21T12:00:00Z"),
        });

        // A delayed older delivery must return the same id without overwriting
        // facts learned from the newer delivery.
        const stale = await store.upsertTarget({
          provider: "github",
          repo,
          number: 881,
          providerId,
          title: "Stale title",
          author: "stale-user",
          state: "draft",
          url: null,
          providerUpdatedAt: new Date("2026-07-21T11:59:59Z"),
        });
        expect(stale.id).toBe(first.id);
        const preserved = await db
          .select()
          .from(targetTable)
          .where(eq(targetTable.id, first.id));
        expect(preserved[0]).toMatchObject({
          title: "Bump quinn-proto",
          author: "dependabot",
          state: "open",
          providerId,
        });

        // A current capture that omitted optional fields does not blank them.
        const partial = await store.upsertTarget({
          provider: "github",
          repo,
          number: 881,
          providerId,
          title: "Bump quinn-proto to 0.11.16",
          author: null,
          state: null,
          url: null,
          providerUpdatedAt: null,
        });
        expect(partial.id).toBe(first.id);

        // The repo is renamed. Resolution by the stable id must MOVE the
        // existing row rather than mint a second target for the same PR —
        // otherwise the PR's whole review history splits in two.
        const afterRename = await store.upsertTarget({
          provider: "github",
          repo: renamed,
          number: 881,
          providerId,
          title: "Bump quinn-proto to 0.11.16",
          author: null,
          state: "merged",
          url: null,
          providerUpdatedAt: new Date("2026-07-21T13:00:00Z"),
        });
        expect(afterRename.id).toBe(first.id);
        const moved = await db
          .select()
          .from(targetTable)
          .where(eq(targetTable.providerId, providerId));
        expect(moved).toHaveLength(1);
        expect(moved[0]).toMatchObject({
          repo: renamed,
          title: "Bump quinn-proto to 0.11.16",
          state: "merged",
          // Untouched by a capture that did not carry them.
          author: "dependabot",
          url: `https://github.com/${repo}/pull/881`,
        });

        // A different PR in the same repo is a different target.
        const other = await store.upsertTarget(blankTarget(renamed, 882));
        expect(other.id).not.toBe(first.id);
      } finally {
        await db
          .delete(targetTable)
          .where(inArray(targetTable.repo, [repo, renamed]))
          .catch(() => {});
      }
    },
  );

  test.skipIf(!dbReachable)(
    "claimTargetId adopts only a null-id target",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const scope = `review-claim-${crypto.randomUUID()}`;
      const waitingRepo = `${scope}/waiting`;
      const identifiedRepo = `${scope}/identified`;
      const rows = await db
        .insert(targetTable)
        .values([
          {
            provider: "github",
            providerId: null,
            repo: waitingRepo,
            number: 7,
          },
          {
            provider: "github",
            providerId: "existing-provider-id",
            repo: identifiedRepo,
            number: 8,
          },
        ])
        .returning({ id: targetTable.id });

      try {
        expect(await store.claimTargetId({
          provider: "github",
          providerId: "claimed-provider-id",
          repo: waitingRepo,
          number: 7,
        })).toEqual({ id: rows[0]!.id });
        expect(await store.claimTargetId({
          provider: "github",
          providerId: "replacement-provider-id",
          repo: identifiedRepo,
          number: 8,
        })).toBeNull();

        const persisted = await db
          .select({
            id: targetTable.id,
            providerId: targetTable.providerId,
          })
          .from(targetTable)
          .where(inArray(targetTable.id, rows.map((row) => row.id)));
        expect(persisted).toEqual(expect.arrayContaining([
          { id: rows[0]!.id, providerId: "claimed-provider-id" },
          { id: rows[1]!.id, providerId: "existing-provider-id" },
        ]));
      } finally {
        await db
          .delete(targetTable)
          .where(inArray(targetTable.id, rows.map((row) => row.id)))
          .catch(() => {});
      }
    },
  );

  test.skipIf(!dbReachable)(
    "the partial unique index arbitrates concurrent active-pass creation",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const repo = `review-race-${crypto.randomUUID()}/engrams`;
      const target = await store.upsertTarget(blankTarget(repo, 100));
      const input = {
        provider: "github",
        targetId: target.id,
        repo,
        prNumber: 100,
        headSha: "race-head",
        baseSha: "race-base",
        trigger: "synchronize",
        headBranch: "feature",
        baseBranch: "main",
        additions: 1,
        deletions: 0,
        changedFiles: 1,
        deduplicateSameHead: true,
      };

      try {
        const results = await Promise.all([
          store.beginReviewPass(input),
          store.beginReviewPass(input),
        ]);
        const activeRows = await db
          .select()
          .from(reviewTable)
          .where(eq(reviewTable.targetId, target.id));

        expect(activeRows).toHaveLength(1);
        expect(results.filter((result) => result.kind === "created")).toHaveLength(1);
        expect(results.filter((result) => result.kind === "deduplicated")).toHaveLength(1);
        expect(new Set(results.map((result) => result.reviewId)).size).toBe(1);
      } finally {
        const tasks = await db
          .select({ id: reviewTable.taskId })
          .from(reviewTable)
          .where(eq(reviewTable.targetId, target.id));
        await db.delete(targetTable).where(eq(targetTable.id, target.id)).catch(() => {});
        if (tasks.length > 0) {
          await db
            .delete(taskTable)
            .where(inArray(taskTable.id, tasks.map((row) => row.id)))
            .catch(() => {});
        }
      }
    },
  );

  test.skipIf(!dbReachable)(
    "a superseded terminal row refuses a late failed transition",
    async () => {
      const db = getDb();
      const store = makeReviewStore(db);
      const repo = `review-terminal-${crypto.randomUUID()}/engrams`;
      const target = await store.upsertTarget(blankTarget(repo, 100));
      const first = await store.beginReviewPass({
        provider: "github",
        targetId: target.id,
        repo,
        prNumber: 100,
        headSha: "head-1",
        baseSha: "base",
        trigger: "opened",
        headBranch: "feature",
        baseBranch: "main",
        additions: 1,
        deletions: 0,
        changedFiles: 1,
        deduplicateSameHead: true,
      });
      if (first.kind !== "created") throw new Error("first pass was not created");
      const second = await store.beginReviewPass({
        provider: "github",
        targetId: target.id,
        repo,
        prNumber: 100,
        headSha: "head-2",
        baseSha: "base",
        trigger: "synchronize",
        headBranch: "feature",
        baseBranch: "main",
        additions: 2,
        deletions: 0,
        changedFiles: 1,
        deduplicateSameHead: true,
      });
      if (second.kind !== "created") throw new Error("successor pass was not created");

      try {
        expect(second.supersededReviewId).toBe(first.reviewId);
        expect(await store.updateReviewStatus(first.reviewId, "failed")).toBe(false);
        expect((await store.getReview(first.reviewId))?.review.status)
          .toBe("superseded");
      } finally {
        const taskIds = [first.taskId, second.taskId];
        await db.delete(targetTable).where(eq(targetTable.id, target.id)).catch(() => {});
        await db
          .delete(taskTable)
          .where(inArray(taskTable.id, taskIds))
          .catch(() => {});
      }
    },
  );
});
