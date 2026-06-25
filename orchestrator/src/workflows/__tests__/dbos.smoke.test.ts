/**
 * DBOS foundation smoke test (ADR 0059 P0).
 *
 * Proves the embedded DBOS engine wiring — `initDbos()` configures + launches
 * the engine against the orchestrator's Postgres (DBOS system tables isolated
 * in the `dbos` schema), a workflow we register runs to completion through it,
 * and `shutdownDbos()` tears it down cleanly.
 *
 * This is a *wiring* smoke, not a durability test: DBOS guarantees workflow
 * replay/recovery, so we do not re-test the framework — we only assert our own
 * setup stands up a working engine.
 *
 * Env-gated on ORCHESTRATOR_DATABASE_URL being set AND reachable (mirrors
 * db.test.ts): absent/unreachable → reported as SKIP (not silent pass), so CI
 * surfaces the gap honestly. The orchestrator CI lane provides a Postgres
 * service + the URL, so this runs there.
 */

import { expect, test, describe, afterAll } from "bun:test";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { initDbos, shutdownDbos } from "../dbos.ts";
import { checkDb } from "../../db/client.ts";

// A trivial durable workflow, registered at module load (before launch — DBOS
// requires registration before `DBOS.launch()`). Exists only to prove the
// engine stands up and executes our workflows end-to-end against Postgres.
const pingWorkflow = DBOS.registerWorkflow(
  async (): Promise<string> => "pong",
  { name: "ping-smoke-workflow" },
);

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("DBOS foundation (requires ORCHESTRATOR_DATABASE_URL)", () => {
  afterAll(async () => {
    await shutdownDbos();
  });

  test.skipIf(!dbReachable)(
    "launches the embedded engine and runs a workflow to completion",
    async () => {
      await initDbos();
      const handle = await DBOS.startWorkflow(pingWorkflow)();
      const result = await handle.getResult();
      expect(result).toBe("pong");
    },
  );
});
