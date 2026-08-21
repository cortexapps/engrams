/**
 * Migration 0075 rewrite test (ADR 0119, live-PG gated).
 *
 * Applies 0000–0074 into a scratch schema, seeds old-shape automations and
 * runs (all five legacy statuses, cron and webhook triggers), applies 0075,
 * and asserts the rewrite: version-1 single-block definitions, the status
 * map, one step run + session binding per legacy run, and the dropped
 * columns. The scratch schema keeps the suite parallel-safe and re-runnable.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const SCHEMA = `mig0075_${Date.now()}`;
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
  // Drizzle emits explicit "public". qualifiers on FK targets; point them at
  // the scratch schema so the test never touches real tables.
  const scoped = raw.replaceAll('"public".', `"${SCHEMA}".`).replaceAll(" public.", ` "${SCHEMA}".`);
  for (const statement of scoped.split("--> statement-breakpoint")) {
    const sql = statement.trim();
    if (sql === "") continue;
    await pool!.query(sql);
  }
}

async function listMigrationFiles(): Promise<string[]> {
  const entries = await readdir(DRIZZLE_DIR);
  return entries.filter((name) => /^\d{4}_.*\.sql$/.test(name)).sort();
}

afterAll(async () => {
  if (pool && reachable) {
    await pool.query(`drop schema if exists "${SCHEMA}" cascade`);
  }
  await pool?.end();
});

describe("migration 0075", () => {
  test.skipIf(!reachable)(
    "rewrites legacy automations and runs into engine shapes",
    async () => {
      await pool!.query(`create schema "${SCHEMA}"`);
      await pool!.query(`set search_path to "${SCHEMA}"`);

      const files = await listMigrationFiles();
      const pre = files.filter((name) => name < "0075_");
      const target = files.find((name) => name.startsWith("0075_"))!;
      expect(target).toBe("0075_automation_workflow_engine.sql");
      for (const file of pre) await applyMigrationFile(file);

      // Old-shape seed: one cron automation, one webhook automation, five runs
      // covering every legacy status.
      await pool!.query(`
        insert into automation (id, name, description, enabled, trigger, action, created_by_user_id, next_fire_at)
        values
          ('auto-cron', 'Cron one', '', true,
           '{"kind":"cron","schedule":"0 9 * * 1-5","timezone":"UTC"}',
           '{"kind":"create_task","profileId":"p1","promptTemplate":"daily ping","includeEventContext":false,"harnessMode":"plan"}',
           'admin-1', now()),
          ('auto-hook', 'Hook one', '', true,
           '{"kind":"webhook","registrationId":"hooks-1","events":["issues.opened"],"filter":{"action":"opened"}}',
           '{"kind":"create_task","profileId":"p2","promptTemplate":"triage ${'${{'} event.raw.issue.title }}","titleTemplate":"triage","includeEventContext":true}',
           'admin-1', null)
      `);
      await pool!.query(`
        insert into automation_run (id, automation_id, trigger, rendered_prompt, rendered_title, task_id, session_id, status, error, scheduled_for, created_at)
        values
          ('run-launched', 'auto-cron', '{"source":"cron"}', 'daily ping', null, null, 'sess-1', 'launched', null, '2026-08-01T09:00:00Z', '2026-08-01T09:00:01Z'),
          ('run-skipped', 'auto-cron', '{"source":"cron"}', null, null, null, null, 'skipped', 'missed', '2026-08-02T09:00:00Z', '2026-08-02T09:20:00Z'),
          ('run-render-failed', 'auto-hook', '{"source":"webhook","eventKey":"issues.opened","deliveryId":"d-1"}', null, null, null, null, 'render_failed', 'bad template', null, '2026-08-03T00:00:00Z'),
          ('run-launch-failed', 'auto-hook', '{"source":"webhook","eventKey":"issues.opened","deliveryId":"d-2"}', 'triage x', 'triage', null, null, 'launch_failed', 'no profile', null, '2026-08-04T00:00:00Z'),
          ('run-pending', 'auto-hook', '{"source":"webhook","eventKey":"issues.opened","deliveryId":"d-3"}', null, null, null, null, 'pending', null, null, '2026-08-05T00:00:00Z')
      `);

      await applyMigrationFile(target);

      // Version-1 backfill: one create_session block carrying the action.
      const versions = await pool!.query(
        `select automation_id, version, trigger, blocks, settings from automation_version order by automation_id`,
      );
      expect(versions.rows).toHaveLength(2);
      const cron = versions.rows[0];
      expect(cron.automation_id).toBe("auto-cron");
      expect(cron.version).toBe(1);
      expect(cron.trigger.kind).toBe("cron");
      expect(cron.blocks).toHaveLength(1);
      expect(cron.blocks[0]).toMatchObject({
        id: "create_session",
        type: "create_session",
        config: { profileId: "p1", promptTemplate: "daily ping", harnessMode: "plan" },
      });
      expect(cron.blocks[0].config.kind).toBeUndefined();
      expect(cron.settings).toEqual({ endSessionsOnFinish: false });
      const hook = versions.rows[1];
      expect(hook.trigger).toMatchObject({ kind: "webhook", registrationId: "hooks-1" });
      expect(hook.blocks[0].config).toMatchObject({ profileId: "p2", includeEventContext: true });

      // Status map + delivery keys + force-fail of the in-flight run.
      const runs = await pool!.query(
        `select id, status, error, delivery_key, started_at, ended_at from automation_run order by id`,
      );
      const byId = new Map(runs.rows.map((row) => [row.id, row]));
      expect(byId.get("run-launched")).toMatchObject({
        status: "completed",
        delivery_key: `cron:${Math.floor(Date.parse("2026-08-01T09:00:00Z") / 1000)}`,
      });
      expect(byId.get("run-skipped")!.status).toBe("filtered");
      expect(byId.get("run-render-failed")).toMatchObject({
        status: "failed",
        delivery_key: "webhook:d-1",
        error: "bad template",
      });
      expect(byId.get("run-launch-failed")!.status).toBe("failed");
      expect(byId.get("run-pending")).toMatchObject({
        status: "failed",
        error: "migrated: engine rebuild",
      });
      for (const row of runs.rows) {
        expect(row.started_at).not.toBeNull();
        expect(row.ended_at).not.toBeNull();
      }

      // One step run per legacy run; outputs carry the denormalized launch.
      const steps = await pool!.query(
        `select run_id, block_id, attempt, status, outputs from automation_step_run order by run_id`,
      );
      expect(steps.rows).toHaveLength(5);
      const launched = steps.rows.find((row) => row.run_id === "run-launched")!;
      expect(launched).toMatchObject({ block_id: "create_session", attempt: 0, status: "succeeded" });
      expect(launched.outputs).toMatchObject({ session_id: "sess-1", prompt: "daily ping" });
      expect(steps.rows.find((row) => row.run_id === "run-pending")!.status).toBe("failed");

      // Session binding, kept alive.
      const sessions = await pool!.query(`select session_id, run_id, keep from automation_session`);
      expect(sessions.rows).toEqual([
        { session_id: "sess-1", run_id: "run-launched", keep: true },
      ]);

      // The legacy columns are gone.
      const columns = await pool!.query(
        `select column_name from information_schema.columns
         where table_schema = current_schema() and table_name = 'automation'`,
      );
      const names = columns.rows.map((row) => row.column_name);
      expect(names).not.toContain("trigger");
      expect(names).not.toContain("action");
      expect(names).toContain("current_version");
      expect(names).toContain("end_sessions_on_finish");
    },
    120_000,
  );
});
