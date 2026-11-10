/**
 * The orchestrator's logger — one pino instance for the whole process.
 *
 *   - default:         structured JSON on stdout.
 *   - local dev:       pretty, colorized lines via pino-pretty when
 *                      `ENGRAM_LOG_FORMAT=pretty` (the Tiltfile sets it). pino-pretty
 *                      is a direct destination stream, NOT a worker transport,
 *                      which is the Bun-safe way — no thread-stream worker to resolve.
 *   - tests:           silent (NODE_ENV=test, set by `bun test`) so the suite
 *                      output stays clean.
 *
 * Level is `LOG_LEVEL` (default `info`). Tag a subsystem with a child logger,
 * e.g. `log.child({ component: "slack" })`, so its lines are filterable.
 */

import pino from "pino";
import pretty from "pino-pretty";

const isTest = process.env.NODE_ENV === "test";
const level = process.env.LOG_LEVEL ?? (isTest ? "silent" : "info");
// The same spellings engram-telemetry accepts for the Rust services.
const FORMATS = new Map<string, "json" | "pretty">([
  ["json", "json"],
  ["JSON", "json"],
  ["pretty", "pretty"],
  ["PRETTY", "pretty"],
]);
const rawFormat = process.env.ENGRAM_LOG_FORMAT ?? "json";
const format = FORMATS.get(rawFormat);
if (!format) {
  throw new Error(`ENGRAM_LOG_FORMAT must be "json" or "pretty", not "${rawFormat}"`);
}

export const log =
  format === "pretty"
    ? pino(
        { level },
        pretty({ colorize: true, translateTime: "SYS:HH:MM:ss", ignore: "pid,hostname" }),
      )
    : pino({ level });

/**
 * ONE formatting rule for unknown thrown values. Use this instead of ad-hoc
 * `error instanceof Error ? error.message : String(error)` copies so log
 * lines and user-facing messages stay uniform.
 */
export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
