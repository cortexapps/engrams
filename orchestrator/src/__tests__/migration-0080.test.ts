/**
 * Migration 0080 test (ADR 0119 phase 4.4, live-PG gated).
 *
 * Applies 0000–0080 into a scratch schema and asserts that
 * `review_enrollment.engine` exists, defaults to 'legacy', and is NOT NULL —
 * so an existing enrolled repo keeps reviewing on the legacy engine after the
 * migration, and only an explicit flip opts it into the built-in.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const SCHEMA = `mig0080_${Date.now()}`;
const DRIZZLE_DIR = join(import.meta.dir, "..", "..", "drizzle");

let pool: Pool | null = null;
let reachable = false;
if (DB_URL) {
  pool = new Pool({ connectionString: DB_URL, max: 1 });
  try {
    await pool.query("select 1");
    reachable = true;
  } catch {
    reachable = false;
  }
}

async function applyMigrationFile(name: string): Promise<void> {
  const raw = await readFile(join(DRIZZLE_DIR, name), "utf8");
  const scoped = raw.replaceAll('"public".', `"${SCHEMA}".`).replaceAll(" public.", ` "${SCHEMA}".`);
  for (const statement of scoped.split("--> statement-breakpoint")) {
    const sql = statement.trim();
    if (sql !== "") await pool!.query(sql);
  }
}

afterAll(async () => {
  if (pool && reachable) await pool.query(`drop schema if exists "${SCHEMA}" cascade`);
  await pool?.end();
});

describe("migration 0080", () => {
  test.skipIf(!reachable)("adds review_enrollment.engine defaulting to legacy", async () => {
    await pool!.query(`create schema "${SCHEMA}"`);
    await pool!.query(`set search_path to "${SCHEMA}"`);

    const files = (await readdir(DRIZZLE_DIR))
      .filter((name) => /^\d{4}_.*\.sql$/.test(name))
      .sort();
    const upTo = files.filter((name) => name <= "0080_");
    expect(upTo.at(-1)).toBe("0080_review_enrollment_engine.sql");
    for (const file of upTo) await applyMigrationFile(file);

    // An enrolled repo from before the migration defaults to legacy.
    await pool!.query(`
      insert into review_enrollment (repo, trigger_mode, autofix)
      values ('engrams/engrams', 'auto', 'off')
    `);
    const row = await pool!.query(
      `select engine from review_enrollment where repo = 'engrams/engrams'`,
    );
    expect(row.rows[0].engine).toBe("legacy");

    // The column is NOT NULL: an insert cannot leave it unset to NULL.
    const col = await pool!.query(
      `select is_nullable, column_default from information_schema.columns
       where table_schema = '${SCHEMA}' and table_name = 'review_enrollment' and column_name = 'engine'`,
    );
    expect(col.rows[0].is_nullable).toBe("NO");
    expect(col.rows[0].column_default).toContain("legacy");
  });
});
