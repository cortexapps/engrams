import { eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { reviewSession } from "./schema.ts";

export interface ReviewSessionBinding {
  reviewWorkflowId: string;
  role: string;
}

export interface ReviewSessionStore {
  record(
    sessionId: string,
    reviewWorkflowId: string,
    role: string,
  ): Promise<void>;
  find(sessionId: string): Promise<ReviewSessionBinding | null>;
}

export type ReviewSessionDb = ReturnType<typeof getDb>;

export function makeReviewSessionStore(
  db: ReviewSessionDb = getDb(),
): ReviewSessionStore {
  return {
    async record(sessionId, reviewWorkflowId, role) {
      await db.insert(reviewSession).values({
        sessionId,
        reviewWorkflowId,
        role,
      });
    },

    async find(sessionId) {
      const rows = await db
        .select({
          reviewWorkflowId: reviewSession.reviewWorkflowId,
          role: reviewSession.role,
        })
        .from(reviewSession)
        .where(eq(reviewSession.sessionId, sessionId))
        .limit(1);
      return rows[0] ?? null;
    },
  };
}
