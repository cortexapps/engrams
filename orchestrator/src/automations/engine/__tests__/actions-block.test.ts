import { beforeEach, describe, expect, test } from "bun:test";

import { invalidateRegistry } from "../../../connectors/registry.ts";
import type { IntegrationOpRequest, IntegrationOpResult } from "../../../integrations/run-op.ts";
import { makeIntegrationActionRuntime } from "../../actions/runtime.ts";
import type { RunSnapshot } from "../context.ts";
import type { EngineDeps, EngineRunStore, EngineSessionOps, EngineStepRecord } from "../deps.ts";
import type { AutomationDefinition, BlockDef } from "../definition.ts";
import { interpretAutomation } from "../interpreter.ts";

/** The integration_action block driven through the REAL catalog runtime with
 * a fake Mode-A op — the end-to-end shape 2.G adds on top of the engine. */

const RUN = { runId: "autorun:auto-1:webhook:d1", automationId: "auto-1" };

function json(status: number, value: unknown): IntegrationOpResult {
  return {
    status,
    body: new TextEncoder().encode(JSON.stringify(value)),
    contentType: "application/json",
    truncated: false,
  };
}

function harness(blocks: BlockDef[], responses: IntegrationOpResult[]) {
  const definition: AutomationDefinition = {
    engine: 1,
    trigger: { kind: "manual" },
    blocks,
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
  const snapshot: RunSnapshot = {
    definition,
    inputs: {},
    automationId: RUN.automationId,
    automationName: "Actions test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-08-21T00:00:00Z" },
    aliases: [],
    startedAtMs: 1_000_000_000,
  };
  const stepRecords: Array<{ framePath: string; attempt: number; record: EngineStepRecord }> = [];
  const calls: Array<{ provider: string; req: IntegrationOpRequest }> = [];
  const store: EngineRunStore = {
    async loadSnapshot() {
      return snapshot;
    },
    async markRunning() {},
    async recordStep(_runId, framePath, attempt, record) {
      stepRecords.push({ framePath, attempt, record });
    },
    async finalizeRun() {},
    async listRunSessions() {
      return [];
    },
    async adoptSession() {
      return "foreign" as const;
    },
    async getSessionBinding() {
      return null;
    },
    async releaseConcurrency() {
      return null;
    },
  };
  const unusedSessions: EngineSessionOps = {
    createSession: () => Promise.reject(new Error("unused")),
    setSessionRelay: async () => {},
    sendPrompt: () => Promise.reject(new Error("unused")),
    endSession: () => Promise.reject(new Error("unused")),
    exec: () => Promise.reject(new Error("unused")),
    writeFiles: () => Promise.reject(new Error("unused")),
    getSession: () => Promise.resolve({ found: false as const }),
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store,
    sessions: unusedSessions,
    clock: { nowMs: () => 1_000_000_000 },
    integrationActions: makeIntegrationActionRuntime({
      runOp: async (provider, req) => {
        calls.push({ provider, req });
        const next = responses.shift();
        if (!next) throw new Error("fake runOp exhausted");
        return next;
      },
      connectors: { list: async () => [] },
    }),
  };
  return { deps, stepRecords, calls };
}

const STATUS_BLOCK: BlockDef = {
  id: "flag",
  type: "integration_action",
  config: {
    provider: "github",
    actionId: "set_commit_status",
    params: {
      repo: "acme/repo",
      sha: "abc",
      state: "success",
      context: "engrams/${{ run.automation.name | downcase }}",
    },
  },
};

beforeEach(() => invalidateRegistry());

describe("integration_action through the engine", () => {
  test("success: Liquid params render and outputs land in steps", async () => {
    const h = harness([STATUS_BLOCK], [json(201, {})]);
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const body = JSON.parse(h.calls[0]!.req.body as string) as Record<string, unknown>;
    expect(body["context"]).toBe("engrams/actions test");
    const step = h.stepRecords.filter((r) => r.framePath === "flag").at(-1)!;
    expect(step.record.status).toBe("succeeded");
    expect(step.record.outputs).toEqual({ status: 201 });
  });

  test("transient failures retry under the block policy; success on the second attempt", async () => {
    const block: BlockDef = { ...STATUS_BLOCK, retry: { attempts: 2, retryOn: "transient" } };
    const h = harness([block], [json(502, {}), json(201, {})]);
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.calls).toHaveLength(2);
    expect(h.stepRecords.filter((r) => r.framePath === "flag" && r.record.status === "failed")).toHaveLength(1);
  });

  test("permanent failures do not retry and fail the run", async () => {
    const block: BlockDef = { ...STATUS_BLOCK, retry: { attempts: 3, retryOn: "transient" } };
    const h = harness([block], [json(404, {})]);
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("integration_action_failed");
    expect(h.calls).toHaveLength(1);
  });
});
