import { describe, expect, test } from "bun:test";
import { eq } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { makeReviewSessionStore } from "../db/review-sessions.ts";
import { reviewSession as reviewSessionTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("ReviewSessionStore", () => {
  test.skipIf(!dbReachable)("records and finds a review workflow binding", async () => {
    const db = getDb();
    const store = makeReviewSessionStore(db);
    const sessionId = `review-session-${crypto.randomUUID()}`;

    try {
      expect(await store.find(sessionId)).toBeNull();
      await store.record(sessionId, "review:openai/engrams#100", "verifier");
      expect(await store.find(sessionId)).toEqual({
        reviewWorkflowId: "review:openai/engrams#100",
        role: "verifier",
      });
      await store.remove(sessionId);
      expect(await store.find(sessionId)).toBeNull();
    } finally {
      await db
        .delete(reviewSessionTable)
        .where(eq(reviewSessionTable.sessionId, sessionId))
        .catch(() => {});
    }
  });
});
