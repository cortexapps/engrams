/** Pull-request review data-access seam (ADR 0100). */

import {
  and,
  desc,
  eq,
  getTableColumns,
  inArray,
  sql,
} from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  review as reviewTable,
  reviewFinding as findingTable,
  reviewVerdict as verdictTable,
} from "./schema.ts";

export interface CreateReviewInput {
  repo: string;
  prNumber: number;
  taskId: string;
  headSha: string;
  baseSha: string;
  trigger: string;
  status?: string;
}

export interface ReviewRow {
  id: string;
  repo: string;
  prNumber: number;
  taskId: string;
  headSha: string;
  baseSha: string;
  trigger: string;
  status: string;
  githubReviewId: string | null;
  statusCommentId: string | null;
  summaryMd: string | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface FindingCounts {
  critical: number;
  high: number;
  medium: number;
  low: number;
  total: number;
}

export interface ReviewListRow extends ReviewRow {
  findingCounts: FindingCounts;
}

export interface ReviewFindingInput {
  reviewId: string;
  path: string;
  startLine: number | null;
  endLine: number | null;
  side: string | null;
  category: string;
  severity: string;
  confidence: string;
  title: string;
  bodyMd: string;
  suggestedFix: string | null;
  evidence: string[];
  state: string;
  verdictReason: string | null;
  githubThreadId: string | null;
  resolution: string | null;
  sessionId: string;
  toolCallId: string;
}

export interface ReviewFindingRow extends ReviewFindingInput {
  id: string;
  createdAt: Date;
}

export interface ReviewVerdictInput {
  findingId: string;
  verdict: string;
  confidence: string;
  reasoning: string;
  sessionId: string;
  toolCallId: string;
}

export interface ReviewVerdictRow extends ReviewVerdictInput {
  id: string;
  createdAt: Date;
}

export interface ReviewDetail {
  review: ReviewRow;
  findings: ReviewFindingRow[];
  verdicts: ReviewVerdictRow[];
}

export interface ReviewStore {
  createReview(input: CreateReviewInput): Promise<string>;
  getReview(id: string): Promise<ReviewDetail | null>;
  listReviews(opts: { repo?: string }): Promise<ReviewListRow[]>;
  getActiveReviewForTask(taskId: string): Promise<ReviewRow | null>;
  getActiveReviewForPr(repo: string, prNumber: number): Promise<ReviewRow | null>;
  insertFinding(input: ReviewFindingInput): Promise<{ id: string; replayed: boolean }>;
  insertVerdict(input: ReviewVerdictInput): Promise<{ id: string; replayed: boolean }>;
  setFinderSummary(reviewId: string, summaryMd: string): Promise<void>;
  setStatusCommentId(reviewId: string, statusCommentId: string): Promise<void>;
  updateReviewStatus(reviewId: string, status: string): Promise<void>;
  updateFindingState(
    findingId: string,
    state: string,
    opts?: { githubThreadId?: string; verdictReason?: string },
  ): Promise<void>;
  finalizeReview(reviewId: string, input: {
    status: string;
    summaryMd: string;
    githubReviewId?: string;
    headSha?: string;
    baseSha?: string;
  }): Promise<void>;
}

function toReviewRow(row: typeof reviewTable.$inferSelect): ReviewRow {
  return {
    id: row.id,
    repo: row.repo,
    prNumber: row.prNumber,
    taskId: row.taskId,
    headSha: row.headSha,
    baseSha: row.baseSha,
    trigger: row.trigger,
    status: row.status,
    githubReviewId: row.githubReviewId ?? null,
    statusCommentId: row.statusCommentId ?? null,
    summaryMd: row.summaryMd ?? null,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

function toFindingRow(row: typeof findingTable.$inferSelect): ReviewFindingRow {
  return {
    id: row.id,
    reviewId: row.reviewId,
    path: row.path,
    startLine: row.startLine ?? null,
    endLine: row.endLine ?? null,
    side: row.side ?? null,
    category: row.category,
    severity: row.severity,
    confidence: row.confidence,
    title: row.title,
    bodyMd: row.bodyMd,
    suggestedFix: row.suggestedFix ?? null,
    evidence: row.evidence,
    state: row.state,
    verdictReason: row.verdictReason ?? null,
    githubThreadId: row.githubThreadId ?? null,
    resolution: row.resolution ?? null,
    sessionId: row.sessionId,
    toolCallId: row.toolCallId,
    createdAt: row.createdAt,
  };
}

function toVerdictRow(row: typeof verdictTable.$inferSelect): ReviewVerdictRow {
  return {
    id: row.id,
    findingId: row.findingId,
    verdict: row.verdict,
    confidence: row.confidence,
    reasoning: row.reasoning,
    sessionId: row.sessionId,
    toolCallId: row.toolCallId,
    createdAt: row.createdAt,
  };
}

const listSelection = {
  ...getTableColumns(reviewTable),
  critical: sql<number>`count(*) filter (where ${findingTable.severity} = 'critical')::int`,
  high: sql<number>`count(*) filter (where ${findingTable.severity} = 'high')::int`,
  medium: sql<number>`count(*) filter (where ${findingTable.severity} = 'medium')::int`,
  low: sql<number>`count(*) filter (where ${findingTable.severity} = 'low')::int`,
  total: sql<number>`count(${findingTable.id})::int`,
};

export function makeReviewStore(
  db: ReturnType<typeof getDb> = getDb(),
): ReviewStore {
  return {
    async createReview(input) {
      const inserted = await db
        .insert(reviewTable)
        .values(input)
        .returning({ id: reviewTable.id });
      const row = inserted[0];
      if (!row) throw new Error("review insert returned no row");
      return row.id;
    },

    async getReview(id) {
      const reviews = await db
        .select()
        .from(reviewTable)
        .where(eq(reviewTable.id, id))
        .limit(1);
      const review = reviews[0];
      if (!review) return null;

      const [findings, verdicts] = await Promise.all([
        db
          .select()
          .from(findingTable)
          .where(eq(findingTable.reviewId, id))
          .orderBy(findingTable.createdAt),
        db
          .select(getTableColumns(verdictTable))
          .from(verdictTable)
          .innerJoin(findingTable, eq(verdictTable.findingId, findingTable.id))
          .where(eq(findingTable.reviewId, id))
          .orderBy(verdictTable.createdAt),
      ]);

      return {
        review: toReviewRow(review),
        findings: findings.map(toFindingRow),
        verdicts: verdicts.map(toVerdictRow),
      };
    },

    async listReviews({ repo }) {
      const query = db
        .select(listSelection)
        .from(reviewTable)
        .leftJoin(findingTable, eq(findingTable.reviewId, reviewTable.id));
      const rows = repo == null
        ? await query
            .groupBy(reviewTable.id)
            .orderBy(desc(reviewTable.createdAt))
        : await query
            .where(eq(reviewTable.repo, repo))
            .groupBy(reviewTable.id)
            .orderBy(desc(reviewTable.createdAt));

      return rows.map((row) => ({
        ...toReviewRow(row),
        findingCounts: {
          critical: row.critical,
          high: row.high,
          medium: row.medium,
          low: row.low,
          total: row.total,
        },
      }));
    },

    async getActiveReviewForTask(taskId) {
      const rows = await db
        .select()
        .from(reviewTable)
        .where(
          and(
            eq(reviewTable.taskId, taskId),
            inArray(reviewTable.status, ["queued", "finding", "verifying"]),
          ),
        )
        .orderBy(desc(reviewTable.createdAt))
        .limit(1);
      return rows[0] ? toReviewRow(rows[0]) : null;
    },

    async getActiveReviewForPr(repo, prNumber) {
      const rows = await db
        .select()
        .from(reviewTable)
        .where(
          and(
            eq(reviewTable.repo, repo),
            eq(reviewTable.prNumber, prNumber),
            inArray(reviewTable.status, ["queued", "finding", "verifying"]),
          ),
        )
        .orderBy(desc(reviewTable.createdAt))
        .limit(1);
      return rows[0] ? toReviewRow(rows[0]) : null;
    },

    async insertFinding(input) {
      const inserted = await db
        .insert(findingTable)
        .values(input)
        .onConflictDoNothing({
          target: [findingTable.sessionId, findingTable.toolCallId],
        })
        .returning({ id: findingTable.id });
      if (inserted[0]) return { id: inserted[0].id, replayed: false };

      const existing = await db
        .select({ id: findingTable.id })
        .from(findingTable)
        .where(
          and(
            eq(findingTable.sessionId, input.sessionId),
            eq(findingTable.toolCallId, input.toolCallId),
          ),
        )
        .limit(1);
      if (!existing[0]) {
        throw new Error("conflicting review finding was not found after insert replay");
      }
      return { id: existing[0].id, replayed: true };
    },

    async insertVerdict(input) {
      // Either replay anchor may conflict: the tool call itself, or the
      // finding's first-write-wins verdict constraint.
      const inserted = await db
        .insert(verdictTable)
        .values(input)
        .onConflictDoNothing()
        .returning({ id: verdictTable.id });
      if (inserted[0]) return { id: inserted[0].id, replayed: false };

      const replay = await db
        .select({ id: verdictTable.id })
        .from(verdictTable)
        .where(
          and(
            eq(verdictTable.sessionId, input.sessionId),
            eq(verdictTable.toolCallId, input.toolCallId),
          ),
        )
        .limit(1);
      if (replay[0]) return { id: replay[0].id, replayed: true };

      const firstForFinding = await db
        .select({ id: verdictTable.id })
        .from(verdictTable)
        .where(eq(verdictTable.findingId, input.findingId))
        .limit(1);
      if (!firstForFinding[0]) {
        throw new Error("conflicting review verdict was not found after insert replay");
      }
      return { id: firstForFinding[0].id, replayed: true };
    },

    async setFinderSummary(reviewId, summaryMd) {
      await db
        .update(reviewTable)
        .set({ summaryMd, updatedAt: new Date() })
        .where(eq(reviewTable.id, reviewId));
    },

    async setStatusCommentId(reviewId, statusCommentId) {
      await db
        .update(reviewTable)
        .set({ statusCommentId, updatedAt: new Date() })
        .where(eq(reviewTable.id, reviewId));
    },

    async updateReviewStatus(reviewId, status) {
      await db
        .update(reviewTable)
        .set({ status, updatedAt: new Date() })
        .where(eq(reviewTable.id, reviewId));
    },

    async updateFindingState(findingId, state, opts) {
      await db
        .update(findingTable)
        .set({
          state,
          ...(opts?.githubThreadId !== undefined
            ? { githubThreadId: opts.githubThreadId }
            : {}),
          ...(opts?.verdictReason !== undefined
            ? { verdictReason: opts.verdictReason }
            : {}),
        })
        .where(eq(findingTable.id, findingId));
    },

    async finalizeReview(reviewId, input) {
      await db
        .update(reviewTable)
        .set({
          status: input.status,
          summaryMd: input.summaryMd,
          ...(input.githubReviewId !== undefined
            ? { githubReviewId: input.githubReviewId }
            : {}),
          ...(input.headSha !== undefined ? { headSha: input.headSha } : {}),
          ...(input.baseSha !== undefined ? { baseSha: input.baseSha } : {}),
          updatedAt: new Date(),
        })
        .where(eq(reviewTable.id, reviewId));
    },
  };
}
