import { describe, expect, test } from "bun:test";

import { interpretAutomation } from "../../engine/interpreter.ts";
import type { EngineDeps, EngineRunStore, EngineSessionOps, EngineStepRecord } from "../../engine/deps.ts";
import type { AutomationDefinition, BlockDef } from "../../engine/definition.ts";
import type { RunSnapshot } from "../../engine/context.ts";
import { makeCodeBlockRuntime } from "../runtime.ts";

/** The real QuickJS runtime driven through the interpreter's code block. */

function makeDeps(blocks: BlockDef[], payload?: Record<string, unknown>) {
  const definition: AutomationDefinition = {
    engine: 1,
    trigger: { kind: "manual" },
    blocks,
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
  const snapshot: RunSnapshot = {
    definition,
    inputs: { threshold: 10 },
    automationId: "auto-1",
    automationName: "Code test",
    version: 1,
    trigger: {
      kind: "manual",
      receivedAt: "2026-08-21T00:00:00Z",
      ...(payload ? { payload, eventKey: "test.event" } : {}),
    },
    aliases: [],
    startedAtMs: 1_000_000_000,
  };
  const stepRecords: Array<{ framePath: string; record: EngineStepRecord }> = [];
  const store: EngineRunStore = {
    async loadSnapshot() {
      return snapshot;
    },
    async markRunning() {},
    async recordStep(_runId, framePath, _attempt, record) {
      stepRecords.push({ framePath, record });
    },
    async finalizeRun() {},
    async listRunSessions() {
      return [];
    },
    async releaseConcurrency() {
      return null;
    },
  };
  const sessions: EngineSessionOps = {
    async createSession() {
      throw new Error("unused");
    },
    async setSessionRelay() {},
    async sendPrompt() {},
    async endSession() {},
    async exec() {
      throw new Error("unused");
    },
    async writeFiles() {
      return [];
    },
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store,
    sessions,
    clock: { nowMs: () => 1_000_000_000 },
    code: makeCodeBlockRuntime(),
  };
  return { deps, stepRecords };
}

const RUN = { runId: "autorun:auto-1:manual:x", automationId: "auto-1" };

describe("code block through the interpreter with the real sandbox", () => {
  test("value mode writes the computed value into steps outputs", async () => {
    const { deps, stepRecords } = makeDeps(
      [
        {
          id: "transform",
          type: "code",
          config: {
            source: `export default ({ event, inputs }) =>
              ({ big: event.raw.count > inputs.threshold, doubled: event.raw.count * 2 });`,
            mode: "value",
          },
        },
      ],
      { count: 21 },
    );
    const result = await interpretAutomation(RUN, deps);
    expect(result.status).toBe("completed");
    const record = stepRecords.filter((r) => r.framePath === "transform").at(-1)!;
    expect(record.record.outputs).toEqual({ value: { big: true, doubled: 42 } });
  });

  test("boolean mode false filters the run", async () => {
    const { deps } = makeDeps(
      [
        {
          id: "gate",
          type: "code",
          config: {
            source: `export default ({ event }) => event.raw.count > 100;`,
            mode: "boolean",
          },
        },
      ],
      { count: 5 },
    );
    const result = await interpretAutomation(RUN, deps);
    expect(result.status).toBe("filtered");
  });

  test("a guest error fails the run with the sandbox error code", async () => {
    const { deps } = makeDeps([
      {
        id: "boom",
        type: "code",
        config: { source: `export default () => { throw new Error("nope"); };`, mode: "value" },
      },
    ]);
    const result = await interpretAutomation(RUN, deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("code_error");
  });
});
