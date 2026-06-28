/**
 * Route DBOS's internal logging through the orchestrator's pino logger
 * (ADR 0060 P0).
 *
 * DBOS 4.x accepts a custom `DLogger` via `DBOS.setConfig({ logger })`; once set,
 * DBOS directs ALL of its internal logging — and every `DBOS.logger` call inside
 * workflows/steps — here, replacing its built-in console + OTLP log sinks
 * (https://docs.dbos.dev/typescript/tutorials/logging). This unifies the engine's
 * output with the rest of the process: one pino instance, one format, one
 * `LOG_LEVEL`, and DBOS lines tagged `component:"dbos"` so they're filterable.
 *
 * Contract we honor (from the `DLogger` docs):
 *  - Entries arrive already stringified; `error()` gets the message string with
 *    the stack trace in `metadata.stack`.
 *  - DBOS does NOT pre-filter by level before delegating — pino owns level
 *    routing (it drops anything below `LOG_LEVEL`).
 *  - In a workflow/step, `metadata.span.attributes` carries the operation context
 *    (workflow id, operation name/type, …); we forward it as structured pino
 *    fields so engine lines carry the same context the app's do.
 *  - We never log back through `DBOS.logger` (only pino), so there's no recursion.
 */

import type { DLogger, ContextualMetadata } from "@dbos-inc/dbos-sdk";
import { log as rootLog } from "../log.ts";

/** The level methods of pino the adapter drives (each accepts a merge object +
 *  message). A `pino.Logger` (or child) satisfies it; tests inject a fake. */
export interface LeveledLogger {
  info(obj: object, msg: string): void;
  debug(obj: object, msg: string): void;
  warn(obj: object, msg: string): void;
  error(obj: object, msg: string): void;
}

/** The DBOS operation context (workflow id, op name/type, …) off the span, as
 *  plain pino fields. Empty object when there's no span (engine-lifecycle logs). */
function fields(metadata?: ContextualMetadata): Record<string, unknown> {
  const attrs = metadata?.span?.attributes;
  return attrs ? { ...attrs } : {};
}

/** Build a `DLogger` that forwards to `logger`. */
export function makeDbosLogger(logger: LeveledLogger): DLogger {
  return {
    info(entry, metadata) {
      logger.info(fields(metadata), String(entry));
    },
    debug(entry, metadata) {
      logger.debug(fields(metadata), String(entry));
    },
    warn(entry, metadata) {
      logger.warn(fields(metadata), String(entry));
    },
    error(inputError, metadata) {
      const f = fields(metadata);
      if (metadata?.stack) f.stack = metadata.stack;
      logger.error(f, String(inputError));
    },
  };
}

/** The production DBOS logger: pino, tagged `component:"dbos"`. */
export const dbosLogger: DLogger = makeDbosLogger(rootLog.child({ component: "dbos" }));
