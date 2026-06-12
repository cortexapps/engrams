/**
 * /healthz — liveness probe.
 *
 * Shape:
 *   200 { ok: true,  db: true  }  — fully healthy
 *   503 { ok: false, db: false }  — DB unreachable (orchestrator still alive)
 *
 * The DB ping uses a 2-second statement_timeout so a dead or slow database
 * does not hang the probe. The orchestrator boots without a DB connection —
 * healthz reports the problem instead of the process crashing on import.
 */

import { Hono } from "hono";
import { checkDb } from "../db/client.ts";

const health = new Hono();

health.get("/healthz", async (c) => {
  const db = await checkDb();
  if (db) {
    return c.json({ ok: true, db: true }, 200);
  }
  return c.json({ ok: false, db: false }, 503);
});

export default health;
