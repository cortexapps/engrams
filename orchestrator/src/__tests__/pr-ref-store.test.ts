import { describe, expect, test } from "bun:test";
import { inArray, sql } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { isUniqueViolation } from "../db/pg-errors.ts";
import { makePrRefStore, type PrRefInput } from "../db/pr-refs.ts";
import { prRef as prRefTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("PrRefStore", () => {
  test.skipIf(!dbReachable)(
    "one row per (provider, repo, number); the previous build's upsert still resolves",
    async () => {
      const db = getDb();
      const store = makePrRefStore(db);
      const repo = `acme/${crypto.randomUUID()}`;
      const input = (provider: string, title: string, prNumber = 7): PrRefInput => ({
        provider,
        repo,
        prNumber,
        authoringTaskId: null,
        sessionId: `session-${provider}`,
        title,
        url: `https://forge.example/${repo}/7`,
        headBranch: "feature",
        baseBranch: "main",
        observedAt: new Date("2026-07-16T00:00:00Z"),
      });

      const first = await store.upsert(input("forge-a", "first"));
      const again = await store.upsert(input("forge-a", "renamed"));
      const other = await store.upsert(input("forge-b", "other forge", 8));
      try {
        expect(again).toBe(first);
        expect(other).not.toBe(first);
        // The (repo, number) index stays for one release: a second forge
        // cannot take the same pair, and the previous build's conflict
        // target resolves.
        await expect(store.upsert(input("forge-b", "same pair"))).rejects.toSatisfy(
          isUniqueViolation,
        );
        await db.execute(sql`
          INSERT INTO pr_ref (id, repo, pr_number, session_id, title, url, head_branch, base_branch, observed_at)
          VALUES (${crypto.randomUUID()}, ${repo}, 7, 'session-old', 'old build', '', '', '', now())
          ON CONFLICT (repo, pr_number) DO UPDATE SET title = excluded.title`);
        expect(await store.getByPr("forge-a", repo, 7)).toMatchObject({
          id: first,
          provider: "forge-a",
          title: "old build",
        });
        expect(await store.getByPr("forge-b", repo, 8)).toMatchObject({
          id: other,
          sessionId: "session-forge-b",
        });
        expect(await store.getByPr("forge-c", repo, 7)).toBeNull();
      } finally {
        await db.delete(prRefTable).where(inArray(prRefTable.id, [first, other]));
      }
    },
  );
});
