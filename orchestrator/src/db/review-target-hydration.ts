/** Data-access seam for the transitional review-target hydrator (ADR 0100). */

import { and, asc, eq, isNull } from "drizzle-orm";

import type { PrContext } from "../reviews/pr-context.ts";
import { getDb } from "./client.ts";
import { reviewTarget as targetTable } from "./schema.ts";

export interface UnhydratedReviewTarget {
  id: string;
  provider: string;
  repo: string;
  number: number;
}

export interface ReviewTargetHydrationStore {
  listUnhydrated(limit: number): Promise<UnhydratedReviewTarget[]>;
  /**
   * Write everything the forge just told us, not only the id.
   *
   * The fetch that produces `pr` is the expensive part, and it returns the whole
   * pull request. A row reached by this path came from the backfill, so its
   * title, author, state and url are whatever old passes happened to record —
   * usually nothing. Writing just the id would leave the row permanently blank,
   * because the scan only ever revisits rows with a null id.
   *
   * This cannot go through `upsertTarget`: that keys on `(provider,
   * provider_id)`, and a null id never conflicts in Postgres, so it would insert
   * a duplicate rather than fill this row.
   */
  hydrate(
    targetId: string,
    providerId: string,
    pr: PrContext,
    updatedAt: Date,
  ): Promise<"updated" | "already-hydrated">;
  markFailed(targetId: string, failedAt: Date): Promise<void>;
}

export function makeReviewTargetHydrationStore(
  db: ReturnType<typeof getDb> = getDb(),
): ReviewTargetHydrationStore {
  return {
    async listUnhydrated(limit) {
      return db
        .select({
          id: targetTable.id,
          provider: targetTable.provider,
          repo: targetTable.repo,
          number: targetTable.number,
        })
        .from(targetTable)
        .where(
          and(
            isNull(targetTable.providerId),
            isNull(targetTable.hydrationFailedAt),
          ),
        )
        .orderBy(asc(targetTable.createdAt), asc(targetTable.id))
        .limit(limit);
    },

    async hydrate(targetId, providerId, pr, updatedAt) {
      // A null field means the forge did not report it, so keep whatever the
      // backfill left behind rather than erasing a known value with nothing.
      const described: Partial<typeof targetTable.$inferInsert> = {};
      if (pr.title !== null) described.title = pr.title;
      if (pr.author !== null) described.author = pr.author;
      if (pr.state !== null) described.state = pr.state;
      if (pr.url !== null) described.url = pr.url;
      if (pr.providerUpdatedAt !== null) {
        described.providerUpdatedAt = pr.providerUpdatedAt;
      }

      const updated = await db
        .update(targetTable)
        .set({ providerId, ...described, updatedAt })
        .where(
          and(
            eq(targetTable.id, targetId),
            isNull(targetTable.providerId),
          ),
        )
        .returning({ id: targetTable.id });
      if (updated[0]) return "updated";

      // claimTargetId may have won between the scan and this update. Any
      // non-null id means the row is now hydrated; never overwrite that winner.
      const current = await db
        .select({ providerId: targetTable.providerId })
        .from(targetTable)
        .where(eq(targetTable.id, targetId))
        .limit(1);
      if (current[0]?.providerId != null) return "already-hydrated";
      throw new Error(`review target disappeared during hydration: ${targetId}`);
    },

    async markFailed(targetId, failedAt) {
      await db
        .update(targetTable)
        .set({ hydrationFailedAt: failedAt, updatedAt: failedAt })
        .where(
          and(
            eq(targetTable.id, targetId),
            isNull(targetTable.providerId),
            isNull(targetTable.hydrationFailedAt),
          ),
        );
    },
  };
}
