/**
 * The drizzle migrator applies a journal entry only when its `when` is
 * greater than the last applied migration's `created_at`
 * (drizzle-orm/pg-core/dialect: `lastDbMigration.created_at < migration.folderMillis`).
 * So an entry whose `when` is below an earlier-merged entry's is skipped for
 * good on every database that applied the later one first — 0080 was skipped
 * in prod this way (repaired by 0082). Two branches that each reserve a
 * migration can interleave in merge order; this test makes the journal
 * itself reject the shape, so the rebase that re-stamps `when` happens before
 * merge, not after a deploy.
 */

import { describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";

const DRIZZLE_DIR = join(import.meta.dir, "..", "..", "drizzle");

type Entry = { idx: number; when: number; tag: string };

describe("drizzle migration journal", () => {
  test("`when` is strictly increasing in idx order and every entry has its file", async () => {
    const journal = JSON.parse(
      await readFile(join(DRIZZLE_DIR, "meta", "_journal.json"), "utf8"),
    ) as {
      entries: Entry[];
    };
    const files = new Set(await readdir(DRIZZLE_DIR));
    let prev: Entry | null = null;
    for (const entry of journal.entries) {
      expect(files.has(`${entry.tag}.sql`)).toBe(true);
      expect(entry.tag.startsWith(String(entry.idx).padStart(4, "0"))).toBe(
        true,
      );
      if (prev) {
        expect(entry.idx).toBe(prev.idx + 1);
        // A `when` at or below the previous entry's would never apply on a
        // database that already ran the previous entry.
        expect(entry.when).toBeGreaterThan(prev.when);
      }
      prev = entry;
    }
  });
});
