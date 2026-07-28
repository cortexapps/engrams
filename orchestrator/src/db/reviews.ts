/** Pull-request review data-access seam (ADR 0100). */

import {
  and,
  desc,
  eq,
  getTableColumns,
  gte,
  inArray,
  isNull,
  or,
  sql,
} from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  ACTIVE_REVIEW_STATUSES,
  review as reviewTable,
  reviewEvent as eventTable,
  reviewFinding as findingTable,
  reviewTarget as targetTable,
  reviewVerdict as verdictTable,
  task as taskTable,
} from "./schema.ts";

/**
 * A capture of the change under review (ADR 0100 decision 11), from either the
 * webhook payload or the API fetch.
 *
 * `providerId` is REQUIRED. Every entry point resolves the pull request before a
 * review starts, so by the time we write a row we always know the forge's id.
 * That is what lets the upsert key on an immutable identity instead of guessing
 * from a mutable repo name.
 *
 * The descriptive fields stay nullable, and a null means "this capture did not
 * learn it" — never "it is now empty" — so the upsert preserves what it already
 * knows rather than blanking it.
 */
export interface UpsertReviewTargetInput {
  provider: string;
  providerId: string;
  repo: string;
  number: number;
  title: string | null;
  author: string | null;
  state: string | null;
  url: string | null;
  /** The forge's own last-modified time. Deliveries are not ordered, so this is
   *  how a late one is recognised as stale instead of overwriting newer facts. */
  providerUpdatedAt: Date | null;
}

export interface ReviewPassFacts {
  targetId: string;
  headSha: string;
  baseSha: string;
  trigger: string;
  /** What THIS pass read — never rewritten by a later pass. */
  headBranch: string | null;
  baseBranch: string | null;
  additions: number | null;
  deletions: number | null;
  changedFiles: number | null;
}

export interface BeginReviewPassInput extends ReviewPassFacts {
  provider: string;
  repo: string;
  prNumber: number;
  /** Automation at an unchanged head is a redelivery; human requests replace
   *  even an unchanged pass because the person may have changed the focus. */
  deduplicateSameHead: boolean;
}

export type BeginReviewPassResult =
  | {
      kind: "created";
      reviewId: string;
      taskId: string;
      supersededReviewId?: string;
    }
  | {
      kind: "deduplicated";
      reviewId: string;
      taskId: string;
    };

export interface UpdateReviewPassContextInput {
  headSha: string;
  baseSha: string;
  headBranch: string | null;
  baseBranch: string | null;
  additions: number | null;
  deletions: number | null;
  changedFiles: number | null;
}

/**
 * A pass, flattened with its PR's identity.
 *
 * The two live in separate tables — the PR is a durable entity, the pass is an
 * event about it — but every reader wants them together, so the store joins and
 * presents one row. `repo`, `prNumber`, `prTitle`, `prAuthor` and `prState` come
 * from the target and are therefore CURRENT for the PR, identical across all of
 * its passes; the branch and diff fields belong to this pass alone.
 */
export interface ReviewRow {
  id: string;
  targetId: string;
  provider: string;
  repo: string;
  prNumber: number;
  taskId: string;
  headSha: string;
  baseSha: string;
  trigger: string;
  status: string;
  githubReviewId: string | null;
  statusCommentId: string | null;
  finderSessionId: string | null;
  verifierSessionId: string | null;
  summaryMd: string | null;
  // From the target: the PR as it is now, not as it was when this pass ran.
  // `providerId` is null only on a row backfilled from before the target table
  // existed, or one whose pull request we can no longer read.
  providerId: string | null;
  prTitle: string | null;
  prAuthor: string | null;
  prState: string | null;
  prUrl: string | null;
  // From the pass: the code this pass actually read.
  headBranch: string | null;
  baseBranch: string | null;
  additions: number | null;
  deletions: number | null;
  changedFiles: number | null;
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

export interface ReviewEventRow {
  id: string;
  reviewId: string;
  kind: string;
  detail: string | null;
  createdAt: Date;
}

export interface ReviewStore {
  /**
   * Give an id to a target row that is still waiting for one, matched on its
   * coordinate. Returns null when there is no such row — the ordinary case.
   * Transitional; see the implementation.
   */
  claimTargetId(input: {
    provider: string;
    providerId: string;
    repo: string;
    number: number;
  }): Promise<{ id: string } | null>;
  /**
   * Record what we currently know about the change under review and return its
   * row, creating it on first sight. Called on every capture, so a renamed pull
   * request is renamed here too — unless this capture is older than the row.
   */
  upsertTarget(input: UpsertReviewTargetInput): Promise<{ id: string }>;
  /**
   * Find a target that a non-reviewing webhook may refresh. Immutable identity
   * wins; the nullable-coordinate fallback exists only for pre-hydration rows.
   */
  getTargetForRefresh(input: {
    provider: string;
    providerId: string;
    repo: string;
    number: number;
  }): Promise<{ id: string; providerId: string | null } | null>;
  /**
   * Atomically deduplicate-or-supersede the active pass and create its successor.
   * The task row is inside the same transaction so a lost uniqueness race cannot
   * leave an orphan automation task.
   */
  beginReviewPass(input: BeginReviewPassInput): Promise<BeginReviewPassResult>;
  updateReviewPassContext(
    reviewId: string,
    input: UpdateReviewPassContextInput,
  ): Promise<boolean>;
  getReview(id: string): Promise<ReviewDetail | null>;
  listReviews(opts: { repo?: string }): Promise<ReviewListRow[]>;
  getActiveReviewForTask(taskId: string): Promise<ReviewRow | null>;
  getActiveReviewForTarget(targetId: string): Promise<ReviewRow | null>;
  getActiveReviewByCoordinate(
    provider: string,
    repo: string,
    number: number,
  ): Promise<ReviewRow | null>;
  insertFinding(input: ReviewFindingInput): Promise<{ id: string; replayed: boolean }>;
  /** Number of findings recorded for a review — used to enforce the per-review cap. */
  countFindings(reviewId: string): Promise<number>;
  insertVerdict(input: ReviewVerdictInput): Promise<{ id: string; replayed: boolean }>;
  recordEvent(reviewId: string, kind: string, detail?: string): Promise<void>;
  listEvents(reviewId: string): Promise<ReviewEventRow[]>;
  setFinderSummary(reviewId: string, summaryMd: string): Promise<void>;
  setStatusCommentId(reviewId: string, statusCommentId: string): Promise<void>;
  setReviewSessionId(
    reviewId: string,
    role: "finder" | "verifier",
    sessionId: string,
  ): Promise<void>;
  /** Applies only while the row is active; false exposes a refused late write. */
  updateReviewStatus(reviewId: string, status: string): Promise<boolean>;
  /**
   * Remove every candidate finding a specific worker session persisted. Used
   * when a failed finder attempt is retried under a fresh session id — its
   * candidates would otherwise re-insert as new rows (dedup is keyed on
   * session_id + tool_call_id) and post twice.
   */
  deleteFindingsForSession(reviewId: string, sessionId: string): Promise<void>;
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
  }): Promise<boolean>;
}

/**
 * Every read of a pass selects its target alongside it. Named explicitly rather
 * than spread from both tables: the two share `id`, `repo`, `created_at` and
 * `updated_at`, and a spread would silently let one shadow the other.
 */
const reviewSelection = {
  ...getTableColumns(reviewTable),
  provider: targetTable.provider,
  providerId: targetTable.providerId,
  repo: targetTable.repo,
  prNumber: targetTable.number,
  prTitle: targetTable.title,
  prAuthor: targetTable.author,
  prState: targetTable.state,
  prUrl: targetTable.url,
};

/** The pass's own columns plus the target's, under the names above. Each field
 *  is tracked from the schema so a column type change surfaces here. */
type TargetRow = typeof targetTable.$inferSelect;
type JoinedReviewRow = typeof reviewTable.$inferSelect & {
  provider: TargetRow["provider"];
  providerId: TargetRow["providerId"];
  repo: TargetRow["repo"];
  prNumber: TargetRow["number"];
  prTitle: TargetRow["title"];
  prAuthor: TargetRow["author"];
  prState: TargetRow["state"];
  prUrl: TargetRow["url"];
};

function toReviewRow(row: JoinedReviewRow): ReviewRow {
  return {
    id: row.id,
    targetId: row.targetId,
    provider: row.provider,
    repo: row.repo,
    prNumber: row.prNumber,
    taskId: row.taskId,
    headSha: row.headSha,
    baseSha: row.baseSha,
    trigger: row.trigger,
    status: row.status,
    githubReviewId: row.githubReviewId ?? null,
    statusCommentId: row.statusCommentId ?? null,
    finderSessionId: row.finderSessionId ?? null,
    verifierSessionId: row.verifierSessionId ?? null,
    summaryMd: row.summaryMd ?? null,
    providerId: row.providerId ?? null,
    prTitle: row.prTitle ?? null,
    prAuthor: row.prAuthor ?? null,
    prState: row.prState ?? null,
    prUrl: row.prUrl ?? null,
    headBranch: row.headBranch ?? null,
    baseBranch: row.baseBranch ?? null,
    additions: row.additions ?? null,
    deletions: row.deletions ?? null,
    changedFiles: row.changedFiles ?? null,
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

function toEventRow(row: typeof eventTable.$inferSelect): ReviewEventRow {
  return {
    id: row.id,
    reviewId: row.reviewId,
    kind: row.kind,
    detail: row.detail ?? null,
    createdAt: row.createdAt,
  };
}

const listSelection = {
  ...reviewSelection,
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
    async claimTargetId(input) {
      // Transitional, and deliberately narrow. Rows backfilled from before this
      // table existed carry no `provider_id`, and the hydrator fills them in over
      // time. Until it reaches a given row, a fresh review of that pull request
      // would insert a SECOND target and split its history — so the write path
      // claims the waiting row first.
      //
      // `provider_id IS NULL` proves only that the old identity is unknown, not
      // that it matches the incoming pull request. There is a narrow coordinate-
      // reuse risk: after a repository rename, a new repository can take the old
      // name and reuse the same pull-request number, causing this transitional
      // claim to join unrelated history. The actual mitigation is to hydrate
      // every row, make `provider_id` NOT NULL, and then delete this method.
      // Retire this when `provider_id` becomes NOT NULL and no un-hydrated rows
      // remain.
      const claimed = await db
        .update(targetTable)
        .set({ providerId: input.providerId, updatedAt: new Date() })
        .where(
          and(
            eq(targetTable.provider, input.provider),
            eq(targetTable.repo, input.repo),
            eq(targetTable.number, input.number),
            isNull(targetTable.providerId),
          ),
        )
        .returning({ id: targetTable.id });
      return claimed[0] ? { id: claimed[0].id } : null;
    },

    async upsertTarget(input) {
      const now = new Date();
      // ONE statement, keyed on `(provider, provider_id)` — an identity that
      // cannot change. An earlier draft keyed on `(repo, number)` and needed a
      // transaction, a retry and a repair pass, all to work around the fact that
      // a repo can be renamed out from under its own conflict target. Keying on
      // something immutable deletes that whole class of problem.
      const refresh = {
        repo: sql`excluded.${sql.raw(targetTable.repo.name)}`,
        number: sql`excluded.${sql.raw(targetTable.number.name)}`,
        // A null field means this capture did not learn it, so keep what we have.
        title: sql`coalesce(excluded.${sql.raw(targetTable.title.name)}, ${targetTable.title})`,
        author: sql`coalesce(excluded.${sql.raw(targetTable.author.name)}, ${targetTable.author})`,
        state: sql`coalesce(excluded.${sql.raw(targetTable.state.name)}, ${targetTable.state})`,
        url: sql`coalesce(excluded.${sql.raw(targetTable.url.name)}, ${targetTable.url})`,
        providerUpdatedAt: sql`coalesce(excluded.${sql.raw(targetTable.providerUpdatedAt.name)}, ${targetTable.providerUpdatedAt})`,
        updatedAt: now,
      };
      const rows = await db
        .insert(targetTable)
        .values({ ...input, createdAt: now, updatedAt: now })
        .onConflictDoUpdate({
          target: [targetTable.provider, targetTable.providerId],
          set: refresh,
          // Forge webhooks are not delivered in order, so a late message can
          // carry OLDER facts than the row already holds. Apply the update only
          // when this capture is at least as fresh. A capture with no forge
          // timestamp (an API fetch) always applies — it read live state.
          setWhere: or(
            isNull(sql`excluded.${sql.raw(targetTable.providerUpdatedAt.name)}`),
            isNull(targetTable.providerUpdatedAt),
            gte(
              sql`excluded.${sql.raw(targetTable.providerUpdatedAt.name)}`,
              targetTable.providerUpdatedAt,
            ),
          ),
        })
        .returning({ id: targetTable.id });
      if (rows[0]) return { id: rows[0].id };

      // No row came back, so `setWhere` rejected this capture as stale. The row
      // exists and its facts are newer than ours; we only need its id.
      const existing = await db
        .select({ id: targetTable.id })
        .from(targetTable)
        .where(
          and(
            eq(targetTable.provider, input.provider),
            eq(targetTable.providerId, input.providerId),
          ),
        )
        .limit(1);
      if (!existing[0]) throw new Error("review target upsert returned no row");
      return { id: existing[0].id };
    },

    async getTargetForRefresh(input) {
      const byIdentity = await db
        .select({ id: targetTable.id, providerId: targetTable.providerId })
        .from(targetTable)
        .where(
          and(
            eq(targetTable.provider, input.provider),
            eq(targetTable.providerId, input.providerId),
          ),
        )
        .limit(1);
      if (byIdentity[0]) return byIdentity[0];

      const waiting = await db
        .select({ id: targetTable.id, providerId: targetTable.providerId })
        .from(targetTable)
        .where(
          and(
            eq(targetTable.provider, input.provider),
            eq(targetTable.repo, input.repo),
            eq(targetTable.number, input.number),
            isNull(targetTable.providerId),
          ),
        )
        .limit(1);
      return waiting[0] ?? null;
    },

    async beginReviewPass(input) {
      return db.transaction(async (tx) => {
        const readActive = async () => {
          const rows = await tx
            .select({
              id: reviewTable.id,
              taskId: reviewTable.taskId,
              headSha: reviewTable.headSha,
            })
            .from(reviewTable)
            .where(
              and(
                eq(reviewTable.targetId, input.targetId),
                inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
              ),
            )
            .orderBy(desc(reviewTable.createdAt))
            .limit(1);
          return rows[0] ?? null;
        };

        const active = await readActive();
        if (
          active
          && input.deduplicateSameHead
          && active.headSha === input.headSha
        ) {
          return {
            kind: "deduplicated",
            reviewId: active.id,
            taskId: active.taskId,
          };
        }

        let supersededReviewId: string | undefined;
        if (active) {
          const superseded = await tx
            .update(reviewTable)
            .set({ status: "superseded", updatedAt: new Date() })
            .where(
              and(
                eq(reviewTable.id, active.id),
                inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
              ),
            )
            .returning({ id: reviewTable.id });
          if (!superseded[0]) {
            const winner = await readActive();
            if (winner) {
              return {
                kind: "deduplicated",
                reviewId: winner.id,
                taskId: winner.taskId,
              };
            }
          } else {
            supersededReviewId = superseded[0].id;
          }
        }

        const taskId = crypto.randomUUID();
        await tx.insert(taskTable).values({
          id: taskId,
          type: "pr_review",
          title: `Review ${input.repo}#${input.prNumber}`,
          status: "working",
          createdByUserId: null,
          source: {
            provider: input.provider,
            repo: input.repo,
            prNumber: input.prNumber,
          },
        });

        const inserted = await tx
          .insert(reviewTable)
          .values({
            targetId: input.targetId,
            taskId,
            headSha: input.headSha,
            baseSha: input.baseSha,
            trigger: input.trigger,
            status: "queued",
            headBranch: input.headBranch,
            baseBranch: input.baseBranch,
            additions: input.additions,
            deletions: input.deletions,
            changedFiles: input.changedFiles,
          })
          // The partial unique index is the final cross-pod arbiter. A second
          // ingress loses cleanly and adopts the pass the winner inserted.
          .onConflictDoNothing()
          .returning({ id: reviewTable.id });
        const created = inserted[0];
        if (!created) {
          await tx.delete(taskTable).where(eq(taskTable.id, taskId));
          const winner = await readActive();
          if (!winner) {
            throw new Error("active review conflict had no winning row");
          }
          return {
            kind: "deduplicated",
            reviewId: winner.id,
            taskId: winner.taskId,
          };
        }

        await tx.insert(eventTable).values({
          reviewId: created.id,
          kind: "queued",
        });
        if (supersededReviewId !== undefined) {
          await tx.insert(eventTable).values({
            reviewId: supersededReviewId,
            kind: "superseded",
            detail: `superseded by ${created.id}`,
          });
        }
        return {
          kind: "created",
          reviewId: created.id,
          taskId,
          ...(supersededReviewId !== undefined
            ? { supersededReviewId }
            : {}),
        };
      });
    },

    async updateReviewPassContext(reviewId, input) {
      const updated = await db
        .update(reviewTable)
        .set({ ...input, updatedAt: new Date() })
        .where(
          and(
            eq(reviewTable.id, reviewId),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
          ),
        )
        .returning({ id: reviewTable.id });
      return updated.length > 0;
    },

    async getReview(id) {
      const reviews = await db
        .select(reviewSelection)
        .from(reviewTable)
        .innerJoin(targetTable, eq(reviewTable.targetId, targetTable.id))
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
        .innerJoin(targetTable, eq(reviewTable.targetId, targetTable.id))
        .leftJoin(findingTable, eq(findingTable.reviewId, reviewTable.id));
      // Grouped by both primary keys because the selection now spans two
      // tables: `review.id` alone leaves the target's columns unaggregated.
      const rows = repo == null
        ? await query
            .groupBy(reviewTable.id, targetTable.id)
            .orderBy(desc(reviewTable.createdAt))
        : await query
            .where(eq(targetTable.repo, repo))
            .groupBy(reviewTable.id, targetTable.id)
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
        .select(reviewSelection)
        .from(reviewTable)
        .innerJoin(targetTable, eq(reviewTable.targetId, targetTable.id))
        .where(
          and(
            eq(reviewTable.taskId, taskId),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
          ),
        )
        .orderBy(desc(reviewTable.createdAt))
        .limit(1);
      return rows[0] ? toReviewRow(rows[0]) : null;
    },

    // Keyed on the target row, not on `(repo, number)`. The coordinate can move
    // under a rename; the target id cannot, so dedup no longer misses an active
    // review just because the repository was renamed mid-flight.
    async getActiveReviewForTarget(targetId) {
      const rows = await db
        .select(reviewSelection)
        .from(reviewTable)
        .innerJoin(targetTable, eq(reviewTable.targetId, targetTable.id))
        .where(
          and(
            eq(reviewTable.targetId, targetId),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
          ),
        )
        .orderBy(desc(reviewTable.createdAt))
        .limit(1);
      return rows[0] ? toReviewRow(rows[0]) : null;
    },

    async getActiveReviewByCoordinate(provider, repo, number) {
      const rows = await db
        .select(reviewSelection)
        .from(reviewTable)
        .innerJoin(targetTable, eq(reviewTable.targetId, targetTable.id))
        .where(
          and(
            eq(targetTable.provider, provider),
            eq(targetTable.repo, repo),
            eq(targetTable.number, number),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
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

    async countFindings(reviewId) {
      const rows = await db
        .select({ count: sql<number>`count(*)::int` })
        .from(findingTable)
        .where(eq(findingTable.reviewId, reviewId));
      return rows[0]?.count ?? 0;
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

    async recordEvent(reviewId, kind, detail) {
      await db.insert(eventTable).values({
        reviewId,
        kind,
        ...(detail !== undefined ? { detail } : {}),
      });
    },

    async listEvents(reviewId) {
      const rows = await db
        .select()
        .from(eventTable)
        .where(eq(eventTable.reviewId, reviewId))
        .orderBy(eventTable.createdAt);
      return rows.map(toEventRow);
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

    async setReviewSessionId(reviewId, role, sessionId) {
      const column = role === "finder"
        ? { finderSessionId: sessionId }
        : { verifierSessionId: sessionId };
      await db
        .update(reviewTable)
        .set({ ...column, updatedAt: new Date() })
        .where(eq(reviewTable.id, reviewId));
    },

    async deleteFindingsForSession(reviewId, sessionId) {
      await db
        .delete(findingTable)
        .where(
          and(
            eq(findingTable.reviewId, reviewId),
            eq(findingTable.sessionId, sessionId),
          ),
        );
    },

    async updateReviewStatus(reviewId, status) {
      const updated = await db
        .update(reviewTable)
        .set({ status, updatedAt: new Date() })
        .where(
          and(
            eq(reviewTable.id, reviewId),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
          ),
        )
        .returning({ id: reviewTable.id });
      return updated.length > 0;
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
      const updated = await db
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
        .where(
          and(
            eq(reviewTable.id, reviewId),
            inArray(reviewTable.status, ACTIVE_REVIEW_STATUSES),
          ),
        )
        .returning({ id: reviewTable.id });
      return updated.length > 0;
    },
  };
}
