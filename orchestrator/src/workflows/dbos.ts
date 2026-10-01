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
import { hostname } from "node:os";
import { config } from "../config.ts";
import { dbosLogger } from "./dbos-logger.ts";

let launched = false;

/** This process's DBOS executor id: the pod name (the hostname inside a
 * pod), or `DBOS__VMID` when set. DBOS stamps it on every workflow the
 * process runs and, at launch, recovers ONLY the PENDING workflows that
 * carry it. The SDK's default is the constant "local" for every process,
 * and with two replicas each new pod then recovered every in-flight
 * workflow at boot — two copies of one run, colliding at the next step
 * write ("Conflicting WF ID"). A pod that is gone never comes back under
 * its name; the orphan sweep re-enqueues what it left (ADR 0104). The
 * sweep's per-pod heartbeat uses this same id, so liveness and ownership
 * are one name. */
export function dbosExecutorId(): string {
  return process.env["DBOS__VMID"] || hostname();
}

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
    executorID: dbosExecutorId(),
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
