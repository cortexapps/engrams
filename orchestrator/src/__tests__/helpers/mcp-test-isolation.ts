import { Pool } from "pg";

/** Verify both actual database identities before any DBOS configuration or fixture write. */
export async function verifyMcpTestDatabases() {
  const app = process.env.ORCHESTRATOR_DATABASE_URL;
  const system = process.env.MCP_TEST_DBOS_DATABASE_URL;
  const expected = process.env.MCP_TEST_DATABASE_NAME;
  if (!app || !system || !expected) throw Error("explicit isolated MCP test databases required");
  const a = new URL(app),
    b = new URL(system);
  if (
    !/^engram_mcp_\d+_\d+_\d+$/.test(expected) ||
    a.pathname !== `/${expected}` ||
    !/^\/engram_mcp_dbos_test_[a-z0-9_]+$/.test(b.pathname) ||
    a.pathname === b.pathname ||
    [a, b].some((u) => u.hostname !== "127.0.0.1" || u.port !== "5435")
  )
    throw Error("MCP test database isolation mismatch");
  for (const url of [a, b]) {
    const pool = new Pool({ connectionString: url.href, connectionTimeoutMillis: 2000 });
    try {
      const result = await pool.query("select current_database() as name");
      if (result.rows[0]?.name !== url.pathname.slice(1))
        throw Error("MCP test database identity mismatch");
    } finally {
      await pool.end();
    }
  }
  return { app, system };
}
