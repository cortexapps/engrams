import { expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

// The built-in harness model options are family ids. Each family migration
// moves stored native selections of removed version-pinned ids to their
// family id in profile.model, task.model, and the `model` member of
// automation session blocks.

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];

interface FamilyMigration {
  file: string;
  harness: string;
  otherHarness: string;
  /** removed id → family id */
  moved: Record<string, string>;
  /** ids the migration must not change (family ids, unknown ids) */
  kept: string[];
}

const MIGRATIONS: FamilyMigration[] = [
  {
    file: "0095_claude_model_family_aliases.sql",
    harness: "claude",
    otherHarness: "codex",
    moved: { "opus-5": "opus", "fable-5": "fable" },
    kept: ["opus", "sonnet", "haiku", "custom-model"],
  },
];

const session = (id: string, model: string) => ({ id, type: "session", config: { model } });

for (const m of MIGRATIONS) {
  test.skipIf(!DB_URL)(
    `${m.file} moves native ${m.harness} selections to family ids and preserves the rest`,
    async () => {
      const pool = new Pool({ connectionString: DB_URL, max: 1, connectionTimeoutMillis: 5_000 });
      const client = await pool.connect();
      try {
        await client.query("BEGIN");
        // The temporary tables shadow the stored tables on this connection.
        // Run the committed SQL without changing any persistent data.
        await client.query(`
          CREATE TEMP TABLE profile (
            id text PRIMARY KEY,
            harness text NOT NULL,
            model_router text,
            model text,
            updated_at timestamp NOT NULL DEFAULT '2000-01-01'
          ) ON COMMIT DROP;
          CREATE TEMP TABLE task (
            id text PRIMARY KEY,
            harness text,
            model_router text,
            model text,
            updated_at timestamp NOT NULL DEFAULT '2000-01-01'
          ) ON COMMIT DROP;
          CREATE TEMP TABLE automation (
            id text PRIMARY KEY,
            block_overrides jsonb NOT NULL DEFAULT '{}',
            updated_at timestamp NOT NULL DEFAULT '2000-01-01'
          ) ON COMMIT DROP;
          CREATE TEMP TABLE automation_version (
            automation_id text NOT NULL,
            version integer NOT NULL,
            blocks jsonb NOT NULL,
            entrypoints jsonb NOT NULL DEFAULT '[]'
          ) ON COMMIT DROP;
        `);

        // [id, harness, router, model, expected model]
        const rows: [string, string, string | null, string | null, string | null][] = [];
        for (const [removed, family] of Object.entries(m.moved)) {
          rows.push([`native-${removed}`, m.harness, null, removed, family]);
          rows.push([`routed-${removed}`, m.harness, "openrouter", removed, removed]);
          rows.push([`other-${removed}`, m.otherHarness, null, removed, removed]);
        }
        for (const model of [...m.kept, null]) {
          rows.push([`kept-${model ?? "default"}`, m.harness, null, model, model]);
        }
        for (const table of ["profile", "task"]) {
          for (const [id, harness, router, model] of rows) {
            await client.query(
              `INSERT INTO ${table} (id, harness, model_router, model) VALUES ($1, $2, $3, $4)`,
              [id, harness, router, model],
            );
          }
        }

        const [first, firstFamily] = Object.entries(m.moved)[0]!;
        const [last, lastFamily] = Object.entries(m.moved).at(-1)!;
        const kept = m.kept[0]!;
        // A prompt that quotes a removed id stays as it is: the quotes are
        // escaped inside a JSON string value.
        const quoted = { id: "q", type: "session", config: { prompt: `say "model": "${first}"` } };
        await client.query(
          "INSERT INTO automation_version (automation_id, version, blocks, entrypoints) VALUES ($1, 1, $2, $3), ($4, 1, $5, '[]')",
          [
            "a1",
            JSON.stringify([
              { id: "b", type: "branch", config: {}, then: [session("s1", first)], else: [session("s2", kept)] },
            ]),
            JSON.stringify([{ id: "e", trigger: {}, blocks: [session("s3", last)] }]),
            "a2",
            JSON.stringify([quoted]),
          ],
        );
        await client.query("INSERT INTO automation (id, block_overrides) VALUES ('a1', $1), ('a2', $2)", [
          JSON.stringify({ s1: { model: last, effort: "high" } }),
          JSON.stringify({ q: quoted.config }),
        ]);

        const sql = await readFile(join(import.meta.dir, "..", "..", "drizzle", m.file), "utf8");
        await client.query(sql);

        for (const table of ["profile", "task"]) {
          const result = await client.query<{ id: string; model: string | null; changed_at: boolean }>(
            `SELECT id, model, updated_at <> '2000-01-01'::timestamp AS changed_at FROM ${table}`,
          );
          expect(result.rows).toHaveLength(rows.length);
          for (const row of result.rows) {
            const expected = rows.find(([id]) => id === row.id);
            if (!expected) throw new Error(`Unexpected ${table} ${row.id}`);
            expect(row.model).toBe(expected[4]);
            // Profiles record the edit; a task keeps its updated_at so the
            // task lists do not reorder.
            expect(row.changed_at).toBe(table === "profile" && row.id.startsWith("native-"));
          }
        }

        const versions = await client.query<{ blocks: unknown; entrypoints: unknown }>(
          "SELECT blocks, entrypoints FROM automation_version ORDER BY automation_id",
        );
        expect(versions.rows[0]!.blocks).toEqual([
          { id: "b", type: "branch", config: {}, then: [session("s1", firstFamily)], else: [session("s2", kept)] },
        ]);
        expect(versions.rows[0]!.entrypoints).toEqual([{ id: "e", trigger: {}, blocks: [session("s3", lastFamily)] }]);
        expect(versions.rows[1]!.blocks).toEqual([quoted]);

        const automations = await client.query<{ id: string; block_overrides: unknown; changed_at: boolean }>(
          "SELECT id, block_overrides, updated_at <> '2000-01-01'::timestamp AS changed_at FROM automation ORDER BY id",
        );
        expect(automations.rows).toEqual([
          { id: "a1", block_overrides: { s1: { model: lastFamily, effort: "high" } }, changed_at: true },
          { id: "a2", block_overrides: { q: quoted.config }, changed_at: false },
        ]);

        // A second run finds nothing left to move.
        const rerun = await client.query(sql);
        expect((Array.isArray(rerun) ? rerun : [rerun]).map((r) => r.rowCount)).toEqual([0, 0, 0, 0]);
      } finally {
        await client.query("ROLLBACK");
        client.release();
        await pool.end();
      }
    },
  );
}
