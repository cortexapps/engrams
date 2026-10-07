import { expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { Pool } from "pg";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const REMOVED_MODELS = [
  "gpt-5.6-sol",
  "gpt-5.6-terra",
  "gpt-5.6-luna",
  "gpt-5.5",
  "gpt-5.4",
  "gpt-5.4-mini",
  "gpt-5.3-codex-spark",
];

test.skipIf(!DB_URL)(
  "migration 0095 updates removed native Codex selections and preserves other profiles",
  async () => {
    const pool = new Pool({ connectionString: DB_URL, max: 1, connectionTimeoutMillis: 5_000 });
    const client = await pool.connect();
    try {
      await client.query("BEGIN");
      // The temporary table shadows the stored profile table on this connection.
      // Run the committed SQL without changing any persistent data.
      await client.query(`
      CREATE TEMP TABLE profile (
        id text PRIMARY KEY,
        harness text NOT NULL,
        model_router text,
        model text,
        effort text DEFAULT 'high',
        updated_at timestamp NOT NULL DEFAULT '2000-01-01'
      ) ON COMMIT DROP
    `);
      const expected = new Map<string, string | null>();
      for (const model of REMOVED_MODELS) {
        for (const [kind, harness, router] of [
          ["native", "codex", null],
          ["routed", "codex", "openrouter"],
          ["other", "claude", null],
        ] as const) {
          const id = `${kind}-${model}`;
          await client.query(
            "INSERT INTO profile (id, harness, model_router, model) VALUES ($1, $2, $3, $4)",
            [id, harness, router, model],
          );
          expected.set(id, kind === "native" ? "gpt-6.1-sol" : model);
        }
      }
      for (const model of [null, "gpt-6.1-sol", "gpt-6-astra", "gpt-6-luna", "custom-model"]) {
        const id = `unchanged-${model ?? "default"}`;
        await client.query("INSERT INTO profile (id, harness, model) VALUES ($1, 'codex', $2)", [
          id,
          model,
        ]);
        expected.set(id, model);
      }
      const sql = await readFile(
        join(import.meta.dir, "..", "..", "drizzle", "0095_migrate_codex_profile_models.sql"),
        "utf8",
      );
      expect((await client.query(sql)).rowCount).toBe(REMOVED_MODELS.length);
      const result = await client.query<{
        id: string;
        model: string | null;
        effort: string;
        changed_at: boolean;
      }>(
        "SELECT id, model, effort, updated_at <> '2000-01-01'::timestamp AS changed_at FROM profile",
      );
      expect(result.rows).toHaveLength(expected.size);
      for (const row of result.rows) {
        const expectedModel = expected.get(row.id);
        if (expectedModel === undefined) throw new Error(`Unexpected profile ${row.id}`);
        expect(row.model).toBe(expectedModel);
        expect(row.effort).toBe("high");
        expect(row.changed_at).toBe(row.id.startsWith("native-"));
      }
      expect((await client.query(sql)).rowCount).toBe(0);
    } finally {
      await client.query("ROLLBACK");
      client.release();
      await pool.end();
    }
  },
);
