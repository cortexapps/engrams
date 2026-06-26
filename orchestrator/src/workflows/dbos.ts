/**
 * Embedded DBOS engine lifecycle (ADR 0060 P0, Decision 3).
 *
 * DBOS runs *inside* the Bun orchestrator process — durable workflows backed by
 * the orchestrator's existing Postgres, with the DBOS system tables isolated in
 * a dedicated `dbos` schema (so they never collide with the app's Drizzle
 * tables). The engine is launched once before the HTTP server starts serving
 * and shut down on SIGTERM.
 *
 * In DBOS 4.x the *application* talks to Postgres through its own connections
 * (Drizzle, here); `setConfig` only configures the engine's **system** database
 * — hence `systemDatabaseUrl` (not a generic `databaseUrl`). `runAdminServer`
 * is off: we expose no DBOS admin HTTP surface.
 *
 * Workflows/steps must be *registered* (module import of the workflow files)
 * before `DBOS.launch()` — index.ts imports them above `initDbos()`.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { config } from "../config.ts";
import { dbosLogger } from "./dbos-logger.ts";

let launched = false;

/** Configure + launch the embedded DBOS engine. Idempotent. */
export async function initDbos(): Promise<void> {
  if (launched) return;
  DBOS.setConfig({
    name: "engrams-orchestrator",
    // The orchestrator's Postgres; DBOS system tables live in the `dbos`
    // schema of that database (privilege-restricted prod pre-creates it via
    // `npx dbos schema`; dev/CI lets launch create it).
    systemDatabaseUrl: config.databaseUrl,
    systemDatabaseSchemaName: "dbos",
    runAdminServer: false,
    // Send the engine's logging through our pino logger (replaces DBOS's
    // built-in console + OTLP sinks) so all process output is one format.
    logger: dbosLogger,
  });
  await DBOS.launch();
  launched = true;
}

/** Tear down the engine. Idempotent; safe to call when never launched. */
export async function shutdownDbos(): Promise<void> {
  if (!launched) return;
  await DBOS.shutdown();
  launched = false;
}
