/**
 * One-shot migration runner for the orchestrator's engram_orchestrator DB.
 *
 * Applies the committed SQL migrations in ./drizzle using drizzle-orm's
 * PROGRAMMATIC migrator — deliberately NOT the `drizzle-kit` CLI.
 *
 * Why: `drizzle-kit` is a devDependency, and the runtime image is built with
 * `bun install --production` (docker/orchestrator.Dockerfile), which strips it.
 * `bunx drizzle-kit migrate` would therefore refetch drizzle-kit (+ esbuild and
 * friends) from npm on EVERY deploy — on the Helm pre-upgrade hook's critical
 * path, blocking the whole rollout. The programmatic migrator needs only
 * `drizzle-orm` + `pg` (both production deps already in the image) and reads the
 * same ./drizzle folder + meta/_journal.json that `drizzle-kit generate`
 * produced. It tracks applied migrations in the same `drizzle.__drizzle_migrations`
 * table the CLI uses, so this is a safe drop-in on an already-migrated DB. This
 * is Drizzle's documented production path (orm.drizzle.team/docs/migrations,
 * "Option 4"); `drizzle-kit generate` is still used locally to author migrations.
 *
 * Invoked by the orchestrator-migrate Helm Job via `bun run migrate`.
 */
import { readMigrationFiles } from "drizzle-orm/migrator";
import { drizzle } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import { Pool } from "pg";

const migrationConfig = { migrationsFolder: "./drizzle" };
const ledger = "drizzle.__drizzle_migrations";
const commonMigration = 1793241391817; // 0100_profile_programmatic_only
const childRoleMigration = 1795115737039; // 0116_automation_session_child_role (formerly 0101)

// The child-role migration can be present before the MCP migrations. Drizzle
// uses the highest recorded timestamp, so it skips this older range.
async function repairMcpMigrationHistory(pool: Pool): Promise<void> {
  const relation = await pool.query<{ exists: boolean }>(
    "select to_regclass($1) is not null as exists",
    [ledger],
  );
  if (!relation.rows[0]?.exists) return;

  const child = await pool.query(
    `select 1 from ${ledger} where created_at = $1`,
    [childRoleMigration],
  );
  if (child.rows.length === 0) return;

  const range = readMigrationFiles(migrationConfig).filter(
    ({ folderMillis }) =>
      folderMillis >= commonMigration && folderMillis <= childRoleMigration,
  );
  const hashes = new Map(
    range.map(({ folderMillis, hash }) => [folderMillis, hash]),
  );
  const mcp = range.filter(
    ({ folderMillis }) =>
      folderMillis > commonMigration && folderMillis < childRoleMigration,
  );

  const client = await pool.connect();
  try {
    await client.query("begin");
    await client.query(`lock table ${ledger} in share row exclusive mode`);
    const { rows } = await client.query<{ created_at: string; hash: string }>(
      `select created_at, hash from ${ledger} where created_at between $1 and $2`,
      [commonMigration, childRoleMigration],
    );
    const applied = new Set<number>();
    for (const row of rows) {
      const timestamp = Number(row.created_at);
      if (hashes.get(timestamp) !== row.hash) {
        throw new Error(`Unexpected migration record at ${timestamp}`);
      }
      applied.add(timestamp);
    }
    if (!applied.has(commonMigration) || !applied.has(childRoleMigration)) {
      throw new Error("Missing journal record for the MCP migration history");
    }

    const appliedCount = mcp.filter(({ folderMillis }) =>
      applied.has(folderMillis),
    ).length;
    if (
      mcp
        .slice(0, appliedCount)
        .some(({ folderMillis }) => !applied.has(folderMillis))
    ) {
      throw new Error("The applied MCP migrations have a gap in the journal");
    }
    for (const migration of mcp.slice(appliedCount)) {
      for (const statement of migration.sql) {
        await client.query(statement);
      }
      await client.query(
        `insert into ${ledger} (hash, created_at) values ($1, $2)`,
        [migration.hash, migration.folderMillis],
      );
    }
    await client.query("commit");
  } catch (error) {
    await client.query("rollback");
    throw error;
  } finally {
    client.release();
  }
}

const url = process.env["ORCHESTRATOR_DATABASE_URL"];
if (!url) {
  console.error(
    "orchestrator migrate: ORCHESTRATOR_DATABASE_URL is not set — cannot connect",
  );
  process.exit(1);
}

// A dedicated single-connection pool for this one-shot process (not the app's
// shared client) so we can end() it and exit cleanly. connectionString carries
// the sslmode (no-verify for the private-IP CloudSQL hop) exactly as the app's
// pool does, so TLS handling is identical.
const pool = new Pool({
  connectionString: url,
  max: 1,
  connectionTimeoutMillis: 10_000,
});

try {
  await repairMcpMigrationHistory(pool);
  await migrate(drizzle(pool), migrationConfig);
  console.log("orchestrator migrate: schema up to date");
} catch (err) {
  console.error("orchestrator migrate: FAILED", err);
  process.exitCode = 1;
} finally {
  await pool.end();
}
