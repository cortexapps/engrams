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
import { drizzle } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import { Pool } from "pg";

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
const pool = new Pool({ connectionString: url, max: 1, connectionTimeoutMillis: 10_000 });

try {
  await migrate(drizzle(pool), { migrationsFolder: "./drizzle" });
  console.log("orchestrator migrate: schema up to date");
} catch (err) {
  console.error("orchestrator migrate: FAILED", err);
  process.exitCode = 1;
} finally {
  await pool.end();
}
