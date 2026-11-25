import { expect, test } from "bun:test";
import {
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { drizzle } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import { Pool } from "pg";

const orchestratorDir = join(import.meta.dir, "..", "..");
const drizzleDir = join(orchestratorDir, "drizzle");
const databaseUrl = process.env["ORCHESTRATOR_DATABASE_URL"];
const ledger = "drizzle.__drizzle_migrations";

test.skipIf(!databaseUrl)(
  "upgrades the upstream child-role history and retries a failed MCP migration",
  async () => {
    const name = `mcp_upgrade_${crypto.randomUUID().replaceAll("-", "")}`;
    const admin = new Pool({ connectionString: databaseUrl, max: 1 });
    const url = new URL(databaseUrl!);
    url.pathname = `/${name}`;
    const connectionString = url.toString();
    const pool = new Pool({ connectionString, max: 1 });
    const upstreamFolder = await mkdtemp(
      join(tmpdir(), "engram-upstream-migrations-"),
    );
    try {
      await admin.query(`create database "${name}"`);
      const journal = JSON.parse(
        await readFile(join(drizzleDir, "meta", "_journal.json"), "utf8"),
      ) as {
        entries: { idx: number; when: number; tag: string }[];
      };
      const child = journal.entries.find(
        (entry) => entry.tag === "0116_automation_session_child_role",
      )!;
      const upstream = {
        ...child,
        idx: 101,
        tag: "0101_automation_session_child_role",
      };
      expect(upstream.when).toBe(1795115737039);
      const entries = [...journal.entries.slice(0, 101), upstream];
      await mkdir(join(upstreamFolder, "meta"));
      await writeFile(
        join(upstreamFolder, "meta", "_journal.json"),
        JSON.stringify({ entries }),
      );
      for (const entry of entries) {
        const tag = entry === upstream ? child.tag : entry.tag;
        await symlink(
          join(drizzleDir, `${tag}.sql`),
          join(upstreamFolder, `${entry.tag}.sql`),
        );
      }
      await migrate(drizzle(pool), { migrationsFolder: upstreamFolder });
      const applied = async () =>
        (
          await pool.query(
            `select hash, created_at from ${ledger} order by created_at`,
          )
        ).rows;
      const before = await applied();
      const childRecord = before.at(-1);
      expect(childRecord?.created_at).toBe(String(upstream.when));
      const run = () =>
        Bun.spawnSync([process.execPath, "run", "migrate"], {
          cwd: orchestratorDir,
          env: { ...process.env, ORCHESTRATOR_DATABASE_URL: connectionString },
        });

      await pool.query(
        `update ${ledger} set hash = 'unexpected' where created_at = $1`,
        [upstream.when],
      );
      const stale = run();
      expect(stale.exitCode).toBe(1);
      expect(stale.stderr.toString()).toContain(
        `Unexpected migration record at ${upstream.when}`,
      );
      await pool.query(`update ${ledger} set hash = $1 where created_at = $2`, [
        childRecord?.hash,
        upstream.when,
      ]);
      expect(await applied()).toEqual(before);

      await pool.query("create table mcp_discovery_admission (id text)");
      const failure = run();
      expect(failure.exitCode).toBe(1);
      expect(failure.stderr.toString()).toContain(
        'relation "mcp_discovery_admission" already exists',
      );
      expect(await applied()).toEqual(before);
      expect(
        (
          await pool.query(
            "select to_regclass('public.mcp_admission') as table_name",
          )
        ).rows,
      ).toEqual([{ table_name: null }]);
      await pool.query("drop table mcp_discovery_admission");

      const success = run();
      expect(success.exitCode, success.stderr.toString()).toBe(0);
      const after = await applied();
      expect(after).toHaveLength(before.length + 15);
      expect(after.slice(0, 101)).toEqual(before.slice(0, 101));
      expect(after.at(-1)).toEqual(before.at(-1));
      expect(
        (
          await pool.query(
            "select to_regclass('public.mcp_discovery_admission') as table_name",
          )
        ).rows,
      ).toEqual([{ table_name: "mcp_discovery_admission" }]);
      expect(run().exitCode).toBe(0);
      expect(await applied()).toEqual(after);
    } finally {
      await pool.end();
      try {
        await admin.query(`drop database if exists "${name}"`);
      } finally {
        await admin.end();
        await rm(upstreamFolder, { recursive: true, force: true });
      }
    }
  },
  60_000,
);
