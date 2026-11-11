import { describe, expect, test } from "bun:test";
import { inArray } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { makePrRefStore, type PrRefInput } from "../db/pr-refs.ts";
import { prRef as prRefTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("PrRefStore", () => {
  test.skipIf(!dbReachable)(
    "one row per (provider, repo, number); two forges keep the same repo and number apart",
    async () => {
      const db = getDb();
      const store = makePrRefStore(db);
      const repo = `acme/${crypto.randomUUID()}`;
      const input = (provider: string, title: string): PrRefInput => ({
        provider,
        repo,
        prNumber: 7,
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
      const other = await store.upsert(input("forge-b", "other forge"));
      try {
        expect(again).toBe(first);
        expect(other).not.toBe(first);
        expect(await store.getByPr("forge-a", repo, 7)).toMatchObject({
          id: first,
          provider: "forge-a",
          title: "renamed",
        });
        expect(await store.getByPr("forge-b", repo, 7)).toMatchObject({
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
