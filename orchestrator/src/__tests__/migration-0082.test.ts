/**
 * Migration 0082 test (ADR 0119 phase 4.4 repair, live-PG gated).
 *
 * 0080 shipped with a journal `when` below 0081's and 0081 merged first, so
 * a database that applied 0081 before 0080 existed (prod) skipped 0080: the
 * drizzle migrator only applies an entry whose `when` is above the last
 * applied one. 0082 re-adds `review_enrollment.engine` idempotently with a
 * `when` above 0081. Both histories must end with the column:
 *
 * - the skipped history: 0000–0079, 0081, 0082 (prod);
 * - the full history: 0000–0082 (a fresh database).
 */

import { afterAll, describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
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

const schemas: string[] = [];

async function applyMigrationFile(schema: string, name: string): Promise<void> {
  const raw = await readFile(join(DRIZZLE_DIR, name), "utf8");
  const scoped = raw
    .replaceAll('"public".', `"${schema}".`)
    .replaceAll(" public.", ` "${schema}".`);
  for (const statement of scoped.split("--> statement-breakpoint")) {
    const sql = statement.trim();
    if (sql !== "") await pool!.query(sql);
  }
}

async function migrationFiles(): Promise<string[]> {
  return (await readdir(DRIZZLE_DIR))
    .filter((name) => /^\d{4}_.*\.sql$/.test(name))
    .sort();
}

async function engineColumn(schema: string) {
  const col = await pool!.query(
    `select is_nullable, column_default from information_schema.columns
     where table_schema = '${schema}' and table_name = 'review_enrollment' and column_name = 'engine'`,
  );
  return col.rows[0] as
    { is_nullable: string; column_default: string } | undefined;
}

async function freshSchema(label: string): Promise<string> {
  const schema = `mig0082_${label}_${Date.now()}`;
  schemas.push(schema);
  await pool!.query(`create schema "${schema}"`);
  await pool!.query(`set search_path to "${schema}"`);
  return schema;
}

afterAll(async () => {
  if (pool && reachable) {
    for (const schema of schemas)
      await pool.query(`drop schema if exists "${schema}" cascade`);
  }
  await pool?.end();
});

describe("migration 0082", () => {
  test.skipIf(!reachable)(
    "repairs a history that skipped 0080 (prod's)",
    async () => {
      const schema = await freshSchema("skipped");
      const files = await migrationFiles();
      const upTo0079 = files.filter((name) => name < "0080_");
      const m0081 = files.find((name) => name.startsWith("0081_"))!;
      const m0082 = files.find((name) => name.startsWith("0082_"))!;
      expect(m0082).toBe("0082_review_enrollment_engine_repair.sql");

      for (const file of [...upTo0079, m0081])
        await applyMigrationFile(schema, file);
      expect(await engineColumn(schema)).toBeUndefined();

      await applyMigrationFile(schema, m0082);
      const col = await engineColumn(schema);
      expect(col?.is_nullable).toBe("NO");
      expect(col?.column_default).toContain("legacy");

      // An enrolled repo from before the repair defaults to legacy.
      await pool!.query(
        `insert into review_enrollment (repo, trigger_mode, autofix) values ('engrams/engrams', 'auto', 'off')`,
      );
      const row = await pool!.query(
        `select engine from review_enrollment where repo = 'engrams/engrams'`,
      );
      expect(row.rows[0].engine).toBe("legacy");
    },
  );

  test.skipIf(!reachable)(
    "is a no-op on a history that applied 0080",
    async () => {
      const schema = await freshSchema("full");
      const files = await migrationFiles();
      const through0082 = files.filter((name) => name < "0083_");
      expect(through0082.at(-1)).toBe(
        "0082_review_enrollment_engine_repair.sql",
      );
      for (const file of through0082) await applyMigrationFile(schema, file);
      const col = await engineColumn(schema);
      expect(col?.is_nullable).toBe("NO");
      expect(col?.column_default).toContain("legacy");
    },
  );
});
