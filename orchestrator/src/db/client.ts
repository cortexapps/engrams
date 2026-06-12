/**
 * Drizzle singleton for the orchestrator's engram_orchestrator database.
 *
 * Design choices:
 *  - `pg` Pool so connections are reused across requests and the process can
 *    close cleanly on SIGTERM.
 *  - The module initialises lazily: `getDb()` creates the pool on first call
 *    so the orchestrator boots even when ORCHESTRATOR_DATABASE_URL is unset
 *    (healthz can then report {ok:false, db:false} rather than crashing at
 *    import time).
 *  - `checkDb()` issues a `SELECT 1` inside a transaction with
 *    `SET LOCAL statement_timeout = 2000` so the timeout is scoped to that
 *    transaction only and does not leak onto the pooled connection for
 *    subsequent callers. A `connectionTimeoutMillis` cap on the Pool ensures
 *    that even a black-holed DB host cannot hang /healthz indefinitely (pg's
 *    default is 0 = wait forever).
 */

import { drizzle, type NodePgDatabase } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import * as schema from "./schema.ts";

let _db: NodePgDatabase<typeof schema> | null = null;
let _pool: Pool | null = null;

/**
 * Return the drizzle singleton, creating it on first call.
 * Throws if ORCHESTRATOR_DATABASE_URL is not set.
 */
export function getDb(): NodePgDatabase<typeof schema> {
  if (_db) return _db;

  const url = process.env["ORCHESTRATOR_DATABASE_URL"];
  if (!url) {
    throw new Error(
      "Orchestrator: ORCHESTRATOR_DATABASE_URL is not set — cannot connect to DB",
    );
  }

  _pool = new Pool({
    connectionString: url,
    max: 10,
    // Cap how long pool.connect() waits for a TCP connection. pg's default is
    // 0 (wait forever), which would cause /healthz to hang ~75 s on a
    // black-holed host before the OS gives up.
    connectionTimeoutMillis: 2000,
  });
  _db = drizzle(_pool, { schema });
  return _db;
}

/**
 * Perform a cheap liveness probe (`SELECT 1`) with a 2-second timeout.
 * Returns true when the DB is reachable, false otherwise (never throws).
 */
export async function checkDb(): Promise<boolean> {
  try {
    const pool = _pool ?? (() => {
      // Attempt to initialise — will throw if URL is missing.
      getDb();
      return _pool!;
    })();

    const client = await pool.connect();
    try {
      // Use SET LOCAL so the timeout is scoped to this transaction only.
      // A plain SET would persist for the lifetime of the pooled connection
      // and silently apply to all subsequent queries issued by other callers.
      await client.query("BEGIN");
      await client.query("SET LOCAL statement_timeout = 2000");
      await client.query("SELECT 1");
      await client.query("COMMIT");
      return true;
    } catch (err) {
      await client.query("ROLLBACK").catch(() => {});
      throw err;
    } finally {
      client.release();
    }
  } catch {
    return false;
  }
}
