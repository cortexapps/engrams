/**
 * Migration 0089 test (ADR 0119 phase 4.7, live-PG gated).
 *
 * Applies 0000–0088 into a scratch schema, plants a designated `pr_reviewer`
 * profile and the PR-review built-in whose `profile` input holds the literal
 * designation, then applies 0089 and asserts the input now carries the
 * profile's id and the column is gone. A second schema with no designated
 * profile asserts the input is emptied instead.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const SCHEMA = `mig0089_${Date.now()}`;
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

async function applyInto(schema: string, name: string): Promise<void> {
  const raw = await readFile(join(DRIZZLE_DIR, name), "utf8");
  const scoped = raw.replaceAll('"public".', `"${schema}".`).replaceAll(" public.", ` "${schema}".`);
  for (const statement of scoped.split("--> statement-breakpoint")) {
    const sql = statement.trim();
    if (sql !== "") await pool!.query(sql);
  }
}

async function migrateUpTo0088(schema: string): Promise<string> {
  await pool!.query(`create schema "${schema}"`);
  await pool!.query(`set search_path to "${schema}"`);
  const files = (await readdir(DRIZZLE_DIR))
    .filter((name) => /^\d{4}_.*\.sql$/.test(name))
    .sort();
  for (const file of files.filter((name) => name < "0089_")) await applyInto(schema, file);
  const target = files.find((name) => name.startsWith("0089_"))!;
  expect(target).toBe("0089_retire_reviewer_designation.sql");
  return target;
}

const BUILTIN_INSERT = `
  insert into automation (id, name, description, enabled, kind, builtin_key, current_version, inputs, end_sessions_on_finish)
  values ('auto-pr', 'PR review', '', true, 'builtin', 'pr_review', 1,
          '{"repos": {}, "profile": "pr_reviewer", "mention": "@engrams"}'::jsonb, true)
`;
const PROFILE_INSERT = (id: string, designation: string | null) => `
  insert into profile (id, name, description, icon, image_id, harness, designation)
  values ('${id}', 'Reviewer', '', 'Bot', 'img-1', 'claude', ${designation === null ? "null" : `'${designation}'`})
`;

afterAll(async () => {
  if (pool && reachable) {
    await pool.query(`drop schema if exists "${SCHEMA}" cascade`);
    await pool.query(`drop schema if exists "${SCHEMA}_none" cascade`);
  }
  await pool?.end();
});

describe("migration 0089", () => {
  test.skipIf(!reachable)("copies the designated profile's id into the built-in's profile input and drops the column", async () => {
    const target = await migrateUpTo0088(SCHEMA);
    await pool!.query(PROFILE_INSERT("prof-other", null));
    await pool!.query(PROFILE_INSERT("prof-reviewer", "pr_reviewer"));
    await pool!.query(BUILTIN_INSERT);

    await applyInto(SCHEMA, target);

    const row = await pool!.query(`select inputs from automation where id = 'auto-pr'`);
    expect(row.rows[0].inputs).toEqual({ repos: {}, profile: "prof-reviewer", mention: "@engrams" });
    const col = await pool!.query(
      `select column_name from information_schema.columns
       where table_schema = '${SCHEMA}' and table_name = 'profile' and column_name = 'designation'`,
    );
    expect(col.rows).toEqual([]);
  });

  test.skipIf(!reachable)("with no designated profile the input is emptied", async () => {
    const schema = `${SCHEMA}_none`;
    const target = await migrateUpTo0088(schema);
    await pool!.query(BUILTIN_INSERT);

    await applyInto(schema, target);

    const row = await pool!.query(`select inputs from automation where id = 'auto-pr'`);
    expect(row.rows[0].inputs).toEqual({ repos: {}, profile: "", mention: "@engrams" });
  });
});
