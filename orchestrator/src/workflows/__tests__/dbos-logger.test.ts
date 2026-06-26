/**
 * makeDbosLogger (ADR 0060 P0 — DBOS logging through pino).
 *
 * The adapter forwards DBOS's `DLogger` calls to a pino-shaped logger, lifting
 * the span attributes (workflow id, op name) into structured fields and the
 * error stack into a `stack` field. Exercised through a fake logger so no pino
 * or engine is involved.
 */

import { expect, test, describe } from "bun:test";
import { makeDbosLogger, type LeveledLogger } from "../dbos-logger.ts";

function fakeLogger() {
  const calls: { level: string; obj: object; msg: string }[] = [];
  const rec = (level: string) => (obj: object, msg: string) => void calls.push({ level, obj, msg });
  const logger: LeveledLogger = {
    info: rec("info"),
    debug: rec("debug"),
    warn: rec("warn"),
    error: rec("error"),
  };
  return { logger, calls };
}

describe("makeDbosLogger()", () => {
  test("forwards info/debug/warn at the matching level with the entry as the message", () => {
    const { logger, calls } = fakeLogger();
    const dl = makeDbosLogger(logger);
    dl.info("engine launched");
    dl.debug("step finished");
    dl.warn("retrying");
    expect(calls).toEqual([
      { level: "info", obj: {}, msg: "engine launched" },
      { level: "debug", obj: {}, msg: "step finished" },
      { level: "warn", obj: {}, msg: "retrying" },
    ]);
  });

  test("lifts span attributes (workflow id, op name) into structured fields", () => {
    const { logger, calls } = fakeLogger();
    const dl = makeDbosLogger(logger);
    dl.info("running workflow", {
      span: { attributes: { workflowUUID: "wf-1", operationName: "SlackThreadWorkflow" } },
    } as unknown as Parameters<typeof dl.info>[1]);
    expect(calls).toHaveLength(1);
    expect(calls[0].level).toBe("info");
    expect(calls[0].msg).toBe("running workflow");
    expect(calls[0].obj).toEqual({ workflowUUID: "wf-1", operationName: "SlackThreadWorkflow" });
  });

  test("stringifies a non-string entry", () => {
    const { logger, calls } = fakeLogger();
    makeDbosLogger(logger).info(42);
    expect(calls[0].msg).toBe("42");
  });

  test("error routes to error level and carries the stack in a `stack` field", () => {
    const { logger, calls } = fakeLogger();
    const dl = makeDbosLogger(logger);
    dl.error("boom", {
      stack: "Error: boom\n  at x",
      span: { attributes: { operationName: "createTask" } },
    } as unknown as Parameters<typeof dl.error>[1]);
    expect(calls).toHaveLength(1);
    expect(calls[0].level).toBe("error");
    expect(calls[0].msg).toBe("boom");
    expect(calls[0].obj).toEqual({ operationName: "createTask", stack: "Error: boom\n  at x" });
  });

  test("error with no metadata still logs the message (no stack field)", () => {
    const { logger, calls } = fakeLogger();
    makeDbosLogger(logger).error("plain failure");
    expect(calls[0]).toEqual({ level: "error", obj: {}, msg: "plain failure" });
  });
});
