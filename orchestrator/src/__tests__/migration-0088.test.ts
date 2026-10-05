/**
 * Migration 0088 test (ADR 0119 phase 4.7, live-PG gated).
 *
 * Applies 0000–0087 into a scratch schema, plants a seeded PR-review built-in
 * with one mapped repo and two enrollment rows (one of them still on the
 * legacy engine), then applies 0088 and asserts:
 *   - every enrollment row is in the built-in's `repos` map, with the
 *     product-written entry left as it was;
 *   - the built-in is enabled;
 *   - `review_enrollment.engine`, `review.status_comment_id`, and the
 *     `review_session` table are gone.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const SCHEMA = `mig0088_${Date.now()}`;
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

describe("migration 0088", () => {
  test.skipIf(!reachable)("lifts every enrollment into the built-in, enables it, and drops the legacy columns", async () => {
    await pool!.query(`create schema "${SCHEMA}"`);
    await pool!.query(`set search_path to "${SCHEMA}"`);

    const files = (await readdir(DRIZZLE_DIR))
      .filter((name) => /^\d{4}_.*\.sql$/.test(name))
      .sort();
    const pre = files.filter((name) => name < "0088_");
    const target = files.find((name) => name.startsWith("0088_"))!;
    expect(target).toBe("0088_retire_legacy_review_engine.sql");
    for (const file of pre) await applyMigrationFile(file);

    // The seeded built-in, disabled, with one repo the product already
    // mapped (autofix on), plus two enrollment rows: the mapped one (with a
    // DIFFERENT autofix in the row — the map's entry must win) and one still
    // on the legacy engine.
    await pool!.query(`
      insert into automation (id, name, description, enabled, kind, builtin_key, current_version, inputs, end_sessions_on_finish)
      values ('auto-pr', 'PR review', '', false, 'builtin', 'pr_review', 1,
              '{"repos": {"acme/app": {"mode": "auto", "autofix": true}}, "mention": "@engrams"}'::jsonb, true)
    `);
    await pool!.query(`
      insert into review_enrollment (repo, trigger_mode, autofix, engine)
      values ('acme/app', 'auto', 'off', 'automation'),
             ('acme/legacy', 'manual', 'auto', 'legacy')
    `);

    await applyMigrationFile(target);

    const row = await pool!.query(`select enabled, inputs from automation where id = 'auto-pr'`);
    expect(row.rows[0].enabled).toBe(true);
    expect(row.rows[0].inputs).toEqual({
      mention: "@engrams",
      repos: {
        "acme/app": { mode: "auto", autofix: true },
        "acme/legacy": { mode: "on_request", autofix: true },
      },
    });

    const columns = await pool!.query(
      `select table_name, column_name from information_schema.columns
       where table_schema = '${SCHEMA}'
         and ((table_name = 'review_enrollment' and column_name = 'engine')
           or (table_name = 'review' and column_name = 'status_comment_id'))`,
    );
    expect(columns.rows).toEqual([]);
    const tables = await pool!.query(
      `select table_name from information_schema.tables
       where table_schema = '${SCHEMA}' and table_name = 'review_session'`,
    );
    expect(tables.rows).toEqual([]);
  });

  test.skipIf(!reachable)("with no enrollment rows the built-in is left alone", async () => {
    // A fresh deployment: the seed creates the built-in enabled with an
    // empty map; the lift must not touch a row it has nothing to add to.
    const schema = `${SCHEMA}_empty`;
    await pool!.query(`create schema "${schema}"`);
    await pool!.query(`set search_path to "${schema}"`);
    try {
      const files = (await readdir(DRIZZLE_DIR))
        .filter((name) => /^\d{4}_.*\.sql$/.test(name))
        .sort();
      const raw = async (name: string) => {
        const text = await readFile(join(DRIZZLE_DIR, name), "utf8");
        const scoped = text.replaceAll('"public".', `"${schema}".`).replaceAll(" public.", ` "${schema}".`);
        for (const statement of scoped.split("--> statement-breakpoint")) {
          const sql = statement.trim();
          if (sql !== "") await pool!.query(sql);
        }
      };
      for (const file of files.filter((name) => name < "0088_")) await raw(file);
      await pool!.query(`
        insert into automation (id, name, description, enabled, kind, builtin_key, current_version, inputs, end_sessions_on_finish)
        values ('auto-pr', 'PR review', '', false, 'builtin', 'pr_review', 1, '{"repos": {}}'::jsonb, true)
      `);
      await raw("0088_retire_legacy_review_engine.sql");
      const row = await pool!.query(`select enabled, inputs from automation where id = 'auto-pr'`);
      expect(row.rows[0].enabled).toBe(false);
      expect(row.rows[0].inputs).toEqual({ repos: {} });
    } finally {
      await pool!.query(`drop schema if exists "${schema}" cascade`);
    }
  });
});
