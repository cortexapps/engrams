/**
 * The orchestrator's logger — one pino instance for the whole process.
 *
 *   - dev (default):   pretty, colorized lines via pino-pretty used as a direct
 *                      destination stream (NOT a worker transport, which is the
 *                      Bun-safe way — no thread-stream worker to resolve).
 *   - production:      structured JSON on stdout (NODE_ENV=production).
 *   - tests:           silent (NODE_ENV=test, set by `bun test`) so the suite
 *                      output stays clean.
 *
 * Level is `LOG_LEVEL` (default `info`). Tag a subsystem with a child logger,
 * e.g. `log.child({ component: "slack" })`, so its lines are filterable.
 */

import pino from "pino";
import pretty from "pino-pretty";

const isProd = process.env.NODE_ENV === "production";
const isTest = process.env.NODE_ENV === "test";
const level = process.env.LOG_LEVEL ?? (isTest ? "silent" : "info");

export const log =
  isProd || isTest
    ? pino({ level })
    : pino(
        { level },
        pretty({ colorize: true, translateTime: "SYS:HH:MM:ss", ignore: "pid,hostname" }),
      );
